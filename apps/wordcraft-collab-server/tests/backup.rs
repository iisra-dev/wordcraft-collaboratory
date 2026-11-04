//! C6.2: encrypted backups and their restoration. A backup taken while a document is open and
//! changing restores into a server that opens every document where it was; it is unreadable
//! without the private key, a damaged or tampered one is refused, restoring never writes over
//! data, and old backups are pruned.

use std::io::Write;
use std::path::{Path, PathBuf};

use age::secrecy::ExposeSecret;
use uuid::Uuid;
use wordcraft_collab_model::BlockId;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId};
use wordcraft_collab_server::backup::{self, BackupConfig, BackupError};
use wordcraft_collab_server::documents::{self, DocumentHandle, SNAPSHOT_EVERY, Submission};
use wordcraft_collab_server::persistence::{Timestamp, User, UserId, repository};
use wordcraft_collab_server::{AppState, Config};
use wordcraft_doc::{CharProps, Document, Path as DocPath, Pos, StoryRef};

const ANA: UserId = UserId(Uuid::from_u128(1));
const PLAN: DocumentId = DocumentId(Uuid::from_u128(0xd0c));
const NOTES: DocumentId = DocumentId(Uuid::from_u128(0xd0d));

struct Fixture {
    dir: PathBuf,
    key: age::x25519::Identity,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("wordcraft-collab-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fixture { dir, key: age::x25519::Identity::generate() }
    }

    fn config(&self) -> Config {
        config_in(&self.dir.join("data"))
    }

    fn settings(&self, keep: usize) -> BackupConfig {
        BackupConfig { dir: self.dir.join("backups"), recipients: vec![self.key.to_public()], keep }
    }

    /// The private key as `age-keygen` writes it.
    fn identity_file(&self) -> PathBuf {
        let path = self.dir.join("key.txt");
        std::fs::write(&path, format!("# test key\n{}\n", self.key.to_string().expose_secret())).unwrap();
        path
    }
}

fn config_in(data: &Path) -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        database_path: data.join("wordcraft.db"),
        document_storage: data.join("documents"),
        oidc: None,
    }
}

fn text_doc(text: &str) -> Document {
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), text, &CharProps::default()).unwrap();
    doc
}

/// Ana, and two documents: "Plan" (to be edited) and "Notes".
fn populate(config: &Config) -> AppState {
    let state = AppState::open(config).unwrap();
    repository::create_user(&state.db.lock().unwrap(), &User { id: ANA, oidc_subject: None, display_name: "Ana".into() }, &Timestamp::now()).unwrap();
    let mut n = 0u128;
    let mut ids = || {
        n += 1;
        BlockId(Uuid::from_u128(n))
    };
    documents::create(&state.db, &state.storage, PLAN, "Plan", ANA, text_doc("Hola ñandú"), &mut ids).unwrap();
    documents::create(&state.db, &state.storage, NOTES, "Notes", ANA, text_doc("Secret notes"), &mut ids).unwrap();
    state
}

async fn type_chars(handle: &DocumentHandle, count: u64, from: u128) {
    let block = handle.current().await.unwrap().1.ids().ids()[0];
    for i in 0..count {
        let revision = handle.current().await.unwrap().0;
        let submission = Submission {
            user: ANA,
            client_id: ClientId::new("desktop-a").unwrap(),
            operation_id: OperationId(Uuid::from_u128(from + u128::from(i))),
            base_revision: revision,
            operation: DocumentOperation::InsertText { block_id: block, offset: 0, text: "a".into() },
        };
        handle.submit(submission).await.unwrap();
    }
}

