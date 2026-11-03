//! WebSocket transport (design §7, milestones C3.1 and C3.3), behind the non-default `transport`
//! feature so wasm builds are unaffected.
//!
//! [`spawn`] starts a Tokio task that owns a [`SharedSession`] and carries its messages:
//! - `ws://` or `wss://` (rustls with the operating system's certificate store, so a homelab CA
//!   or Tailscale certificates work); the token goes in the `Authorization` header, never in the
//!   URL;
//! - on connecting, whatever [`SharedSession::connected`] says (`hello`, `join`, pending
//!   resends); every frame from the server goes through [`SharedSession::receive`], size-capped
//!   at [`MAX_SERVER_MESSAGE_BYTES`];
//! - when the connection drops, [`SharedSession::disconnected`] and a new attempt after an
//!   exponential backoff (reset once connected).
//!
//! The editor talks to it through [`Client`]: `submit`, `acknowledge_conflict`, `replica`,
//! `close`, and a stream of [`Event`]s. All protocol decisions stay in the sans-I/O core.

#[cfg(feature = "transport")]
pub use native::*;

#[cfg(feature = "transport")]
mod native {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    use wordcraft_collab_model::SharedDocument;
    use wordcraft_collab_protocol::{DocumentOperation, Envelope, MAX_SERVER_MESSAGE_BYTES, OperationId, Revision, ServerMessage};

    use crate::coordinator::{Event, Output, SharedSession, SubmitError};

    /// Events queued for the editor before the transport waits for it to read them.
    pub const EVENT_QUEUE: usize = 1024;

    /// Where and how to connect.
    #[derive(Clone, Debug)]
    pub struct TransportConfig {
        /// `wss://host/api/v1/ws` (or `ws://` on a trusted network or loopback).
        pub url: String,
        /// The bearer token (local token now, OIDC access token with C5.1).
        pub token: String,
        /// First wait after a failed or dropped connection; doubles up to `backoff_max`.
        pub backoff_min: Duration,
        pub backoff_max: Duration,
    }

