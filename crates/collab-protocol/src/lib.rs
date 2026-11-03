//! WordCraft Collaborative wire protocol (docs/collab/design.md §5 and §7).
//!
//! The types every party agrees on: stable ids ([`ids`]), the collaborative document operations
//! ([`operations`]), the versioned WebSocket messages ([`messages`]) and the reasons a submission
//! is refused ([`errors`]). This crate depends on no other workspace crate, runs no I/O and must
//! build for wasm, so the desktop client, the server and tests share one definition.
//!
//! The protocol carries document *effects* (insert these bytes into this block), never WordCraft
//! command ids: a command depends on the sender's selection and would edit a different place when
//! replayed elsewhere (§5.4).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod errors;
pub mod ids;
pub mod messages;
pub mod operations;

pub use errors::{ProtocolError, RejectReason};
pub use ids::{BlockId, ClientId, DocumentId, OperationId, Revision};
pub use messages::{ClientMessage, Envelope, ServerMessage};
pub use operations::DocumentOperation;

/// Version of the message format. A peer speaking another version is refused at `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest WebSocket text message the server accepts, in bytes. Checked before parsing.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Largest message a client accepts from the server, in bytes: a `snapshot` carries the whole
/// document, pictures included, so it can be far larger than any submission.
pub const MAX_SERVER_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
