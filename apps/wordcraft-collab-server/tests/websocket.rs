//! C2.3: the WebSocket endpoint end to end — a real server on a loopback port, real WebSocket
//! clients, local tokens (design §13 "Integración con varios clientes", first part).

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;
use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::{
    ClientId, ClientMessage, DocumentId, DocumentOperation, Envelope, MAX_MESSAGE_BYTES, OperationId, RejectReason, ServerMessage,
};
use wordcraft_collab_server::persistence::{Role, UserId, repository};
use wordcraft_collab_server::{AppState, Config, auth, documents, serve};
use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));

struct Server {
    url: String,
    dir: PathBuf,
    state: AppState,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn server(name: &str) -> Server {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-ws-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        database_path: dir.join("wordcraft.db"),
        document_storage: dir.join("documents"),
        oidc: None,
    };
    let state = AppState::open(&config).unwrap();
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await.unwrap();
    let url = format!("ws://{}/api/v1/ws", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(serve(listener, state.clone(), async {
        let _ = stopped.await;
    }));
    Server { url, dir, state, stop: Some(stop) }
}

/// Ana (owner), Bea (editor), Vic (viewer) with tokens, and [`DOC`] holding "Hello world".
fn people(server: &Server) -> [(UserId, String); 3] {
    let [ana, bea, vic] = ["Ana", "Bea", "Vic"].map(|n| auth::create_local_user(&server.state.db, n).unwrap());
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hello world", &CharProps::default()).unwrap();
    documents::create(&server.state.db, &server.state.storage, DOC, "Plan", ana.0, doc, || BlockId(Uuid::from_u128(0xb1))).unwrap();
    let mut conn = server.state.db.lock().unwrap();
    repository::set_member(&mut conn, DOC, bea.0, Role::Editor).unwrap();
    repository::set_member(&mut conn, DOC, vic.0, Role::Viewer).unwrap();
    [ana, bea, vic]
}

async fn connect(server: &Server, token: &str) -> Result<Socket, WsError> {
    let mut request = server.url.as_str().into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    Ok(connect_async(request).await?.0)
}

async fn send(socket: &mut Socket, message: ClientMessage) {
    socket.send(Message::Text(Envelope::new(message).encode().unwrap().into())).await.unwrap();
}

async fn recv(socket: &mut Socket) -> ServerMessage {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next()).await.expect("no message within 5 s").unwrap().unwrap();
        if let Message::Text(text) = frame {
            return Envelope::<ServerMessage>::decode(text.as_str()).unwrap().message;
        }
    }
}

/// Nothing more arrives for a moment.
async fn quiet(socket: &mut Socket) {
    assert!(tokio::time::timeout(Duration::from_millis(200), socket.next()).await.is_err(), "unexpected message");
}

/// Connect, hello, join: returns the socket and the replica from the snapshot.
async fn participant(server: &Server, token: &str, client: &str) -> (Socket, SharedDocument, u64) {
    let mut socket = connect(server, token).await.unwrap();
    send(&mut socket, ClientMessage::Hello { client_id: ClientId::new(client).unwrap() }).await;
    assert_eq!(recv(&mut socket).await, ServerMessage::Welcome {});
    send(&mut socket, ClientMessage::Join { document_id: DOC }).await;
    let ServerMessage::Snapshot { revision, snapshot, .. } = recv(&mut socket).await else { panic!("no snapshot") };
    (socket, SharedDocument::from_json(snapshot).unwrap(), revision)
}

fn submit(client: &str, n: u128, base_revision: u64, operation: DocumentOperation) -> ClientMessage {
    ClientMessage::Submit {
        document_id: DOC,
        operation_id: OperationId(Uuid::from_u128(n)),
        client_id: ClientId::new(client).unwrap(),
        base_revision,
        operation,
    }
}

fn insert(replica: &SharedDocument, offset: u32, text: &str) -> DocumentOperation {
    DocumentOperation::InsertText { block_id: replica.ids().ids()[0], offset, text: text.into() }
}

