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
    let title = "Plan ñ — 📝 & + # / ?";
    let published = a.execute("ui.collab.publish", json!({"server": server.url, "token": ana, "title": title})).unwrap();
    assert_eq!(published, json!({"pending": "publish"}));
    assert_eq!(collab::status_text(&a).as_deref(), Some("Shared · publishing…"));
    assert!(a.execute("text.insert", json!({"text": "must not be lost"})).is_err(), "editing pauses while publishing");
    assert_eq!(texts(&a), ["Hola ñandú"]);
    frames_until(&mut [&mut a], |apps| synchronized(apps[0]));
    assert_eq!(texts(&a), ["Hola ñandú"]);
    assert_eq!(collab::status_text(&a).as_deref(), Some("Shared · up to date"));

    // She invites Bea, who opens it (in a new document of her editor).
    let docs = server.db.lock().unwrap().query_row("SELECT id FROM documents", [], |r| r.get::<_, String>(0)).unwrap();
    let doc_id = wordcraft_collab_protocol::DocumentId(uuid::Uuid::parse_str(&docs).unwrap());
    let saved_title: String = server.db.lock().unwrap().query_row("SELECT title FROM documents", [], |r| r.get(0)).unwrap();
    assert_eq!(saved_title, title, "publication preserves Unicode and URL delimiters");
    let status = a.execute("ui.collab.status", json!({})).unwrap();
    assert_eq!(status["document"], docs);
    assert!(!status.to_string().contains(&ana));
    let original_document = a.session.document_id();
    assert!(a.execute("ui.collab.connect", json!({"server": server.url, "token": ana, "document": docs})).is_err());
    assert_eq!(a.session.document_id(), original_document, "opening twice never replaces the existing connection");
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
    // Each sees the other in the participant list (C5.3).
    frames_until(&mut [&mut a, &mut b], |apps| collab::participants(apps[0]) == ["Bea"] && collab::participants(apps[1]) == ["Ana"]);

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
    assert!(a.execute("format.strikethrough", json!({})).is_err());
    assert!(a.execute("format.bold", json!({})).is_err(), "formatting needs a selection");
    assert!(a.execute("edit.undo", json!({})).is_err());
    assert_eq!(a.session.doc, before);
    // Navigation still works.
    a.execute("caret.docStart", json!({})).unwrap();

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

