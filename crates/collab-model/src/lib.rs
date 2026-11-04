//! WordCraft Collaborative operation model (docs/collab/design.md §5).
//!
//! Pure document logic shared by the server and the desktop client:
//! - [`identity`]: the registry that names paragraphs with stable [`BlockId`]s and maps them to
//!   WordCraft paths;
//! - [`apply`]: validating and applying a [`DocumentOperation`] to a `Document`;
//! - [`transform`]: rebasing an operation over concurrent commits, or refusing with `Conflict`;
//! - [`snapshot`]: the shared state (document + registry) as stored and sent to joining clients;
//! - [`anchors`]: carets and selections as structural anchors that survive remote edits.
//!
//! The milestone-01 proof of concept lives here: two independent documents that receive the same
//! committed operations end up identical (design §16).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod anchors;
pub mod apply;
pub mod error;
pub mod format;
pub mod identity;
pub mod snapshot;
pub mod transform;

pub use error::ModelError;
pub use identity::BlockIdentityMap;
pub use snapshot::{SNAPSHOT_FORMAT, SharedDocument};
pub use wordcraft_collab_protocol::{BlockId, DocumentOperation};
