-- C5.4: an audit trail of who did what to which document (design §12): actions and ids only,
-- never document content. Edits themselves are already in document_operations.

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    actor_id TEXT,
    action TEXT NOT NULL,
    document_id TEXT,
    subject_id TEXT,
    detail TEXT NOT NULL DEFAULT ''
);

CREATE INDEX audit_log_by_document ON audit_log (document_id, id);
