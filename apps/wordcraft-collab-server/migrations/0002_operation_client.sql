-- C2.1: the client that submitted each operation, so a resubmitted operation_id gets back its
-- original `committed` message (design §7, idempotence), client id included.
-- Rows written before this migration have none (empty string).

ALTER TABLE document_operations ADD COLUMN client_id TEXT NOT NULL DEFAULT '';

-- Listing a user's documents goes through their memberships.
CREATE INDEX document_members_by_user ON document_members (user_id);
