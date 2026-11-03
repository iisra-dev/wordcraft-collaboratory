//! C2.4: snapshots and recovery. A document created through `documents::create` has a
//! revision-0 snapshot; the actor adds one every `SNAPSHOT_EVERY` commits and one when it stops.
//! Opening takes the newest snapshot that reads back intact and fits the log, then replays the
//! commits after it — falling back to older snapshots when the newest is damaged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;
use wordcraft_collab_model::BlockId;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId, RejectReason, ServerMessage};
use wordcraft_collab_server::documents::{self, DocumentHandle, SNAPSHOT_EVERY, Submission};
use wordcraft_collab_server::persistence::repository;
use wordcraft_collab_server::persistence::{Database, Timestamp, User, UserId};
use wordcraft_collab_server::storage::DocumentStorage;
use wordcraft_doc::{CharProps, Document, Pos};

const ANA: UserId = UserId(Uuid::from_u128(1));
const DOC: DocumentId = DocumentId(Uuid::from_u128(0xd0c));

struct Server {
    dir: PathBuf,
    db: Arc<Database>,
    storage: Arc<DocumentStorage>,
}

impl Server {
    /// A server on `dir` (kept across "restarts").
    fn on(dir: &Path) -> Self {
        let db = Arc::new(Database::open(&dir.join("collab.sqlite3")).unwrap());
        let storage = Arc::new(DocumentStorage::open(&dir.join("documents")).unwrap());
        Server { dir: dir.to_path_buf(), db, storage }
    }

