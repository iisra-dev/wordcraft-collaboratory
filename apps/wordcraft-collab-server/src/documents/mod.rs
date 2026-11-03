//! Document actors (design §8, milestone C2.2).
//!
//! One logical actor per open document, fed by a Tokio channel, serialises that document's
//! submissions; different documents run concurrently. For each submission:
//!
//! 1. authenticate and authorise (editor or owner);
//! 2. validate the operation and its base revision;
//! 3. transform it over the commits since its base (`wordcraft_collab_model::transform`), or
//!    reject it with `Conflict`;
//! 4. apply it to a *working copy* (`wordcraft_collab_model::apply`);
//! 5. persist the operation and the new revision (one SQLite transaction);
//! 6. only then replace the authoritative state and publish `committed` to every subscriber.
//!
//! A resubmitted `operation_id` returns its original commit without applying it again. Slow
//! subscribers get bounded queues (backpressure) and are dropped, not waited for.
