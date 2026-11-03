//! C1.5: convergence of several replicas and an in-memory server (design §13, §16).
//!
//! The server holds the authoritative [`SharedDocument`] and the committed history. Each client
//! holds a replica and the revision it has applied. A client makes an operation against its
//! replica (often behind the server) and submits it with that base revision; the server
//! transforms it over the commits since (`transform_all`), applies it, and appends it to the
//! history, or refuses it (conflict or invalid) without changing anything. Clients apply commits
//! in order, as in the MVP's confirm-before-apply policy (§6).
//!
//! Property: after everyone catches up, every replica equals the server — document *and* block
//! registry — whatever the interleaving. A client that joins late from a snapshot converges too.

use proptest::prelude::*;
use uuid::Uuid;
use wordcraft_collab_model::transform::transform_all;
use wordcraft_collab_model::{BlockId, DocumentOperation, ModelError, SharedDocument};
use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

const CLIENTS: usize = 3;

struct Server {
    doc: SharedDocument,
    history: Vec<DocumentOperation>,
}

impl Server {
    /// Transform, apply, commit; or refuse with nothing changed.
    fn submit(&mut self, base: usize, op: &DocumentOperation) -> Result<(), ModelError> {
        let since = self.history.get(base..).unwrap_or_default();
        let op = transform_all(op, since)?;
        let before = self.doc.clone();
        let applied = self.doc.apply(&op);
        if applied.is_err() {
            assert_eq!(self.doc, before, "a refused operation changed the server: {op:?}");
        }
        applied?;
        self.history.push(op);
        Ok(())
    }
}

struct Client {
    doc: SharedDocument,
    revision: usize,
}

impl Client {
    /// Apply every commit this client has not seen. Commits were valid on the server, so they
    /// must be valid on every replica.
    fn catch_up(&mut self, server: &Server) {
        for op in server.history.get(self.revision..).unwrap_or_default() {
            self.doc.apply(op).unwrap_or_else(|e| panic!("commit {op:?} failed on a replica: {e}"));
        }
        self.revision = server.history.len();
    }
}

/// What a client does next. The numbers are resolved against the client's replica when the step
/// runs, so every generated step is meaningful whatever happened before it.
#[derive(Clone, Debug)]
enum Step {
    Edit { client: usize, kind: u8, para: u32, a: u32, b: u32, text: String },
    CatchUp { client: usize },
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0..CLIENTS, 0u8..6, any::<u32>(), any::<u32>(), any::<u32>(), "[a-zé ñ🦀\t]{1,5}")
            .prop_map(|(client, kind, para, a, b, text)| Step::Edit { client, kind, para, a, b, text }),
        1 => (0..CLIENTS).prop_map(|client| Step::CatchUp { client }),
    ]
}

/// The paragraphs of a replica as (id, text).
fn paragraphs(doc: &SharedDocument) -> Vec<(BlockId, String)> {
    doc.ids()
        .ids()
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, doc.document().para(StoryRef::Body, &Path::top(i)).map(|p| p.text.clone()).unwrap_or_default()))
        .collect()
}

/// A char boundary of `text` picked by `n`.
fn boundary(text: &str, n: u32) -> u32 {
    let cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    cuts.get(n as usize % cuts.len()).copied().unwrap_or(0) as u32
}

/// The operation a step makes on `doc`: inserts (weighted up), deletions, splits and merges of a
/// paragraph with the next one.
fn operation(doc: &SharedDocument, kind: u8, para: u32, a: u32, b: u32, text: &str, new_id: BlockId) -> Option<DocumentOperation> {
    let paras = paragraphs(doc);
    let i = para as usize % paras.len().max(1);
    let (block_id, t) = paras.get(i)?.clone();
    let (x, y) = (boundary(&t, a), boundary(&t, b));
    Some(match kind {
        0..=2 => DocumentOperation::InsertText { block_id, offset: x, text: text.into() },
        3 => DocumentOperation::DeleteText { block_id, start: x.min(y), end: x.max(y) },
        4 => DocumentOperation::SplitParagraph { block_id, offset: x, new_block_id: new_id },
        _ => {
            let (second, _) = paras.get(i + 1)?;
            DocumentOperation::MergeParagraphs { first: block_id, second: *second, first_len: t.len() as u32 }
        }
    })
}