/// C7: two editors paste and format at the same time, cut across paragraphs, and find the
/// document in Open Shared's list; a viewer copies but cannot paste.
#[test]
fn two_editors_paste_format_cut_and_list() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-editor-c7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = Running::start(&dir);
    let (_, ana) = auth::create_local_user(&server.db, "Ana").unwrap();
    let (bea_id, bea) = auth::create_local_user(&server.db, "Bea").unwrap();
    let (cris_id, cris) = auth::create_local_user(&server.db, "Cris").unwrap();

    let mut a = editor("Hola ñandú");
    a.execute("ui.collab.publish", json!({"server": server.url, "token": ana, "title": "Plan C7"})).unwrap();
    frames_until(&mut [&mut a], |apps| synchronized(apps[0]));
    let docs = server.db.lock().unwrap().query_row("SELECT id FROM documents", [], |r| r.get::<_, String>(0)).unwrap();
    let doc_id = wordcraft_collab_protocol::DocumentId(uuid::Uuid::parse_str(&docs).unwrap());
    let mut db = server.db.lock().unwrap();
    use wordcraft_collab_server::persistence::{Role, repository::set_member};
    set_member(&mut db, doc_id, bea_id, Role::Editor).unwrap();
    set_member(&mut db, doc_id, cris_id, Role::Viewer).unwrap();
    drop(db);

    // Bea finds it in her list (C7.4) instead of copying its id; Cris's lists only what he may open.
    let mut b = editor("");
    b.execute("ui.collab.listDocuments", json!({"server": server.url, "token": bea})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| collab::listing(apps[1]).loaded);
    let listed = collab::listing(&b);
    assert_eq!(
        listed.documents.iter().map(|d| (d.title.as_str(), d.role, d.id.as_str())).collect::<Vec<_>>(),
        [("Plan C7", "editor", docs.as_str())]
    );
    assert!(!listed.error);
    let mut stranger = editor("");
    let (_, dan) = auth::create_local_user(&server.db, "Dan").unwrap();
    stranger.execute("ui.collab.listDocuments", json!({"server": server.url, "token": dan})).unwrap();
    frames_until(&mut [&mut stranger], |apps| collab::listing(apps[0]).loaded);
    assert!(collab::listing(&stranger).documents.is_empty());
    assert!(!collab::listing(&stranger).message.is_empty(), "an empty list is explained");
    let mut bad = editor("");
    bad.execute("ui.collab.listDocuments", json!({"server": server.url, "token": "wct_nope"})).unwrap();
    frames_until(&mut [&mut bad], |apps| !collab::listing(apps[0]).busy);
    assert!(collab::listing(&bad).error);
    assert!(!collab::listing(&bad).message.contains("wct_nope"));

    b.execute("ui.collab.connect", json!({"server": server.url, "token": bea, "document": listed.documents[0].id})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| synchronized(apps[1]));

    // At the same time: Ana bolds "Hola", Bea pastes two lines at the end.
    a.session.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, 4) };
    assert_eq!(a.execute("format.bold", json!({})).unwrap(), json!({"submitted": true}));
    caret_at_end(&mut b);
    assert_eq!(b.execute("edit.pasteText", json!({"text": "\nUno ✓\nDos"})).unwrap(), json!({"submitted": true}));
    assert_eq!(texts(&b), ["Hola ñandú"], "confirm before apply");
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 2 && revision(apps[1]) == 2);
    assert_eq!(texts(&a), ["Hola ñandú", "Uno ✓", "Dos"]);
    assert_eq!(a.session.doc, b.session.doc);
    let first = |app: &WordApp| app.session.doc.para(StoryRef::Body, &Path::top(0)).unwrap().clone();
    assert_eq!(first(&b).props_of_char(0).bold, Some(true));
    assert_ne!(first(&b).props_of_char(5).bold, Some(true));
    assert_eq!(b.session.sel, Selection::caret(Pos::body(2, 3)), "the caret ends after the paste");

    // Bea cuts from inside the first paragraph to inside the last: one revision; the copy is hers.
    b.session.sel = Selection { anchor: Pos::body(0, 4), focus: Pos::body(2, 1) };
    b.execute("edit.cut", json!({})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 3 && revision(apps[1]) == 3);
    assert_eq!(texts(&a), ["Holaos"]);
    assert_eq!(a.session.doc, b.session.doc);
    assert!(b.session.clipboard_text.contains("Uno ✓"), "{:?}", b.session.clipboard_text);
    // And pastes it back with its formatting: the document is as before the cut.
    b.session.sel = Selection::caret(Pos::body(0, 4));
    b.execute("edit.paste", json!({})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| revision(apps[0]) == 4 && revision(apps[1]) == 4);
    assert_eq!(texts(&a), ["Hola ñandú", "Uno ✓", "Dos"]);
    assert_eq!(a.session.doc, b.session.doc);

    // A viewer may copy but not paste, cut or format; nothing is sent.
    let mut c = editor("");
    c.execute("ui.collab.connect", json!({"server": server.url, "token": cris, "document": docs})).unwrap();
    frames_until(&mut [&mut a, &mut b, &mut c], |apps| matches!(collab::state(apps[2]), Some(CollaborationState::ReadOnly { revision: 4, .. })));
    c.session.sel = Selection { anchor: Pos::body(1, 0), focus: Pos::body(1, 3) };
    c.execute("edit.copy", json!({})).unwrap();
    for (id, params) in [("edit.paste", json!({})), ("edit.pasteText", json!({"text": "x"})), ("edit.cut", json!({})), ("format.italic", json!({}))] {
        assert!(c.execute(id, params).is_err(), "{id}");
    }
    assert_eq!(c.session.doc, a.session.doc);
    assert_eq!(revision(&a), 4);

    drop((b, c));
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_viewer_opens_read_only_before_the_first_edit_and_can_be_promoted() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-viewer-{}", std::process::id()));
    let server = Running::start(&dir);
    let (_, owner) = auth::create_local_user(&server.db, "Owner").unwrap();
    let (viewer_id, viewer) = auth::create_local_user(&server.db, "Viewer").unwrap();
    let mut a = editor("Shared notes");
    a.execute("ui.collab.publish", json!({"server": server.url, "token": owner, "title": "Permissions"})).unwrap();
    frames_until(&mut [&mut a], |apps| synchronized(apps[0]));
    let status = a.execute("ui.collab.status", json!({})).unwrap();
    let id = wordcraft_collab_protocol::DocumentId(uuid::Uuid::parse_str(status["document"].as_str().unwrap()).unwrap());
    wordcraft_collab_server::persistence::repository::set_member(
        &mut server.db.lock().unwrap(),
        id,
        viewer_id,
        wordcraft_collab_server::persistence::Role::Viewer,
    )
    .unwrap();
    let mut v = editor("Local notes");
    v.execute("ui.collab.connect", json!({"server": server.url, "token": viewer, "document": id})).unwrap();
    frames_until(&mut [&mut a, &mut v], |apps| matches!(collab::state(apps[1]), Some(CollaborationState::ReadOnly { revision: 0, .. })));
    assert_eq!(collab::status_text(&v).as_deref(), Some("Shared · read-only"));
    assert_eq!(texts(&v), ["Shared notes"]);
    assert!(collab::disabled_reason(&v, "text.insert").is_some());
    assert!(collab::disabled_reason(&v, "format.bold").is_some());
    assert!(collab::disabled_reason(&v, "caret.docEnd").is_none());
    let before = v.session.doc.clone();
    assert!(v.execute("text.insert", json!({"text": "not sent"})).is_err());
    assert_eq!(v.session.doc, before);
    v.execute("ui.collab.disconnect", json!({})).unwrap();
    wordcraft_collab_server::persistence::repository::set_member(
        &mut server.db.lock().unwrap(),
        id,
        viewer_id,
        wordcraft_collab_server::persistence::Role::Editor,
    )
    .unwrap();
    v.execute("ui.collab.connect", json!({"server": server.url, "token": viewer, "document": id})).unwrap();
    frames_until(&mut [&mut a, &mut v], |apps| synchronized(apps[1]));
    caret_at_end(&mut v);
    v.execute("text.insert", json!({"text": "!"})).unwrap();
    frames_until(&mut [&mut a, &mut v], |apps| revision(apps[0]) == 1 && revision(apps[1]) == 1);
    assert_eq!(texts(&v), ["Shared notes!"]);
    drop(v);
    drop(a);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// C5.5: the owner invites from the editor — no curl. Ana shares her document, finds Bea by
