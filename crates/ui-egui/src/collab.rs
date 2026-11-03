//! WordCraft Collaborative in the editor (docs/collab/design.md §6, §10; milestone C4.4).
//!
//! UI commands (programmatic, no dialogs; Review › Share opens the connection forms):
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
    /// The server and token last used, to fill in the dialogs: in memory only (a token is never
    /// written to disk; the system's credential store comes with C5.1).
    last_server: String,
    last_token: String,
    /// The identity provider and client id last signed in with (C5.1b).
    last_issuer: String,
    last_client_id: String,
    #[cfg(not(target_arch = "wasm32"))]
    signing_in: Option<std::sync::mpsc::Receiver<Result<native::SignedIn, String>>>,
    /// The signed-in OIDC session (renews its access token); see `credentials`.
    #[cfg(not(target_arch = "wasm32"))]
    session: Option<std::sync::Arc<wordcraft_collab_client::oidc::TokenSource>>,
    /// The kept sign-in was looked for (once, when a share dialog first opens).
    #[cfg(not(target_arch = "wasm32"))]
    restore_tried: bool,
    #[cfg(not(target_arch = "wasm32"))]
    active: Option<native::Active>,
    #[cfg(not(target_arch = "wasm32"))]
    publishing: Option<native::Publishing>,
}

impl Collab {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn remember(&mut self, server: &str, token: &str) {
        self.last_server = server.to_string();
        self.last_token = token.to_string();
    }
}

/// Whether a second connection would replace an existing shared document or publication.
pub fn busy(app: &WordApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return app.collab.active.is_some() || app.collab.publishing.is_some();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        false
    }
}

/// Discoverable app-owned commands. Listing them never opens a dialog or makes a connection.
pub fn commands(app: &WordApp) -> Vec<Value> {
    use serde_json::json;
    let native = !cfg!(target_arch = "wasm32");
    [
        ("connect", "Open a Shared Document", "server: ws:// or wss://…/ws; token: bearer token; document: UUID. Opens in a new document; never opens a dialog.", native && !busy(app)),
        ("publish", "Share Document", "server: ws:// or wss://…/ws; token: bearer token; title?: 1..200 characters. Publishes the current DOCX in the background; never opens a dialog.", native && app.has_document() && !busy(app)),
        ("disconnect", "Stop Sharing", "Stops the connection or pending publication; keeps a local copy. An upload already sent may still finish on the server.", native && busy(app)),
        ("copyId", "Copy Document ID", "Copies the current shared document's UUID to the clipboard; never copies credentials.", native && state(app).is_some()),
        ("acknowledge", "Continue after Conflict", "Acknowledges the reported conflict; never replays a rejected edit.", native && matches!(state(app), Some(CollaborationState::Conflict { .. }))),
        ("signIn", "Sign In", "issuer: OIDC issuer URL (https, or http on 127.0.0.1); clientId: WordCraft's client id there. Opens the system browser (Authorization Code + PKCE); the token stays in memory.", native && !signing_in(app)),
        ("signOut", "Sign Out", "Forgets the identity provider sign-in kept in the system credential store.", native),
        ("status", "Shared Document Status", "Reports shared state, document UUID and publishing flag; never credentials.", native),
    ].into_iter().map(|(id, label, params, enabled)| json!({
        "id": format!("ui.collab.{id}"), "label": label, "params": params,
        "location": "Review › Share", "shortcut": null, "enabled": enabled,
    })).collect()
}

/// Whether a browser sign-in is waiting for the user.
/// Look for a sign-in kept in the OS credential store (once; off the UI thread).
pub fn restore_sign_in(app: &mut WordApp) {
    #[cfg(not(target_arch = "wasm32"))]
    native::restore_sign_in(app);
    #[cfg(target_arch = "wasm32")]
    let _ = app;
}

pub fn signing_in(app: &WordApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    return app.collab.signing_in.is_some();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        false
    }
}

