//! WordCraft Collaborative in the editor (docs/collab/design.md §6, §10; milestone C4.4).
//!
//! UI commands (programmatic, no dialogs; Review › Share opens the connection forms):
//! - `ui.collab.connect {"server": "wss://host/api/v1/ws", "token": "…", "document": "<uuid>"}`
//!   opens a shared document in a new document;
//! - `ui.collab.publish {"server", "token", "title"?}` shares the current document (its DOCX is
//!   posted off the UI thread) and then connects it;
//! - `ui.collab.disconnect` stops sharing; the document stays open as a local copy;
//! - `ui.collab.acknowledge` resumes editing after a conflict was shown;
//! - `ui.collab.status` reports the state;
//! - `ui.collab.members`, `ui.collab.findPeople {"query"}`, `ui.collab.invite {"user", "role"}`
//!   and `ui.collab.removeMember {"user"}` (C5.5) load who has access, find people by name and
//!   change access, in the background; Review › Share › Participants shows them.
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
    /// Who has access to the shared document, and the last search (C5.5).
    #[cfg(not(target_arch = "wasm32"))]
    people: native::PeopleState,
    /// The documents on the server the account can open, for Open Shared (C7.4).
    #[cfg(not(target_arch = "wasm32"))]
    listing: native::ListingState,
    /// Joining through a share link (C6.5b).
    #[cfg(not(target_arch = "wasm32"))]
    join: native::JoinState,
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
        ("members", "Participants", "Loads who has access to the current shared document (names and roles) in the background; Review › Share › Participants shows it.", native && state(app).is_some()),
        ("findPeople", "Find People", "query: 2..100 characters of a name, or a user id. Searches the server's people in the background; never opens a dialog.", native && state(app).is_some()),
        ("invite", "Invite to Document", "user: user id (UUID); role: editor | viewer. Gives access or changes it (owner only), in the background.", native && state(app).is_some()),
        ("removeMember", "Remove Access", "user: user id (UUID). Takes the person's access away (owner only), in the background.", native && state(app).is_some()),
        ("listDocuments", "Your Shared Documents", "server: ws:// or wss://…/ws; token: bearer token. Lists the documents the account can open, in the background (Open Shared shows them).", native),
        ("createLink", "Copy Share Link", "role: editor | viewer; days?: 1..30 (7); guests?: bool (false); maxGuests?: 1..100 (10). Makes a share link (owner only) and copies it; the link is shown once.", native && state(app).is_some()),
        ("revokeLink", "Revoke Share Link", "link: link id (UUID). Stops new people joining with it (owner only); people who joined keep access.", native && state(app).is_some()),
        ("openLink", "Open Share Link", "link: https://server/s/…; token?: bearer token (join as that account); name?: guest name (1..80). Joins and opens the document in a new document; a guest session is kept in the system credential store.", native && !busy(app)),
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
    /// Open Shared: a share link (C6.5b), and the name to join with as a guest.
    pub link: String,
    pub guest_name: String,
    /// Open Shared's title filter for the server's documents (C7.4).
    pub filter: String,
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
            // A link is an invitation credential: never in state dumps.
            "hasLink": !self.link.is_empty(),
            "guestName": self.guest_name,
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
            link: String::new(),
            guest_name: String::new(),
            filter: String::new(),
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

/// The Participants dialog's fields (C5.5). What the server said lives in the app's
/// collaboration state ([`people`]), so the dialog only holds what the person typed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ParticipantsForm {
    /// The name being searched for.
    pub query: String,
    /// The role to invite with: `editor` or `viewer`.
    pub role: String,
    /// The access list was asked for (once, when the dialog first shows).
    pub loaded: bool,
    /// A new share link (C6.5b): `editor` or `viewer`, days it lasts, guests without an account.
    pub link_role: String,
    pub link_days: u32,
    pub link_guests: bool,
    pub link_max_guests: u32,
}

impl Default for ParticipantsForm {
    fn default() -> Self {
        Self {
            query: String::new(),
            role: "editor".into(),
            loaded: false,
            link_role: "editor".into(),
            link_days: 7,
            link_guests: false,
            link_max_guests: 10,
        }
    }
}

/// A person in the Participants dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonRow {
    pub id: String,
    pub name: String,
    /// `owner`, `editor` or `viewer`; empty for a search result without access.
    pub role: &'static str,
    /// The signed-in person.
    pub me: bool,
}

