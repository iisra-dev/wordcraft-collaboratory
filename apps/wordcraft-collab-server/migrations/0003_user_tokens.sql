-- C2.3: local sign-in for setups without an identity provider (design §12, "autenticación local").
-- Only the SHA-256 of each bearer token is stored; the token itself is shown once, when created.

CREATE TABLE user_tokens (
    token_sha256 TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id)
);