/// The Share Document / Open Shared dialog's fields.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareForm {
    /// Share the current document (else open a shared one).
    pub publish: bool,
    pub server: String,
    pub token: String,
    pub title: String,
    pub document: String,
    /// Identity provider (OIDC issuer) and WordCraft's client id there, to sign in instead of
    /// pasting a token.
    pub issuer: String,
    pub client_id: String,
    pub message: String,
    pub focus: bool,
}

impl std::fmt::Debug for ShareForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareForm").field("publish", &self.publish).field("credentials", &"[redacted]").finish_non_exhaustive()
    }
}

/// The form as the control channel reports it: everything but the token, which never leaves the
/// dialog (state dumps, logs).
impl serde::Serialize for ShareForm {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let view = serde_json::json!({
            "publish": self.publish,
            "server": self.server,
            "hasToken": !self.token.is_empty(),
            "title": self.title,
            "document": self.document,
            "issuer": self.issuer,
            "clientId": self.client_id,
            "message": self.message,
        });
        view.serialize(serializer)
    }
}

impl ShareForm {
    pub fn new(app: &WordApp, publish: bool) -> Self {
        Self {
            publish,
            server: app.collab.last_server.clone(),
            token: app.collab.last_token.clone(),
            title: app.title_stem(),
            document: String::new(),
            issuer: app.collab.last_issuer.clone(),
            client_id: app.collab.last_client_id.clone(),
            message: String::new(),
            focus: true,
        }
    }

    pub fn name(&self) -> &'static str {
        if self.publish { "sharePublish" } else { "shareOpen" }
    }

    pub fn title(&self) -> &'static str {
        if self.publish { "Share Document" } else { "Open a Shared Document" }
    }
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