/// A starting document with multibyte text in several paragraphs.
fn base() -> SharedDocument {
    let mut doc = Document::default();
    let texts = ["Hola ñandú", "🦀 crab — καλημέρα", "", "last line"];
    for (i, t) in texts.iter().enumerate() {
        if i > 0 {
            let end = doc.para(StoryRef::Body, &Path::top(i - 1)).map_or(0, |p| p.len());
            doc.split_paragraph(&Pos::body(i - 1, end)).unwrap();
        }
        doc.insert_text(&Pos::body(i, 0), t, &CharProps::default()).unwrap();
    }
    let mut n = 0u128;
    SharedDocument::new(doc, || {
        n += 1;
        BlockId(Uuid::from_u128(n))
    })
    .unwrap()
}

/// Run `steps`; returns (commits, refusals).
fn simulate(steps: &[Step], late_join_at: usize) -> (usize, usize) {
    let start = base();
    let mut server = Server { doc: start.clone(), history: Vec::new() };
    let mut clients: Vec<Client> = (0..CLIENTS).map(|_| Client { doc: start.clone(), revision: 0 }).collect();
    let mut late: Option<Client> = None;
    let (mut refused, mut next_id) = (0, 1_000u128);
    for (n, step) in steps.iter().enumerate() {
        if n == late_join_at {
            // A client joining now gets a snapshot of the server's state and its revision.
            let snapshot = server.doc.to_json().unwrap();
            late = Some(Client { doc: SharedDocument::from_json(snapshot).unwrap(), revision: server.history.len() });
        }
        match step {
            Step::Edit { client, kind, para, a, b, text } => {
                let Some(c) = clients.get(*client) else { continue };
                next_id += 1;
                let Some(op) = operation(&c.doc, *kind, *para, *a, *b, text, BlockId(Uuid::from_u128(next_id))) else { continue };
                match server.submit(c.revision, &op) {
                    Ok(()) => {}
                    Err(ModelError::Conflict) => refused += 1,
                    // Operations made on a replica are valid at their base; after transformation
                    // only a conflict may refuse them.
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
    clients.extend(late);
    for c in &mut clients {
        c.catch_up(&server);
        assert_eq!(paragraphs(&c.doc), paragraphs(&server.doc));
        assert_eq!(c.doc, server.doc, "a replica differs from the server");
    }
    // The server's own state survives a snapshot round trip.
    assert_eq!(SharedDocument::from_bytes(&server.doc.to_bytes().unwrap()).unwrap(), server.doc);
    (server.history.len(), refused)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// D_A = D_B = D_C = D_server, with a late joiner from a snapshot.
    #[test]
    fn replicas_converge_with_the_server(steps in proptest::collection::vec(arb_step(), 0..60), late in 0usize..60) {
        simulate(&steps, late);
    }
}

/// The schedules above really are concurrent and mostly commit: a fixed long run, checked for
/// both, so a generator change that made them trivial would be noticed.
#[test]
fn long_concurrent_run_mostly_commits() {
    let mut steps = Vec::new();
    let mut seed = 0x2545_f491_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..3_000 {
        let r = next();
        steps.push(if r % 7 == 0 {
            Step::CatchUp { client: (r >> 8) as usize % CLIENTS }
        } else {
            let text = ["a", "ñ", "🦀", " x", "é\t"][(r >> 40) as usize % 5].to_string();
            Step::Edit {
                client: (r >> 8) as usize % CLIENTS,
                kind: (r >> 16) as u8 % 6,
                para: (r >> 24) as u32,
                a: (r >> 32) as u32,
                b: (r >> 48) as u32,
                text,
            }
        });
    }
    let (commits, refused) = simulate(&steps, 1_500);
    eprintln!("{commits} commits, {refused} conflicts out of {} steps", steps.len());
    assert!(commits > 1_500, "only {commits} commits");
    assert!(refused > 0, "no concurrent conflict at all: the run is not concurrent");
}
