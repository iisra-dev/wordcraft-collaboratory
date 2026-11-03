//! C6.1: three real clients (`wordcraft-collab-client`, feature `transport`) edit one document
//! through a faulty network while the server restarts, and must end up with the same document
//! as the server, with every edit either committed exactly once or refused exactly once.
//!
//! Each client talks to the server through its own [`Proxy`], a WebSocket relay that cuts the
//! connection at random moments (without a close frame, like a dropped Wi-Fi link) and
//! delivers some `submit`, `committed` and `rejected` messages twice. The server itself goes
//! away twice and comes back on the same port with the same data. Meanwhile each client makes
//! random edits — typing (ASCII and multibyte), deleting, splitting and joining paragraphs —
//! against its replica as it last read it, which may be behind by the time the edit is sent,
//! exactly like the editor's UI thread.
//!
//! The run is driven by a seeded generator: the seed is printed, and `COLLAB_CHAOS_SEED=<n>`
//! repeats its choices (thread timing still varies). `COLLAB_CHAOS_ROUNDS=<n>` runs more rounds.
//!
//! At the end, with the network healthy again:
//! - the three replicas and a late joiner (after one more restart, so from disk) are equal;
//! - the operation log has no operation id twice, and only ids the clients submitted;
//! - submitted = committed + refused: nothing was lost, nothing counted twice;
//! - the only refusals are conflicts (no safe transformation): an edit valid where its author
//!   made it never becomes invalid on the way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use uuid::Uuid;
use wordcraft_collab_client::connection::{self, Client, ClientError, TransportConfig};
use wordcraft_collab_client::{CollaborationState, Event, SharedSession, SubmitError};
use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::{
    ClientId, ClientMessage, DocumentId, DocumentOperation, Envelope, MAX_MESSAGE_BYTES, MAX_SERVER_MESSAGE_BYTES, OperationId, RejectReason,
    Revision, ServerMessage,
};
use wordcraft_collab_server::persistence::{Database, Role, repository};
use wordcraft_collab_server::{AppState, Config, auth, documents, serve};
use wordcraft_doc::{CharProps, Document, Pos};

const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));
const BLOCK: BlockId = BlockId(Uuid::from_u128(0xb1));
/// Edits each client makes per round.
const EDITS: usize = 80;
/// Most paragraphs a client lets the document grow to (it joins rather than splits beyond).
const MAX_PARAGRAPHS: usize = 8;
/// Typed text: ASCII, two-byte, three-byte and four-byte UTF-8.
const TYPED: [&str; 6] = ["a", "bc", "ñ", "€", "📝", " "];

// --- a seeded generator ----------------------------------------------------------------------

/// SplitMix64: tiny, seeded, good enough to pick edits and faults.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn uuid(&mut self) -> Uuid {
        Uuid::from_u128((u128::from(self.next()) << 64) | u128::from(self.next()))
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn seed() -> u64 {
    std::env::var("COLLAB_CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1))
}

// --- the server, stopped by dropping it ------------------------------------------------------

/// A server process stand-in: its own thread and runtime, so stopping it really closes every
/// socket (dropping that runtime cancels its connection tasks).
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
                let listener = TcpListener::bind(config.listen_addr).await.unwrap();
                ready_tx.send(()).unwrap();
                tokio::select! {
                    _ = serve(listener, state, std::future::pending()) => {}
                    _ = stopped => {}
                }
            });
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

