//! Who has access to a shared document, and inviting people (C5.5), over the server's HTTP API
//! (`/api/v1/me`, `/api/v1/users?q=`, `/api/v1/documents/{id}/members`). Native only (feature
//! `transport`); every call blocks, so the editor runs them off the UI thread.
//!
//! Errors are sentences for the person (the server's status, explained), never the token.

use std::time::Duration;

use serde::Deserialize;
use uuid::Uuid;
use wordcraft_collab_protocol::DocumentId;

use crate::connection::{TransportConfig, encode_query, http_base};

/// How long one request may take before it is given up.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Largest answer read (a member list or a search; both are small).
const MAX_ANSWER_BYTES: u64 = 1024 * 1024;

/// A person on the server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Person {
    pub id: Uuid,
    pub name: String,
}

/// What a member may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Editor,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    /// `editor` or `viewer`: the roles that can be given (ownership is not transferred).
    pub fn grantable(name: &str) -> Option<Self> {
        match name {
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

/// Someone with access to a document.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Member {
    pub id: Uuid,
    pub name: String,
    pub role: Role,
}

/// A share link as its owner sees it (C6.5b): never its token, which is shown once.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct LinkInfo {
    pub id: Uuid,
    pub role: Role,
    pub created_at: String,
    pub expires_at: String,
    pub allow_guests: bool,
    pub max_guests: u32,
    pub guests_used: u32,
    pub revoked: bool,
}

/// A link just created: its metadata and the URL to send, built from the configured server
/// (never from anything the server says about its own address).
#[derive(Clone, PartialEq, Eq)]
pub struct CreatedLink {
    pub info: LinkInfo,
    pub url: String,
}

impl std::fmt::Debug for CreatedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedLink").field("info", &self.info).field("url", &"[redacted]").finish()
    }
}

/// How long a new link lets people in, and who may use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkOptions {
    pub role: Role,
    pub expires_in_days: u32,
    /// People without an account on the server may join with just a name.
    pub allow_guests: bool,
    pub max_guests: u32,
}

/// What opening a link gave: the document, the role, the server to connect to and — for a new
/// guest — the guest's own session token (keep it in the system's credential store).
#[derive(Clone, PartialEq, Eq)]
pub struct Joined {
    pub server: String,
    pub document: DocumentId,
    pub role: Role,
    pub guest_token: Option<String>,
}

impl std::fmt::Debug for Joined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Joined").field("server", &self.server).field("document", &self.document).field("role", &self.role).finish_non_exhaustive()
    }
}

/// A share link (`https://host/s/<token>`, `http://` for a local server) split into the server's
/// WebSocket address (`wss://host/api/v1/ws`) and the invitation token. Anything else is `None`.
pub fn parse_link(link: &str) -> Option<(String, String)> {
    let link = link.trim();
    let (scheme, rest) = if let Some(r) = link.strip_prefix("https://") { ("wss", r) } else { ("ws", link.strip_prefix("http://")?) };
    let (host, token) = rest.split_once("/s/")?;
    let token = token.split(['?', '#']).next()?.trim_end_matches('/');
    let token_ok = (16..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let host_ok = !host.is_empty() && host.len() <= 255 && !host.contains(['/', '@', ' ', '\\']) && !host.chars().any(char::is_control);
    (token_ok && host_ok).then(|| (format!("{scheme}://{host}/api/v1/ws"), token.to_string()))
}

/// The link URL for `path` (`/s/<token>`) on the server at `ws_url`.
fn link_url(ws_url: &str, path: &str) -> Option<String> {
    let site = http_base(ws_url)?;
    let site = site.strip_suffix("/api/v1")?;
    path.starts_with("/s/").then(|| format!("{site}{path}"))
}

/// Open the share link `link`: as the account of `token` if given, else as a guest called
/// `name` (if the link allows guests). Blocking; call it off the UI thread.
pub fn join_link(link: &str, token: Option<&str>, name: Option<&str>) -> Result<Joined, String> {
    let (server, invitation) =
        parse_link(link).ok_or_else(|| "That is not a WordCraft share link (it looks like https://server/s/…).".to_string())?;
    let base = http_base(&server).ok_or_else(|| "That is not a WordCraft share link.".to_string())?;
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
    let url = format!("{base}/links/{invitation}/join");
    let body = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if token.is_none() => serde_json::json!({ "name": n }),
        _ => serde_json::json!({}),
    }
    .to_string();
    let mut request = agent.post(&url).header("Content-Type", "application/json");
    if let Some(token) = token {
        TransportConfig::new(&server, token).validate().map_err(str::to_string)?;
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request.send(body.as_bytes()).map_err(|e| explain(&e, Context::Join))?;
    let text = response.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|e| explain(&e, Context::Join))?;
    #[derive(Deserialize)]
    struct Answer {
        document_id: Uuid,
        role: Role,
        token: Option<String>,
    }
    let answer: Answer =
        serde_json::from_str(&text).map_err(|_| "The server sent an answer WordCraft does not understand. Is it up to date?".to_string())?;
    Ok(Joined { server, document: DocumentId(answer.document_id), role: answer.role, guest_token: answer.token })
}

