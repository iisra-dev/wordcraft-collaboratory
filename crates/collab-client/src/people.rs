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

/// The server at a `ws://…/ws` address, with a sign-in token.
pub struct People {
    base: String,
    authorization: String,
    agent: ureq::Agent,
}

impl People {
    pub fn new(ws_url: &str, token: &str) -> Result<Self, String> {
        TransportConfig::new(ws_url, token).validate().map_err(str::to_string)?;
        let base = http_base(ws_url).ok_or_else(|| "Enter a ws:// or wss:// server address ending in /ws.".to_string())?;
        let agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
        Ok(Self { base, authorization: format!("Bearer {token}"), agent })
    }

    /// Who the token belongs to.
    pub fn me(&self) -> Result<Person, String> {
        self.get_json(&format!("{}/me", self.base), Context::Account)
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
}

fn explain(error: &ureq::Error, context: Context) -> String {
    match (error, context) {
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
