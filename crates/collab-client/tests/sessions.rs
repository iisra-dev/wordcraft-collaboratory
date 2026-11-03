//! C1.6: the proof of concept of design §16 with real editing sessions.
//!
//! Two independent `wordcraft_engine::Session`s join a shared document from a snapshot and receive
//! the same committed operations, applied to `Session::doc` by `collab-model` (never through
//! `set_document` per edit). They must end with documents identical to each other and to the
//! server's, keep laying out, and keep behaving the same under real editor commands.
//!
//! The full remote-edit bridge (selection and anchors transformed, layout invalidated, no outgoing
//! event) is C4.2; here the caret is parked at the start after each catch-up.

use proptest::prelude::*;
use serde_json::json;
use uuid::Uuid;
use wordcraft_collab_model::transform::transform_all;
use wordcraft_collab_model::{BlockId, BlockIdentityMap, DocumentOperation, ModelError, SharedDocument, apply};
use wordcraft_doc::{Block, CharProps, Document, Pos, StoryRef};
use wordcraft_engine::{Selection, Session};

/// A desktop client: a real session plus the block registry kept beside it.
struct Client {
    session: Session,
    ids: BlockIdentityMap,
    revision: usize,
}

impl Client {
    /// Join from the server's snapshot.
    fn join(server: &Server) -> Self {
        let shared = SharedDocument::from_json(server.doc.to_json().unwrap()).unwrap();
        let ids = shared.ids().clone();
        let session = Session::new(shared.into_document());
        Client { session, ids, revision: server.history.len() }
    }

    fn catch_up(&mut self, server: &Server) {
        for op in server.history.get(self.revision..).unwrap_or_default() {
            apply::apply(&mut self.session.doc, &mut self.ids, op).unwrap_or_else(|e| panic!("commit {op:?} failed in a session: {e}"));
        }
        self.revision = server.history.len();
        self.session.sel = Selection::caret(self.session.doc.start_of(StoryRef::Body));
        assert!(!self.session.layout().pages.is_empty());
    }
}

struct Server {
    doc: SharedDocument,
    history: Vec<DocumentOperation>,
}

impl Server {
    fn submit(&mut self, base: usize, op: &DocumentOperation) -> Result<(), ModelError> {
        let op = transform_all(op, self.history.get(base..).unwrap_or_default())?;
        self.doc.apply(&op)?;
        self.history.push(op);
        Ok(())
    }
}

/// The text paragraphs of a session's body as (index, id, text); tables are skipped (not
/// shared-editable yet).
fn paragraphs(doc: &Document, ids: &BlockIdentityMap) -> Vec<(BlockId, String)> {
    doc.body
        .iter()
        .zip(ids.ids())
        .filter_map(|(b, id)| match &**b {
            Block::Para(p) => Some((*id, p.text.clone())),
            Block::Table(_) => None,
        })
        .collect()
}

fn boundary(text: &str, n: u32) -> u32 {
    let cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    cuts.get(n as usize % cuts.len()).copied().unwrap_or(0) as u32
}

/// The operation a user of `client` makes: typing, deleting, Enter, or Backspace at a paragraph
/// start (merging with the previous paragraph, when both are text paragraphs and adjacent).
fn operation(client: &Client, kind: u8, para: u32, a: u32, b: u32, text: &str, new_id: BlockId) -> Option<DocumentOperation> {
    let paras = paragraphs(&client.session.doc, &client.ids);
    let i = para as usize % paras.len().max(1);
    let (block_id, t) = paras.get(i)?.clone();
    let (x, y) = (boundary(&t, a), boundary(&t, b));
    Some(match kind {
        0..=2 => DocumentOperation::InsertText { block_id, offset: x, text: text.into() },
        3 => DocumentOperation::DeleteText { block_id, start: x.min(y), end: x.max(y) },
        4 => DocumentOperation::SplitParagraph { block_id, offset: x, new_block_id: new_id },
        _ => {
            let at = client.ids.index_of(block_id)?;
            let second = client.ids.id_at(at + 1)?;
            paras.iter().any(|(id, _)| *id == second).then_some(())?;
            DocumentOperation::MergeParagraphs { first: block_id, second, first_len: t.len() as u32 }
        }
    })
}

