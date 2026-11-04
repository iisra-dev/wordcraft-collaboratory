//! C7.2–C7.3: cut, paste, cross-paragraph selections and basic formatting as collaborative
//! edits — one atomic batch each, validated before anything is sent.

use serde_json::json;
use uuid::Uuid;
use wordcraft_collab_client::capture::{CaptureError, capture, capture_format, capture_paste};
use wordcraft_collab_client::paste::{MAX_PASTE_TEXT_BYTES, PastedRun, Payload, payload, plain};
use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::DocumentOperation;
use wordcraft_collab_protocol::operations::BasicFormat;
use wordcraft_doc::props::{Highlight, Underline};
use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};
use wordcraft_engine::{Selection, Session};

fn block(n: u128) -> BlockId {
    BlockId(Uuid::from_u128(n))
}

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

fn applied(r: &SharedDocument, op: &Option<DocumentOperation>) -> SharedDocument {
    let mut after = r.clone();
    after.apply(op.as_ref().unwrap()).unwrap();
    after
}

fn fresh() -> impl FnMut() -> BlockId {
    let mut n = 100;
    move || {
        n += 1;
        block(n)
    }
}

#[test]
fn typing_and_deleting_over_a_selection_across_paragraphs_is_one_batch() {
    let r = replica(&["Hello there", "middle", "big world"]);
    let edit = capture(&r, &Pos::body(0, 5), &Pos::body(2, 4), "text.insert", &json!({"text": ","}), || block(99)).unwrap();
    assert!(matches!(edit.operation, Some(DocumentOperation::Batch { .. })));
    assert_eq!(edit.follow_up, None);
    assert_eq!(texts(&applied(&r, &edit.operation)), ["Hello,world"]);
    // Backwards selections, Delete, Enter and Cut too.
    let edit = capture(&r, &Pos::body(2, 4), &Pos::body(0, 5), "text.delete", &json!({}), || block(99)).unwrap();
    assert_eq!(texts(&applied(&r, &edit.operation)), ["Helloworld"]);
    let edit = capture(&r, &Pos::body(0, 5), &Pos::body(1, 6), "text.newParagraph", &json!({}), || block(99)).unwrap();
    assert_eq!(texts(&applied(&r, &edit.operation)), ["Hello", "", "big world"]);
    let edit = capture(&r, &Pos::body(0, 0), &Pos::body(2, 9), "edit.cut", &json!({}), || block(99)).unwrap();
    assert_eq!(texts(&applied(&r, &edit.operation)), [""]);
    // Cut with nothing selected changes nothing.
    assert_eq!(capture(&r, &Pos::body(0, 1), &Pos::body(0, 1), "edit.cut", &json!({}), || block(99)).unwrap().operation, None);
}

#[test]
fn pasting_paragraphs_replaces_the_selection_atomically() {
    let r = replica(&["Hello world"]);
    let paste = plain("one\r\ntwo ñ🦀\nthree").unwrap();
    let edit = capture_paste(&r, &Pos::body(0, 6), &Pos::body(0, 11), &paste, fresh()).unwrap();
    let after = applied(&r, &edit.operation);
    assert_eq!(texts(&after), ["Hello one", "two ñ🦀", "three"]);
    let caret = edit.caret_after.unwrap();
    assert_eq!((after.ids().index_of(caret.block_id), caret.offset), (Some(2), 5));
    // A trailing line break ends with an empty paragraph, as in the editor.
    let edit = capture_paste(&r, &Pos::body(0, 5), &Pos::body(0, 5), &plain("!\n").unwrap(), fresh()).unwrap();
    assert_eq!(texts(&applied(&r, &edit.operation)), ["Hello!", " world"]);
}

#[test]
fn pasted_runs_keep_their_supported_formatting() {
    let r = replica(&["ab"]);
    let bold = BasicFormat { bold: Some(true), highlight: Some("yellow".into()), ..BasicFormat::default() };
    let paste = Payload {
        // A run without formatting of its own takes the formatting to its left, as typing does.
        paragraphs: vec![vec![PastedRun { text: "y".into(), format: None }, PastedRun { text: "X".into(), format: Some(bold) }]],
    };
    let edit = capture_paste(&r, &Pos::body(0, 1), &Pos::body(0, 1), &paste, fresh()).unwrap();
    let after = applied(&r, &edit.operation);
    let p = after.document().para(StoryRef::Body, &Path::top(0)).unwrap();
    assert_eq!(p.text, "ayXb");
    assert_eq!(p.props_of_char(2).bold, Some(true));
    assert_eq!(p.props_of_char(2).highlight, Some(Highlight::Yellow));
    assert_ne!(p.props_of_char(1).bold, Some(true));
    assert_ne!(p.props_of_char(3).bold, Some(true));
}