fn config(dir: &FsPath, port: u16) -> Config {
    Config {
        listen_addr: format!("127.0.0.1:{port}").parse().unwrap(),
        database_path: dir.join("wordcraft.db"),
        document_storage: dir.join("documents"),
        oidc: None,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Ana (owner), Bea and Cai (editors) with tokens, and [`DOC`] with one paragraph.
fn setup(dir: &PathBuf) -> Vec<String> {
    let _ = std::fs::remove_dir_all(dir);
    let state = AppState::open(&config(dir, 0)).unwrap();
    let mut tokens = Vec::new();
    let mut users = Vec::new();
    for name in ["Ana", "Bea", "Cai"] {
        let (user, token) = auth::create_local_user(&state.db, name).unwrap();
        users.push(user);
        tokens.push(token);
    }
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hola ñandú, ¿qué tal? 📝", &CharProps::default()).unwrap();
    documents::create(&state.db, &state.storage, DOC, "Caos", users[0], doc, || BLOCK).unwrap();
    for &user in &users[1..] {
        repository::set_member(&mut state.db.lock().unwrap(), DOC, user, Role::Editor).unwrap();
    }
    tokens
}

// --- the faulty network ----------------------------------------------------------------------

/// A WebSocket relay between one client and the server that can cut every connection it
/// carries and deliver some messages twice.
struct Proxy {
    port: u16,
    /// Bumped to cut every live connection.
    cut: watch::Sender<u64>,
    /// While set, some edits and their answers are delivered twice.
    duplicate: Arc<AtomicBool>,
    /// What it did: live connections cut, messages delivered twice.
    faults: Arc<Faults>,
}

#[derive(Default)]
struct Faults {
    cut: AtomicUsize,
    duplicated: AtomicUsize,
}

impl Proxy {
    async fn start(server_port: u16, seed: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cut, cuts) = watch::channel(0u64);
        let duplicate = Arc::new(AtomicBool::new(true));
        let dup = duplicate.clone();
        let rng = Arc::new(Mutex::new(Rng(seed)));
        let faults = Arc::new(Faults::default());
        let counted = faults.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(relay(stream, server_port, cuts.clone(), dup.clone(), rng.clone(), counted.clone()));
            }
        });
        Proxy { port, cut, duplicate, faults }
    }

    fn cut(&self) {
        self.cut.send_modify(|n| *n += 1);
    }
}

/// Whether a client message is one to deliver twice: only edits (a second `hello` or `join`
/// is not what a retransmission looks like).
fn duplicable_up(text: &str) -> bool {
    matches!(Envelope::<ClientMessage>::decode_with_limit(text, MAX_MESSAGE_BYTES), Ok(e) if matches!(e.message, ClientMessage::Submit { .. }))
}

fn duplicable_down(text: &str) -> bool {
    matches!(
        Envelope::<ServerMessage>::decode_with_limit(text, MAX_SERVER_MESSAGE_BYTES),
        Ok(e) if matches!(e.message, ServerMessage::Committed { .. } | ServerMessage::Rejected { .. })
    )
}

// The handshake callback's error type is tungstenite's, not ours.
#[allow(clippy::result_large_err)]
async fn relay(
    stream: tokio::net::TcpStream,
    server_port: u16,
    mut cuts: watch::Receiver<u64>,
    duplicate: Arc<AtomicBool>,
    rng: Arc<Mutex<Rng>>,
    faults: Arc<Faults>,
) {
    let mut authorization: Option<HeaderValue> = None;
    let callback = |request: &Request, response: Response| {
        authorization = request.headers().get("authorization").cloned();
        Ok(response)
    };
    let Ok(client) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { return };
    let mut request = format!("ws://127.0.0.1:{server_port}/api/v1/ws").into_client_request().unwrap();
    if let Some(value) = authorization {
        request.headers_mut().insert("authorization", value);
    }
    // The server is down: the client sees its connection drop and tries again.
    let Ok((server, _)) = tokio_tungstenite::connect_async(request).await else { return };
    cuts.mark_unchanged();
    let (mut to_client, mut from_client) = client.split();
    let (mut to_server, mut from_server) = server.split();
    let twice = |rng: &Arc<Mutex<Rng>>| {
        let again = duplicate.load(Ordering::Relaxed) && rng.lock().unwrap().chance(15);
        if again {
            faults.duplicated.fetch_add(1, Ordering::Relaxed);
        }
        again
    };
    loop {
        tokio::select! {
            // Dropping both halves closes the sockets with no close frame.
            _ = cuts.changed() => {
                faults.cut.fetch_add(1, Ordering::Relaxed);
                return;
            }
            message = from_client.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let again = duplicable_up(text.as_str()) && twice(&rng);
                    if to_server.send(Message::Text(text.clone())).await.is_err() { return }
                    if again && to_server.send(Message::Text(text)).await.is_err() { return }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(other)) => if to_server.send(other).await.is_err() { return },
            },
            message = from_server.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let again = duplicable_down(text.as_str()) && twice(&rng);
                    if to_client.send(Message::Text(text.clone())).await.is_err() { return }
                    if again && to_client.send(Message::Text(text)).await.is_err() { return }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(other)) => if to_client.send(other).await.is_err() { return },
            },
        }
    }
}

