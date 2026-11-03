//! C4.3: what a shared document lets the editor run.

use serde_json::json;
use wordcraft_collab_client::capture::SUPPORTED_COMMANDS;
use wordcraft_collab_client::gate::{
    Gate, MUTATING_DESPITE_LABEL, NOT_SHARED_YET, PureError, READ_ONLY, UNDO_BLOCKED, UNDO_COMMANDS, gate, run_pure,
};
use wordcraft_engine::Session;

#[test]
fn the_shared_commands_are_real_editing_commands() {
    let session = Session::new(wordcraft_engine::sample::sample_document());
    for id in SUPPORTED_COMMANDS.iter().chain(UNDO_COMMANDS) {
        let spec = session.registry.get(id).unwrap_or_else(|| panic!("{id} is not an engine command"));
        assert!(spec.mutates || UNDO_COMMANDS.contains(id), "{id} is labelled pure");
    }
}

#[test]
fn commands_are_captured_run_or_blocked() {
    let session = Session::new(wordcraft_engine::sample::sample_document());
    assert_eq!(gate(&session, "text.insert", false), Gate::Capture);
    assert_eq!(gate(&session, "text.insert", true), Gate::Blocked(READ_ONLY));
    assert_eq!(gate(&session, "edit.undo", false), Gate::Blocked(UNDO_BLOCKED));
    assert_eq!(gate(&session, "format.bold", false), Gate::Blocked(NOT_SHARED_YET));
    assert_eq!(gate(&session, "insert.table", false), Gate::Blocked(NOT_SHARED_YET));
    assert_eq!(gate(&session, "ui.zoomIn", false), Gate::Run);
    let pure = session.registry.all().iter().find(|s| !s.mutates).unwrap();
    assert_eq!(gate(&session, pure.id, true), Gate::Run);
}

/// Every command the engine labels as not changing the document leaves the shared document
/// as it was when run through `run_pure` — and any that would not is reported here.
#[test]
fn pure_commands_never_change_a_shared_document() {
    let mut session = Session::new(wordcraft_engine::sample::sample_document());
    let ids: Vec<&'static str> = session.registry.all().iter().filter(|s| !s.mutates && !s.id.starts_with("file.")).map(|s| s.id).collect();
    assert!(ids.len() > 50, "only {} pure commands", ids.len());
    let mut mislabelled = Vec::new();
    for id in ids {
        let before = session.doc.clone();
        if let Err(PureError::ChangedDocument { command }) = run_pure(&mut session, id, &json!({})) {
            mislabelled.push(command);
        }
        assert_eq!(session.doc, before, "{id} left the document changed");
    }
    // Each is blocked by name, so it never runs on a shared document; a new one fails here.
    for id in &mislabelled {
        assert!(MUTATING_DESPITE_LABEL.contains(&id.as_str()), "{id} is labelled pure but changes the document: add it to MUTATING_DESPITE_LABEL");
        assert_eq!(gate(&session, id, false), Gate::Blocked(NOT_SHARED_YET));
    }
}