    fn fresh(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("wordcraft-collab-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server = Self::on(&dir);
        repository::create_user(&server.db.lock().unwrap(), &User { id: ANA, oidc_subject: None, display_name: "Ana".into() }, &Timestamp::now())
            .unwrap();
        let mut doc = Document::default();
        doc.insert_text(&Pos::body(0, 0), "Hola ñandú", &CharProps::default()).unwrap();
        let mut n = 0u128;
        documents::create(&server.db, &server.storage, DOC, "Plan", ANA, doc, || {
            n += 1;
            BlockId(Uuid::from_u128(n))
        })
        .unwrap();
        server
    }

    async fn open(&self) -> Result<DocumentHandle, documents::Rejection> {
        documents::open(self.db.clone(), self.storage.clone(), DOC).await
    }

    fn snapshot_revisions(&self) -> Vec<u64> {
        repository::snapshots(&self.db.lock().unwrap(), DOC).unwrap().into_iter().map(|s| s.revision).collect()
    }

    /// Wait (bounded) until the stopped actor has written its final snapshot.
    async fn wait_for_snapshot(&self, revision: u64) {
        for _ in 0..200 {
            if self.snapshot_revisions().first() == Some(&revision) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no snapshot at revision {revision}: {:?}", self.snapshot_revisions());
    }

    fn snapshot_path(&self, revision: u64) -> PathBuf {
        let all = repository::snapshots(&self.db.lock().unwrap(), DOC).unwrap();
        let record = all.into_iter().find(|s| s.revision == revision).unwrap();
        self.dir.join("documents").join(record.stored.storage_key)
    }
}

/// `count` commits, each typing a character at the start of the first paragraph.
async fn type_chars(handle: &DocumentHandle, count: u64) {
    let block = handle.current().await.unwrap().1.ids().ids()[0];
    for _ in 0..count {
        let revision = handle.current().await.unwrap().0;
        let submission = Submission {
            user: ANA,
            client_id: ClientId::new("desktop-a").unwrap(),
            operation_id: OperationId(Uuid::from_u128(0x0900_0000 + u128::from(revision))),
            base_revision: revision,
            operation: DocumentOperation::InsertText { block_id: block, offset: 0, text: if revision % 2 == 0 { "ñ" } else { "a" }.into() },
        };
        handle.submit(submission).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_are_taken_periodically_and_on_close() {
    let server = Server::fresh("periodic");
    assert_eq!(server.snapshot_revisions(), [0]);
    let handle = server.open().await.unwrap();
    type_chars(&handle, 2 * SNAPSHOT_EVERY + 50).await;
    let before = handle.current().await.unwrap();
    assert_eq!(server.snapshot_revisions(), [2 * SNAPSHOT_EVERY, SNAPSHOT_EVERY, 0]);
    drop(handle);
    server.wait_for_snapshot(before.0).await;

    // Restart: a new process on the same directory opens the document where it was.
    let restarted = Server::on(&server.dir);
    let handle = restarted.open().await.unwrap();
    assert_eq!(handle.current().await.unwrap(), before);
    let _ = std::fs::remove_dir_all(&server.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_snapshot_falls_back_to_an_older_one() {
    let server = Server::fresh("damaged");
    let handle = server.open().await.unwrap();
    type_chars(&handle, SNAPSHOT_EVERY + 30).await;
    let before = handle.current().await.unwrap();
    drop(handle);
    server.wait_for_snapshot(before.0).await;
    assert_eq!(server.snapshot_revisions(), [SNAPSHOT_EVERY + 30, SNAPSHOT_EVERY, 0]);

    // The newest file is truncated (a crash mid-copy, a bad disk): checksum mismatch.
    let newest = server.snapshot_path(SNAPSHOT_EVERY + 30);
    let bytes = std::fs::read(&newest).unwrap();
    std::fs::write(&newest, &bytes[..bytes.len() / 2]).unwrap();
    assert_eq!(Server::on(&server.dir).open().await.unwrap().current().await.unwrap(), before);

    // The one before is gone too: back to revision 0 and the whole log.
    std::fs::remove_file(server.snapshot_path(SNAPSHOT_EVERY)).unwrap();
    assert_eq!(Server::on(&server.dir).open().await.unwrap().current().await.unwrap(), before);

    // A snapshot row claiming a revision the log never reached is refused, not believed.
    let ahead = server.storage.write_snapshot(DOC, 9_999, &before.1.to_bytes().unwrap()).unwrap();
    repository::record_snapshot(&server.db.lock().unwrap(), DOC, 9_999, &ahead, &Timestamp::now()).unwrap();
    assert_eq!(Server::on(&server.dir).open().await.unwrap().current().await.unwrap(), before);
    let _ = std::fs::remove_dir_all(&server.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_any_usable_snapshot_the_document_does_not_open() {
    let server = Server::fresh("none");
    std::fs::write(server.snapshot_path(0), b"{}").unwrap();
    let refused = server.open().await.err().unwrap();
    assert_eq!(refused.reason, RejectReason::Unavailable);
    let _ = std::fs::remove_dir_all(&server.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_document_twice_leaves_the_first_intact() {
    let server = Server::fresh("twice");
    let handle = server.open().await.unwrap();
    let original = handle.current().await.unwrap();
    drop(handle);
    let again = documents::create(&server.db, &server.storage, DOC, "Other", ANA, Document::default(), || BlockId(Uuid::from_u128(99)));
    assert_eq!(again.err().unwrap().reason, RejectReason::InvalidOperation);
    // Its revision-0 snapshot was not touched.
    assert_eq!(server.open().await.unwrap().current().await.unwrap(), original);
    assert_eq!(repository::document(&server.db.lock().unwrap(), DOC).unwrap().unwrap().title, "Plan");
    let _ = std::fs::remove_dir_all(&server.dir);
}

/// C6.1: an edit still in flight across a restart is transformed, not refused. The actor's
/// final snapshot is at the newest revision, so the history it reloads must reach back past it:
/// a client that was behind resends what it typed, based on the revision it saw.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_based_before_the_restart_snapshot_still_lands() {
    let server = Server::fresh("in-flight");
    let handle = server.open().await.unwrap();
    type_chars(&handle, 5).await;
    let block = handle.current().await.unwrap().1.ids().ids()[0];
    drop(handle);
    server.wait_for_snapshot(5).await;

    let restarted = Server::on(&server.dir);
    let handle = restarted.open().await.unwrap();
    // Typed at the end of "Hola ñandú" (12 bytes) while the client was at revision 0.
    let late = Submission {
        user: ANA,
        client_id: ClientId::new("desktop-b").unwrap(),
        operation_id: OperationId(Uuid::from_u128(0xbeef)),
        base_revision: 0,
        operation: DocumentOperation::InsertText { block_id: block, offset: 12, text: "!".into() },
    };
    let committed = handle.submit(late).await.unwrap().message;
    assert!(matches!(committed, ServerMessage::Committed { revision: 6, .. }), "{committed:?}");
    let (_, shared) = handle.current().await.unwrap();
    let text = &shared.document().body.first().and_then(|b| b.as_para()).unwrap().text;
    assert!(text.ends_with("Hola ñandú!"), "{text}");
    // And a client behind the snapshot catches up with commits, not a whole snapshot.
    let caught_up = handle.resync(ANA, 2).await.unwrap();
    assert!(caught_up.iter().all(|m| !matches!(m, ServerMessage::Snapshot { .. })), "{caught_up:?}");
    let _ = std::fs::remove_dir_all(&server.dir);
}
