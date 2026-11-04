//! C4.1: the supported editor commands as collaborative edits, and their follow-ups through
//! `SharedSession` against an in-memory server (same `transform`/`apply` as the real one).

use serde_json::json;
use uuid::Uuid;
use wordcraft_collab_client::capture::{CaptureError, FollowUp, capture, is_supported};
use wordcraft_collab_client::{Output, SharedSession};
use wordcraft_collab_model::transform::transform_all;
use wordcraft_collab_model::{BlockId, ModelError, SharedDocument};
use wordcraft_collab_protocol::messages::Anchor;
use wordcraft_collab_protocol::{ClientId, ClientMessage, DocumentId, DocumentOperation, OperationId, ServerMessage};
use wordcraft_doc::{CharProps, Document, InlineObject, Path, Pos, StoryRef};

const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));

fn block(n: u128) -> BlockId {
    BlockId(Uuid::from_u128(n))
}

/// Paragraphs with `texts`, ids 1, 2, 3…
fn replica(texts: &[&str]) -> SharedDocument {
    let mut doc = Document::default();
    for (i, t) in texts.iter().enumerate() {
        if i > 0 {
            let end = doc.para(StoryRef::Body, &Path::top(i - 1)).unwrap().len();
            doc.split_paragraph(&Pos::body(i - 1, end)).unwrap();
        }
        doc.insert_text(&Pos::body(i, 0), t, &CharProps::default()).unwrap();
    }
    let mut n = 0;
    SharedDocument::new(doc, || {
        n += 1;
        block(n)
    })
    .unwrap()
}

fn texts(r: &SharedDocument) -> Vec<String> {
    (0..r.ids().len()).map(|i| r.document().para(StoryRef::Body, &Path::top(i)).unwrap().text.clone()).collect()
}

fn caret(i: usize, off: usize) -> Pos {
    Pos::body(i, off)
}

/// Capture `command` at a caret (or selection `from..to`) and apply it (and its follow-up) as if
/// nobody else edited.
fn run(r: &SharedDocument, from: Pos, to: Pos, command: &str, params: serde_json::Value) -> Result<(SharedDocument, Option<Anchor>), CaptureError> {
    let edit = capture(r, &from, &to, command, &params, || block(99))?;
    let mut after = r.clone();
    if let Some(op) = &edit.operation {
        after.apply(op)?;
        if let Some(then) = &edit.follow_up {
            after.apply(&then.after(op).unwrap())?;
        }
    }
    Ok((after, edit.caret_after))
}

#[test]
fn caret_commands_are_single_operations() {
    let r = replica(&["Hola ñandú", "🦀 end"]);
    let (after, at) = run(&r, caret(0, 4), caret(0, 4), "text.insert", json!({"text": "!"})).unwrap();
    assert_eq!(texts(&after), ["Hola! ñandú", "🦀 end"]);
    assert_eq!(at, Some(Anchor { block_id: block(1), offset: 5 }));

    // Backspace deletes a whole character (2-byte "ú"), joins at a paragraph start, does nothing
    // at the very start.
    let len = "Hola ñandú".len();
    assert_eq!(texts(&run(&r, caret(0, len), caret(0, len), "text.backspace", json!({})).unwrap().0), ["Hola ñand", "🦀 end"]);
    let (joined, at) = run(&r, caret(1, 0), caret(1, 0), "text.backspace", json!({})).unwrap();
    assert_eq!(texts(&joined), ["Hola ñandú🦀 end"]);
    assert_eq!(at, Some(Anchor { block_id: block(1), offset: len as u32 }));
    assert!(capture(&r, &caret(0, 0), &caret(0, 0), "text.backspace", &json!({}), || block(99)).unwrap().operation.is_none());

    // Delete: the 4-byte crab, or the next paragraph at the end, or nothing at the very end.
    assert_eq!(texts(&run(&r, caret(1, 0), caret(1, 0), "text.delete", json!({})).unwrap().0), ["Hola ñandú", " end"]);
    assert_eq!(texts(&run(&r, caret(0, len), caret(0, len), "text.delete", json!({})).unwrap().0), ["Hola ñandú🦀 end"]);
    let end = "🦀 end".len();
    assert!(capture(&r, &caret(1, end), &caret(1, end), "text.delete", &json!({}), || block(99)).unwrap().operation.is_none());

    // Enter, Tab, line break.
    let (split, at) = run(&r, caret(0, 4), caret(0, 4), "text.newParagraph", json!({})).unwrap();
    assert_eq!(texts(&split), ["Hola", " ñandú", "🦀 end"]);
    assert_eq!(at, Some(Anchor { block_id: block(99), offset: 0 }));
    assert_eq!(texts(&run(&r, caret(0, 4), caret(0, 4), "text.tab", json!({})).unwrap().0)[0], "Hola\t ñandú");
    assert_eq!(texts(&run(&r, caret(0, 4), caret(0, 4), "text.lineBreak", json!({})).unwrap().0)[0], "Hola\n ñandú");
}

