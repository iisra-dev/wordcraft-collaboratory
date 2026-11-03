//! The bridge between a shared document and the editing [`Session`] (design §6 "Cambios
//! remotos"; milestone C4.2).
//!
//! [`SessionBridge::apply_remote_edit`] applies a committed operation to `Session::doc` the way
//! every replica does (`wordcraft_collab_model::apply`), keeping the bridge's own block identity
//! registry in step, and then:
//! - moves the selection with the edit (structural anchors, [`transform_anchor`]); a caret inside
//!   a table keeps its cell and follows its table's top-level block;
//! - invalidates the layout (`Session::touch`: the session revision is the layout cache key; the
//!   per-paragraph caches already follow each paragraph's own revision);
//! - emits nothing outgoing and adds no undo step (it never goes through `Session::run`), and
//!   never calls `Session::set_document`, which would replace the session's document and reset
//!   its history.
//!
//! [`SessionBridge::load`] puts a snapshot in the session (joining, or recovering after the
//! replica was dropped).

use wordcraft_collab_model::anchors::transform_anchor;
use wordcraft_collab_model::{BlockIdentityMap, ModelError, SharedDocument, apply};
use wordcraft_collab_protocol::DocumentOperation;
use wordcraft_collab_protocol::messages::Anchor;
use wordcraft_doc::{Path, Pos, StoryRef};
use wordcraft_engine::{Selection, Session};

/// Keeps a session's document in step with the shared one.
#[derive(Debug, Default)]
pub struct SessionBridge {
    ids: BlockIdentityMap,
}

/// A position of the selection as the bridge tracks it across an edit.
enum Tracked {
    /// In a top-level body paragraph.
    Paragraph(Anchor),
    /// Deeper in the body (a table cell): its top-level block, and the rest of the path.
    Nested { top: Anchor, rest: Vec<u32>, off: usize },
    /// Another story (header, footer, note…): shared edits never move it.
    Elsewhere(Pos),
}

impl SessionBridge {
    /// Show `replica` (a snapshot just restored) in `session`, keeping the caret where it was if
    /// it still fits.
    pub fn load(&mut self, session: &mut Session, replica: &SharedDocument) {
        session.doc = replica.document().clone();
        self.ids = replica.ids().clone();
        let (anchor, focus) = (session.doc.clamp(&session.sel.anchor), session.doc.clamp(&session.sel.focus));
        session.sel = Selection { anchor, focus };
        session.touch();
    }

    /// The registry for the session's document (what [`crate::capture`] maps positions with).
    pub fn ids(&self) -> &BlockIdentityMap {
        &self.ids
    }

    /// Apply a committed operation to the session. `own` if this client made it (its caret
    /// follows its own typing). On error nothing changed and the session should be reloaded
    /// from the replica ([`Self::load`]).
    pub fn apply_remote_edit(&mut self, session: &mut Session, operation: &DocumentOperation, own: bool) -> Result<(), ModelError> {
        let anchor = self.track(&session.sel.anchor);
        let focus = self.track(&session.sel.focus);
        apply::apply(&mut session.doc, &mut self.ids, operation)?;
        let anchor = self.place(session, anchor, operation, own);
        let focus = self.place(session, focus, operation, own);
        session.sel = Selection { anchor, focus };
        session.touch();
        Ok(())
    }

    fn track(&self, pos: &Pos) -> Tracked {
        let top = pos.path.0.first().and_then(|i| self.ids.id_at(*i as usize));
        match (pos.story, top) {
            (StoryRef::Body, Some(block_id)) if pos.path.0.len() == 1 => {
                Tracked::Paragraph(Anchor { block_id, offset: u32::try_from(pos.off).unwrap_or(u32::MAX) })
            }
            (StoryRef::Body, Some(block_id)) => {
                Tracked::Nested { top: Anchor { block_id, offset: 0 }, rest: pos.path.0.get(1..).unwrap_or_default().to_vec(), off: pos.off }
            }
            _ => Tracked::Elsewhere(pos.clone()),
        }
    }

    /// The position after `op`, clamped to the document (never out of range).
    fn place(&self, session: &Session, tracked: Tracked, op: &DocumentOperation, own: bool) -> Pos {
        let pos = match tracked {
            Tracked::Paragraph(anchor) => {
                let a = transform_anchor(anchor, op, own);
                self.ids.index_of(a.block_id).map(|i| Pos::body(i, a.offset as usize))
            }
            Tracked::Nested { top, rest, off } => self.ids.index_of(top.block_id).and_then(|i| {
                let mut path = vec![u32::try_from(i).ok()?];
                path.extend(rest);
                Some(Pos::new(StoryRef::Body, Path(path), off))
            }),
            Tracked::Elsewhere(pos) => Some(pos),
        };
        let pos = pos.unwrap_or_else(|| session.doc.start_of(StoryRef::Body));
        session.doc.clamp(&pos)
    }
}
