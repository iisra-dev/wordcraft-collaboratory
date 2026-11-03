//! C3.1 + C3.3: the real client transport (`wordcraft-collab-client`, feature `transport`)
//! against a real server: two clients join, edit and converge; the server goes away and comes
//! back on the same port with the same data; both clients reconnect by themselves, get a fresh
//! snapshot and edit on.
//!
//! The server runs on its own thread and runtime so "going away" really closes every socket
//! (dropping that runtime cancels its connection tasks).

use std::path::PathBuf;
use std::time::Duration;

use uuid::Uuid;
use wordcraft_collab_client::connection::{self, Client, TransportConfig};
use wordcraft_collab_client::{CollaborationState, Event, SharedSession};
use wordcraft_collab_model::BlockId;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId};
use wordcraft_collab_server::persistence::{Role, repository};
use wordcraft_collab_server::{AppState, Config, auth, documents, serve};
use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));
const BLOCK: BlockId = BlockId(Uuid::from_u128(0xb1));

/// A server process stand-in: its own thread and runtime, stopped by dropping it.
struct Running {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(config: Config) -> Self {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let state = AppState::open(&config).unwrap();
                let listener = tokio::net::TcpListener::bind(config.listen_addr).await.unwrap();
                ready_tx.send(()).unwrap();
                tokio::select! {
                    _ = serve(listener, state, std::future::pending()) => {}
                    _ = stopped => {}
                }
            });
            // Dropping the runtime cancels every connection task: the sockets close.
            drop(runtime);
        });
        ready.recv().unwrap();
        Running { stop: Some(stop), thread: Some(thread) }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn config(dir: &std::path::Path, port: u16) -> Config {
    Config {
        listen_addr: format!("127.0.0.1:{port}").parse().unwrap(),
        database_path: dir.join("wordcraft.db"),
        document_storage: dir.join("documents"),
        oidc: None,
    }
}

/// Ana (owner) and Bea (editor) with tokens, and [`DOC`] with "Hello".
fn setup(dir: &PathBuf) -> (String, String) {
    let _ = std::fs::remove_dir_all(dir);
    let state = AppState::open(&config(dir, 0)).unwrap();
    let (ana, ana_token) = auth::create_local_user(&state.db, "Ana").unwrap();
    let (bea, bea_token) = auth::create_local_user(&state.db, "Bea").unwrap();
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hello", &CharProps::default()).unwrap();
    documents::create(&state.db, &state.storage, DOC, "Plan", ana, doc, || BLOCK).unwrap();
    repository::set_member(&mut state.db.lock().unwrap(), DOC, bea, Role::Editor).unwrap();
    (ana_token, bea_token)
}

fn client(port: u16, token: &str, name: &str) -> Client {
    let mut config = TransportConfig::new(format!("ws://127.0.0.1:{port}/api/v1/ws"), token);
    config.backoff_min = Duration::from_millis(50);
    config.backoff_max = Duration::from_millis(400);
    connection::spawn(config, SharedSession::new(DOC, ClientId::new(name).unwrap()))
}

/// Read events until `until` matches one (within 10 s); returns them all.
async fn wait_for(client: &mut Client, until: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = client.events.recv().await {
            let done = until(&event);
            seen.push(event);
            if done {
                return true;
            }
        }
        false
    })
    .await;
    assert!(matches!(found, Ok(true)), "never got the awaited event; saw {seen:?}");
    seen
}

fn synchronized(event: &Event) -> bool {
    matches!(event, Event::State(CollaborationState::Synchronized { .. }))
}

fn applied(revision: u64) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::Applied { revision: r, .. } if *r == revision)
}

async fn text(client: &Client) -> (u64, String) {
    let (revision, replica) = client.replica().await.unwrap().unwrap();
    (revision, replica.document().para(StoryRef::Body, &Path::top(0)).unwrap().text.clone())
}

fn insert(offset: u32, text: &str) -> DocumentOperation {
    DocumentOperation::InsertText { block_id: BLOCK, offset, text: text.into() }
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_converge_and_survive_a_server_restart() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-client-{}", std::process::id()));
    let (ana_token, bea_token) = setup(&dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server = Running::start(config(&dir, port));

    let mut a = client(port, &ana_token, "desktop-a");
    let mut b = client(port, &bea_token, "desktop-b");
    wait_for(&mut a, synchronized).await;
    wait_for(&mut b, synchronized).await;

    // Both edit; each sees both commits, its own marked as such.
    a.submit(OperationId(Uuid::from_u128(1)), insert(5, " world")).await.unwrap();
    let seen = wait_for(&mut a, applied(1)).await;
    assert!(seen.iter().any(|e| matches!(e, Event::Applied { own: true, .. })));
    b.submit(OperationId(Uuid::from_u128(2)), insert(0, "¡")).await.unwrap();
    wait_for(&mut a, applied(2)).await;
    wait_for(&mut b, applied(2)).await;
    assert_eq!(text(&a).await, (2, "¡Hello world".into()));
    assert_eq!(text(&b).await, text(&a).await);

    // The server goes away: both clients notice and stop accepting edits.
    drop(server);
    wait_for(&mut a, |e| matches!(e, Event::State(CollaborationState::Reconnecting { .. }))).await;
    wait_for(&mut b, |e| matches!(e, Event::State(CollaborationState::Reconnecting { .. }))).await;
    assert!(a.submit(OperationId(Uuid::from_u128(3)), insert(0, "x")).await.is_err());

    // It comes back with the same data: both reconnect on their own and carry on.
    let _server = Running::start(config(&dir, port));
    wait_for(&mut a, synchronized).await;
    wait_for(&mut b, synchronized).await;
    assert_eq!(text(&a).await, (2, "¡Hello world".into()));
    b.submit(OperationId(Uuid::from_u128(4)), insert(13, "!")).await.unwrap();
    wait_for(&mut a, applied(3)).await;
    wait_for(&mut b, applied(3)).await;
    assert_eq!(text(&a).await, (3, "¡Hello world!".into()));
    assert_eq!(text(&b).await, text(&a).await);

    a.close().await;
    b.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_token_is_renewed_before_the_next_attempt() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-client-renew-{}", std::process::id()));
    let (ana_token, _) = setup(&dir);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _server = Running::start(config(&dir, port));
    // A source whose cached token has expired on the server's side: only a renewal works.
    let (calls, renewals) = (std::sync::Arc::new(AtomicUsize::new(0)), std::sync::Arc::new(AtomicUsize::new(0)));
    let (c, r) = (calls.clone(), renewals.clone());
    let source: connection::TokenSourceFn = std::sync::Arc::new(move |renew| {
        c.fetch_add(1, Ordering::SeqCst);
        if renew {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(ana_token.clone())
        } else {
            Ok("wct_0000000000000000000000000000000000000000000000000000000000000000".into())
        }
    });
    let mut config = TransportConfig::new(format!("ws://127.0.0.1:{port}/api/v1/ws"), "").with_token_source(source);
    config.backoff_min = Duration::from_millis(50);
    config.backoff_max = Duration::from_millis(200);
    let mut a = connection::spawn(config, SharedSession::new(DOC, ClientId::new("desktop-a").unwrap()));
    wait_for(&mut a, synchronized).await;
    assert_eq!(renewals.load(Ordering::SeqCst), 1, "renewed once, after the 401");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    a.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}
