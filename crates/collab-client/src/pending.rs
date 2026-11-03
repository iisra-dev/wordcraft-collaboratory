//! Operations sent and not yet acknowledged (design §7, reconnection and idempotence).
//!
//! The server may accept an operation and the connection drop before the client hears about it.
//! Every submission therefore stays here until its `committed` (or `rejected`) arrives, keyed by
//! `operation_id`; after a reconnect they are all resent and the server answers duplicates with
//! the original commit instead of applying them again.

use std::collections::VecDeque;

use wordcraft_collab_protocol::{DocumentOperation, OperationId, Revision};

/// Most operations a client keeps in flight; past this the UI stops accepting edits.
pub const MAX_PENDING: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOperation {
    pub operation_id: OperationId,
    pub base_revision: Revision,
    pub operation: DocumentOperation,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PendingError {
    #[error("too many unacknowledged operations ({MAX_PENDING})")]
    Full,
    #[error("operation {0:?} is already pending")]
    Duplicate(OperationId),
}

/// Unacknowledged operations, oldest first.
#[derive(Clone, Debug, Default)]
pub struct PendingQueue {
    ops: VecDeque<PendingOperation>,
}

impl PendingQueue {
    pub fn push(&mut self, op: PendingOperation) -> Result<(), PendingError> {
        if self.ops.len() >= MAX_PENDING {
            return Err(PendingError::Full);
        }
        if self.ops.iter().any(|p| p.operation_id == op.operation_id) {
            return Err(PendingError::Duplicate(op.operation_id));
        }
        self.ops.push_back(op);
        Ok(())
    }

    /// Settle an operation (committed or rejected). Unknown ids — another client's commit, or a
    /// duplicate acknowledgement — return `None`.
    pub fn settle(&mut self, id: OperationId) -> Option<PendingOperation> {
        let index = self.ops.iter().position(|p| p.operation_id == id)?;
        self.ops.remove(index)
    }

    /// What to resend after a reconnect, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &PendingOperation> {
        self.ops.iter()
    }

    /// Forget everything (access to the document was removed: nothing may be resent).
    pub fn clear(&mut self) {
        self.ops.clear();
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use wordcraft_collab_protocol::BlockId;

    fn op(n: u128) -> PendingOperation {
        PendingOperation {
            operation_id: OperationId(Uuid::from_u128(n)),
            base_revision: 1,
            operation: DocumentOperation::InsertText { block_id: BlockId(Uuid::nil()), offset: 0, text: "x".into() },
        }
    }

    #[test]
    fn acknowledgements_settle_once() {
        let mut q = PendingQueue::default();
        q.push(op(1)).unwrap();
        q.push(op(2)).unwrap();
        assert_eq!(q.push(op(1)), Err(PendingError::Duplicate(op(1).operation_id)));
        assert_eq!(q.settle(op(1).operation_id), Some(op(1)));
        assert_eq!(q.settle(op(1).operation_id), None);
        assert_eq!(q.iter().map(|p| p.operation_id).collect::<Vec<_>>(), [op(2).operation_id]);
    }

    #[test]
    fn queue_is_bounded() {
        let mut q = PendingQueue::default();
        for n in 0..MAX_PENDING as u128 {
            q.push(op(n)).unwrap();
        }
        assert_eq!(q.push(op(u128::MAX)), Err(PendingError::Full));
    }
}