    impl TransportConfig {
        pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
            Self { url: url.into(), token: token.into(), backoff_min: Duration::from_millis(250), backoff_max: Duration::from_secs(30) }
        }
    }

    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum ClientError {
        #[error(transparent)]
        Submit(#[from] SubmitError),
        #[error("the connection task has stopped")]
        Stopped,
    }

    enum Command {
        Submit { operation_id: OperationId, operation: DocumentOperation, reply: oneshot::Sender<Result<(), SubmitError>> },
        AcknowledgeConflict,
        Replica { reply: oneshot::Sender<Option<(Revision, SharedDocument)>> },
        Close,
    }

    /// The editor's side of a shared document's connection.
    pub struct Client {
        commands: mpsc::Sender<Command>,
        /// What happened: snapshots, applied commits, rejections, status changes.
        pub events: mpsc::Receiver<Event>,
        task: JoinHandle<()>,
    }

    /// Start connecting `session` (a fresh [`SharedSession`]) with `config`. Needs a Tokio
    /// runtime.
    pub fn spawn(config: TransportConfig, session: SharedSession) -> Client {
        let (commands, command_rx) = mpsc::channel(64);
        let (event_tx, events) = mpsc::channel(EVENT_QUEUE);
        let task = tokio::spawn(run(config, session, command_rx, event_tx));
        Client { commands, events, task }
    }

    impl Client {
        /// Submit a local edit made against the current replica; it shows up as an
        /// [`Event::Applied`] once committed.
        pub async fn submit(&self, operation_id: OperationId, operation: DocumentOperation) -> Result<(), ClientError> {
            let (reply, answer) = oneshot::channel();
            self.commands.send(Command::Submit { operation_id, operation, reply }).await.map_err(|_| ClientError::Stopped)?;
            Ok(answer.await.map_err(|_| ClientError::Stopped)??)
        }

        pub async fn acknowledge_conflict(&self) -> Result<(), ClientError> {
            self.commands.send(Command::AcknowledgeConflict).await.map_err(|_| ClientError::Stopped)
        }

        /// The replica and its revision (none before the first snapshot).
        pub async fn replica(&self) -> Result<Option<(Revision, SharedDocument)>, ClientError> {
            let (reply, answer) = oneshot::channel();
            self.commands.send(Command::Replica { reply }).await.map_err(|_| ClientError::Stopped)?;
            answer.await.map_err(|_| ClientError::Stopped)
        }

        /// Close the connection and stop reconnecting.
        pub async fn close(self) {
            let _ = self.commands.send(Command::Close).await;
            let _ = self.task.await;
        }
    }

    /// How a connection ended.
    enum Ended {
        /// Dropped or refused: try again.
        Lost,
        /// The editor closed it (or went away): stop.
        Closed,
    }

    async fn run(config: TransportConfig, mut session: SharedSession, mut commands: mpsc::Receiver<Command>, events: mpsc::Sender<Event>) {
        let mut backoff = config.backoff_min;
        loop {
            match connect(&config).await {
                Ok(socket) => {
                    backoff = config.backoff_min;
                    if let Ended::Closed = drive(socket, &mut session, &mut commands, &events).await {
                        return;
                    }
                }
                Err(e) => log::info!("collaboration: cannot connect to {}: {e}", config.url),
            }
            if emit(&events, session.disconnected().events).await.is_err() {
                return;
            }
            // Wait before the next attempt, still answering the editor (edits are paused).
            let wait = tokio::time::sleep(backoff);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    () = &mut wait => break,
                    command = commands.recv() => match command {
                        None | Some(Command::Close) => return,
                        Some(command) => {
                            if let Some(out) = handle_command(command, &mut session) {
                                // Nothing can be sent while disconnected; only events matter.
                                if emit(&events, out.events).await.is_err() {
                                    return;
                                }
                            }
                        }
                    },
                }
            }
            backoff = (backoff * 2).min(config.backoff_max);
        }
    }

    type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    async fn connect(config: &TransportConfig) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = config.url.as_str().into_client_request()?;
        let token = format!("Bearer {}", config.token)
            .parse()
            .map_err(|_| tokio_tungstenite::tungstenite::Error::Url(tokio_tungstenite::tungstenite::error::UrlError::NoHostName))?;
        request.headers_mut().insert("authorization", token);
        let limits = WebSocketConfig::default().max_message_size(Some(MAX_SERVER_MESSAGE_BYTES)).max_frame_size(Some(MAX_SERVER_MESSAGE_BYTES));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(limits), false).await?;
        Ok(socket)
    }

    async fn drive(socket: Socket, session: &mut SharedSession, commands: &mut mpsc::Receiver<Command>, events: &mpsc::Sender<Event>) -> Ended {
        let (mut sink, mut stream) = socket.split();
        if flush(&mut sink, events, session.connected()).await.is_err() {
            return Ended::Lost;
        }
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => match Envelope::<ServerMessage>::decode_with_limit(text.as_str(), MAX_SERVER_MESSAGE_BYTES) {
                        Ok(envelope) => {
                            if flush(&mut sink, events, session.receive(envelope.message)).await.is_err() {
                                return Ended::Lost;
                            }
                        }
                        Err(e) => log::warn!("collaboration: undecodable server message: {e}"),
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
                    Some(Ok(_)) => {}
                },
                command = commands.recv() => match command {
                    None | Some(Command::Close) => {
                        let _ = sink.close().await;
                        return Ended::Closed;
                    }
                    Some(command) => {
                        if let Some(out) = handle_command(command, session)
                            && flush(&mut sink, events, out).await.is_err()
                        {
                            return Ended::Lost;
                        }
                    }
                },
            }
        }
    }

    /// Apply an editor command to the session; what to send and emit, if anything.
    fn handle_command(command: Command, session: &mut SharedSession) -> Option<Output> {
        match command {
            Command::Submit { operation_id, operation, reply } => {
                let result = session.submit(operation_id, operation);
                let out = result.as_ref().ok().map(|message| Output { send: vec![message.clone()], events: Vec::new() });
                let _ = reply.send(result.map(|_| ()));
                out
            }
            Command::AcknowledgeConflict => Some(session.acknowledge_conflict()),
            Command::Replica { reply } => {
                let _ = reply.send(session.replica().map(|r| (session.revision(), r.clone())));
                None
            }
            Command::Close => None,
        }
    }

    /// Send `out.send` in order, then hand `out.events` to the editor.
    async fn flush(sink: &mut futures_util::stream::SplitSink<Socket, Message>, events: &mpsc::Sender<Event>, out: Output) -> Result<(), ()> {
        for message in &out.send {
            let text = Envelope::new(message).encode().map_err(|e| log::error!("collaboration: encoding {message:?}: {e}"))?;
            sink.send(Message::Text(text.into())).await.map_err(|_| ())?;
        }
        emit(events, out.events).await
    }

    async fn emit(events: &mpsc::Sender<Event>, list: Vec<Event>) -> Result<(), ()> {
        for event in list {
            events.send(event).await.map_err(|_| ())?;
        }
        Ok(())
    }
}