fn text(replica: &SharedDocument) -> String {
    replica.document().para(StoryRef::Body, &Path::top(0)).unwrap().text.clone()
}

/// Apply a `committed` to a replica; returns its revision.
fn apply_commit(replica: &mut SharedDocument, message: &ServerMessage) -> u64 {
    let ServerMessage::Committed { revision, operation, .. } = message else { panic!("not a commit: {message:?}") };
    replica.apply(operation).unwrap();
    *revision
}

#[tokio::test(flavor = "multi_thread")]
async fn three_clients_see_the_same_document() {
    let server = server("three").await;
    let [(_, ana), (_, bea), (_, vic)] = people(&server);
    let (mut a, mut ra, rev) = participant(&server, &ana, "desktop-a").await;
    let (mut b, mut rb, _) = participant(&server, &bea, "desktop-b").await;
    let (mut v, mut rv, _) = participant(&server, &vic, "desktop-v").await;
    assert_eq!(rev, 0);

    // A and B edit concurrently at revision 0 (design §5.3); everyone gets revisions 1 and 2.
    send(&mut a, submit("desktop-a", 1, 0, insert(&ra, 6, "beautiful "))).await;
    send(&mut b, submit("desktop-b", 2, 0, insert(&rb, 11, "!"))).await;
    for (socket, replica) in [(&mut a, &mut ra), (&mut b, &mut rb), (&mut v, &mut rv)] {
        let first = recv(socket).await;
        let second = recv(socket).await;
        assert_eq!((apply_commit(replica, &first), apply_commit(replica, &second)), (1, 2));
        assert_eq!(text(replica), "Hello beautiful world!");
    }
    assert_eq!(ra, rb);
    assert_eq!(rb, rv);
    assert_eq!(ra, server.state.documents.get(DOC).await.unwrap().current().await.unwrap().1);

    // The viewer may not edit; nothing is published.
    send(&mut v, submit("desktop-v", 3, 2, insert(&rv, 0, "v"))).await;
    assert!(matches!(recv(&mut v).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
    quiet(&mut a).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_membership_changes_enforce_every_websocket_operation() {
    let server = server("permissions").await;
    let [(owner, _), (editor, bea), (viewer, vic)] = people(&server);
    let (mut b, replica, _) = participant(&server, &bea, "desktop-b").await;
    let (mut v, _, _) = participant(&server, &vic, "desktop-v").await;
    let actor = server.state.documents.get(DOC).await.unwrap();
    actor.change_member(owner, editor, Some(Role::Viewer)).await.unwrap();
    assert!(matches!(recv(&mut b).await, ServerMessage::Permissions { can_read: true, can_edit: false, .. }));
    // The subscription is alive: resync restates the capability and sends nothing else.
    send(&mut b, ClientMessage::Resync { document_id: DOC, since_revision: 0 }).await;
    assert!(matches!(recv(&mut b).await, ServerMessage::Permissions { can_read: true, can_edit: false, .. }));
    send(&mut b, submit("desktop-b", 20, 0, insert(&replica, 0, "no"))).await;
    assert!(matches!(recv(&mut b).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
    assert_eq!(actor.current().await.unwrap().0, 0);
    actor.change_member(owner, viewer, Some(Role::Editor)).await.unwrap();
    assert!(matches!(recv(&mut v).await, ServerMessage::Permissions { can_read: true, can_edit: true, .. }));
    send(&mut v, submit("desktop-v", 21, 0, insert(&replica, 0, ">"))).await;
    assert!(matches!(recv(&mut v).await, ServerMessage::Committed { revision: 1, .. }));
    assert!(matches!(recv(&mut b).await, ServerMessage::Committed { revision: 1, .. }));
    actor.change_member(owner, editor, None).await.unwrap();
    assert!(matches!(recv(&mut b).await, ServerMessage::Permissions { can_read: false, can_edit: false, .. }));
    send(&mut v, submit("desktop-v", 22, 1, insert(&replica, 0, "private"))).await;
    assert!(matches!(recv(&mut v).await, ServerMessage::Committed { revision: 2, .. }));
    quiet(&mut b).await;
    for request in [
        submit("desktop-b", 23, 2, insert(&replica, 0, "no")),
        ClientMessage::Presence { document_id: DOC, cursor: None },
        ClientMessage::Resync { document_id: DOC, since_revision: 0 },
        ClientMessage::Join { document_id: DOC },
    ] {
        send(&mut b, request).await;
        assert!(matches!(recv(&mut b).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
    }
    assert_eq!(actor.current().await.unwrap().0, 2);
    actor.change_member(owner, editor, Some(Role::Editor)).await.unwrap();
    send(&mut b, ClientMessage::Join { document_id: DOC }).await;
    assert!(matches!(recv(&mut b).await, ServerMessage::Snapshot { revision: 2, can_edit: true, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_ack_is_recovered_without_duplicating() {
    let server = server("ack").await;
    let [(_, ana), ..] = people(&server);
    let (mut a, replica, _) = participant(&server, &ana, "desktop-a").await;
    send(&mut a, submit("desktop-a", 1, 0, insert(&replica, 0, ">"))).await;
    // The connection drops before the commit is read (the server got the frame first).
    drop(a);
    let actor = server.state.documents.get(DOC).await.unwrap();
    for _ in 0..500 {
        if actor.current().await.unwrap().0 == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Reconnect: resync from the last confirmed revision (0), then resend the unconfirmed op.
    let mut a = connect(&server, &ana).await.unwrap();
    send(&mut a, ClientMessage::Hello { client_id: ClientId::new("desktop-a").unwrap() }).await;
    recv(&mut a).await;
    send(&mut a, ClientMessage::Join { document_id: DOC }).await;
    let ServerMessage::Snapshot { revision, .. } = recv(&mut a).await else { panic!() };
    assert_eq!(revision, 1, "the operation was committed before the drop");
    // Joined, so resync is incremental: the capability, then the missed commit (no snapshot).
    send(&mut a, ClientMessage::Resync { document_id: DOC, since_revision: 0 }).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Permissions { can_read: true, can_edit: true, .. }));
    assert!(matches!(recv(&mut a).await, ServerMessage::Committed { revision: 1, .. }));
    send(&mut a, submit("desktop-a", 1, 0, insert(&replica, 0, ">"))).await;
    assert_eq!(
        recv(&mut a).await,
        ServerMessage::Committed {
            document_id: DOC,
            revision: 1,
            operation_id: OperationId(Uuid::from_u128(1)),
            client_id: ClientId::new("desktop-a").unwrap(),
            operation: insert(&replica, 0, ">"),
        },
        "the resend gets the original commit"
    );
    quiet(&mut a).await;
    let (revision, state) = server.state.documents.get(DOC).await.unwrap().current().await.unwrap();
    assert_eq!((revision, text(&state).as_str()), (1, ">Hello world"));
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_are_authenticated_and_messages_checked() {
    let server = server("checks").await;
    let [(_, ana), ..] = people(&server);
    // No token, or a wrong one: refused before the upgrade.
    for token in ["", "wct_0000000000000000000000000000000000000000000000000000000000000000"] {
        let err = connect(&server, token).await.unwrap_err();
        assert!(matches!(err, WsError::Http(ref r) if r.status() == 401), "{err:?}");
    }
    let mut a = connect(&server, &ana).await.unwrap();
    // Hello comes first.
    send(&mut a, ClientMessage::Join { document_id: DOC }).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::InvalidOperation, .. }));
    send(&mut a, ClientMessage::Hello { client_id: ClientId::new("desktop-a").unwrap() }).await;
    recv(&mut a).await;
    // Other protocol versions, junk and binary frames are refused, and the connection stays up.
    a.send(Message::Text(r#"{"protocol_version":3,"type":"hello"}"#.into())).await.unwrap();
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::UnsupportedVersion, .. }));
    a.send(Message::Text("not json".into())).await.unwrap();
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::InvalidOperation, .. }));
    a.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::InvalidOperation, .. }));
    // Submitting needs a join; a client id other than the hello's is refused.
    let replica = SharedDocument::new(Document::default(), || BlockId(Uuid::from_u128(0xb1))).unwrap();
    send(&mut a, submit("desktop-a", 1, 0, insert(&replica, 0, "x"))).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::InvalidOperation, .. }));
    send(&mut a, ClientMessage::Join { document_id: DOC }).await;
    recv(&mut a).await;
    send(&mut a, submit("desktop-z", 2, 0, insert(&replica, 0, "x"))).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
    // A document that doesn't exist looks like one you may not see.
    send(&mut a, ClientMessage::Join { document_id: DocumentId(Uuid::from_u128(0xbad)) }).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
    // Over the size limit, the server closes the connection rather than parse it.
    let huge = format!(r#"{{"protocol_version":2,"type":"hello","client_id":"{}"}}"#, "a".repeat(MAX_MESSAGE_BYTES));
    let _ = a.send(Message::Text(huge.into())).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match a.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the connection stayed open after an oversized message");
}