/// A share link in the Participants dialog (C6.5b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRow {
    pub id: String,
    /// `editor` or `viewer`.
    pub role: &'static str,
    pub expires: String,
    pub allow_guests: bool,
    pub guests_used: u32,
    pub max_guests: u32,
    pub revoked: bool,
}

/// What joining through a share link shows in Open Shared (C6.5b).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinView {
    pub busy: bool,
    pub message: String,
    pub error: bool,
    /// The link was accepted and the document is opening: the dialog can close.
    pub done: bool,
}

/// How joining through a link is going.
pub fn joining(app: &WordApp) -> JoinView {
    #[cfg(not(target_arch = "wasm32"))]
    return native::join_view(app);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        JoinView::default()
    }
}

/// Open Shared saw the link's outcome: forget it.
pub fn join_seen(app: &mut WordApp) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.collab.join = native::JoinState::default();
    }
    #[cfg(target_arch = "wasm32")]
    let _ = app;
}

/// What the Participants dialog shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeopleView {
    /// The active document is shared.
    pub shared: bool,
    /// Everyone with access, the owner first.
    pub members: Vec<PersonRow>,
    /// The last search's results (with their role if they already have access).
    pub matches: Vec<PersonRow>,
    /// The signed-in person owns the document: they may invite and change access.
    pub can_manage: bool,
    /// The document's share links (owner only), and the URL of the one just made (shown once,
    /// already copied).
    pub links: Vec<LinkRow>,
    pub new_link: Option<String>,
    /// A request is on its way.
    pub busy: bool,
    pub message: String,
    pub error: bool,
}

/// A document Open Shared lists (C7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRow {
    pub id: String,
    pub title: String,
    /// `owner`, `editor` or `viewer`.
    pub role: &'static str,
    /// When it last changed, as the server reports it.
    pub updated: String,
}

/// What Open Shared's document list shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListingView {
    /// The list was loaded (it may be empty).
    pub loaded: bool,
    pub documents: Vec<DocumentRow>,
    pub busy: bool,
    pub message: String,
    pub error: bool,
}

/// The documents on the server the account can open, as last loaded.
pub fn listing(app: &WordApp) -> ListingView {
    #[cfg(not(target_arch = "wasm32"))]
    return native::listing_view(app);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        ListingView::default()
    }
}

/// Who has access to the active shared document, as last loaded (C5.5).
pub fn people(app: &WordApp) -> PeopleView {
    #[cfg(not(target_arch = "wasm32"))]
    return native::people_view(app);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        PeopleView::default()
    }
}

/// Show `message` as the Participants dialog's error (a request that could not even start).
pub fn people_failed(app: &mut WordApp, message: String) {
    #[cfg(not(target_arch = "wasm32"))]
    native::people_failed(app, message);
    #[cfg(target_arch = "wasm32")]
    let _ = (app, message);
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

/// The other participants' names, when the active document is shared (C5.3).
pub fn participants(app: &WordApp) -> Vec<String> {
    #[cfg(not(target_arch = "wasm32"))]
    return native::participants(app);
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        Vec::new()
    }
}