#[test]
fn selections_inside_a_paragraph_delete_then_follow_up() {
    let r = replica(&["Hello cruel world"]);
    // Typing over "cruel": the deletion now, the insertion once it commits.
    let edit = capture(&r, &caret(0, 11), &caret(0, 6), "text.insert", &json!({"text": "big"}), || block(99)).unwrap();
    assert_eq!(edit.operation, Some(DocumentOperation::DeleteText { block_id: block(1), start: 6, end: 11 }));
    assert_eq!(edit.follow_up, Some(FollowUp::Insert { text: "big".into() }));
    assert_eq!(texts(&run(&r, caret(0, 6), caret(0, 11), "text.insert", json!({"text": "big"})).unwrap().0), ["Hello big world"]);
    // Backspace or Delete over a selection is only the deletion.
    let edit = capture(&r, &caret(0, 6), &caret(0, 12), "text.backspace", &json!({}), || block(99)).unwrap();
    assert_eq!((edit.operation.is_some(), edit.follow_up), (true, None));
    // Enter over a selection splits where it was.
    assert_eq!(texts(&run(&r, caret(0, 5), caret(0, 12), "text.newParagraph", json!({})).unwrap().0), ["Hello", "world"]);
}

#[test]
fn what_shared_documents_cannot_do_is_refused_before_sending() {
    let r = replica(&["one", "two"]);
    let refused = |from: Pos, to: Pos, command: &str, params: serde_json::Value| capture(&r, &from, &to, command, &params, || block(99)).unwrap_err();
    // C7.2: a selection across paragraphs is supported now (tests/c7.rs).
    assert!(matches!(refused(caret(0, 0), caret(0, 0), "format.bold", json!({})), CaptureError::Unsupported(_)));
    assert!(matches!(refused(caret(0, 0), caret(0, 0), "text.insert", json!({})), CaptureError::BadParams { .. }));
    assert!(matches!(refused(caret(0, 0), caret(0, 0), "text.insert", json!({"text": "a\rb"})), CaptureError::Model(ModelError::InvalidText(_))));
    // Not a body paragraph, or off a char boundary / past the end.
    let header = Pos::new(StoryRef::Part(1), Path::top(0), 0);
    assert!(matches!(refused(header.clone(), header, "text.insert", json!({"text": "x"})), CaptureError::Unsupported(_)));
    assert!(matches!(refused(caret(5, 0), caret(5, 0), "text.insert", json!({"text": "x"})), CaptureError::Unsupported(_)));
    assert!(matches!(refused(caret(0, 9), caret(0, 9), "text.insert", json!({"text": "x"})), CaptureError::Model(ModelError::InvalidOffset { .. })));
    // An embedded object in the way.
    let mut doc = r.document().clone();
    doc.insert_object(&Pos::body(0, 1), InlineObject::BookmarkStart { name: "m".into() }, &CharProps::default()).unwrap();
    let with_object = SharedDocument::new(doc, {
        let mut n = 0;
        move || {
            n += 1;
            block(n)
        }
    })
    .unwrap();
    let err = capture(&with_object, &caret(0, 4), &caret(0, 4), "text.backspace", &json!({}), || block(99)).unwrap_err();
    assert!(matches!(err, CaptureError::Model(ModelError::Unsupported(_))), "{err:?}");
    assert!(is_supported("text.insert") && !is_supported("edit.undo"));
}

/// A minimal in-memory server: transform, apply, commit.
struct Server {
    doc: SharedDocument,
    history: Vec<DocumentOperation>,
}

impl Server {
    fn handle(&mut self, message: &ClientMessage) -> ServerMessage {
        let ClientMessage::Submit { operation_id, client_id, base_revision, operation, .. } = message else { panic!("{message:?}") };
        let op = transform_all(operation, &self.history[*base_revision as usize..]).unwrap();
        self.doc.apply(&op).unwrap();
        self.history.push(op.clone());
        ServerMessage::Committed {
            document_id: DOC,
            revision: self.history.len() as u64,
            operation_id: *operation_id,
            client_id: client_id.clone(),
            operation: op,
        }
    }

