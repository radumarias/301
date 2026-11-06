-- Accounts (Google sign-in), sessions, link ownership, anonymous links and abuse reports.

CREATE TABLE IF NOT EXISTS users (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  google_sub    TEXT UNIQUE,
  email         TEXT NOT NULL UNIQUE,
  name          TEXT,
  banned        INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  last_login_at INTEGER
);

-- Only a SHA-256 of the session token is stored.
CREATE TABLE IF NOT EXISTS sessions (
  token_hash TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);

-- owner_id NULL + manage_hash NULL: a link from before accounts existed (assigned to the
-- first admin who signs in). manage_hash set: an anonymous link, managed by secret token.
-- creator_ip is an HMAC of the creator's IP (anonymous links only), used for rate limits.
ALTER TABLE links ADD COLUMN owner_id INTEGER;
ALTER TABLE links ADD COLUMN manage_hash TEXT;
ALTER TABLE links ADD COLUMN creator_ip TEXT;
-- Set by an admin; unlike `disabled` the owner cannot undo it.
ALTER TABLE links ADD COLUMN blocked INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS links_owner ON links(owner_id, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS links_manage ON links(manage_hash) WHERE manage_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS links_creator_ip ON links(creator_ip, created_at) WHERE creator_ip IS NOT NULL;

CREATE TABLE IF NOT EXISTS reports (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  link_id    INTEGER NOT NULL,
  reason     TEXT NOT NULL,
  reporter   TEXT,
  created_at INTEGER NOT NULL,
  resolved   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS reports_open ON reports(resolved, created_at);
CREATE INDEX IF NOT EXISTS reports_link ON reports(link_id);
CREATE INDEX IF NOT EXISTS reports_reporter ON reports(reporter, created_at);