// --- one editing client ----------------------------------------------------------------------

/// What a client's events said, kept by a task that drains them (a full event queue would
/// stall the connection).
#[derive(Default)]
struct Observed {
    state: Option<CollaborationState>,
    /// Refusals of this client's own edits, by reason (a repeated answer has no operation).
    refused: BTreeMap<String, usize>,
    snapshots: usize,
}

struct Editor {
    name: &'static str,
    client: Client,
    observed: Arc<Mutex<Observed>>,
    submitted: BTreeSet<OperationId>,
    rng: Rng,
}

impl Editor {
    fn start(name: &'static str, proxy_port: u16, token: &str, seed: u64) -> Self {
        let mut config = TransportConfig::new(format!("ws://127.0.0.1:{proxy_port}/api/v1/ws"), token);
        config.backoff_min = Duration::from_millis(20);
        config.backoff_max = Duration::from_millis(200);
        let mut client = connection::spawn(config, SharedSession::new(DOC, ClientId::new(name).unwrap()));
        let mut events = std::mem::replace(&mut client.events, mpsc::channel(1).1);
        let observed = Arc::new(Mutex::new(Observed::default()));
        let seen = observed.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let mut seen = seen.lock().unwrap();
                match event {
                    Event::State(state) => seen.state = Some(state),
                    Event::Rejected { operation: Some(_), reason, .. } => *seen.refused.entry(format!("{reason:?}")).or_default() += 1,
                    Event::Snapshot { .. } => seen.snapshots += 1,
                    _ => {}
                }
            }
        });
        Editor { name, client, observed, submitted: BTreeSet::new(), rng: Rng(seed) }
    }

    fn state(&self) -> Option<CollaborationState> {
        self.observed.lock().unwrap().state.clone()
    }

    fn refused(&self) -> usize {
        self.observed.lock().unwrap().refused.values().sum()
    }

    /// Make `count` random edits, each against the replica as just read (it may move on before
    /// the edit is sent: the edit says which revision it was made against).
    async fn edit(&mut self, count: usize) {
        let mut made = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while made < count {
            assert!(tokio::time::Instant::now() < deadline, "{}: stuck after {made} edits, state {:?}", self.name, self.state());
            let Ok(Some((revision, replica))) = self.client.replica().await else {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            };
            let Some(operation) = random_edit(&replica, &mut self.rng) else { continue };
            // The editor shows what it last polled, and the person takes a moment: the others'
            // commits keep arriving meanwhile.
            tokio::time::sleep(Duration::from_millis(10 + self.rng.below(40) as u64)).await;
            let id = OperationId(self.rng.uuid());
            match self.client.submit_at(revision, id, operation).await {
                Ok(()) => {
                    self.submitted.insert(id);
                    made += 1;
                }
                // Someone else's edit won: the person is told, and carries on.
                Err(ClientError::Submit(SubmitError::NotEditable(CollaborationState::Conflict { .. }))) => {
                    self.client.acknowledge_conflict().await.unwrap();
                }
                // Offline, or too much in flight: wait.
                Err(ClientError::Submit(SubmitError::NotEditable(_) | SubmitError::Pending(_))) => {}
                Err(e) => panic!("{}: {e}", self.name),
            }
        }
    }
}