/// A shared document this account can open (C7.4), as `GET /api/v1/documents` lists it: only
/// documents the account is a member of, most recently changed first.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct DocumentEntry {
    pub id: Uuid,
    pub title: String,
    pub role: Role,
    #[serde(default)]
    pub updated_at: String,
}

impl DocumentEntry {
    /// Whether `query` (trimmed, any case) is part of the title; an empty query matches all.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        q.is_empty() || self.title.to_lowercase().contains(&q)
    }
}

/// The server at a `ws://…/ws` address, with a sign-in token.
pub struct People {
    server: String,
    base: String,
    authorization: String,
    agent: ureq::Agent,
}

impl People {
    pub fn new(ws_url: &str, token: &str) -> Result<Self, String> {
        TransportConfig::new(ws_url, token).validate().map_err(str::to_string)?;
        let base = http_base(ws_url).ok_or_else(|| "Enter a ws:// or wss:// server address ending in /ws.".to_string())?;
        let agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
        Ok(Self { server: ws_url.to_string(), base, authorization: format!("Bearer {token}"), agent })
    }

    /// Who the token belongs to.
    pub fn me(&self) -> Result<Person, String> {
        self.get_json(&format!("{}/me", self.base), Context::Account)
    }

    /// The documents this account can open.
    pub fn documents(&self) -> Result<Vec<DocumentEntry>, String> {
        self.get_json(&format!("{}/documents", self.base), Context::Account)
    }

    /// The share links of `document` (owner only), newest first.
    pub fn links(&self, document: DocumentId) -> Result<Vec<LinkInfo>, String> {
        self.get_json(&format!("{}/documents/{}/links", self.base, document.0), Context::Links)
    }

    /// Create a share link for `document` (owner only).
    pub fn create_link(&self, document: DocumentId, options: LinkOptions) -> Result<CreatedLink, String> {
        if options.role == Role::Owner {
            return Err("A link can let people edit or only view.".into());
        }
        let days = options.expires_in_days.clamp(1, 30);
        let mut body = serde_json::json!({
            "role": options.role.as_str(),
            "expires_in_seconds": days * 24 * 60 * 60,
            "allow_guests": options.allow_guests,
        });
        if options.allow_guests {
            body["max_guests"] = serde_json::json!(options.max_guests.clamp(1, 100));
        }
        let url = format!("{}/documents/{}/links", self.base, document.0);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| explain(&e, Context::Links))?;
        let text = response.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|e| explain(&e, Context::Links))?;
        #[derive(Deserialize)]
        struct Answer {
            #[serde(flatten)]
            info: LinkInfo,
            path: String,
        }
        let answer: Answer =
            serde_json::from_str(&text).map_err(|_| "The server sent an answer WordCraft does not understand. Is it up to date?".to_string())?;
        let url = link_url(&self.server, &answer.path).ok_or_else(|| "The server sent a link WordCraft does not understand.".to_string())?;
        Ok(CreatedLink { info: answer.info, url })
    }

    /// Stop `link` from letting anyone else in (people who joined keep their access).
    pub fn revoke_link(&self, document: DocumentId, link: Uuid) -> Result<(), String> {
        let url = format!("{}/documents/{}/links/{link}", self.base, document.0);
        let result = self.agent.delete(&url).header("Authorization", &self.authorization).call();
        result.map(|_| ()).map_err(|e| explain(&e, Context::Links))
    }

    /// Everyone with access to `document`, the owner first.
    pub fn members(&self, document: DocumentId) -> Result<Vec<Member>, String> {
        self.get_json(&format!("{}/documents/{}/members", self.base, document.0), Context::Document)
    }

    /// People whose name contains `query` (or whose id it is).
    pub fn find(&self, query: &str) -> Result<Vec<Person>, String> {
        let query = query.trim();
        if query.chars().count() < 2 || query.chars().count() > 100 {
            return Err("Type at least two letters of the person's name.".into());
        }
        self.get_json(&format!("{}/users?q={}", self.base, encode_query(query)), Context::Search)
    }

    /// Give `user` access to `document` as `role` (editor or viewer), or change their role.
    pub fn set_role(&self, document: DocumentId, user: Uuid, role: Role) -> Result<(), String> {
        if role == Role::Owner {
            return Err("A document has one owner; ownership cannot be given away here.".into());
        }
        let url = format!("{}/documents/{}/members/{user}", self.base, document.0);
        let body = serde_json::json!({ "role": role.as_str() }).to_string();
        let result =
            self.agent.put(&url).header("Authorization", &self.authorization).header("Content-Type", "application/json").send(body.as_bytes());
        result.map(|_| ()).map_err(|e| explain(&e, Context::Members))
    }

    /// Take `user`'s access to `document` away (not the owner's).
    pub fn remove(&self, document: DocumentId, user: Uuid) -> Result<(), String> {
        let url = format!("{}/documents/{}/members/{user}", self.base, document.0);
        let result = self.agent.delete(&url).header("Authorization", &self.authorization).call();
        result.map(|_| ()).map_err(|e| explain(&e, Context::Members))
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, context: Context) -> Result<T, String> {
        let mut response = self.agent.get(url).header("Authorization", &self.authorization).call().map_err(|e| explain(&e, context))?;
        let text = response.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|e| explain(&e, context))?;
        serde_json::from_str(&text).map_err(|_| "The server sent an answer WordCraft does not understand. Is it up to date?".to_string())
    }
}

