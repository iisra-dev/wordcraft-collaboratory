//! C2.2: the document actor against a real (in-memory or on-disk) SQLite database.
//!
//! Covers the design §13 scenarios that belong to the actor: concurrent inserts converge and
//! reach every participant, a resent operation applies once, an unsupported or conflicting one
//! changes nothing, viewers and strangers are refused, a restart keeps revision and content, a
//! slow participant is dropped rather than waited for, and a storage failure changes nothing.

use std::sync::Arc;

use uuid::Uuid;
use wordcraft_collab_model::{BlockId, SharedDocument};
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId, RejectReason, ServerMessage};
use wordcraft_collab_server::documents::{self, DocumentHandle, SUBSCRIBER_QUEUE, Submission};
use wordcraft_collab_server::persistence::repository;
use wordcraft_collab_server::persistence::{Database, Role, Timestamp, User, UserId};
use wordcraft_doc::{CharProps, Document, Path, Pos, StoryRef};

const ANA: UserId = UserId(Uuid::from_u128(1));
const BEA: UserId = UserId(Uuid::from_u128(2));
const VIC: UserId = UserId(Uuid::from_u128(3));
const EVE: UserId = UserId(Uuid::from_u128(4));
const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));
const BLOCK: BlockId = BlockId(Uuid::from_u128(0xb1));

/// The document as created: one paragraph "Hello world" with id [`BLOCK`].
fn base() -> SharedDocument {
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hello world", &CharProps::default()).unwrap();
    SharedDocument::new(doc, || BLOCK).unwrap()
}

/// A database where Ana owns [`DOC`], Bea edits it and Vic views it; Eve is a stranger.
fn database(db: Database) -> Arc<Database> {
    {
        let mut conn = db.lock().unwrap();
        let now = Timestamp::now();
        for (id, name) in [(ANA, "Ana"), (BEA, "Bea"), (VIC, "Vic"), (EVE, "Eve")] {
            repository::create_user(&conn, &User { id, oidc_subject: None, display_name: name.into() }, &now).unwrap();
        }
        repository::create_document(&mut conn, DOC, "Plan", ANA, &now).unwrap();
        repository::set_member(&mut conn, DOC, BEA, Role::Editor).unwrap();
        repository::set_member(&mut conn, DOC, VIC, Role::Viewer).unwrap();
    }
    Arc::new(db)
}

async fn actor() -> (Arc<Database>, DocumentHandle) {
    let db = database(Database::open_in_memory().unwrap());
    let handle = documents::start(db.clone(), DOC, base(), 0).await.unwrap();
    (db, handle)
}

fn op_id(n: u128) -> OperationId {
    OperationId(Uuid::from_u128(0x0900 + n))
}

fn submission(user: UserId, client: &str, n: u128, base_revision: u64, operation: DocumentOperation) -> Submission {
    Submission { user, client_id: ClientId::new(client).unwrap(), operation_id: op_id(n), base_revision, operation }
}

fn insert(offset: u32, text: &str) -> DocumentOperation {
    DocumentOperation::InsertText { block_id: BLOCK, offset, text: text.into() }
}

async fn text(handle: &DocumentHandle) -> (u64, String) {
    let (revision, shared) = handle.current().await.unwrap();
    (revision, shared.document().para(StoryRef::Body, &Path::top(0)).unwrap().text.clone())
}