/// Other participants' carets on the page (C5.3): a coloured bar and their name above it. Each
/// participant keeps one colour (picked from their client id), so a caret is easy to follow.
pub fn paint_carets(app: &WordApp, painter: &egui::Painter, layout: &wordcraft_layout::DocLayout, rects: &[egui::Rect], scale: f32) {
    #[cfg(target_arch = "wasm32")]
    let _ = (app, painter, layout, rects, scale);
    #[cfg(not(target_arch = "wasm32"))]
    for (name, colour, pos) in native::remote_carets(app) {
        let Some(c) = layout.caret_on(&pos, app.session.page_hint) else { continue };
        let Some(page) = rects.get(c.page) else { continue };
        let x = page.min.x + c.x * scale;
        let (top, bottom) = (page.min.y + c.top * scale, page.min.y + (c.top + c.height) * scale);
        painter.line_segment([egui::pos2(x, top), egui::pos2(x, bottom)], egui::Stroke::new(2.0, colour));
        let font = egui::FontId::proportional(11.0);
        let galley = painter.layout_no_wrap(name, font, egui::Color32::WHITE);
        let tag = egui::Rect::from_min_size(egui::pos2(x, top - galley.size().y - 2.0), galley.size() + egui::vec2(6.0, 2.0));
        painter.rect_filled(tag, 2.0, colour);
        painter.galley(tag.min + egui::vec2(3.0, 1.0), galley, egui::Color32::WHITE);
    }
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
    use wordcraft_collab_client::capture::{capture, capture_format, capture_paste};
    use wordcraft_collab_client::connection::{ThreadedClient, TransportConfig, new_uuid, publish, spawn_threaded};
    use wordcraft_collab_client::credentials;
    use wordcraft_collab_client::gate::{Gate, gate, run_pure};
    use wordcraft_collab_client::oidc::{OidcClient, TokenSource, Tokens};
    use wordcraft_collab_client::people::{CreatedLink, DocumentEntry, Joined, LinkInfo, LinkOptions, Member, People, Person, Role};
    use wordcraft_collab_client::{CollaborationState, Event, SessionBridge, SharedSession};
    use wordcraft_collab_model::{BlockId, SharedDocument};
    use wordcraft_collab_protocol::{ClientId, DocumentId, OperationId, Revision};

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
        /// Where it is connected, and with which token (for the HTTP calls of C5.5; in memory).
        server: String,
        token: String,
        bridge: SessionBridge,
        state: CollaborationState,
        /// A snapshot has been shown: the document is the shared one.
        loaded: bool,
        /// The revision the session shows (edits are captured against it, and sent as based
        /// on it: the connection's replica may be ahead).
        shown: Revision,
        /// The other participants and their carets (C5.3).
        pub(super) participants: Vec<wordcraft_collab_protocol::messages::Participant>,
        /// This client's caret as last sent, and when.
        presence_sent: Option<(Option<wordcraft_collab_protocol::messages::Anchor>, std::time::Instant)>,
    }

    /// Carets are sent at most this often.
    const PRESENCE_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

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
            "ui.collab.listDocuments" => {
                let (server, token) = (param(params, "server")?.trim().to_string(), param(params, "token")?.trim().to_string());
                list_documents(app, server, token)
            }
            "ui.collab.members" => {
                let loading = tl!("Loading who has access…").to_string();
                people_request(app, loading, |people, document| {
                    let (me, members) = (people.me()?, people.members(document)?);
                    let owner = members.iter().any(|m| m.id == me.id && m.role == Role::Owner);
                    // Only the owner sees (and makes) links.
                    let links = if owner { people.links(document)? } else { Vec::new() };
                    Ok(PeopleAnswer::Loaded { me, members, links })
                })
            }
            "ui.collab.createLink" => {
                let role = Role::grantable(param(params, "role")?).ok_or_else(|| tl!("Choose whether they can edit or only view.").to_string())?;
                let days = params.get("days").and_then(Value::as_u64).unwrap_or(7);
                if !(1..=30).contains(&days) {
                    return Err(tl!("A link can last from 1 to 30 days.").into());
                }
                let allow_guests = params.get("guests").and_then(Value::as_bool).unwrap_or(false);
                let max_guests = params.get("maxGuests").and_then(Value::as_u64).unwrap_or(10);
                if allow_guests && !(1..=100).contains(&max_guests) {
                    return Err(tl!("A link can admit 1 to 100 guests.").into());
                }
                let options = LinkOptions { role, expires_in_days: days as u32, allow_guests, max_guests: max_guests as u32 };
                people_request(app, tl!("Making the link…").to_string(), move |people, document| {
                    let created = people.create_link(document, options)?;
                    Ok(PeopleAnswer::LinkCreated { created, links: people.links(document)? })
                })
            }
            "ui.collab.revokeLink" => {
                let link = Uuid::parse_str(param(params, "link")?).map_err(|e| format!("link: {e}"))?;
                let done = tl!("Link revoked: nobody else can join with it. People who already joined keep their access until removed.").to_string();
                people_request(app, tl!("Revoking the link…").to_string(), move |people, document| {
                    people.revoke_link(document, link)?;
                    Ok(PeopleAnswer::Links { links: people.links(document)?, message: done })
                })
            }
            "ui.collab.openLink" => {
                ensure_available(app)?;
                let link = param(params, "link")?.trim().to_string();
                if wordcraft_collab_client::people::parse_link(&link).is_none() {
                    return Err(tl!("That is not a WordCraft share link (it looks like https://server/s/…).").into());
                }
                let token = params.get("token").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
                let name = params.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
                if name.as_ref().is_some_and(|n| n.chars().count() > 80 || n.chars().any(char::is_control)) {
                    return Err(tl!("Enter a name of up to 80 characters.").into());
                }
                open_link(app, link, token, name)
            }
            "ui.collab.findPeople" => {
                let query = param(params, "query")?.trim().to_string();
                let length = query.chars().count();
                if Uuid::parse_str(&query).is_err() && !(2..=100).contains(&length) {
                    return Err(tl!("Type at least two letters of the person's name.").into());
                }
                let searching = tl!("Searching…").to_string();
                people_request(app, searching, move |people, _| Ok(PeopleAnswer::Found(people.find(&query)?)))
            }
            "ui.collab.invite" => {
                let user = Uuid::parse_str(param(params, "user")?).map_err(|e| format!("user: {e}"))?;
                let role = Role::grantable(param(params, "role")?).ok_or_else(|| tl!("Choose whether they can edit or only view.").to_string())?;
                let done = tl!("Access updated.").to_string();
                people_request(app, tl!("Updating access…").to_string(), move |people, document| {
                    people.set_role(document, user, role)?;
                    Ok(PeopleAnswer::Changed { members: people.members(document)?, message: done })
                })
            }
            "ui.collab.removeMember" => {
                let user = Uuid::parse_str(param(params, "user")?).map_err(|e| format!("user: {e}"))?;
                let done = tl!("Access removed.").to_string();
                people_request(app, tl!("Updating access…").to_string(), move |people, document| {
                    people.remove(document, user)?;
                    Ok(PeopleAnswer::Changed { members: people.members(document)?, message: done })
                })
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

    /// Who has access to a shared document, the last search, and the request on its way (C5.5).
    #[derive(Default)]
    pub(super) struct PeopleState {
        /// The shared document this is about; another one starts afresh.
        document: Option<DocumentId>,
        me: Option<Person>,
        members: Vec<Member>,
        matches: Vec<Person>,
        links: Vec<LinkInfo>,
        /// The URL of the link just made: shown once in the dialog.
        new_link: Option<String>,
        message: String,
        error: bool,
        answer: Option<mpsc::Receiver<PeopleAnswer>>,
    }

    pub(super) enum PeopleAnswer {
        Loaded { me: Person, members: Vec<Member>, links: Vec<LinkInfo> },
        LinkCreated { created: CreatedLink, links: Vec<LinkInfo> },
        Links { links: Vec<LinkInfo>, message: String },
        Found(Vec<Person>),
        Changed { members: Vec<Member>, message: String },
        Failed(String),
    }

    /// Run `job` against the active shared document's server off the UI thread; its answer is
    /// picked up by [`poll`]. One request at a time.
    fn people_request(
        app: &mut WordApp,
        working: String,
        job: impl FnOnce(&People, DocumentId) -> Result<PeopleAnswer, String> + Send + 'static,
    ) -> Result<Value, String> {
        let active = app
            .collab
            .active
            .as_ref()
            .filter(|a| a.document == app.session.document_id())
            .ok_or_else(|| tl!("This document is not shared.").to_string())?;
        if app.collab.people.answer.is_some() {
            return Err(tl!("Wait for the server to answer first.").into());
        }
        let (server, token, document) = (active.server.clone(), active.token.clone(), active.shared_document);
        let source = session_for(app, &token);
        let (tx, answer) = mpsc::channel();
        std::thread::Builder::new()
            .name("wordcraft-collab-people".into())
            .spawn(move || {
                // A signed-in session gives a current token (renewed if it expired).
                let token = match &source {
                    Some(source) => source.token(false).unwrap_or(token),
                    None => token,
                };
                let result = People::new(&server, &token).and_then(|people| job(&people, document));
                let _ = tx.send(result.unwrap_or_else(PeopleAnswer::Failed));
            })
            .map_err(|e| e.to_string())?;
        let people = &mut app.collab.people;
        if people.document != Some(document) {
            *people = PeopleState { document: Some(document), ..PeopleState::default() };
        }
        people.answer = Some(answer);
        people.message = working;
        people.error = false;
        Ok(json!({"pending": true}))
    }

    /// The documents the account can open on a server, and the request on its way (C7.4).
    #[derive(Default)]
    pub(super) struct ListingState {
        loaded: bool,
        documents: Vec<DocumentEntry>,
        message: String,
        error: bool,
        answer: Option<mpsc::Receiver<Result<Vec<DocumentEntry>, String>>>,
    }

    /// Ask `server` for the documents `token`'s account can open, off the UI thread; [`poll`]
    /// picks the answer up. The server lists only documents the account is a member of.
    fn list_documents(app: &mut WordApp, server: String, token: String) -> Result<Value, String> {
        if app.collab.listing.answer.is_some() {
            return Err(tl!("Wait for the server to answer first.").into());
        }
        if token.is_empty() {
            return Err(tl!("Sign in or enter a sign-in token first.").into());
        }
        // Checked here so a mistyped address is explained at once, not by the thread.
        People::new(&server, &token)?;
        let source = session_for(app, &token);
        let (tx, answer) = mpsc::channel();
        std::thread::Builder::new()
            .name("wordcraft-collab-documents".into())
            .spawn(move || {
                let token = match &source {
                    Some(source) => source.token(false).unwrap_or(token),
                    None => token,
                };
                let _ = tx.send(People::new(&server, &token).and_then(|people| people.documents()));
            })
            .map_err(|e| e.to_string())?;
        app.collab.listing.answer = Some(answer);
        app.collab.listing.message = tl!("Loading your documents…").into();
        app.collab.listing.error = false;
        Ok(json!({"pending": true}))
    }

    fn finish_listing(app: &mut WordApp) {
        let listing = &mut app.collab.listing;
        let Some(answer) = &listing.answer else { return };
        let result = match answer.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(tl!("The request stopped before the server answered.").into()),
        };
        listing.answer = None;
        listing.message.clear();
        listing.error = false;
        match result {
            Ok(documents) => {
                listing.loaded = true;
                if documents.is_empty() {
                    listing.message = tl!("No shared documents yet. Share one, or ask its owner to invite you.").into();
                }
                listing.documents = documents;
            }
            Err(message) => {
                listing.message = message;
                listing.error = true;
            }
        }
    }

    pub(super) fn listing_view(app: &WordApp) -> super::ListingView {
        let listing = &app.collab.listing;
        super::ListingView {
            loaded: listing.loaded,
            documents: listing
                .documents
                .iter()
                .map(|d| super::DocumentRow { id: d.id.to_string(), title: d.title.clone(), role: d.role.as_str(), updated: d.updated_at.clone() })
                .collect(),
            busy: listing.answer.is_some(),
            message: listing.message.clone(),
            error: listing.error,
        }
    }

    /// Joining through a share link, off the UI thread (C6.5b).
    #[derive(Default)]
    pub(crate) struct JoinState {
        answer: Option<mpsc::Receiver<Result<(Joined, String), String>>>,
        message: String,
        error: bool,
        done: bool,
    }

    /// Accept `link` (as the account of `token`, as the guest `name`, or with the guest session
    /// kept for the link's server), then open its document.
    fn open_link(app: &mut WordApp, link: String, token: Option<String>, name: Option<String>) -> Result<Value, String> {
        if app.collab.join.answer.is_some() {
            return Err(tl!("Wait for the server to answer first.").into());
        }
        let source = token.as_deref().and_then(|t| session_for(app, t));
        let (tx, answer) = mpsc::channel();
        std::thread::Builder::new()
            .name("wordcraft-collab-join".into())
            .spawn(move || {
                let result = (|| {
                    let token = match (&source, token) {
                        (Some(source), Some(t)) => Some(source.token(false).unwrap_or(t)),
                        (_, t) => t,
                    };
                    let (server, _) = wordcraft_collab_client::people::parse_link(&link).ok_or_else(|| "not a link".to_string())?;
                    if let Some(token) = token {
                        let joined = wordcraft_collab_client::people::join_link(&link, Some(&token), None)?;
                        return Ok((joined, token));
                    }
                    // A guest who joined before (on this computer) uses their kept session.
                    if name.is_none()
                        && let Ok(Some((_, kept))) = credentials::load_last_guest(&server)
                        && let Ok(joined) = wordcraft_collab_client::people::join_link(&link, Some(&kept), None)
                    {
                        return Ok((joined, kept));
                    }
                    let Some(name) = name else {
                        return Err(tl!("Sign in, or enter your name to join as a guest.").to_string());
                    };
                    let joined = wordcraft_collab_client::people::join_link(&link, None, Some(&name))?;
                    let token =
                        joined.guest_token.clone().ok_or_else(|| tl!("The server did not give a guest session. Is it up to date?").to_string())?;
                    // Kept so the guest can come back after a restart; joining works without it.
                    if let Err(e) = credentials::save_guest(&joined.server, joined.document.0, &token)
                        .and_then(|()| credentials::save_last_guest(&joined.server, joined.document.0))
                    {
                        log::warn!("{e}");
                    }
                    Ok((joined, token))
                })();
                let _ = tx.send(result);
            })
            .map_err(|e| e.to_string())?;
        app.collab.join = JoinState { answer: Some(answer), message: tl!("Opening the link…").into(), error: false, done: false };
        Ok(json!({"pending": "openLink"}))
    }

    fn finish_join(app: &mut WordApp) {
        let join = &mut app.collab.join;
        let Some(answer) = &join.answer else { return };
        let result = match answer.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err(tl!("The request stopped before the server answered.").into()),
        };
        join.answer = None;
        match result {
            Ok((joined, token)) => {
                let params = json!({"server": joined.server, "token": token, "document": joined.document.0.to_string()});
                match command(app, "ui.collab.connect", &params) {
                    Ok(_) => {
                        app.collab.remember(&joined.server, &token);
                        app.collab.join = JoinState { done: true, ..JoinState::default() };
                    }
                    Err(e) => app.collab.join = JoinState { message: e, error: true, ..JoinState::default() },
                }
            }
            Err(e) => app.collab.join = JoinState { message: e, error: true, ..JoinState::default() },
        }
    }

    pub(super) fn join_view(app: &WordApp) -> super::JoinView {
        let j = &app.collab.join;
        super::JoinView { busy: j.answer.is_some(), message: j.message.clone(), error: j.error, done: j.done }
    }

    /// Take the answer of the people request, if it came.
    fn finish_people(app: &mut WordApp, ctx: &egui::Context) {
        let people = &mut app.collab.people;
        let Some(answer) = &people.answer else { return };
        let result = match answer.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => PeopleAnswer::Failed(tl!("The request stopped before the server answered.").into()),
        };
        people.answer = None;
        people.error = false;
        people.message.clear();
        match result {
            PeopleAnswer::Loaded { me, members, links } => {
                people.me = Some(me);
                people.members = members;
                people.links = links;
            }
            PeopleAnswer::LinkCreated { created, links } => {
                // Copied at once, like Word's Copy Link; shown once (the server keeps no copy).
                ctx.copy_text(created.url.clone());
                people.new_link = Some(created.url);
                people.links = links;
                people.message = tl!("Link copied. Send it only to the people it is for: anyone with it can join.").into();
            }
            PeopleAnswer::Links { links, message } => {
                people.links = links;
                people.message = message;
            }
            PeopleAnswer::Found(found) => {
                if found.is_empty() {
                    people.message = tl!("Nobody on this server has that name. People appear once they have signed in.").into();
                }
                people.matches = found;
            }
            PeopleAnswer::Changed { members, message } => {
                people.members = members;
                people.message = message;
            }
            PeopleAnswer::Failed(message) => {
                people.message = message;
                people.error = true;
            }
        }
    }

    pub(super) fn people_failed(app: &mut WordApp, message: String) {
        app.collab.people.message = message;
        app.collab.people.error = true;
    }

    pub(super) fn people_view(app: &WordApp) -> super::PeopleView {
        let Some(active) = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id()) else {
            return super::PeopleView::default();
        };
        let people = &app.collab.people;
        if people.document != Some(active.shared_document) {
            return super::PeopleView { shared: true, busy: people.answer.is_some(), ..super::PeopleView::default() };
        }
        let me = people.me.as_ref().map(|p| p.id);
        let role_of = |id: Uuid| people.members.iter().find(|m| m.id == id).map_or("", |m| m.role.as_str());
        let row = |id: Uuid, name: &str| super::PersonRow { id: id.to_string(), name: name.to_string(), role: role_of(id), me: Some(id) == me };
        super::PeopleView {
            shared: true,
            members: people.members.iter().map(|m| row(m.id, &m.name)).collect(),
            matches: people.matches.iter().map(|p| row(p.id, &p.name)).collect(),
            can_manage: people.members.iter().any(|m| m.role == Role::Owner && Some(m.id) == me),
            links: people
                .links
                .iter()
                .map(|l| super::LinkRow {
                    id: l.id.to_string(),
                    role: l.role.as_str(),
                    expires: l.expires_at.clone(),
                    allow_guests: l.allow_guests,
                    guests_used: l.guests_used,
                    max_guests: l.max_guests,
                    revoked: l.revoked,
                })
                .collect(),
            new_link: people.new_link.clone(),
            busy: people.answer.is_some(),
            message: people.message.clone(),
            error: people.error,
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
            server: server.clone(),
            token: token.clone(),
            bridge: SessionBridge::default(),
            state: CollaborationState::Connecting,
            loaded: false,
            shown: 0,
            participants: Vec::new(),
            presence_sent: None,
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
        let (anchor, focus) = (app.session.sel.anchor.clone(), app.session.sel.focus.clone());
        let edit = if wordcraft_collab_client::paste::PASTE_COMMANDS.contains(&id) {
            // Reading the clipboard may adopt styles into the document: put it back after.
            let before = app.session.doc.clone();
            let payload = wordcraft_collab_client::paste::payload(&mut app.session, id, params);
            app.session.doc = before;
            let payload = payload.map_err(|e| e.to_string())?;
            let mut ids = Vec::new();
            for _ in 1..payload.paragraphs.len() {
                ids.push(BlockId(fresh_id()?));
            }
            let mut ids = ids.into_iter();
            capture_paste(&replica, &anchor, &focus, &payload, || ids.next().unwrap_or(BlockId(Uuid::nil())))
        } else if wordcraft_collab_client::format::is_format_command(id) {
            let patch = wordcraft_collab_client::format::patch(&app.session, id, params).map_err(|e| e.to_string())?;
            capture_format(&replica, &anchor, &focus, &patch)
        } else {
            if id == "edit.cut" {
                // The copy half: it does not change the document (checked by run_pure).
                run_pure(&mut app.session, "edit.copy", &json!({})).map_err(|e| e.to_string())?;
            }
            let new_block = BlockId(fresh_id()?);
            capture(&replica, &anchor, &focus, id, params, || new_block)
        }
        .map_err(|e| e.to_string())?;
        let Some(operation) = edit.operation else { return Ok(json!({"submitted": false})) };
        let follow_up = match edit.follow_up {
            Some(f) => Some((OperationId(fresh_id()?), f)),
            None => None,
        };
        if !active.client.submit_edit(active.shown, (OperationId(fresh_id()?), operation), follow_up) {
            return Err(tl!("The connection is busy; try again.").into());
        }
        Ok(json!({"submitted": true}))
    }

    pub(super) fn poll(app: &mut WordApp, ctx: &egui::Context) {
        finish_publish(app);
        finish_sign_in(app);
        finish_people(app, ctx);
        finish_listing(app);
        finish_join(app);
        if app.collab.join.answer.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if app.collab.listing.answer.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if app.collab.people.answer.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
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
            send_presence(app, &mut active);
        }
        app.collab.active = Some(active);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Tell the others where the caret is, when it moved, at most every [`PRESENCE_EVERY`].
    fn send_presence(app: &WordApp, active: &mut Active) {
        if !active.loaded {
            return;
        }
        let cursor = wordcraft_collab_model::anchors::anchor_from_pos(active.bridge.ids(), &app.session.sel.focus);
        let due = match &active.presence_sent {
            None => true,
            Some((sent, at)) => *sent != cursor && at.elapsed() >= PRESENCE_EVERY,
        };
        if due && active.client.presence(cursor) {
            active.presence_sent = Some((cursor, std::time::Instant::now()));
        }
    }

    /// Colours for other carets: dark enough for white names, apart from each other.
    const CARET_COLOURS: [egui::Color32; 6] = [
        egui::Color32::from_rgb(0xC2, 0x41, 0x0C),
        egui::Color32::from_rgb(0x15, 0x80, 0x3D),
        egui::Color32::from_rgb(0x6D, 0x28, 0xD9),
        egui::Color32::from_rgb(0xBE, 0x18, 0x5D),
        egui::Color32::from_rgb(0x0E, 0x74, 0x90),
        egui::Color32::from_rgb(0xA1, 0x62, 0x07),
    ];

    /// Other participants' carets as (name, colour, position in the session's document).
    pub(super) fn remote_carets(app: &WordApp) -> Vec<(String, egui::Color32, wordcraft_doc::Pos)> {
        let Some(active) = app.collab.active.as_ref().filter(|a| a.document == app.session.document_id() && a.loaded) else { return Vec::new() };
        active
            .participants
            .iter()
            .filter_map(|p| {
                let pos = wordcraft_collab_model::anchors::pos_from_anchor(active.bridge.ids(), p.cursor.as_ref()?)?;
                let hash = p.client_id.as_str().bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b)));
                let colour = CARET_COLOURS.get(hash % CARET_COLOURS.len()).copied().unwrap_or(egui::Color32::DARK_GRAY);
                Some((p.display_name.clone(), colour, app.session.doc.clamp(&pos)))
            })
            .collect()
    }

    pub(super) fn participants(app: &WordApp) -> Vec<String> {
        app.collab
            .active
            .as_ref()
            .filter(|a| a.document == app.session.document_id())
            .map(|a| a.participants.iter().map(|p| p.display_name.clone()).collect())
            .unwrap_or_default()
    }

    /// What a refusal means for the person editing, and what to do about it.
    pub(super) fn explain(reason: wordcraft_collab_protocol::RejectReason, edit: bool) -> &'static str {
        use wordcraft_collab_protocol::RejectReason as R;
        match reason {
            R::Conflict => "Someone changed that text at the same moment, so your edit was not applied. Check the text and try again.",
            R::Forbidden if edit => "You can view this document but not edit it. Ask its owner for edit access.",
            R::Forbidden => "You no longer have access to this shared document.",
            R::UnsupportedOperation => "That change is not available in shared documents yet.",
            R::InvalidOperation if edit => "Your edit no longer fits the document (it changed meanwhile) and was not applied.",
            R::InvalidOperation => "The server could not use that request.",
            R::StaleRevision => "Catching up with changes made while you were away…",
            R::Unauthenticated => "Your sign-in has expired. Sign in again to keep editing.",
            R::TooLarge => "That is too large to send to the server.",
            R::UnsupportedVersion => "This version of WordCraft cannot talk to that server. Update WordCraft or the server.",
            R::Unavailable => "The server could not save that change; nothing was lost. Try again in a moment.",
            R::RateLimited => "Too many changes at once; the server is slowing down this connection. Try again in a moment.",
        }
    }

    fn handle(app: &mut WordApp, active: &mut Active, event: Event) {
        match event {
            Event::Snapshot { revision, replica } => {
                active.bridge.load(&mut app.session, &replica);
                active.loaded = true;
                active.shown = revision;
                // Replaces "Connecting…", which would otherwise linger on the status line.
                app.status(tl!("Connected to the shared document."));
            }
            Event::Applied { revision, operation, own } => {
                active.shown = revision;
                if let Err(e) = active.bridge.apply_remote_edit(&mut app.session, &operation, own) {
                    // The session no longer matches the shared document: editing pauses until a
                    // snapshot comes (the next reconnect brings one).
                    log::error!("shared document: a commit did not apply: {e}");
                    active.loaded = false;
                    app.status(tl!("The shared document is out of step; reconnecting will fix it."));
                }
            }
            Event::Rejected { reason, message, operation } => {
                // The server's wording is for the log; the person gets what happened and what
                // to do (C5.4).
                log::info!("shared document: {reason:?}: {message}");
                app.status(tl!(explain(reason, operation.is_some())));
            }
            Event::Refused { message } => app.status(crate::i18n::fmt(tl!("Shared document: {reason}"), &[("reason", &message)])),
            Event::State(state) => active.state = state,
            Event::Presence(participants) => active.participants = participants,
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use wordcraft_collab_protocol::RejectReason as R;

    /// Every refusal has a sentence for people: no server jargon, and the "edit" wording only
    /// where it differs.
    #[test]
    fn every_refusal_is_explained() {
        let all = [
            R::Conflict,
            R::InvalidOperation,
            R::UnsupportedOperation,
            R::StaleRevision,
            R::Unauthenticated,
            R::Forbidden,
            R::TooLarge,
            R::UnsupportedVersion,
            R::Unavailable,
            R::RateLimited,
        ];
        for reason in all {
            for edit in [true, false] {
                let text = super::native::explain(reason, edit);
                assert!(text.ends_with('.') || text.ends_with('…'), "{reason:?}: {text}");
                for jargon in ["revision", "operation", "block", "transform", "rejected"] {
                    assert!(!text.to_lowercase().contains(jargon), "{reason:?} says {jargon:?}: {text}");
                }
            }
        }
        assert_ne!(super::native::explain(R::Forbidden, true), super::native::explain(R::Forbidden, false));
    }
}