/// A random edit that fits `replica`: offsets on char boundaries, real neighbours to join.
fn random_edit(replica: &SharedDocument, rng: &mut Rng) -> Option<DocumentOperation> {
    let ids = replica.ids();
    let body = &replica.document().body;
    let index = rng.below(ids.len());
    let block_id = ids.id_at(index)?;
    let text = &body.get(index)?.as_para()?.text;
    let boundaries: Vec<u32> = text.char_indices().map(|(i, _)| i as u32).chain([text.len() as u32]).collect();
    let at = |rng: &mut Rng| boundaries[rng.below(boundaries.len())];
    let roll = rng.below(100);
    Some(match roll {
        0..50 => DocumentOperation::InsertText { block_id, offset: at(rng), text: rng.pick(&TYPED).repeat(1 + rng.below(3)) },
        50..75 if boundaries.len() > 1 => {
            let start = rng.below(boundaries.len() - 1);
            let end = (start + 1 + rng.below(4)).min(boundaries.len() - 1);
            DocumentOperation::DeleteText { block_id, start: boundaries[start], end: boundaries[end] }
        }
        75..88 if ids.len() < MAX_PARAGRAPHS => DocumentOperation::SplitParagraph { block_id, offset: at(rng), new_block_id: BlockId(rng.uuid()) },
        _ if index + 1 < ids.len() && body.get(index + 1)?.as_para().is_some() => {
            DocumentOperation::MergeParagraphs { first: block_id, second: ids.id_at(index + 1)?, first_len: text.len() as u32 }
        }
        _ => DocumentOperation::InsertText { block_id, offset: at(rng), text: "z".into() },
    })
}

// --- the run ---------------------------------------------------------------------------------

/// The server's revision, read from its database (another connection, as an operator would).
fn server_revision(dir: &FsPath) -> Revision {
    let db = Database::open(&dir.join("wordcraft.db")).unwrap();
    let conn = db.lock().unwrap();
    repository::document(&conn, DOC).unwrap().unwrap().current_revision
}

/// Wait until every editor is synchronized at the server's revision, and it stays there.
async fn settle(editors: &[Editor], dir: &FsPath) -> (Revision, SharedDocument) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut stable = 0;
    loop {
        assert!(tokio::time::Instant::now() < deadline, "never settled: {:?}", editors.iter().map(Editor::state).collect::<Vec<_>>());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let revision = server_revision(dir);
        let mut replicas = Vec::new();
        for editor in editors {
            if matches!(editor.state(), Some(CollaborationState::Conflict { .. })) {
                editor.client.acknowledge_conflict().await.unwrap();
            }
            match (editor.state(), editor.client.replica().await) {
                (Some(CollaborationState::Synchronized { .. }), Ok(Some((r, replica)))) if r == revision => replicas.push(replica),
                _ => break,
            }
        }
        stable = if replicas.len() == editors.len() { stable + 1 } else { 0 };
        if stable >= 3 {
            for (editor, replica) in editors.iter().zip(&replicas) {
                assert_eq!(replica, &replicas[0], "{} diverged from {}", editor.name, editors[0].name);
            }
            return (revision, replicas.swap_remove(0));
        }
    }
}

