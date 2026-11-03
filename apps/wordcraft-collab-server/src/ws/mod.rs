//! WebSocket endpoint `/api/v1/ws` (design §7, milestone C2.3).
//!
//! The user is authenticated from the `Authorization` header *before* the upgrade (401
//! otherwise). On the socket, every text frame is decoded with
//! `wordcraft_collab_protocol::Envelope::decode` (size limit and version first); the first message
//! must be `hello`. Then `join` (snapshot + every later commit), `submit`, `resync` and
//! `presence` go to the document actors through the [`Registry`]. Nothing received here is ever
//! run as a WordCraft command (§12).
//!
//! Outgoing messages share one bounded queue per connection. A participant whose actor dropped
//! it for falling behind is told with a `rejected` (`stale_revision`) for that document and must
//! `resync` or `join` again.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wordcraft_collab_protocol::{
    ClientId, ClientMessage, DocumentId, Envelope, MAX_MESSAGE_BYTES, OperationId, ProtocolError, RejectReason, ServerMessage,
};

use crate::AppState;
use crate::documents::{DocumentHandle, Registry, Rejection, Submission};
use crate::persistence::UserId;

/// Messages queued for one connection before its forwarders wait.
pub const OUTGOING_QUEUE: usize = 1024;

/// `GET /api/v1/ws`.
pub async fn handler(State(state): State<AppState>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let db = state.db.clone();
    let user = tokio::task::spawn_blocking(move || crate::auth::authenticate(&db, authorization.as_deref())).await;
    let user = match user {
        Ok(Ok(Some(user))) => user,
        Ok(Ok(None)) => return (StatusCode::UNAUTHORIZED, "sign in first").into_response(),
        Ok(Err(e)) => {
            tracing::error!("authenticating a WebSocket: {e}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(e) => {
            tracing::error!("authenticating a WebSocket: {e}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let documents = state.documents.clone();
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| Connection::new(user, documents).run(socket))
}

struct Connection {
    user: UserId,
    documents: Arc<Registry>,
    client: Option<ClientId>,
    joined: HashMap<DocumentId, (DocumentHandle, JoinHandle<()>)>,
    out: mpsc::Sender<ServerMessage>,
    outgoing: mpsc::Receiver<ServerMessage>,
}

impl Connection {
    fn new(user: UserId, documents: Arc<Registry>) -> Self {
        let (out, outgoing) = mpsc::channel(OUTGOING_QUEUE);
        Self { user, documents, client: None, joined: HashMap::new(), out, outgoing }
    }

    async fn run(mut self, mut socket: WebSocket) {
        loop {
            tokio::select! {
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        for reply in self.handle(text.as_str()).await {
                            if self.out.try_send(reply).is_err() {
                                return self.close();
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let reply = rejected(None, None, RejectReason::InvalidOperation, "binary frames are not part of the protocol");
                        if self.out.try_send(reply).is_err() {
                            return self.close();
                        }
                    }
                    // Pings are answered by the library; pongs need nothing.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return self.close(),
                },
                Some(message) = self.outgoing.recv() => {
                    if send(&mut socket, &message).await.is_err() {
                        return self.close();
                    }
                }
            }
        }
    }

    fn close(self) {
        for (_, (_, forwarder)) in self.joined {
            forwarder.abort();
        }
    }

    /// The replies to one text frame (commits reach the connection through its subscriptions).
    async fn handle(&mut self, text: &str) -> Vec<ServerMessage> {
        let message = match Envelope::<ClientMessage>::decode(text) {
            Ok(envelope) => envelope.message,
            Err(e) => return vec![protocol_error(&e)],
        };
        let Some(client) = self.client.clone() else {
            return match message {
                ClientMessage::Hello { client_id } => {
                    self.client = Some(client_id);
                    vec![ServerMessage::Welcome {}]
                }
                _ => vec![rejected(None, None, RejectReason::InvalidOperation, "say hello first")],
            };
        };
        match message {
            ClientMessage::Hello { .. } => vec![rejected(None, None, RejectReason::InvalidOperation, "already said hello")],
            ClientMessage::Join { document_id } => match self.join(document_id).await {
                Ok(()) => Vec::new(),
                Err(r) => vec![rejection(Some(document_id), None, r)],
            },
            ClientMessage::Submit { document_id, operation_id, client_id, base_revision, operation } => {
                if client_id != client {
                    return vec![rejected(Some(document_id), Some(operation_id), RejectReason::Forbidden, "client id differs from hello")];
                }
                let Some((handle, _)) = self.joined.get(&document_id) else {
                    return vec![rejected(Some(document_id), Some(operation_id), RejectReason::InvalidOperation, "join the document first")];
                };
                let submission = Submission { user: self.user, client_id, operation_id, base_revision, operation };
                match handle.submit(submission).await {
                    // A new commit reaches this connection through its subscription, like everyone
                    // else's; a duplicate was not published, so it is answered here.
                    Ok(submitted) if submitted.duplicate => vec![submitted.message],
                    Ok(_) => Vec::new(),
                    Err(r) => vec![rejection(Some(document_id), Some(operation_id), r)],
                }
            }
            // A resync must restore the subscription as well as the content. A fresh join is
            // atomic in the actor and carries current capabilities, even when backpressure
            // dropped a permission notification. The protocol permits a snapshot response.
            // While the subscription is alive, only what was missed (and the current capability)
            // is sent. If the actor dropped it (backpressure, revocation) or there is none, a
            // fresh join restores it with a snapshot.
            ClientMessage::Resync { document_id, since_revision } => {
                let live = self.joined.get(&document_id).filter(|(_, forwarder)| !forwarder.is_finished()).map(|(handle, _)| handle.clone());
                let result = match live {
                    Some(handle) => handle.resync(self.user, since_revision).await,
                    None => self.join(document_id).await.map(|()| Vec::new()),
                };
                result.unwrap_or_else(|r| vec![rejection(Some(document_id), None, r)])
            }
            // Presence (participants and carets) is milestone C5.3; until then it is accepted and
            // dropped, never persisted (§10).
            ClientMessage::Presence { document_id, .. } => {
                let Some((handle, _)) = self.joined.get(&document_id) else {
                    return vec![rejected(Some(document_id), None, RejectReason::Forbidden, "join an accessible document first")];
                };
                match handle.access(self.user).await {
                    Ok(_) => Vec::new(),
                    Err(r) => vec![rejection(Some(document_id), None, r)],
                }
            }
        }
    }

    /// Join (or join again: a fresh snapshot replaces the old subscription).
    async fn join(&mut self, document_id: DocumentId) -> Result<(), Rejection> {
        // Finish the old forwarder before taking the snapshot. Replies and subscription events
        // share one FIFO, so an old grant can never overtake a newer read-only snapshot.
        if let Some((_, old)) = self.joined.remove(&document_id) {
            old.abort();
            let _ = old.await;
        }
        let handle = self.documents.get(document_id).await?;
        let joined = handle.join(self.user).await?;
        self.out
            .try_send(joined.snapshot)
            .map_err(|_| Rejection { reason: RejectReason::Unavailable, message: "connection queue is full; reconnect to synchronize".into() })?;
        let out = self.out.clone();
        let mut updates = joined.updates;
        let forwarder = tokio::spawn(async move {
            while let Some(message) = updates.recv().await {
                let revoked = matches!(message, ServerMessage::Permissions { can_read: false, .. });
                if out.send(message).await.is_err() {
                    return;
                }
                if revoked {
                    return;
                }
            }
            // The actor dropped this subscription (too far behind) or stopped.
            let _ = out.send(rejected(Some(document_id), None, RejectReason::StaleRevision, "fell behind: resync or join again")).await;
        });
        self.joined.insert(document_id, (handle, forwarder));
        Ok(())
    }
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    let text = Envelope::new(message).encode().map_err(|e| tracing::error!("encoding {message:?}: {e}"))?;
    socket.send(Message::Text(text.into())).await.map_err(|_| ())
}

fn rejected(document_id: Option<DocumentId>, operation_id: Option<OperationId>, reason: RejectReason, message: &str) -> ServerMessage {
    ServerMessage::Rejected { document_id, operation_id, reason, message: message.into() }
}

fn rejection(document_id: Option<DocumentId>, operation_id: Option<OperationId>, r: Rejection) -> ServerMessage {
    ServerMessage::Rejected { document_id, operation_id, reason: r.reason, message: r.message }
}

fn protocol_error(e: &ProtocolError) -> ServerMessage {
    rejected(None, None, e.reason(), &e.to_string())
}
