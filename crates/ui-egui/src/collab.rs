//! WordCraft Collaborative in the editor (docs/collab/design.md §6, §10; milestone C4.4).
//!
//! UI commands (programmatic, no dialogs; the ribbon and a sign-in dialog come later):
//! - `ui.collab.connect {"server": "wss://host/api/v1/ws", "token": "…", "document": "<uuid>"}`
//!   opens a shared document in a new document;
//! - `ui.collab.publish {"server", "token", "title"?}` shares the current document (its DOCX is
//!   posted off the UI thread) and then connects it;
//! - `ui.collab.disconnect` stops sharing; the document stays open as a local copy;
//! - `ui.collab.acknowledge` resumes editing after a conflict was shown;
//! - `ui.collab.status` reports the state.
//!
//! While a shared document is the active one, every command the engine would run goes through
//! [`intercept`] first: supported edits are captured and submitted (the document changes when
//! the server commits them), undo/redo and unsupported edits are refused with a reason on the
//! status line, everything else runs (`wordcraft_collab_client::gate`). Each frame, [`poll`]
//! applies what arrived through `SessionBridge`. The status bar always shows the sync state
//! ([`status_text`]).
//!
//! Native only: the web build has no transport and answers these commands with an error.

use serde_json::Value;
use wordcraft_collab_client::CollaborationState;

use crate::WordApp;

/// The collaboration state of the app: at most one shared document for now.
#[derive(Default)]
pub struct Collab {
    #[cfg(not(target_arch = "wasm32"))]
    active: Option<native::Active>,
    #[cfg(not(target_arch = "wasm32"))]
    publishing: Option<native::Publishing>,
}