#[derive(Clone, Debug)]
enum Step {
    Edit { client: usize, kind: u8, para: u32, a: u32, b: u32, text: String },
    CatchUp { client: usize },
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0..2usize, 0u8..6, any::<u32>(), any::<u32>(), any::<u32>(), "[a-zñ🦀 ]{1,4}")
            .prop_map(|(client, kind, para, a, b, text)| Step::Edit { client, kind, para, a, b, text }),
        1 => (0..2usize).prop_map(|client| Step::CatchUp { client }),
    ]
}

fn multibyte_document() -> Document {
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hola ñandú — 🦀 καλημέρα", &CharProps::default()).unwrap();
    doc.split_paragraph(&Pos::body(0, 5)).unwrap();
    doc
}

/// Run `steps` from `start` with two sessions; returns the number of commits.
fn simulate(start: Document, steps: &[Step]) -> usize {
    let mut n = 0u128;
    let shared = SharedDocument::new(start, || {
        n += 1;
        BlockId(Uuid::from_u128(n))
    })
    .unwrap();
    let mut server = Server { doc: shared, history: Vec::new() };
    let mut clients = [Client::join(&server), Client::join(&server)];
    // A session takes the document as it is: joining changes nothing.
    for c in &clients {
        assert_eq!(&c.session.doc, server.doc.document(), "Session::new changed the joined document");
    }
    let mut next_id = 1_000u128;
    for step in steps {
        match step {
            Step::Edit { client, kind, para, a, b, text } => {
                next_id += 1;
                let Some(c) = clients.get(*client) else { continue };
                let Some(op) = operation(c, *kind, *para, *a, *b, text, BlockId(Uuid::from_u128(next_id))) else { continue };
                match server.submit(c.revision, &op) {
                    // Conflicts, and deletions over embedded objects (the sample has some), are
                    // refused; nothing else may be.
                    Ok(()) | Err(ModelError::Conflict | ModelError::Unsupported(_)) => {}
                    Err(e) => panic!("{op:?} at revision {} refused with {e}", c.revision),
                }
            }
            Step::CatchUp { client } => {
                if let Some(c) = clients.get_mut(*client) {
                    c.catch_up(&server);
                }
            }
        }
    }
    for c in &mut clients {
        c.catch_up(&server);
        assert_eq!(&c.session.doc, server.doc.document(), "a session differs from the server");
        assert_eq!(c.ids.ids(), server.doc.ids().ids());
    }
    // The converged sessions behave the same under real editor commands.
    for c in &mut clients {
        for (id, params) in [("text.insert", json!({"text": "fin "})), ("format.bold", json!({})), ("text.insert", json!({"text": "ñ"}))] {
            c.session.run(id, &params).unwrap();
        }
    }
    let [a, b] = &clients;
    assert_eq!(a.session.doc, b.session.doc);
    server.history.len()
}

#[test]
fn design_example_reaches_two_sessions() {
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hello world", &CharProps::default()).unwrap();
    let steps = [
        // Both at revision 0: A inserts "beautiful " at 6, B "!" at 11 (design §5.3).
        Step::Edit { client: 0, kind: 0, para: 0, a: 6, b: 0, text: "beautiful ".into() },
        Step::Edit { client: 1, kind: 0, para: 0, a: 11, b: 0, text: "!".into() },
    ];
    assert_eq!(simulate(doc, &steps), 2);
}

#[test]
fn the_sample_document_converges_in_two_sessions() {
    let mut steps = Vec::new();
    let mut seed = 0x9e37_79b9_u64;
    for _ in 0..400 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let r = seed;
        steps.push(if r.is_multiple_of(5) {
            Step::CatchUp { client: (r >> 8) as usize % 2 }
        } else {
            let text = ["a", "ñ", "🦀", " word"][(r >> 40) as usize % 4].to_string();
            Step::Edit {
                client: (r >> 8) as usize % 2,
                kind: (r >> 16) as u8 % 6,
                para: (r >> 24) as u32,
                a: (r >> 32) as u32,
                b: (r >> 48) as u32,
                text,
            }
        });
    }
    let start = wordcraft_engine::sample::sample_document();
    let (blocks, tables) = (start.body.len(), start.body.iter().filter(|b| matches!(&***b, Block::Table(_))).count());
    let commits = simulate(start, &steps);
    eprintln!("sample: {blocks} body blocks ({tables} tables), {commits} commits from {} steps", steps.len());
    assert!(commits > 250, "only {commits} commits");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn two_sessions_converge(steps in proptest::collection::vec(arb_step(), 0..40)) {
        simulate(multibyte_document(), &steps);
    }
}