/// name, invites her as a viewer and then makes her an editor; Bea opens it and types. Bea sees
/// who has access but cannot change it; removing her takes her access away.
#[test]
fn the_owner_invites_from_the_participants_screen() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-invite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = Running::start(&dir);
    let (_, ana) = auth::create_local_user(&server.db, "Ana").unwrap();
    let (bea_id, bea) = auth::create_local_user(&server.db, "Beatriz").unwrap();
    let mut a = editor("Hola");
    a.execute("ui.collab.publish", json!({"server": server.url, "token": ana, "title": "Plan"})).unwrap();
    frames_until(&mut [&mut a], |apps| synchronized(apps[0]));
    let document = a.execute("ui.collab.status", json!({})).unwrap()["document"].as_str().unwrap().to_owned();

    // The Participants dialog asks for the list itself; the owner alone is there.
    a.execute("ui.dialog", json!({"name": "shareMembers"})).unwrap();
    a.execute("ui.collab.members", json!({})).unwrap();
    assert!(a.execute("ui.collab.members", json!({})).is_err(), "one request at a time");
    frames_until(&mut [&mut a], |apps| !collab::people(apps[0]).busy);
    let view = collab::people(&a);
    assert!(view.can_manage, "{view:?}");
    assert_eq!(view.members.iter().map(|m| (m.name.as_str(), m.role, m.me)).collect::<Vec<_>>(), [("Ana", "owner", true)]);

    // Find Bea by part of her name and invite her as a viewer.
    assert!(a.execute("ui.collab.findPeople", json!({"query": "b"})).is_err(), "too short");
    a.execute("ui.collab.findPeople", json!({"query": "beat"})).unwrap();
    frames_until(&mut [&mut a], |apps| !collab::people(apps[0]).busy);
    let found = collab::people(&a).matches;
    assert_eq!(found.iter().map(|p| (p.name.as_str(), p.role)).collect::<Vec<_>>(), [("Beatriz", "")]);
    assert_eq!(found[0].id, bea_id.0.to_string());
    a.execute("ui.collab.invite", json!({"user": found[0].id, "role": "viewer"})).unwrap();
    frames_until(&mut [&mut a], |apps| !collab::people(apps[0]).busy);
    let view = collab::people(&a);
    assert!(!view.error, "{view:?}");
    assert_eq!(view.members.iter().map(|m| (m.name.as_str(), m.role)).collect::<Vec<_>>(), [("Ana", "owner"), ("Beatriz", "viewer")]);
    assert_eq!(view.matches[0].role, "viewer", "the search result now shows her access");

    // Bea opens it read-only; promoted, she types.
    let mut b = editor("Mine");
    b.execute("ui.collab.connect", json!({"server": server.url, "token": bea, "document": document})).unwrap();
    frames_until(&mut [&mut b], |apps| matches!(collab::state(apps[0]), Some(CollaborationState::ReadOnly { .. })));
    a.execute("ui.collab.invite", json!({"user": bea_id.0.to_string(), "role": "editor"})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| !collab::people(apps[0]).busy && synchronized(apps[1]));
    caret_at_end(&mut b);
    b.execute("text.insert", json!({"text": "!"})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| texts(apps[0]) == ["Hola!"] && texts(apps[1]) == ["Hola!"]);

    // Bea sees who has access, and cannot change it.
    b.execute("ui.collab.members", json!({})).unwrap();
    frames_until(&mut [&mut b], |apps| !collab::people(apps[0]).busy);
    let view = collab::people(&b);
    assert!(!view.can_manage);
    assert_eq!(view.members.iter().map(|m| (m.name.as_str(), m.me)).collect::<Vec<_>>(), [("Ana", false), ("Beatriz", true)]);
    b.execute("ui.collab.removeMember", json!({"user": bea_id.0.to_string()})).unwrap();
    frames_until(&mut [&mut b], |apps| !collab::people(apps[0]).busy);
    let view = collab::people(&b);
    assert!(view.error && view.message.contains("owner"), "{view:?}");

    // Ana removes her: Bea loses the document.
    a.execute("ui.collab.removeMember", json!({"user": bea_id.0.to_string()})).unwrap();
    frames_until(&mut [&mut a, &mut b], |apps| {
        !collab::people(apps[0]).busy && !matches!(collab::state(apps[1]), Some(CollaborationState::Synchronized { .. }))
    });
    assert_eq!(collab::people(&a).members.len(), 1);
    drop((a, b));
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