#[tokio::test(flavor = "multi_thread")]
async fn resync_without_a_subscription_rejoins_with_a_snapshot() {
    let server = server("resync").await;
    let [(_, ana), ..] = people(&server);
    let mut a = connect(&server, &ana).await.unwrap();
    send(&mut a, ClientMessage::Hello { client_id: ClientId::new("desktop-a").unwrap() }).await;
    recv(&mut a).await;
    // Not joined on this connection (or the actor dropped it): resync joins, with a snapshot.
    send(&mut a, ClientMessage::Resync { document_id: DOC, since_revision: 0 }).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Snapshot { revision: 0, can_edit: true, .. }));
    // And from then on it is subscribed: a second resync is incremental.
    send(&mut a, ClientMessage::Resync { document_id: DOC, since_revision: 0 }).await;
    assert!(matches!(recv(&mut a).await, ServerMessage::Permissions { can_read: true, can_edit: true, .. }));
}

/// The next `presence` message, skipping anything else.
async fn next_presence(socket: &mut Socket) -> Vec<wordcraft_collab_protocol::messages::Participant> {
    loop {
        if let ServerMessage::Presence { participants, .. } = recv(socket).await {
            return participants;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn participants_and_their_carets_follow_the_document() {
    use wordcraft_collab_protocol::messages::Anchor;
    let server = server("presence").await;
    let [(_, ana), (_, bea), (_, vic)] = people(&server);
    let (mut a, ra, _) = participant(&server, &ana, "desktop-a").await;
    let (mut b, _, _) = participant(&server, &bea, "desktop-b").await;
    let block = ra.ids().ids()[0];

    // Ana's caret after "Hello" reaches Bea, with Ana's name.
    send(&mut a, ClientMessage::Presence { document_id: DOC, cursor: Some(Anchor { block_id: block, offset: 5 }) }).await;
    let seen = next_presence(&mut b).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].client_id.as_str(), seen[0].display_name.as_str(), seen[0].cursor),
        ("desktop-a", "Ana", Some(Anchor { block_id: block, offset: 5 }))
    );

    // Bea types before it; Vic, joining later, sees Ana's caret moved with the text.
    send(&mut b, submit("desktop-b", 1, 0, insert(&ra, 0, ">> "))).await;
    let (mut v, _, _) = participant(&server, &vic, "desktop-v").await;
    let seen = next_presence(&mut v).await;
    assert_eq!(seen[0].cursor, Some(Anchor { block_id: block, offset: 8 }));

    // A caret that doesn't fit the document is shown as "here, without a caret".
    tokio::time::sleep(Duration::from_millis(80)).await;
    send(&mut a, ClientMessage::Presence { document_id: DOC, cursor: Some(Anchor { block_id: BlockId(Uuid::from_u128(99)), offset: 0 }) }).await;
    let seen = next_presence(&mut v).await;
    assert_eq!(seen.iter().find(|p| p.client_id.as_str() == "desktop-a").map(|p| p.cursor), Some(None));

    // Ana leaves: she is no longer listed.
    drop(a);
    let mut gone = false;
    for _ in 0..5 {
        if next_presence(&mut v).await.iter().all(|p| p.client_id.as_str() != "desktop-a") {
            gone = true;
            break;
        }
    }
    assert!(gone, "a closed connection leaves the participant list");
    // Presence for a document one hasn't joined is refused.
    send(&mut v, ClientMessage::Presence { document_id: DocumentId(Uuid::from_u128(7)), cursor: None }).await;
    assert!(matches!(recv(&mut v).await, ServerMessage::Rejected { reason: RejectReason::Forbidden, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn floods_are_refused_without_closing_a_well_behaved_connection() {
    let server = server("flood").await;
    let [(_, ana), ..] = people(&server);
    let (mut a, _, _) = participant(&server, &ana, "desktop-a").await;
    // 400 messages at once: the burst passes, the rest is refused unread.
    for _ in 0..400 {
        a.send(Message::Text(Envelope::new(ClientMessage::Presence { document_id: DOC, cursor: None }).encode().unwrap().into())).await.unwrap();
    }
    let mut limited = 0;
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_millis(300), a.next()).await {
        if let Message::Text(t) = frame
            && matches!(
                Envelope::<ServerMessage>::decode_with_limit(t.as_str(), usize::MAX).unwrap().message,
                ServerMessage::Rejected { reason: RejectReason::RateLimited, .. }
            )
        {
            limited += 1;
        }
    }
    assert!(limited >= 150, "only {limited} refused");
    // A moment later the connection works as before.
    tokio::time::sleep(Duration::from_millis(500)).await;
    send(&mut a, ClientMessage::Join { document_id: DOC }).await;
    loop {
        if matches!(recv(&mut a).await, ServerMessage::Snapshot { .. }) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_per_user_and_documents_per_connection_are_bounded() {
    use wordcraft_collab_server::ws::{MAX_CONNECTIONS_PER_USER, MAX_JOINED};
    let server = server("limits").await;
    let [(ana_id, ana), ..] = people(&server);
    let mut open = Vec::new();
    for _ in 0..MAX_CONNECTIONS_PER_USER {
        open.push(connect(&server, &ana).await.unwrap());
    }
    let err = connect(&server, &ana).await.unwrap_err();
    assert!(matches!(err, WsError::Http(ref r) if r.status() == 429), "{err:?}");
    // Closing one frees its slot.
    drop(open.pop());
    let mut a = None;
    for _ in 0..50 {
        if let Ok(s) = connect(&server, &ana).await {
            a = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut a = a.expect("a closed connection gives its slot back");
    drop(open);

    // At most MAX_JOINED documents on one connection.
    send(&mut a, ClientMessage::Hello { client_id: ClientId::new("desktop-a").unwrap() }).await;
    recv(&mut a).await;
    for n in 0..=MAX_JOINED as u128 {
        let id = DocumentId(Uuid::from_u128(0x1000 + n));
        documents::create(&server.state.db, &server.state.storage, id, "Doc", ana_id, Document::default(), move || {
            BlockId(Uuid::from_u128(0x2000 + n))
        })
        .unwrap();
        send(&mut a, ClientMessage::Join { document_id: id }).await;
        let reply = recv(&mut a).await;
        if (n as usize) < MAX_JOINED {
            assert!(matches!(reply, ServerMessage::Snapshot { .. }), "{n}: {reply:?}");
        } else {
            assert!(matches!(reply, ServerMessage::Rejected { reason: RejectReason::RateLimited, .. }), "{reply:?}");
        }
    }
}