/// The same shared-document gate used by execution, for disabled ribbon controls and tooltips.
pub fn disabled_reason(app: &WordApp, id: &str) -> Option<&'static str> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, id);
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    native::disabled_reason(app, id)
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
    #[cfg(not(target_arch = "wasm32"))]
    if app.collab.publishing.as_ref().is_some_and(|p| p.document == app.session.document_id()) {
        return Some(tl!("Shared · publishing…").to_string());
    }
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
    use wordcraft_collab_client::credentials;
    use wordcraft_collab_client::gate::{Gate, gate, run_pure};
    use wordcraft_collab_client::oidc::{OidcClient, TokenSource, Tokens};
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
        shared_document: DocumentId,
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
        pub(super) document: u64,
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
                ensure_available(app)?;
                let (server, token) = (param(params, "server")?.to_string(), param(params, "token")?.to_string());
                validate_connection(&server, &token)?;
                let document = param(params, "document")?;
                let document = DocumentId(Uuid::parse_str(document).map_err(|e| format!("document: {e}"))?);
                // The shared document opens in a document of its own, never over the user's.
                app.execute("file.new", json!({}))?;
                connect(app, server, token, document)
            }
            "ui.collab.publish" => {
                ensure_available(app)?;
                let (server, token) = (param(params, "server")?.to_string(), param(params, "token")?.to_string());
                validate_connection(&server, &token)?;
                let title = params.get("title").and_then(Value::as_str).map_or_else(|| app.title_stem(), str::to_string);
                if title.trim().is_empty() || title.chars().count() > 200 || title.chars().any(char::is_control) {
                    return Err(tl!("Enter a title of 1 to 200 characters.").into());
                }
                let docx = wordcraft_engine::io::save_bytes("shared.docx", &app.session.doc)?;
                let (tx, answer) = mpsc::channel();
                let (s, t) = (server.clone(), token.clone());
                let source = session_for(app, &token);
                std::thread::Builder::new()
                    .name("wordcraft-collab-publish".into())
                    .spawn(move || {
                        // A signed-in session gives a current token (renewed if it expired).
                        let t = match &source {
                            Some(source) => source.token(false).unwrap_or(t),
                            None => t,
                        };
                        let _ = tx.send(publish(&s, &t, &title, &docx));
                    })
                    .map_err(|e| e.to_string())?;
                app.collab.remember(&server, &token);
                app.collab.publishing = Some(Publishing { answer, server, token, document: app.session.document_id() });
                app.status(tl!("Sharing the document…"));
                Ok(json!({"pending": "publish"}))
            }
            "ui.collab.disconnect" => {
                // Dropping the client closes the connection; the document stays as it is.
                let was_active = app.collab.active.take().is_some();
                let was = app.collab.publishing.take().is_some() || was_active;
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
            "ui.collab.copyId" => {
                let active = app
                    .collab
                    .active
                    .as_ref()
                    .filter(|a| a.document == app.session.document_id())
                    .ok_or_else(|| tl!("This document is not shared.").to_string())?;
                let id = active.shared_document.0.to_string();
                let ctx = app.ctx.as_ref().ok_or_else(|| tl!("The clipboard is not available yet.").to_string())?;
                ctx.copy_text(id.clone());
                Ok(json!({"document": id}))
            }
            "ui.collab.signIn" => {
                if app.collab.signing_in.is_some() {
                    return Err(tl!("A sign-in is already waiting in your browser.").into());
                }
                let issuer = param(params, "issuer")?.trim().to_string();
                let client_id = param(params, "clientId")?.trim().to_string();
                let loopback = issuer.starts_with("http://127.0.0.1:") || issuer.starts_with("http://localhost:");
                if !(issuer.starts_with("https://") || loopback) || issuer.len() > 2048 || issuer.contains(['?', '#', '@']) {
                    return Err(tl!("Enter the identity provider's https:// issuer address.").into());
                }
                if client_id.len() > 255 || !client_id.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(tl!("Enter WordCraft's client id at the identity provider.").into());
                }
                let client = wordcraft_collab_client::oidc::OidcClient {
                    issuer: issuer.clone(),
                    client_id: client_id.clone(),
                    // offline_access asks for a refresh token, kept in the OS credential store.
                    scopes: "openid profile offline_access".into(),
                };
                let (tx, answer) = mpsc::channel();
                std::thread::Builder::new()
                    .name("wordcraft-oidc-sign-in".into())
                    .spawn(move || {
                        let result = wordcraft_collab_client::oidc::sign_in(
                            &client,
                            wordcraft_collab_client::oidc::open_in_browser,
                            std::time::Duration::from_secs(300),
                        )
                        .map_err(|e| e.to_string())
                        .map(|tokens| SignedIn::new(client, &tokens));
                        let _ = tx.send(result);
                    })
                    .map_err(|e| e.to_string())?;
                app.collab.last_issuer = issuer;
                app.collab.last_client_id = client_id;
                app.collab.signing_in = Some(answer);
                app.status(tl!("Finish signing in in your browser…"));
                Ok(json!({"pending": "signIn"}))
            }
            "ui.collab.signOut" => {
                let was = app.collab.session.take().is_some();
                app.collab.last_token.clear();
                // Forget the kept sign-in off the UI thread (the store may ask to be unlocked).
                let _ = std::thread::Builder::new().name("wordcraft-oidc-sign-out".into()).spawn(|| {
                    if let Err(e) = credentials::forget() {
                        log::warn!("{e}");
                    }
                });
                app.status(tl!("Signed out."));
                Ok(json!({"signedOut": was}))
            }
            "ui.collab.status" => {
                let mut value = status_json(state(app));
                if let Some(active) = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id()) {
                    value["document"] = json!(active.shared_document);
                }
                value["publishing"] = json!(app.collab.publishing.is_some());
                Ok(value)
            }
            _ => Err(format!("unknown command {id}")),
        }
    }

    fn ensure_available(app: &WordApp) -> Result<(), String> {
        if app.collab.active.is_some() || app.collab.publishing.is_some() {
            return Err(tl!("A document is already shared; disconnect it first.").into());
        }
        Ok(())
    }

    fn validate_connection(server: &str, token: &str) -> Result<(), String> {
        TransportConfig::new(server, token).validate().map_err(|e| crate::i18n::t(e).to_string())
    }

    /// Start the connection for the active session document.
    fn connect(app: &mut WordApp, server: String, token: String, document: DocumentId) -> Result<Value, String> {
        let client_id = ClientId::new(format!("desktop-{}", &fresh_id()?.simple().to_string()[..12])).map_err(|e| e.to_string())?;
        let mut config = TransportConfig::new(&server, &token);
        if let Some(source) = session_for(app, &token) {
            // Reconnects renew the access token instead of failing once it expires.
            config = config.with_token_source(std::sync::Arc::new(move |renew| source.token(renew).map_err(|e| e.to_string())));
        }
        let client = spawn_threaded(config, SharedSession::new(document, client_id)).map_err(|e| e.to_string())?;
        app.collab.remember(&server, &token);
        app.collab.active = Some(Active {
            document: app.session.document_id(),
            shared_document: document,
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

    pub(super) fn disabled_reason(app: &WordApp, id: &str) -> Option<&'static str> {
        let read_only = if app.collab.publishing.as_ref().is_some_and(|p| p.document == app.session.document_id()) {
            true
        } else {
            let active = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id())?;
            !active.loaded || !active.state.can_edit()
        };
        match gate(&app.session, id, read_only) {
            Gate::Blocked(reason) => Some(reason),
            _ => None,
        }
    }

    pub(super) fn intercept(app: &mut WordApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
        if app.collab.publishing.as_ref().is_some_and(|p| p.document == app.session.document_id()) {
            // The DOCX has already left: never accept local edits that the initial snapshot
            // would silently overwrite. Pure navigation and inspection remain available.
            let result = match gate(&app.session, id, true) {
                Gate::Run => run_pure(&mut app.session, id, params).map_err(|e| e.to_string()),
                _ => Err(tl!("Wait until the document has finished sharing before editing.").to_string()),
            };
            if let Err(e) = &result {
                app.status(e.clone());
            }
            return Some(result);
        }
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
        finish_sign_in(app);
        if app.collab.signing_in.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        if app.collab.publishing.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
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
                // Replaces "Connecting…", which would otherwise linger on the status line.
                app.status(tl!("Connected to the shared document."));
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

    /// A signed-in session and its current access token.
    pub(crate) struct SignedIn {
        pub(super) source: std::sync::Arc<TokenSource>,
        pub(super) access: String,
    }

    impl SignedIn {
        /// From a fresh sign-in: the refresh token (if the provider gave one) goes to the OS
        /// credential store, and so does every renewed one. Runs on the sign-in thread.
        fn new(client: OidcClient, tokens: &Tokens) -> Self {
            let source = TokenSource::new(client.clone(), tokens).on_renew(save_renewed(client.clone()));
            if let Some(refresh_token) = &tokens.refresh_token {
                keep(&client, refresh_token);
            }
            Self { source: std::sync::Arc::new(source), access: tokens.access_token.clone() }
        }
    }

    fn keep(client: &OidcClient, refresh_token: &str) {
        let saved =
            credentials::SavedSignIn { issuer: client.issuer.clone(), client_id: client.client_id.clone(), refresh_token: refresh_token.into() };
        if let Err(e) = credentials::save(&saved) {
            // Signed in for this session all the same.
            log::warn!("{e}");
        }
    }

    fn save_renewed(client: OidcClient) -> impl Fn(&Tokens) + Send + Sync + 'static {
        move |tokens| {
            if let Some(refresh_token) = &tokens.refresh_token {
                keep(&client, refresh_token);
            }
        }
    }

    /// The signed-in session, if `token` is the one it gave (the form was not edited by hand).
    fn session_for(app: &WordApp, token: &str) -> Option<std::sync::Arc<TokenSource>> {
        app.collab.session.clone().filter(|_| !token.is_empty() && token == app.collab.last_token)
    }

    /// Look once, off the UI thread, for a sign-in kept in the OS store and renew it.
    pub(super) fn restore_sign_in(app: &mut WordApp) {
        if app.collab.restore_tried || app.collab.session.is_some() || app.collab.signing_in.is_some() {
            return;
        }
        app.collab.restore_tried = true;
        let (tx, answer) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("wordcraft-oidc-restore".into()).spawn(move || {
            let result = match credentials::load() {
                // Nothing kept: an empty error is not reported.
                Ok(None) => Err(String::new()),
                Ok(Some(saved)) => {
                    let client = OidcClient { issuer: saved.issuer, client_id: saved.client_id, scopes: "openid profile offline_access".into() };
                    let source = TokenSource::restore(client.clone(), saved.refresh_token).on_renew(save_renewed(client));
                    source.token(false).map(|access| SignedIn { source: std::sync::Arc::new(source), access }).map_err(|e| e.to_string())
                }
                Err(e) => {
                    log::info!("{e}");
                    Err(String::new())
                }
            };
            let _ = tx.send(result);
        });
        if spawned.is_ok() {
            app.collab.signing_in = Some(answer);
        }
    }

    /// A finished browser sign-in: its access token becomes the one the forms use (in memory).
    fn finish_sign_in(app: &mut WordApp) {
        let Some(answer) = &app.collab.signing_in else { return };
        let result = match answer.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("stopped".into()),
        };
        app.collab.signing_in = None;
        match result {
            Ok(signed_in) => {
                let SignedIn { source, access } = signed_in;
                let (issuer, client_id) = (source.client().issuer.clone(), source.client().client_id.clone());
                app.collab.last_token = access.clone();
                app.collab.last_issuer = issuer.clone();
                app.collab.last_client_id = client_id.clone();
                app.collab.session = Some(source);
                if let Some(crate::dialogs::Dialog::Share(form)) = &mut app.dialog {
                    form.token = access;
                    form.issuer = issuer;
                    form.client_id = client_id;
                    form.message.clear();
                }
                app.status(tl!("Signed in."));
            }
            // Nothing kept to restore is not an error worth showing.
            Err(e) if e.is_empty() => {}
            Err(e) => {
                let message = crate::i18n::fmt(tl!("Couldn't sign in: {reason}"), &[("reason", &e)]);
                if let Some(crate::dialogs::Dialog::Share(form)) = &mut app.dialog {
                    form.message = message.clone();
                }
                app.status(message);
            }
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn editor() -> WordApp {
            WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Local notes")), crate::Services::default())
        }

        #[test]
        fn invalid_connection_leaves_the_local_document_and_windows_untouched() {
            let mut app = editor();
            let before = app.session.doc.clone();
            let id = app.session.document_id();
            let count = app.session.documents().len();
            for server in ["", "https://host/ws", "wss://secret@host/ws", "wss://host/ws?token=secret"] {
                let error = app.execute("ui.collab.connect", json!({"server": server, "token": "test-token", "document": Uuid::nil()})).unwrap_err();
                assert!(!error.contains("secret"));
                assert_eq!(app.session.doc, before);
                assert_eq!(app.session.document_id(), id);
                assert_eq!(app.session.documents().len(), count);
                assert!(app.collab.active.is_none());
                assert!(app.dialog.is_none(), "programmatic commands never open a dialog");
            }
        }

        #[test]
        fn pending_publication_blocks_edits_and_a_second_connection_until_cancelled() {
            let mut app = editor();
            let (_tx, answer) = mpsc::channel();
            app.collab.publishing =
                Some(Publishing { answer, server: "ws://localhost/ws".into(), token: "test-token".into(), document: app.session.document_id() });
            let before = app.session.doc.clone();
            let id = app.session.document_id();
            assert_eq!(super::super::status_text(&app).as_deref(), Some("Shared · publishing…"));
            for (command, params) in [("text.insert", json!({"text": "lost"})), ("edit.undo", json!({})), ("format.bold", json!({}))] {
                assert!(app.execute(command, params).is_err(), "{command}");
                assert_eq!(app.session.doc, before);
            }
            assert!(app.execute("caret.docEnd", json!({})).is_ok());
            assert!(
                app.execute("ui.collab.connect", json!({"server": "ws://localhost/ws", "token": "test-token", "document": Uuid::nil()})).is_err()
            );
            assert_eq!(app.session.document_id(), id);
            assert_eq!(app.execute("ui.collab.disconnect", json!({})).unwrap()["disconnected"], true);
            assert_eq!(super::super::status_text(&app), None);
            app.execute("text.insert", json!({"text": " (local)"})).unwrap();
            assert_ne!(app.session.doc, before);
        }
    }
}
