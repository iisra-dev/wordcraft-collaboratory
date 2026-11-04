//! Turning the editor's supported commands into collaborative operations (design §5.4, §6;
//! milestone C4.1).
//!
//! The protocol carries the document *effect*, never the command (§5.4). For the commands shared
//! documents support — typing, Tab, manual line break, Backspace, Delete, Enter — the effect is
//! derived here from the selection and the shared replica, *without running the command*: under
//! confirm-before-apply (§6) the local document only changes when the server commits, so the
//! command must not touch it first.
//!
//! An edit is **one operation**, plus at most one [`FollowUp`]. Typing, Backspace, Delete, Enter
//! and joins are single operations. Replacing a selection inside one paragraph (typing or Enter
//! over it) sends the deletion first; the insertion or split *follows up* once that deletion is
//! committed, placed where the server collapsed it ([`FollowUp::after`]), with that revision as
//! its base. A second operation computed up front would be based on "replica + first operation",
//! a state the server never had if it transformed the first one, and would land in the wrong
//! place.
//!
//! C7.2: a selection across paragraphs, a paste and Cut are **one atomic [`DocumentOperation::Batch`]**
//! instead: its steps are validated on the scratch copy in order, and the server commits all of
//! them as one revision or none (a refused paste never leaves half of it, or a deleted selection
//! without the text that replaced it). Copying never changes the document and runs as usual.
//!
//! Each edit is checked by applying it to a scratch copy of the replica: an edit the server would
//! refuse (an embedded object in the way, a table, a control character) fails here, before
//! anything is sent, and the caret position after it is read from that copy.
//!
//! MVP limits (documented in docs/collab/roadmap.md): no AutoCorrect or AutoFormat as you type,
//! and Enter/Backspace never change list numbering, styles or paragraph formatting in a shared
//! document; selections and carets must be in the body's top-level paragraphs.

use serde_json::Value;
use wordcraft_collab_model::anchors::anchor_from_pos;
use wordcraft_collab_model::{BlockId, ModelError, SharedDocument};
use wordcraft_collab_protocol::DocumentOperation;
use wordcraft_collab_protocol::messages::Anchor;
use wordcraft_collab_protocol::operations::{BasicFormat, MAX_BATCH_STEPS};
use wordcraft_doc::{Block, Paragraph, Pos};

/// Commands a shared document runs as collaborative edits. Every other command that changes the
/// document is blocked on shared documents (C4.3).
pub const SUPPORTED_COMMANDS: &[&str] =
    &["text.insert", "text.tab", "text.lineBreak", "text.backspace", "text.delete", "text.newParagraph", "edit.cut"];

pub fn is_supported(command: &str) -> bool {
    SUPPORTED_COMMANDS.contains(&command) || crate::paste::PASTE_COMMANDS.contains(&command) || crate::format::is_format_command(command)
}

/// A local edit: the operation to submit now (none for a no-op, e.g. Backspace at the very
/// start), what follows once it commits, and where the caret goes (assuming nobody else edits
/// meanwhile; the bridge moves it with later commits).
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedEdit {
    pub operation: Option<DocumentOperation>,
    pub follow_up: Option<FollowUp>,
    pub caret_after: Option<Anchor>,
}

/// The second half of a selection replacement, placed when its deletion commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    Insert { text: String },
    Split { new_block_id: BlockId },
}

