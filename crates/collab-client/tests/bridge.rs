//! C4.2: committed edits applied to a real `Session` through `SessionBridge`: the document
//! changes as on every replica, the caret moves with the edit, the layout is recomputed, nothing
//! enters the local undo history, and the editor keeps working.

use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;
use wordcraft_collab_client::SessionBridge;
use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::DocumentOperation;
use wordcraft_doc::{Block, CharProps, Document, Path, Pos, StoryRef};
use wordcraft_engine::{Selection, Session};

fn block(n: u128) -> BlockId {
    BlockId(Uuid::from_u128(n))
}

/// A replica with paragraphs `texts` (ids 1, 2…), a session showing it, and its bridge.
fn joined(texts: &[&str]) -> (SharedDocument, Session, SessionBridge) {
    let mut doc = Document::default();
    for (i, t) in texts.iter().enumerate() {
        if i > 0 {
            let end = doc.para(StoryRef::Body, &Path::top(i - 1)).unwrap().len();
            doc.split_paragraph(&Pos::body(i - 1, end)).unwrap();
        }
        doc.insert_text(&Pos::body(i, 0), t, &CharProps::default()).unwrap();
    }
    let mut n = 0;
    let replica = SharedDocument::new(doc, || {
        n += 1;
        block(n)
    })
    .unwrap();
    let mut session = Session::new(Document::default());
    let mut bridge = SessionBridge::default();
    bridge.load(&mut session, &replica);
    (replica, session, bridge)
}

fn caret(session: &mut Session, i: usize, off: usize) {
    session.sel = Selection::caret(Pos::body(i, off));
}

fn texts(doc: &Document) -> Vec<String> {
    doc.body
        .iter()
        .filter_map(|b| match &**b {
            Block::Para(p) => Some(p.text.clone()),
            Block::Table(_) => None,
        })
        .collect()
}

/// The same commit on the replica and, through the bridge, on the session.
fn commit(replica: &mut SharedDocument, session: &mut Session, bridge: &mut SessionBridge, op: &DocumentOperation, own: bool) {
    replica.apply(op).unwrap();
    bridge.apply_remote_edit(session, op, own).unwrap();
    assert_eq!(&session.doc, replica.document());
    assert_eq!(bridge.ids(), replica.ids());
}

#[test]
fn remote_edits_move_the_caret_with_them() {
    let (mut replica, mut session, mut bridge) = joined(&["Hello world", "second"]);
    caret(&mut session, 0, 6);
    // Someone types before the caret: it moves on. At the caret: it stays in front.
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::InsertText { block_id: block(1), offset: 0, text: "¡".into() }, false);
    assert_eq!(session.sel, Selection::caret(Pos::body(0, 8)));
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::InsertText { block_id: block(1), offset: 8, text: "big ".into() }, false);
    assert_eq!(session.sel, Selection::caret(Pos::body(0, 8)));
    // A paragraph split before the caret's paragraph moves it down a paragraph, same offset.
    caret(&mut session, 1, 3);
    commit(
        &mut replica,
        &mut session,
        &mut bridge,
        &DocumentOperation::SplitParagraph { block_id: block(1), offset: 0, new_block_id: block(9) },
        false,
    );
    assert_eq!(session.sel, Selection::caret(Pos::body(2, 3)));
    // Its paragraph merged into the previous one: it lands where its text went.
    let first_len = session.doc.para(StoryRef::Body, &Path::top(1)).unwrap().len() as u32;
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::MergeParagraphs { first: block(9), second: block(2), first_len }, false);
    assert_eq!(session.sel, Selection::caret(Pos::body(1, first_len as usize + 3)));
    // A deletion around it collapses it; a selection shrinks with it.
    session.sel = Selection { anchor: Pos::body(1, 2), focus: Pos::body(1, first_len as usize + 5) };
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::DeleteText { block_id: block(9), start: 0, end: 4 }, false);
    assert_eq!(session.sel, Selection { anchor: Pos::body(1, 0), focus: Pos::body(1, first_len as usize + 1) });
    assert_eq!(texts(&session.doc), ["", "llo big worldsecond"]);
}

#[test]
fn own_edits_carry_the_caret_like_typing() {
    let (mut replica, mut session, mut bridge) = joined(&["Hello"]);
    caret(&mut session, 0, 5);
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::InsertText { block_id: block(1), offset: 5, text: " ñ".into() }, true);
    assert_eq!(session.sel, Selection::caret(Pos::body(0, 8)));
    commit(
        &mut replica,
        &mut session,
        &mut bridge,
        &DocumentOperation::SplitParagraph { block_id: block(1), offset: 8, new_block_id: block(2) },
        true,
    );
    assert_eq!(session.sel, Selection::caret(Pos::body(1, 0)));
}

#[test]
fn the_layout_follows_and_undo_does_not_see_remote_edits() {
    let (mut replica, mut session, mut bridge) = joined(&["Hello"]);
    let before = session.layout();
    assert!(!session.can_undo());
    commit(&mut replica, &mut session, &mut bridge, &DocumentOperation::InsertText { block_id: block(1), offset: 5, text: " world".into() }, false);
    let after = session.layout();
    assert!(!Arc::ptr_eq(&before, &after), "the layout was not recomputed");
    assert!(!session.can_undo(), "a remote edit became a local undo step");
    // The editor keeps working on top of it.
    caret(&mut session, 0, 11);
    session.run("text.insert", &json!({"text": "!"})).unwrap();
    assert_eq!(texts(&session.doc), ["Hello world!"]);
}

#[test]
fn a_caret_in_a_table_keeps_its_cell_when_paragraphs_move() {
    let (mut replica, mut session, mut bridge) = joined(&["before"]);
    // Put a table after the paragraph, on the replica, then load it.
    let mut doc = replica.document().clone();
    doc.insert_block(StoryRef::Body, &Path::top(1), Block::Table(wordcraft_doc::Table::new(2, 2, 400.0))).unwrap();
    let mut n = 0;
    replica = SharedDocument::new(doc, || {
        n += 1;
        block(n)
    })
    .unwrap();
    bridge.load(&mut session, &replica);
    let cell = Pos::new(StoryRef::Body, Path(vec![1, 1, 0, 0]), 0);
    assert!(session.doc.para(StoryRef::Body, &cell.path).is_some(), "test setup: no cell paragraph at {cell:?}");
    session.sel = Selection::caret(cell.clone());
    commit(
        &mut replica,
        &mut session,
        &mut bridge,
        &DocumentOperation::SplitParagraph { block_id: block(1), offset: 3, new_block_id: block(9) },
        false,
    );
    assert_eq!(session.sel, Selection::caret(Pos::new(StoryRef::Body, Path(vec![2, 1, 0, 0]), 0)));
}

#[test]
fn a_refused_edit_changes_nothing() {
    let (_replica, mut session, mut bridge) = joined(&["Hello"]);
    caret(&mut session, 0, 2);
    let before = (session.doc.clone(), session.sel.clone());
    let bad = DocumentOperation::InsertText { block_id: block(77), offset: 0, text: "x".into() };
    assert!(bridge.apply_remote_edit(&mut session, &bad, false).is_err());
    assert_eq!((session.doc.clone(), session.sel.clone()), before);
}