async fn round(seed: u64) {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-chaos-{}-{seed}", std::process::id()));
    let tokens = setup(&dir);
    let server_port = free_port();
    let mut server = Some(Running::start(config(&dir, server_port)));
    let mut rng = Rng(seed);

    let names = ["desktop-ana", "desktop-bea", "desktop-cai"];
    let mut proxies = Vec::new();
    let mut editors = Vec::new();
    for (n, name) in names.into_iter().enumerate() {
        let proxy = Proxy::start(server_port, seed ^ (0x100 + n as u64)).await;
        editors.push(Editor::start(name, proxy.port, &tokens[n], seed ^ (0x200 + n as u64)));
        proxies.push(proxy);
    }

    // Everyone edits while the network fails and the server restarts twice.
    let chaos = async {
        let mut restarts = 0;
        for step in 0..40 {
            tokio::time::sleep(Duration::from_millis(50 + rng.below(150) as u64)).await;
            if step % 14 == 13 && restarts < 2 {
                drop(server.take());
                tokio::time::sleep(Duration::from_millis(100 + rng.below(300) as u64)).await;
                server = Some(Running::start(config(&dir, server_port)));
                restarts += 1;
            } else if rng.chance(25) {
                proxies.iter().for_each(Proxy::cut);
            } else {
                proxies[rng.below(proxies.len())].cut();
            }
        }
        restarts
    };
    let [a, b, c] = editors.as_mut_slice() else { unreachable!() };
    let (restarts, (), (), ()) = tokio::join!(chaos, a.edit(EDITS), b.edit(EDITS), c.edit(EDITS));
    assert_eq!(restarts, 2);

    // The network heals: everyone ends where the server is.
    for proxy in &proxies {
        proxy.duplicate.store(false, Ordering::Relaxed);
    }
    let (revision, document) = settle(&editors, &dir).await;

    // One more restart; a late joiner gets the same document, from disk.
    drop(server.take());
    let _server = Running::start(config(&dir, server_port));
    let late = Editor::start("desktop-late", proxies[0].port, &tokens[0], seed);
    let (late_revision, late_document) = settle(std::slice::from_ref(&late), &dir).await;
    assert_eq!((late_revision, &late_document), (revision, &document), "the late joiner sees another document");

    // Every edit committed once or refused once; nothing else in the log.
    let log = {
        let db = Database::open(&dir.join("wordcraft.db")).unwrap();
        let conn = db.lock().unwrap();
        repository::operations_since(&conn, DOC, 0, usize::MAX).unwrap()
    };
    let logged: BTreeSet<OperationId> = log.iter().map(|o| o.operation_id).collect();
    assert_eq!(logged.len(), log.len(), "an operation id was committed twice");
    assert_eq!(log.len() as u64, revision);
    let submitted: BTreeSet<OperationId> = editors.iter().flat_map(|e| e.submitted.iter().copied()).collect();
    assert!(logged.is_subset(&submitted), "the log holds operations nobody submitted");
    let refused: usize = editors.iter().map(Editor::refused).sum();
    assert_eq!(submitted.len(), logged.len() + refused, "submitted = committed + refused");
    for editor in &editors {
        let (refused, snapshots) = {
            let observed = editor.observed.lock().unwrap();
            (observed.refused.clone(), observed.snapshots)
        };
        let unexpected: Vec<_> = refused.keys().filter(|reason| reason.as_str() != format!("{:?}", RejectReason::Conflict)).collect();
        assert!(unexpected.is_empty(), "{}: refused for {unexpected:?} ({refused:?})", editor.name);
        assert!(snapshots >= 2, "{} never reconnected", editor.name);
    }
    let cut: usize = proxies.iter().map(|p| p.faults.cut.load(Ordering::Relaxed)).sum();
    let duplicated: usize = proxies.iter().map(|p| p.faults.duplicated.load(Ordering::Relaxed)).sum();
    assert!(cut > 0 && duplicated > 0, "the network never failed ({cut} cut, {duplicated} duplicated)");
    eprintln!(
        "chaos seed {seed}: {} edits, {} committed, {refused} refused as conflicts, revision {revision}; {cut} connections cut, {duplicated} messages duplicated",
        submitted.len(),
        log.len()
    );

    for editor in editors.into_iter().chain([late]) {
        editor.client.close().await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn three_clients_converge_through_cuts_duplicates_and_restarts() {
    let seed = seed();
    let rounds = std::env::var("COLLAB_CHAOS_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1u64);
    for n in 0..rounds {
        let seed = seed.wrapping_add(n);
        eprintln!("chaos seed {seed} (repeat with COLLAB_CHAOS_SEED={seed})");
        round(seed).await;
    }
}
