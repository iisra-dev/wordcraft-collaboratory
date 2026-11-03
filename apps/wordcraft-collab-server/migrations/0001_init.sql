-- WordCraft Collaborative: initial schema (docs/collab/design.md §9).
-- Applied once by persistence::migrate inside a transaction; PRAGMA user_version tracks it.
-- Never edit after release: add 0002_*.sql instead. Timestamps are RFC 3339 UTC text.

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    oidc_subject TEXT UNIQUE,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE documents (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    current_revision INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (owner_id) REFERENCES users(id)
);

CREATE TABLE document_members (
    document_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL
        CHECK (role IN ('owner', 'editor', 'viewer')),
    PRIMARY KEY (document_id, user_id),
    FOREIGN KEY (document_id) REFERENCES documents(id),
    FOREIGN KEY (user_id) REFERENCES users(id)
);

CREATE TABLE document_operations (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    operation_id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    payload BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (document_id, revision),
    UNIQUE (document_id, operation_id),
    FOREIGN KEY (document_id) REFERENCES documents(id)
);

CREATE TABLE document_snapshots (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    storage_key TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (document_id, revision)
);