fn revision_of(message: &ServerMessage) -> u64 {
    match message {
        ServerMessage::Committed { revision, .. } => *revision,
        other => panic!("not a commit: {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_inserts_converge_and_reach_everyone() {
    let (_db, handle) = actor().await;
    let mut a = handle.join(ANA).await.unwrap();
    let mut v = handle.join(VIC).await.unwrap();
    assert_eq!(a.revision, 0);
    let ServerMessage::Snapshot { snapshot, revision: 0, .. } = &a.snapshot else { panic!("{:?}", a.snapshot) };
    assert_eq!(SharedDocument::from_json(snapshot.clone()).unwrap(), base());

    // Design §5.3: both at revision 0.
    let first = handle.submit(submission(ANA, "desktop-a", 1, 0, insert(6, "beautiful "))).await.unwrap();
    let second = handle.submit(submission(BEA, "desktop-b", 2, 0, insert(11, "!"))).await.unwrap();
    assert_eq!((revision_of(&first), revision_of(&second)), (1, 2));
    // The second was transformed: what was committed is what everyone applies.
    assert!(matches!(&second, ServerMessage::Committed { operation, .. } if *operation == insert(21, "!")));
    assert_eq!(text(&handle).await, (2, "Hello beautiful world!".into()));

    // Every participant, the author and a viewer included, got both commits in order; a replica
    // that applies them ends where the server is.
    for joined in [&mut a, &mut v] {
        let mut replica = base();
        for want in [&first, &second] {
            let got = joined.updates.recv().await.unwrap();
            assert_eq!(&got, want);
            let ServerMessage::Committed { operation, .. } = got else { unreachable!() };
            replica.apply(&operation).unwrap();
        }
        assert_eq!(replica, handle.current().await.unwrap().1);
    }
}

#[tokio::test]
async fn a_resent_operation_applies_once() {
    let (db, handle) = actor().await;
    let mut a = handle.join(ANA).await.unwrap();
    let original = handle.submit(submission(ANA, "desktop-a", 1, 0, insert(0, ">"))).await.unwrap();
    // The connection dropped before the ACK; the client resends after reconnecting.
    let again = handle.submit(submission(ANA, "desktop-a", 1, 0, insert(0, ">"))).await.unwrap();
    assert_eq!(again, original);
    assert_eq!(text(&handle).await, (1, ">Hello world".into()));
    assert_eq!(repository::operations_since(&db.lock().unwrap(), DOC, 0, 10).unwrap().len(), 1);
    // Published once.
    assert_eq!(a.updates.recv().await.unwrap(), original);
    assert!(a.updates.try_recv().is_err());
}

#[tokio::test]
async fn refused_submissions_change_nothing() {
    let (db, handle) = actor().await;
    handle.submit(submission(ANA, "desktop-a", 1, 0, insert(2, "x"))).await.unwrap();
    let cases = [
        (submission(VIC, "desktop-v", 2, 1, insert(0, "v")), RejectReason::Forbidden),
        (submission(EVE, "desktop-e", 3, 1, insert(0, "e")), RejectReason::Forbidden),
        (submission(ANA, "desktop-a", 4, 9, insert(0, "future")), RejectReason::StaleRevision),
        (submission(ANA, "desktop-a", 5, 1, insert(99, "far")), RejectReason::InvalidOperation),
        (submission(ANA, "desktop-a", 6, 1, insert(1, "a\u{FFFC}")), RejectReason::InvalidOperation),
        (
            submission(ANA, "desktop-a", 7, 1, DocumentOperation::DeleteText { block_id: BlockId(Uuid::from_u128(77)), start: 0, end: 1 }),
            RejectReason::InvalidOperation,
        ),
        // Made at revision 0 over "Hello", where "x" was then inserted: it would delete their text.
        (submission(BEA, "desktop-b", 8, 0, DocumentOperation::DeleteText { block_id: BLOCK, start: 0, end: 5 }), RejectReason::Conflict),
    ];
    for (s, reason) in cases {
        let id = s.operation_id;
        let rejection = handle.submit(s).await.unwrap_err();
        assert_eq!(rejection.reason, reason, "{rejection:?}");
        assert!(repository::find_operation(&db.lock().unwrap(), DOC, id).unwrap().is_none());
    }
    assert_eq!(text(&handle).await, (1, "Hexllo world".into()));
    assert!(matches!(handle.join(EVE).await, Err(r) if r.reason == RejectReason::Forbidden));
}

#[tokio::test]
async fn a_restart_keeps_revision_and_content() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-actor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("collab.sqlite3");
    let before = {
        let db = database(Database::open(&path).unwrap());
        let handle = documents::start(db, DOC, base(), 0).await.unwrap();
        for (n, (offset, t)) in [(0, "¡"), (13, "!"), (8, "ñandú ")].into_iter().enumerate() {
            let rev = text(&handle).await.0;
            handle.submit(submission(ANA, "desktop-a", n as u128, rev, insert(offset, t))).await.unwrap();
        }
        handle.current().await.unwrap()
    };
    // The process stopped; a new one starts from the document as created and replays the log.
    let db = Arc::new(Database::open(&path).unwrap());
    let handle = documents::start(db, DOC, base(), 0).await.unwrap();
    let after = handle.current().await.unwrap();
    assert_eq!(after, before);
    assert_eq!(text(&handle).await, (3, "¡Hello ñandú world!".into()));
    // And it carries on from there, still idempotent for what was committed before the restart.
    let again = handle.submit(submission(ANA, "desktop-a", 0, 0, insert(0, "¡"))).await.unwrap();
    assert_eq!(revision_of(&again), 1);
    let next = handle.submit(submission(BEA, "desktop-b", 10, 3, insert(0, "> "))).await.unwrap();
    assert_eq!(revision_of(&next), 4);
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_participant_that_falls_behind_is_dropped() {
    let (_db, handle) = actor().await;
    let mut slow = handle.join(VIC).await.unwrap();
    let mut fast = handle.join(ANA).await.unwrap();
    for n in 0..(SUBSCRIBER_QUEUE as u128 + 10) {
        let rev = n as u64;
        handle.submit(submission(ANA, "desktop-a", n, rev, insert(0, "a"))).await.unwrap();
        // The fast one keeps up.
        assert_eq!(revision_of(&fast.updates.recv().await.unwrap()), rev + 1);
    }
    // The slow one gets what fit in its queue, then its channel closes: it must resync.
    let mut received = 0;
    while slow.updates.recv().await.is_some() {
        received += 1;
    }
    assert_eq!(received, SUBSCRIBER_QUEUE);
    let missed = handle.resync(VIC, received as u64).await.unwrap();
    assert_eq!(missed.len(), 10);
    assert_eq!(revision_of(&missed[0]), received as u64 + 1);
}

#[tokio::test]
async fn resync_returns_what_was_missed() {
    let (_db, handle) = actor().await;
    for n in 0..3u128 {
        handle.submit(submission(BEA, "desktop-b", n, n as u64, insert(0, "b"))).await.unwrap();
    }
    let missed = handle.resync(VIC, 1).await.unwrap();
    assert_eq!(missed.iter().map(revision_of).collect::<Vec<_>>(), [2, 3]);
    assert!(handle.resync(VIC, 3).await.unwrap().is_empty());
    assert_eq!(handle.resync(VIC, 4).await.unwrap_err().reason, RejectReason::StaleRevision);
    assert_eq!(handle.resync(EVE, 0).await.unwrap_err().reason, RejectReason::Forbidden);
}

#[tokio::test]
async fn a_storage_failure_changes_nothing() {
    let (db, handle) = actor().await;
    let mut a = handle.join(ANA).await.unwrap();
    db.lock().unwrap().execute_batch("PRAGMA query_only = ON").unwrap();
    let refused = handle.submit(submission(ANA, "desktop-a", 1, 0, insert(0, "lost?"))).await.unwrap_err();
    assert_eq!(refused.reason, RejectReason::Unavailable);
    assert_eq!(text(&handle).await, (0, "Hello world".into()));
    assert!(a.updates.try_recv().is_err());
    // Storage back: the client retries the same operation and it commits once.
    db.lock().unwrap().execute_batch("PRAGMA query_only = OFF").unwrap();
    let committed = handle.submit(submission(ANA, "desktop-a", 1, 0, insert(0, "kept "))).await.unwrap();
    assert_eq!(revision_of(&committed), 1);
    assert_eq!(text(&handle).await, (1, "kept Hello world".into()));
}

#[tokio::test]
async fn a_log_that_does_not_match_the_base_is_refused_at_start() {
    let db = database(Database::open_in_memory().unwrap());
    let handle = documents::start(db.clone(), DOC, base(), 0).await.unwrap();
    handle.submit(submission(ANA, "desktop-a", 1, 0, insert(0, "x"))).await.unwrap();
    drop(handle);
    // A base whose paragraph has another id: the logged operation names a block it lacks.
    let other = SharedDocument::new(Document::default(), || BlockId(Uuid::from_u128(0xff))).unwrap();
    assert_eq!(documents::start(db.clone(), DOC, other, 0).await.err().unwrap().reason, RejectReason::Unavailable);
    // A base claimed to be newer than the database.
    assert_eq!(documents::start(db, DOC, base(), 5).await.err().unwrap().reason, RejectReason::Unavailable);
}
