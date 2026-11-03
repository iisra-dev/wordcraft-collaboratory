//! WordCraft Collaborative client (docs/collab/design.md §6, §7, §10).
//!
//! - [`coordinator`]: the collaboration state shown in the UI and the bridge between the editing
//!   session and the server (local edits out, committed edits in);
//! - [`bridge`]: committed edits applied to the editing `Session`;
//! - [`capture`]: the supported editor commands as collaborative edits;
//! - [`pending`]: operations submitted and not yet acknowledged, resent after a reconnect;
//! - [`presence`]: other participants and their carets;
//! - [`connection`]: the native WebSocket transport (feature `transport`).
//!
//! MVP policy (§6): *confirm before visible apply*. A local edit becomes an operation, is sent,
//! and only the server's `committed` message changes the local document.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod bridge;
pub mod capture;
pub mod connection;
pub mod coordinator;
pub mod pending;
pub mod presence;

pub use bridge::SessionBridge;
pub use coordinator::{CollaborationState, Event, Output, SharedSession, SubmitError};
pub use pending::PendingQueue;