    fn snapshot(&self) -> ServerMessage {
        ServerMessage::Snapshot { document_id: DOC, revision: self.history.len() as u64, snapshot: self.doc.to_json().unwrap(), can_edit: true }
    }
}

fn session(name: &str, server: &Server) -> SharedSession {
    let mut s = SharedSession::new(DOC, ClientId::new(name).unwrap());
    s.connected();
    s.receive(server.snapshot());
    s
}

/// Deliver a commit to every session; collect what they send back.
fn broadcast(commit: &ServerMessage, sessions: &mut [&mut SharedSession]) -> Vec<ClientMessage> {
    sessions.iter_mut().flat_map(|s| s.receive(commit.clone()).send).collect()
}

#[test]
fn a_replacement_lands_where_the_selection_was_despite_concurrent_typing() {
    let mut server = Server { doc: replica(&["Hello cruel world"]), history: Vec::new() };
    let mut a = session("desktop-a", &server);
    let mut b = session("desktop-b", &server);

    // A selects "cruel" and types "big"; B, concurrently, types ">> " at the paragraph start.
    let edit = capture(a.replica().unwrap(), &caret(0, 6), &caret(0, 11), "text.insert", &json!({"text": "big"}), || block(99)).unwrap();
    let a_first = a
        .submit_edit(0, (OperationId(Uuid::from_u128(1)), edit.operation.unwrap()), edit.follow_up.map(|f| (OperationId(Uuid::from_u128(2)), f)))
        .unwrap();
    let b_edit = capture(b.replica().unwrap(), &caret(0, 0), &caret(0, 0), "text.insert", &json!({"text": ">> "}), || block(98)).unwrap();
    let b_op = b.submit(OperationId(Uuid::from_u128(3)), b_edit.operation.unwrap()).unwrap();

    // The server gets B's first: A's deletion is transformed (shifted by 3).
    let commit = server.handle(&b_op);
    assert!(broadcast(&commit, &mut [&mut a, &mut b]).is_empty());
    let commit = server.handle(&a_first);
    // A's deletion committing releases the insertion, based on that commit's revision.
    let sent = broadcast(&commit, &mut [&mut a, &mut b]);
    let [follow_up] = sent.as_slice() else { panic!("{sent:?}") };
    assert!(matches!(follow_up, ClientMessage::Submit { base_revision: 2, operation: DocumentOperation::InsertText { offset: 9, .. }, .. }));
    let commit = server.handle(follow_up);
    broadcast(&commit, &mut [&mut a, &mut b]);

    assert_eq!(texts(&server.doc), [">> Hello big world"]);
    assert_eq!(a.replica(), Some(&server.doc));
    assert_eq!(b.replica(), Some(&server.doc));
    assert!(a.pending().is_empty());
}

#[test]
fn a_refused_deletion_takes_its_follow_up_with_it() {
    let server = Server { doc: replica(&["Hello cruel world"]), history: Vec::new() };
    let mut a = session("desktop-a", &server);
    let edit = capture(a.replica().unwrap(), &caret(0, 6), &caret(0, 11), "text.insert", &json!({"text": "big"}), || block(99)).unwrap();
    a.submit_edit(0, (OperationId(Uuid::from_u128(1)), edit.operation.unwrap()), edit.follow_up.map(|f| (OperationId(Uuid::from_u128(2)), f)))
        .unwrap();
    let out: Output = a.receive(ServerMessage::Rejected {
        document_id: Some(DOC),
        operation_id: Some(OperationId(Uuid::from_u128(1))),
        reason: wordcraft_collab_protocol::RejectReason::Conflict,
        message: "conflict".into(),
    });
    assert!(out.send.is_empty());
    a.acknowledge_conflict();
    // Even if a commit with that id turned up later, nothing follows.
    let late = ServerMessage::Committed {
        document_id: DOC,
        revision: 1,
        operation_id: OperationId(Uuid::from_u128(1)),
        client_id: ClientId::new("desktop-a").unwrap(),
        operation: DocumentOperation::DeleteText { block_id: block(1), start: 6, end: 11 },
    };
    assert!(a.receive(late).send.is_empty());
}
