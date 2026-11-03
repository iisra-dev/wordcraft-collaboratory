//! WebSocket transport (design §7, milestone C3.1).
//!
//! To come, behind the non-default `transport` feature so wasm builds are unaffected:
//! - `wss://` connection with the access token from the OIDC login (§12), never in the URL;
//! - `hello` → `welcome`, then `join` → `snapshot`;
//! - reconnection with backoff: `resync` from the last confirmed revision, then resend every
//!   [`crate::PendingQueue`] entry (the server deduplicates by `operation_id`);
//! - message size limit ([`wordcraft_collab_protocol::MAX_MESSAGE_BYTES`]) on receive.
//!
//! The rest of the client only sees decoded [`wordcraft_collab_protocol::ServerMessage`]s, so it
//! is tested without a network.
