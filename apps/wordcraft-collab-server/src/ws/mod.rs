//! WebSocket endpoint `/api/v1/ws` (design §7, milestone C2.3).
//!
//! Decodes frames with `wordcraft_collab_protocol::Envelope::decode` (size limit and version
//! first), routes `join`/`submit`/`resync`/`presence` to the document actor and writes its
//! answers back. Never runs WordCraft commands received from the network (§12).
