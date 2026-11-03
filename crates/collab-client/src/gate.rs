//! Which editor commands a shared document may run (design §5.4, §6; milestone C4.3).
//!
//! On a shared document every command goes through [`gate`]:
//! - the supported edits ([`crate::capture::SUPPORTED_COMMANDS`]) are captured as operations and
//!   submitted, never run locally;
//! - undo and redo are off: the local history would restore snapshots that predate other
//!   people's commits (collaborative undo comes later, as inverse operations, §6);
//! - commands that do not change the document (`CommandSpec::mutates == false`: navigation,
//!   selection, view, export…) run as usual, through [`run_pure`], which puts the document back
//!   and refuses if one changed it after all — the label is trusted, not blindly;
//! - every other command that changes the document is blocked, with a reason the UI shows.
//!
//! Commands the engine does not know (the UI's own `ui.*`) are the UI's business and run.

use serde_json::Value;
use wordcraft_engine::Session;

use crate::capture::is_supported;

/// Commands that walk the local undo history.
pub const UNDO_COMMANDS: &[&str] = &["edit.undo", "edit.redo", "edit.undoTo", "edit.undoHistory"];

/// Commands the engine labels as not changing the document that do change it: Repeat replays
/// the last edit, and entering a header or footer creates it. Found by
/// `tests/gate.rs::pure_commands_never_change_a_shared_document`; blocked outright. (The labels
/// are upstream's; they are not changed here to keep merges from `upstream/main` easy.)
pub const MUTATING_DESPITE_LABEL: &[&str] = &["edit.repeat", "insert.editHeader", "insert.editFooter"];

/// What to do with a command on a shared document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Run it on the session (it does not change the document): use [`run_pure`].
    Run,
    /// Capture it as a collaborative edit and submit it.
    Capture,
    /// Refuse it, telling the user why.
    Blocked(&'static str),
}

pub const UNDO_BLOCKED: &str = "Undo and redo are off in shared documents.";
pub const READ_ONLY: &str = "You can view this shared document but not edit it.";
pub const NOT_SHARED_YET: &str = "Not available in shared documents yet: only typing, Tab, line breaks, Backspace, Delete and Enter are shared.";

/// How `command` is handled on a shared document; `read_only` for a viewer (or while editing is
/// paused: reconnecting, a conflict to acknowledge).
pub fn gate(session: &Session, command: &str, read_only: bool) -> Gate {
    if UNDO_COMMANDS.contains(&command) {
        return Gate::Blocked(UNDO_BLOCKED);
    }
    if is_supported(command) {
        return if read_only { Gate::Blocked(READ_ONLY) } else { Gate::Capture };
    }
    if MUTATING_DESPITE_LABEL.contains(&command) {
        return Gate::Blocked(if read_only { READ_ONLY } else { NOT_SHARED_YET });
    }
    match session.registry.get(command) {
        Some(spec) if spec.mutates => Gate::Blocked(if read_only { READ_ONLY } else { NOT_SHARED_YET }),
        _ => Gate::Run,
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PureError {
    #[error("{0}")]
    Command(String),
    /// A command labelled as not changing the document changed it; it was put back.
    #[error("{command} changed the shared document; it was undone ({NOT_SHARED_YET})")]
    ChangedDocument { command: String },
}

/// Run a command [`gate`] let through as [`Gate::Run`]. If it changed the document after all,
/// the document is put back (other state, like the selection, keeps the command's effect) and
/// the command is reported.
pub fn run_pure(session: &mut Session, command: &str, params: &Value) -> Result<Value, PureError> {
    let before = session.doc.clone();
    let result = session.run(command, params).map_err(|e| PureError::Command(e.to_string()));
    if session.doc != before {
        session.doc = before;
        session.sel = wordcraft_engine::Selection { anchor: session.doc.clamp(&session.sel.anchor), focus: session.doc.clamp(&session.sel.focus) };
        session.touch();
        return Err(PureError::ChangedDocument { command: command.into() });
    }
    result
}
