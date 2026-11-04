-- C6.5: bearer invitations. Only hashes are persisted; the UUID is for owner management.
CREATE TABLE share_links (
    id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL REFERENCES documents(id),
    token_sha256 TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK (role IN ('editor', 'viewer')),
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    allow_guests INTEGER NOT NULL CHECK (allow_guests IN (0, 1)),
    max_guests INTEGER NOT NULL CHECK (max_guests BETWEEN 0 AND 100),
    guests_used INTEGER NOT NULL DEFAULT 0 CHECK (guests_used BETWEEN 0 AND max_guests),
    revoked_at TEXT
);
CREATE INDEX share_links_by_document ON share_links(document_id, created_at, id);

-- A guest session is scoped to one document, not a normal server account.
CREATE TABLE guest_accounts (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    document_id TEXT NOT NULL REFERENCES documents(id),
    link_id TEXT NOT NULL REFERENCES share_links(id)
);