/// What a request was for, to explain its failure.
#[derive(Clone, Copy)]
enum Context {
    Account,
    Document,
    Search,
    Members,
    Links,
    Join,
}

fn explain(error: &ureq::Error, context: Context) -> String {
    match (error, context) {
        (ureq::Error::StatusCode(401), Context::Join) => {
            "This link needs you to sign in to the server (it does not admit guests), or the server refused the sign-in token.".into()
        }
        (ureq::Error::StatusCode(404), Context::Join) => {
            "This link does not work any more: it expired, was revoked, or was mistyped. Ask for a new one.".into()
        }
        (ureq::Error::StatusCode(400 | 422), Context::Join) => {
            "This link needs an account on the server, or a name (up to 80 characters) to join as a guest.".into()
        }
        (ureq::Error::StatusCode(403), Context::Join) => {
            "A guest session only opens the document it was invited to: sign in with an account to use this link.".into()
        }
        (ureq::Error::StatusCode(409), Context::Join) => "This link has no guest places left. Ask its owner for another.".into(),
        (ureq::Error::StatusCode(403), Context::Links) => "Only the document's owner can make or revoke links.".into(),
        (ureq::Error::StatusCode(400 | 422), Context::Links) => "The server refused those link options (or the document has too many links).".into(),
        (ureq::Error::StatusCode(401), _) => "The server refused the sign-in token. Sign in again.".into(),
        (ureq::Error::StatusCode(403), _) => "Only the document's owner can change who has access.".into(),
        (ureq::Error::StatusCode(404), Context::Members) => "That person is not on this server, or the document is gone.".into(),
        (ureq::Error::StatusCode(404), _) => "This document is not on the server, or you no longer have access.".into(),
        (ureq::Error::StatusCode(400), Context::Search) => "Type at least two letters of the person's name.".into(),
        (ureq::Error::StatusCode(400), Context::Members) => "The owner's access cannot change.".into(),
        (ureq::Error::StatusCode(429), _) => "The server is busy; try again in a moment.".into(),
        (ureq::Error::StatusCode(code), _) => format!("The server could not do that (HTTP {code}). Try again in a moment."),
        _ => "Cannot reach the server. Check the connection and try again.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_links_name_their_server_and_token_only() {
        let token = "AbCdEfGh0123456789_-xyzAbCdEfGh0123456789";
        assert_eq!(parse_link(&format!("https://wc.example.org/s/{token}")), Some(("wss://wc.example.org/api/v1/ws".into(), token.into())));
        assert_eq!(parse_link(&format!(" http://127.0.0.1:3000/s/{token}/?x=1#y ")), Some(("ws://127.0.0.1:3000/api/v1/ws".into(), token.into())));
        for bad in [
            format!("ftp://wc.example.org/s/{token}"),
            format!("https://wc.example.org/x/{token}"),
            "https://wc.example.org/s/short".to_string(),
            format!("https://user@evil/s/{token}"),
            format!("https:///s/{token}"),
            format!("https://wc.example.org/s/{token}<script>"),
            String::new(),
        ] {
            assert_eq!(parse_link(&bad), None, "{bad}");
        }
        assert_eq!(link_url("wss://wc.example.org/api/v1/ws", "/s/abc").as_deref(), Some("https://wc.example.org/s/abc"));
        assert_eq!(link_url("ws://127.0.0.1:3000/api/v1/ws", "https://evil/s/abc"), None);
        let joined = Joined {
            server: "wss://x/api/v1/ws".into(),
            document: DocumentId(Uuid::nil()),
            role: Role::Viewer,
            guest_token: Some("wct_secret".into()),
        };
        assert!(!format!("{joined:?}").contains("wct_secret"));
    }

    #[test]
    fn people_need_a_valid_server_and_never_put_the_token_in_errors() {
        assert!(People::new("https://host/api/v1/ws", "wct_x").is_err());
        assert!(People::new("wss://host/api/v1/ws", "bad\r\ntoken").is_err());
        let people = People::new("ws://127.0.0.1:9/api/v1/ws", "wct_secret").unwrap();
        assert_eq!(people.base, "http://127.0.0.1:9/api/v1");
        // Port 9 (discard) is closed: an unreachable server, explained without the token.
        let error = people.me().unwrap_err();
        assert!(!error.contains("wct_secret"), "{error}");
        assert!(people.find("a").is_err(), "too short to search");
        assert!(people.set_role(DocumentId(Uuid::nil()), Uuid::nil(), Role::Owner).is_err());
        assert_eq!(Role::grantable("editor"), Some(Role::Editor));
        assert_eq!(Role::grantable("owner"), None);
    }
}
