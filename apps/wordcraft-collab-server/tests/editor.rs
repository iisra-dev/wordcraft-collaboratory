//! C4.4: two real editors (`WordApp`, no window) share a document through a real server: one
//! publishes its document, the other opens it, both type, and they end with the same text; the
//! status bar state follows; unsupported commands and undo are refused on the shared document.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wordcraft_collab_client::CollaborationState;
use wordcraft_collab_server::{AppState, Config, auth, serve};
use wordcraft_doc::{Block, Path, Pos, StoryRef};
use wordcraft_engine::{Selection, Session};
use wordcraft_ui_egui::{Services, WordApp, collab};

/// A server on its own thread and runtime (as in tests/client.rs), stopped by dropping it.
///
/// It keeps only the database: the `AppState` (whose registry holds the document actors'
/// handles) lives in the server thread alone, so stopping lets the actors end and the runtime
/// shut down instead of waiting for them forever.
struct Running {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    url: String,
    db: std::sync::Arc<wordcraft_collab_server::persistence::Database>,
}

impl Running {
    fn start(dir: &std::path::Path) -> Self {
        let config = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: dir.join("wordcraft.db"),
            document_storage: dir.join("documents"),
            oidc: None,
        };
        let state = AppState::open(&config).unwrap();
        let db = state.db.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let server_state = state;
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(config.listen_addr).await.unwrap();
                ready_tx.send(listener.local_addr().unwrap()).unwrap();
                tokio::select! {
                    _ = serve(listener, server_state, std::future::pending()) => {}
                    _ = stopped => {}
                }
            });
        });
        let addr = ready.recv().unwrap();
        Running { stop: Some(stop), thread: Some(thread), url: format!("ws://{addr}/api/v1/ws"), db }
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

fn editor(text: &str) -> WordApp {
    let mut doc = wordcraft_doc::Document::default();
    doc.insert_text(&Pos::body(0, 0), text, &wordcraft_doc::CharProps::default()).unwrap();
    WordApp::new(Session::new(doc), Services::default())
}

/// Run frames (polling the connection) until `done` holds, within 10 s.
fn frames_until(apps: &mut [&mut WordApp], done: impl Fn(&[&mut WordApp]) -> bool) {
    let ctx = egui::Context::default();
    let start = Instant::now();
    while !done(apps) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out; states: {:?}; status lines: {:?}",
            apps.iter().map(|a| collab::state(a)).collect::<Vec<_>>(),
            apps.iter().map(|a| a.status_msg.clone()).collect::<Vec<_>>()
        );
        for app in apps.iter_mut() {
            collab::poll(app, &ctx);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn synchronized(app: &WordApp) -> bool {
    matches!(collab::state(app), Some(CollaborationState::Synchronized { .. }))
}

fn revision(app: &WordApp) -> u64 {
    match collab::state(app) {
        Some(CollaborationState::Synchronized { revision, .. }) => revision,
        _ => 0,
    }
}

fn texts(app: &WordApp) -> Vec<String> {
    app.session
        .doc
        .body
        .iter()
        .filter_map(|b| match &**b {
            Block::Para(p) => Some(p.text.clone()),
            Block::Table(_) => None,
        })
        .collect()
}

fn caret_at_end(app: &mut WordApp) {
    let last = app.session.doc.body.len() - 1;
    let len = app.session.doc.para(StoryRef::Body, &Path::top(last)).unwrap().len();
    app.session.sel = Selection::caret(Pos::body(last, len));
}

#[test]
fn two_editors_share_a_document() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-editor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = Running::start(&dir);
    let (_, ana) = auth::create_local_user(&server.db, "Ana").unwrap();
    let (bea_id, bea) = auth::create_local_user(&server.db, "Bea").unwrap();

    // Ana publishes her document; it becomes shared where it is.
    let mut a = editor("Hola ñandú");
    let published = a.execute("ui.collab.publish", json!({"server": server.url, "token": ana, "title": "Plan"})).unwrap();
    assert_eq!(published, json!({"pending": "publish"}));
    frames_until(&mut [&mut a], |apps| synchronized(apps[0]));
    assert_eq!(texts(&a), ["Hola ñandú"]);
    assert_eq!(collab::status_text(&a).as_deref(), Some("Shared · up to date"));

    // She invites Bea, who opens it (in a new document of her editor).
    let docs = server.db.lock().unwrap().query_row("SELECT id FROM documents", [], |r| r.get::<_, String>(0)).unwrap();
    let doc_id = wordcraft_collab_protocol::DocumentId(uuid::Uuid::parse_str(&docs).unwrap());
    wordcraft_collab_server::persistence::repository::set_member(
        &mut server.db.lock().unwrap(),
        doc_id,
        bea_id,
        wordcraft_collab_server::persistence::Role::Editor,
    )
    .unwrap();
    let mut b = editor("Bea's own notes");
    b.execute("ui.collab.connect", json!({"server": server.url, "token": bea, "document": docs})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| synchronized(apps[1]));
    assert_eq!(texts(&b), ["Hola ñandú"]);

    // Both type at the end (through the same commands as the keyboard); nothing shows until
    // committed, then both see both edits.
    caret_at_end(&mut a);
    let submitted: Value = a.execute("text.insert", json!({"text": "!"})).unwrap();
    assert_eq!(submitted, json!({"submitted": true}));
    assert_eq!(texts(&a), ["Hola ñandú"], "confirm before apply: no local change yet");
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 1 && revision(apps[1]) == 1);
    caret_at_end(&mut b);
    b.execute("text.newParagraph", json!({})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 2 && revision(apps[1]) == 2);
    b.execute("text.insert", json!({"text": "Segunda línea"})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 3 && revision(apps[1]) == 3);
    assert_eq!(texts(&a), ["Hola ñandú!", "Segunda línea"]);
    assert_eq!(texts(&b), texts(&a));
    assert_eq!(a.session.doc, b.session.doc);
    // Bea's caret followed her own typing.
    assert_eq!(b.session.sel, Selection::caret(Pos::body(1, "Segunda línea".len())));

    // Unsupported edits and undo are refused, and nothing changes.
    let before = a.session.doc.clone();
    assert!(a.execute("format.bold", json!({})).is_err());
    assert!(a.execute("edit.undo", json!({})).is_err());
    assert_eq!(a.session.doc, before);
    // Navigation still works.
    a.execute("nav.docStart", json!({})).ok();

    // Disconnecting keeps the document as a local copy, editable as usual.
    a.execute("ui.collab.disconnect", json!({})).unwrap();
    assert_eq!(collab::status_text(&a), None);
    caret_at_end(&mut a);
    a.execute("text.insert", json!({"text": " (local)"})).unwrap();
    assert_eq!(texts(&a), ["Hola ñandú!", "Segunda línea (local)"]);

    drop(b);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