fn text_of(shared: &wordcraft_collab_model::SharedDocument) -> String {
    shared.document().para(StoryRef::Body, &DocPath::top(0)).unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_backup_restores_every_document_where_it_was() {
    let fx = Fixture::new("live");
    let state = populate(&fx.config());
    // "Plan" is open and has gone past a periodic snapshot, as on a running server.
    let plan = documents::open(state.db.clone(), state.storage.clone(), PLAN).await.unwrap();
    type_chars(&plan, SNAPSHOT_EVERY + 7, 0x100).await;
    let before = plan.current().await.unwrap();

    let (config, settings) = (fx.config(), fx.settings(14));
    let summary = tokio::task::spawn_blocking(move || backup::create(&config, &settings)).await.unwrap().unwrap();
    assert_eq!(summary.documents, 2);
    assert!(summary.snapshots >= 3, "revision 0 of both documents and Plan's periodic one: {summary:?}");
    let name = summary.path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("wordcraft-") && name.ends_with(".wcbackup.age"), "{name}");
    // Encrypted: none of the text, titles or names are in the file; nothing else was left behind.
    let bytes = std::fs::read(&summary.path).unwrap();
    for plain in ["Secret notes", "Notes", "Ana", "SQLite format"] {
        assert!(!bytes.windows(plain.len()).any(|w| w == plain.as_bytes()), "{plain:?} in the backup");
    }
    assert_eq!(std::fs::read_dir(fx.dir.join("backups")).unwrap().count(), 1, "no temporary files left");

    // The live document keeps going after the backup; the backup has the state it was taken at.
    type_chars(&plan, 3, 0x900).await;
    let identities = backup::identities(&fx.identity_file()).unwrap();
    let verified = backup::verify(&summary.path, &identities).unwrap();
    assert_eq!((verified.documents, verified.snapshots), (2, summary.snapshots));
    let target = fx.dir.join("restored");
    backup::restore(&summary.path, &identities, &target).unwrap();

    // A server on the restored data opens both documents where they were.
    let restored = AppState::open(&config_in(&target)).unwrap();
    let plan_again = documents::open(restored.db.clone(), restored.storage.clone(), PLAN).await.unwrap();
    assert_eq!(plan_again.current().await.unwrap(), before);
    let notes = documents::open(restored.db.clone(), restored.storage.clone(), NOTES).await.unwrap();
    assert_eq!(text_of(&notes.current().await.unwrap().1), "Secret notes");
    assert_eq!(repository::user(&restored.db.lock().unwrap(), ANA).unwrap().unwrap().display_name, "Ana");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_key_or_intact_bytes_nothing_is_restored() {
    let fx = Fixture::new("refused");
    drop(populate(&fx.config()));
    let summary = backup::create(&fx.config(), &fx.settings(14)).unwrap();
    let identities = backup::identities(&fx.identity_file()).unwrap();

    // Another key cannot read it.
    let other = Fixture::new("refused-other");
    let wrong = backup::identities(&other.identity_file()).unwrap();
    assert!(matches!(backup::verify(&summary.path, &wrong), Err(BackupError::Decrypt(_))));

    // A flipped byte is caught by age's authentication.
    let mut bytes = std::fs::read(&summary.path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    let tampered = fx.dir.join("tampered.wcbackup.age");
    std::fs::write(&tampered, &bytes).unwrap();
    assert!(backup::verify(&tampered, &identities).is_err());

    // Restoring never writes over anything.
    let occupied = fx.dir.join("occupied");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("keep.txt"), "mine").unwrap();
    assert!(matches!(backup::restore(&summary.path, &identities, &occupied), Err(BackupError::Config(_))));
    assert_eq!(std::fs::read_dir(&occupied).unwrap().count(), 1);
    assert!(backup::restore(&summary.path, &identities, &fx.dir.join("fresh")).is_ok());
}

/// A crafted archive (right key, wrong contents) cannot write outside the target or pass a
/// checksum it does not match.
#[test]
fn a_crafted_archive_is_refused() {
    let fx = Fixture::new("crafted");
    let identities = backup::identities(&fx.identity_file()).unwrap();
    let seal = |entries: &[(&str, &[u8])]| -> PathBuf {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let plain = zip.finish().unwrap().into_inner();
        let path = fx.dir.join(format!("crafted-{}.age", entries.len()));
        let recipient = fx.key.to_public();
        let encryptor = age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient)).unwrap();
        let mut out = encryptor.wrap_output(std::fs::File::create(&path).unwrap()).unwrap();
        out.write_all(&plain).unwrap();
        out.finish().unwrap();
        path
    };
    let manifest =
        |files: &str| format!(r#"{{"format":"wordcraft-collab-backup/1","created_at":"x","schema_version":4,"documents":0,"files":[{files}]}}"#);
    let escape = format!(r#"{{"path":"documents/../../evil","sha256":"{}"}}"#, wordcraft_collab_server::storage::sha256_hex(b"x"));
    let path = seal(&[("manifest.json", manifest(&escape).as_bytes()), ("documents/../../evil", b"x")]);
    let target = fx.dir.join("t1");
    assert!(matches!(backup::restore(&path, &identities, &target), Err(BackupError::Damaged(_))));
    assert!(!fx.dir.join("evil").exists());

    let lying = r#"{"path":"wordcraft.db","sha256":"00"}"#;
    let path = seal(&[("manifest.json", manifest(lying).as_bytes()), ("wordcraft.db", b"not a database"), ("extra", b"")]);
    assert!(matches!(backup::restore(&path, &identities, &fx.dir.join("t2")), Err(BackupError::Damaged(_))));

    let path = seal(&[("manifest.json", br#"{"format":"something-else/9","created_at":"","schema_version":0,"documents":0,"files":[]}"#)]);
    assert!(matches!(backup::restore(&path, &identities, &fx.dir.join("t3")), Err(BackupError::Damaged(_))));
}

#[test]
fn old_backups_are_pruned_and_settings_are_checked() {
    let fx = Fixture::new("prune");
    drop(populate(&fx.config()));
    let mut made = Vec::new();
    for _ in 0..3 {
        made.push(backup::create(&fx.config(), &fx.settings(2)).unwrap().path);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A file of someone else's in the folder is never touched.
    std::fs::write(fx.dir.join("backups").join("notes.txt"), "mine").unwrap();
    assert_eq!(backup::list(&fx.dir.join("backups")).unwrap(), made[1..]);
    assert!(fx.dir.join("backups").join("notes.txt").exists());

    let public = fx.key.to_public().to_string();
    let vars = |recipients: &'static str, keep: Option<&'static str>| {
        move |name: &str| match name {
            "BACKUP_RECIPIENTS" => Some(recipients.to_string()),
            "BACKUP_KEEP" => keep.map(str::to_string),
            _ => None,
        }
    };
    assert!(matches!(BackupConfig::from_vars(vars("", None)), Err(BackupError::Config(_))));
    assert!(matches!(BackupConfig::from_vars(vars("not-a-key", None)), Err(BackupError::Config(_))));
    let both: &'static str = Box::leak(format!("{public}, {public}\n").into_boxed_str());
    let settings = BackupConfig::from_vars(vars(both, Some("3"))).unwrap();
    assert_eq!((settings.recipients.len(), settings.keep, settings.dir), (2, 3, PathBuf::from("backups")));
    assert!(matches!(BackupConfig::from_vars(vars(both, Some("many"))), Err(BackupError::Config(_))));
}