#[test]
fn unsupported_or_oversized_pastes_are_refused_before_sending() {
    assert!(matches!(plain("a\u{FFFC}b"), Err(CaptureError::Unsupported(_))));
    let big = "x".repeat(MAX_PASTE_TEXT_BYTES + 1);
    let mut session = Session::new(Document::from_text("Hello"));
    assert!(matches!(payload(&mut session, "edit.pasteText", &json!({"text": big})), Err(CaptureError::Unsupported(_))));
    assert!(payload(&mut session, "edit.pasteText", &json!({"text": ""})).is_err());
    // A paste that cannot apply (a control character the protocol refuses is dropped; a bad
    // position is refused) sends nothing.
    let r = replica(&["ab"]);
    assert!(capture_paste(&r, &Pos::body(0, 9), &Pos::body(0, 9), &plain("x").unwrap(), fresh()).is_err());
    let clean = plain("a\u{7}b\tc").unwrap();
    assert_eq!(clean.paragraphs[0][0].text, "ab\tc");
}

#[test]
fn the_editors_own_copy_pastes_with_its_formatting() {
    let mut session = Session::new(Document::from_text("Plain and bold"));
    session.run("select.text", &json!({"text": "bold"})).unwrap();
    session.run("format.bold", &json!({})).unwrap();
    session.run("edit.copy", &json!({})).unwrap();
    let pasted = payload(&mut session, "edit.paste", &json!({})).unwrap();
    assert_eq!(pasted.paragraphs.len(), 1);
    let run = &pasted.paragraphs[0][0];
    assert_eq!((run.text.as_str(), run.format.as_ref().and_then(|f| f.bold)), ("bold", Some(true)));
    // Keep Text Only drops it.
    let text = payload(&mut session, "edit.pasteText", &json!({})).unwrap();
    assert_eq!(text.paragraphs[0][0].format, None);
}

#[test]
fn formatting_patches_the_selected_text_of_each_paragraph() {
    let r = replica(&["Hello", "world"]);
    let f = BasicFormat { underline: Some("double".into()), ..BasicFormat::default() };
    let edit = capture_format(&r, &Pos::body(0, 3), &Pos::body(1, 2), &f).unwrap();
    let after = applied(&r, &edit.operation);
    let para = |i| after.document().para(StoryRef::Body, &Path::top(i)).unwrap().clone();
    assert_eq!(para(0).props_of_char(4).underline, Some(Underline::Double));
    assert_ne!(para(0).props_of_char(1).underline, Some(Underline::Double));
    assert_eq!(para(1).props_of_char(0).underline, Some(Underline::Double));
    assert_ne!(para(1).props_of_char(3).underline, Some(Underline::Double));
    assert_eq!(texts(&after), texts(&r));
    // A caret formats nothing: refused with an explanation.
    assert!(matches!(capture_format(&r, &Pos::body(0, 1), &Pos::body(0, 1), &f), Err(CaptureError::Unsupported(_))));
}

#[test]
fn formatting_commands_become_patches_and_toggles_follow_the_selection() {
    use wordcraft_collab_client::format::patch;
    let mut session = Session::new(Document::from_text("Hello world"));
    session.sel = Selection { anchor: Pos::body(0, 0), focus: Pos::body(0, 5) };
    assert_eq!(patch(&session, "format.bold", &json!({})).unwrap().bold, Some(true));
    session.run("format.bold", &json!({})).unwrap();
    assert_eq!(patch(&session, "format.bold", &json!({})).unwrap().bold, Some(false));
    assert_eq!(patch(&session, "format.bold", &json!({"value": true})).unwrap().bold, Some(true));
    assert_eq!(patch(&session, "format.size", &json!({"size": 10.5})).unwrap().size_half_points, Some(21));
    assert_eq!(patch(&session, "format.color", &json!({"color": "c00000"})).unwrap().color.as_deref(), Some("C00000"));
    assert_eq!(patch(&session, "format.highlight", &json!({})).unwrap().highlight.as_deref(), Some("yellow"));
    assert_eq!(patch(&session, "format.doubleUnderline", &json!({})).unwrap().underline.as_deref(), Some("double"));
    assert_eq!(patch(&session, "format.font", &json!({"name": "Liberation Serif"})).unwrap().font.as_deref(), Some("Liberation Serif"));
    for (id, params) in [
        ("format.size", json!({"size": 0})),
        ("format.size", json!({})),
        ("format.color", json!({"color": "red!"})),
        ("format.highlight", json!({"color": "sparkly"})),
    ] {
        assert!(patch(&session, id, &params).is_err(), "{id} {params}");
    }
}