/// `ui.collab.*` commands; `None` for other ids.
pub fn command(app: &mut WordApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if !id.starts_with("ui.collab.") {
        return None;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, params);
        Some(Err(tl!("Shared documents need the desktop app.").into()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    Some(native::command(app, id, params))
}

/// Run `id` the shared-document way if the active document is shared; `None` otherwise (the
/// engine runs it as usual).
pub fn intercept(app: &mut WordApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, id, params);
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    native::intercept(app, id, params)
}

/// Apply what arrived since the last frame.
pub fn poll(app: &mut WordApp, ctx: &egui::Context) {
    #[cfg(target_arch = "wasm32")]
    let _ = (app, ctx);
    #[cfg(not(target_arch = "wasm32"))]
    native::poll(app, ctx);
}

/// The status bar's sync state, when the active document is shared.
pub fn status_text(app: &WordApp) -> Option<String> {
    let state = state(app)?;
    Some(
        match state {
            CollaborationState::Local => return None,
            CollaborationState::Connecting => tl!("Shared · connecting…"),
            CollaborationState::Synchronized { .. } => tl!("Shared · up to date"),
            CollaborationState::Reconnecting { .. } => tl!("Shared · offline, reconnecting…"),
            CollaborationState::Conflict { .. } => tl!("Shared · conflict, click to go on"),
            CollaborationState::ReadOnly { .. } => tl!("Shared · read-only"),
        }
        .to_string(),
    )
}

/// The active document's collaboration state, if it is shared.
pub fn state(app: &WordApp) -> Option<CollaborationState> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    native::state(app)
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::mpsc;

    use serde_json::{Value, json};
    use uuid::Uuid;
    use wordcraft_collab_client::capture::capture;
    use wordcraft_collab_client::connection::{ThreadedClient, TransportConfig, new_uuid, publish, spawn_threaded};
    use wordcraft_collab_client::gate::{Gate, gate, run_pure};
    use wordcraft_collab_client::{CollaborationState, Event, SessionBridge, SharedSession};
    use wordcraft_collab_model::{BlockId, SharedDocument};
    use wordcraft_collab_protocol::{ClientId, DocumentId, OperationId};

    use crate::WordApp;

    pub(super) fn status_json(state: Option<CollaborationState>) -> Value {
        match state {
            None => json!({"shared": false}),
            Some(s) => json!({"shared": true, "state": format!("{s:?}"), "canEdit": s.can_edit()}),
        }
    }

    pub(super) struct Active {
        /// The session document it is shown in.
        document: u64,
        client: ThreadedClient,
        bridge: SessionBridge,
        state: CollaborationState,
        /// A snapshot has been shown: the document is the shared one.
        loaded: bool,
    }

    /// A publish in flight: the server's answer, and where to connect once it comes.
    pub(super) struct Publishing {
        answer: mpsc::Receiver<Result<DocumentId, String>>,
        server: String,
        token: String,
        document: u64,
    }

    fn param<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
        params.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| format!("missing \"{key}\""))
    }

    fn fresh_id() -> Result<Uuid, String> {
        new_uuid().ok_or_else(|| tl!("No random source for new ids.").to_string())
    }

    pub(super) fn command(app: &mut WordApp, id: &str, params: &Value) -> Result<Value, String> {
        match id {
            "ui.collab.connect" => {
                let (server, token) = (param(params, "server")?.to_string(), param(params, "token")?.to_string());
                let document = param(params, "document")?;
                let document = DocumentId(Uuid::parse_str(document).map_err(|e| format!("document: {e}"))?);
                // The shared document opens in a document of its own, never over the user's.
                app.execute("file.new", json!({}))?;
                connect(app, server, token, document)
            }
            "ui.collab.publish" => {
                if app.collab.active.is_some() || app.collab.publishing.is_some() {
                    return Err(tl!("A document is already shared; disconnect it first.").into());
                }
                let (server, token) = (param(params, "server")?.to_string(), param(params, "token")?.to_string());
                let title = params.get("title").and_then(Value::as_str).map_or_else(|| app.title_stem(), str::to_string);
                let docx = wordcraft_engine::io::save_bytes("shared.docx", &app.session.doc)?;
                let (tx, answer) = mpsc::channel();
                let (s, t) = (server.clone(), token.clone());
                std::thread::Builder::new()
                    .name("wordcraft-collab-publish".into())
                    .spawn(move || {
                        let _ = tx.send(publish(&s, &t, &title, &docx));
                    })
                    .map_err(|e| e.to_string())?;
                app.collab.publishing = Some(Publishing { answer, server, token, document: app.session.document_id() });
                app.status(tl!("Sharing the document…"));
                Ok(json!({"pending": "publish"}))
            }
            "ui.collab.disconnect" => {
                // Dropping the client closes the connection; the document stays as it is.
                let was = app.collab.active.take().is_some();
                app.collab.publishing = None;
                if was {
                    app.status(tl!("Stopped sharing; this is now a local copy."));
                }
                Ok(json!({"disconnected": was}))
            }
            "ui.collab.acknowledge" => {
                if let Some(active) = &app.collab.active {
                    active.client.acknowledge_conflict();
                }
                Ok(Value::Null)
            }
            "ui.collab.status" => Ok(status_json(state(app))),
            _ => Err(format!("unknown command {id}")),
        }
    }

    /// Start the connection for the active session document.
    fn connect(app: &mut WordApp, server: String, token: String, document: DocumentId) -> Result<Value, String> {
        let client_id = ClientId::new(format!("desktop-{}", &fresh_id()?.simple().to_string()[..12])).map_err(|e| e.to_string())?;
        let client = spawn_threaded(TransportConfig::new(server, token), SharedSession::new(document, client_id)).map_err(|e| e.to_string())?;
        app.collab.active = Some(Active {
            document: app.session.document_id(),
            client,
            bridge: SessionBridge::default(),
            state: CollaborationState::Connecting,
            loaded: false,
        });
        app.status(tl!("Connecting to the shared document…"));
        Ok(json!({"connecting": document.0.to_string()}))
    }

    pub(super) fn state(app: &WordApp) -> Option<CollaborationState> {
        let active = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id())?;
        Some(active.state.clone())
    }

    pub(super) fn intercept(app: &mut WordApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
        let active = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id())?;
        let read_only = !active.loaded || !active.state.can_edit();
        let result = match gate(&app.session, id, read_only) {
            Gate::Run => run_pure(&mut app.session, id, params).map_err(|e| e.to_string()),
            Gate::Blocked(reason) => Err(crate::i18n::t(reason).to_string()),
            Gate::Capture => submit(app, id, params),
        };
        if let Err(e) = &result {
            app.status(e.clone());
        }
        Some(result)
    }

    /// Capture a supported edit and hand it to the connection; it shows when committed.
    fn submit(app: &mut WordApp, id: &str, params: &Value) -> Result<Value, String> {
        let active = app.collab.active.as_ref().ok_or_else(|| "not shared".to_string())?;
        let replica = SharedDocument::from_parts(app.session.doc.clone(), active.bridge.ids().clone()).map_err(|e| e.to_string())?;
        let new_block = BlockId(fresh_id()?);
        let edit = capture(&replica, &app.session.sel.anchor, &app.session.sel.focus, id, params, || new_block).map_err(|e| e.to_string())?;
        let Some(operation) = edit.operation else { return Ok(json!({"submitted": false})) };
        let follow_up = match edit.follow_up {
            Some(f) => Some((OperationId(fresh_id()?), f)),
            None => None,
        };
        if !active.client.submit_edit((OperationId(fresh_id()?), operation), follow_up) {
            return Err(tl!("The connection is busy; try again.").into());
        }
        Ok(json!({"submitted": true}))
    }

    pub(super) fn poll(app: &mut WordApp, ctx: &egui::Context) {
        finish_publish(app);
        let Some(mut active) = app.collab.active.take() else { return };
        // Events wait while another document is shown (they apply to the shared one only).
        if active.document == app.session.document_id() {
            while let Some(event) = active.client.poll() {
                handle(app, &mut active, event);
            }
        }
        app.collab.active = Some(active);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn handle(app: &mut WordApp, active: &mut Active, event: Event) {
        match event {
            Event::Snapshot { replica, .. } => {
                active.bridge.load(&mut app.session, &replica);
                active.loaded = true;
            }
            Event::Applied { operation, own, .. } => {
                if let Err(e) = active.bridge.apply_remote_edit(&mut app.session, &operation, own) {
                    // The session no longer matches the shared document: editing pauses until a
                    // snapshot comes (the next reconnect brings one).
                    log::error!("shared document: a commit did not apply: {e}");
                    active.loaded = false;
                    app.status(tl!("The shared document is out of step; reconnecting will fix it."));
                }
            }
            Event::Rejected { message, .. } => app.status(crate::i18n::fmt(tl!("Shared document: {reason}"), &[("reason", &message)])),
            Event::Refused { message } => app.status(crate::i18n::fmt(tl!("Shared document: {reason}"), &[("reason", &message)])),
            Event::State(state) => active.state = state,
        }
    }

    fn finish_publish(app: &mut WordApp) {
        let Some(publishing) = &app.collab.publishing else { return };
        let answer = match publishing.answer.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("publishing stopped".into()),
        };
        let Some(Publishing { server, token, document, .. }) = app.collab.publishing.take() else { return };
        match answer {
            // Connect it where it was published from, if that document is still the one shown.
            Ok(id) if document == app.session.document_id() => {
                if let Err(e) = connect(app, server, token, id) {
                    app.status(e);
                }
            }
            Ok(_) => app.status(tl!("Shared; open it from the server to edit it together.")),
            Err(e) => app.status(crate::i18n::fmt(tl!("Couldn't share the document: {reason}"), &[("reason", &e)])),
        }
    }
}