impl FollowUp {
    /// The operation to submit after `committed` — this edit's deletion as the server committed
    /// it (possibly transformed) — at the revision it was committed: where that deletion
    /// collapsed. `None` if the committed operation is not a deletion (it always is).
    pub fn after(&self, committed: &DocumentOperation) -> Option<DocumentOperation> {
        let DocumentOperation::DeleteText { block_id, start, .. } = committed else { return None };
        Some(match self {
            Self::Insert { text } => DocumentOperation::InsertText { block_id: *block_id, offset: *start, text: text.clone() },
            Self::Split { new_block_id } => DocumentOperation::SplitParagraph { block_id: *block_id, offset: *start, new_block_id: *new_block_id },
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("not available in shared documents yet: {0}")]
    Unsupported(&'static str),
    #[error("bad parameters for {command}: {reason}")]
    BadParams { command: String, reason: &'static str },
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// The edit `command` (with `params`) makes with the selection `anchor..focus` on `replica`.
/// `new_block_id` names the paragraph an Enter creates.
pub fn capture(
    replica: &SharedDocument,
    anchor: &Pos,
    focus: &Pos,
    command: &str,
    params: &Value,
    new_block_id: impl FnOnce() -> BlockId,
) -> Result<CommittedEdit, CaptureError> {
    let mut edit = Builder::new(replica, anchor, focus)?;
    let text_param = || {
        params
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(CaptureError::BadParams { command: command.into(), reason: "needs \"text\"" })
    };
    match command {
        "text.insert" => edit.replace_with(FollowUp::Insert { text: text_param()? })?,
        "text.tab" => edit.replace_with(FollowUp::Insert { text: "\t".into() })?,
        "text.lineBreak" => edit.replace_with(FollowUp::Insert { text: "\n".into() })?,
        "text.newParagraph" => edit.replace_with(FollowUp::Split { new_block_id: new_block_id() })?,
        "text.backspace" => {
            if !edit.delete_selection()? {
                edit.backspace()?;
            }
        }
        "text.delete" => {
            if !edit.delete_selection()? {
                edit.delete_forward()?;
            }
        }
        // The copy half runs in the editor first (it does not change the document).
        "edit.cut" => {
            edit.delete_selection()?;
        }
        _ => return Err(CaptureError::Unsupported("this command")),
    }
    edit.finish()
}

/// The edit pasting `payload` over the selection `anchor..focus` makes: one batch that deletes
/// the selection, inserts the text (splitting paragraphs between its paragraphs) and gives each
/// formatted run its formatting. `new_block_id` names each paragraph the paste creates.
pub fn capture_paste(
    replica: &SharedDocument,
    anchor: &Pos,
    focus: &Pos,
    payload: &crate::paste::Payload,
    mut new_block_id: impl FnMut() -> BlockId,
) -> Result<CommittedEdit, CaptureError> {
    let mut edit = Builder::new(replica, anchor, focus)?;
    edit.delete_selection()?;
    for (n, runs) in payload.paragraphs.iter().enumerate() {
        let (mut i, mut off) = edit.caret();
        if n > 0 {
            edit.push(DocumentOperation::SplitParagraph { block_id: edit.id(i)?, offset: Builder::offset(off)?, new_block_id: new_block_id() })?;
            (i, off) = (i + 1, 0);
        }
        for run in runs.iter().filter(|r| !r.text.is_empty()) {
            let id = edit.id(i)?;
            let start = Builder::offset(off)?;
            edit.push(DocumentOperation::InsertText { block_id: id, offset: start, text: run.text.clone() })?;
            off += run.text.len();
            if let Some(format) = run.format.clone().filter(|f| !f.is_empty()) {
                edit.push(DocumentOperation::FormatText { block_id: id, start, end: Builder::offset(off)?, format })?;
            }
        }
        edit.start = (i, off);
        edit.end = edit.start;
    }
    edit.finish()
}

/// The edit a formatting command (C7.3) makes on the selection: one `FormatText` per paragraph
/// it covers, as a batch when there are several. A caret formats nothing (an error the UI shows:
/// in shared documents formatting applies to selected text).
pub fn capture_format(replica: &SharedDocument, anchor: &Pos, focus: &Pos, format: &BasicFormat) -> Result<CommittedEdit, CaptureError> {
    let mut edit = Builder::new(replica, anchor, focus)?;
    let ((i, a), (j, b)) = (edit.start, edit.end);
    if (i, a) == (j, b) {
        return Err(CaptureError::Unsupported("formatting without a selection (select the text first)"));
    }
    for k in i..=j {
        let len = edit.para(k)?.len();
        let from = if k == i { a } else { 0 };
        let to = if k == j { b } else { len };
        if from < to {
            edit.push(DocumentOperation::FormatText {
                block_id: edit.id(k)?,
                start: Builder::offset(from)?,
                end: Builder::offset(to)?,
                format: format.clone(),
            })?;
        }
    }
    // The selection stays as it was.
    let caret = edit.end;
    let mut done = edit.finish()?;
    done.caret_after = replica.ids().id_at(caret.0).and_then(|block_id| Some(Anchor { block_id, offset: u32::try_from(caret.1).ok()? }));
    Ok(done)
}

/// Builds an edit step by step on a scratch replica, tracking the caret as a body index and
/// byte offset.
struct Builder {
    scratch: SharedDocument,
    /// The edit's operations, in order: one is sent as it is, several as one batch.
    steps: Vec<DocumentOperation>,
    follow_up: Option<FollowUp>,
    /// Selection in document order, as (paragraph index, byte offset).
    start: (usize, usize),
    end: (usize, usize),
}

impl Builder {
    fn new(replica: &SharedDocument, anchor: &Pos, focus: &Pos) -> Result<Self, CaptureError> {
        let a = Self::locate(replica, anchor)?;
        let b = Self::locate(replica, focus)?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        Ok(Self { scratch: replica.clone(), steps: Vec::new(), follow_up: None, start, end })
    }

    /// A position as (index, offset): it must name a top-level body paragraph of the replica.
    fn locate(replica: &SharedDocument, pos: &Pos) -> Result<(usize, usize), CaptureError> {
        let anchor = anchor_from_pos(replica.ids(), pos).ok_or(CaptureError::Unsupported("editing outside the body's top-level paragraphs"))?;
        let index = replica.ids().index_of(anchor.block_id).ok_or(CaptureError::Unsupported("editing outside the body's top-level paragraphs"))?;
        Ok((index, pos.off))
    }

    fn id(&self, index: usize) -> Result<BlockId, CaptureError> {
        self.scratch.ids().id_at(index).ok_or(CaptureError::Unsupported("editing outside the body's top-level paragraphs"))
    }

    fn para(&self, index: usize) -> Result<&Paragraph, CaptureError> {
        match self.scratch.document().body.get(index).map(|b| &**b) {
            Some(Block::Para(p)) => Ok(p),
            Some(Block::Table(_)) => Err(CaptureError::Unsupported("editing tables")),
            None => Err(CaptureError::Unsupported("editing outside the body's top-level paragraphs")),
        }
    }

    fn offset(off: usize) -> Result<u32, CaptureError> {
        u32::try_from(off).map_err(|_| CaptureError::Unsupported("paragraphs this long"))
    }

    /// Apply to the scratch copy (validating it) and add it to the edit.
    fn push(&mut self, op: DocumentOperation) -> Result<(), CaptureError> {
        if self.steps.len() >= MAX_BATCH_STEPS {
            return Err(CaptureError::Unsupported(crate::paste::TOO_BIG));
        }
        self.scratch.apply(&op)?;
        self.steps.push(op);
        Ok(())
    }

    fn caret(&self) -> (usize, usize) {
        self.start
    }

    /// Delete the selection (collapsing it to its start); whether there was one.
    fn delete_selection(&mut self) -> Result<bool, CaptureError> {
        let ((i, a), (j, b)) = (self.start, self.end);
        if (i, a) == (j, b) {
            return Ok(false);
        }
        if i == j {
            self.push(DocumentOperation::DeleteText { block_id: self.id(i)?, start: Self::offset(a)?, end: Self::offset(b)? })?;
            self.end = self.start;
            return Ok(true);
        }
        // Across paragraphs (C7.2): empty the selected part of each paragraph, last first, then
        // join the emptied ones and the tail of the last onto the first.
        for k in (i..=j).rev() {
            let len = self.para(k)?.len();
            let (from, to) = (if k == i { a } else { 0 }, if k == j { b } else { len });
            if from < to {
                self.push(DocumentOperation::DeleteText { block_id: self.id(k)?, start: Self::offset(from)?, end: Self::offset(to)? })?;
            }
        }
        let first = self.id(i)?;
        for _ in i + 1..=j {
            self.para(i + 1)?;
            self.push(DocumentOperation::MergeParagraphs { first, second: self.id(i + 1)?, first_len: Self::offset(a)? })?;
        }
        self.end = self.start;
        Ok(true)
    }

    /// Typing (or Enter) at the caret, or over the selection: with a selection, the deletion is
    /// the operation and `then` follows up once it commits; without, `then` is the operation.
    fn replace_with(&mut self, then: FollowUp) -> Result<(), CaptureError> {
        if let FollowUp::Insert { text } = &then
            && text.is_empty()
        {
            return Ok(());
        }
        let before = self.steps.len();
        let selected = self.delete_selection()?;
        // Across paragraphs the deletion is a batch already: the rest joins it.
        let batched = self.steps.len() > before + 1;
        let (i, off) = self.caret();
        let op = then.after(&DocumentOperation::DeleteText { block_id: self.id(i)?, start: Self::offset(off)?, end: Self::offset(off)? });
        let Some(op) = op else { return Ok(()) };
        // Validate it on the scratch copy, where the deletion (if any) is already applied.
        self.scratch.apply(&op)?;
        self.start = match &then {
            FollowUp::Insert { text } => (i, off + text.len()),
            FollowUp::Split { .. } => (i + 1, 0),
        };
        self.end = self.start;
        if selected && !batched {
            self.follow_up = Some(then);
        } else {
            self.steps.push(op);
        }
        Ok(())
    }

    /// Backspace at a caret: the cluster before it, or join with the previous paragraph.
    fn backspace(&mut self) -> Result<(), CaptureError> {
        let (i, off) = self.caret();
        if off > 0 {
            let from = self.para(i)?.backspace_from(off);
            self.push(DocumentOperation::DeleteText { block_id: self.id(i)?, start: Self::offset(from)?, end: Self::offset(off)? })?;
            self.start = (i, from);
        } else if i > 0 {
            // Joining onto a table is not shared-editable; para() refuses it.
            let first_len = self.para(i - 1)?.len();
            self.push(DocumentOperation::MergeParagraphs { first: self.id(i - 1)?, second: self.id(i)?, first_len: Self::offset(first_len)? })?;
            self.start = (i - 1, first_len);
        }
        self.end = self.start;
        Ok(())
    }

    /// Delete at a caret: the cluster after it, or join the next paragraph.
    fn delete_forward(&mut self) -> Result<(), CaptureError> {
        let (i, off) = self.caret();
        let para = self.para(i)?;
        let len = para.len();
        if off < len {
            let to = para.next_boundary(off);
            self.push(DocumentOperation::DeleteText { block_id: self.id(i)?, start: Self::offset(off)?, end: Self::offset(to)? })?;
        } else if i + 1 < self.scratch.ids().len() {
            self.para(i + 1)?;
            self.push(DocumentOperation::MergeParagraphs { first: self.id(i)?, second: self.id(i + 1)?, first_len: Self::offset(len)? })?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<CommittedEdit, CaptureError> {
        let (i, off) = self.start;
        let caret_after = self.scratch.ids().id_at(i).and_then(|block_id| Some(Anchor { block_id, offset: u32::try_from(off).ok()? }));
        let operation = match self.steps.len() {
            0 => None,
            1 => self.steps.pop(),
            _ => Some(DocumentOperation::Batch { steps: self.steps }),
        };
        Ok(CommittedEdit { operation, follow_up: self.follow_up, caret_after })
    }
}
