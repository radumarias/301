-- 301.xorio.rs schema
CREATE TABLE IF NOT EXISTS links (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  slug          TEXT NOT NULL UNIQUE,
  url           TEXT NOT NULL,
  title         TEXT,
  notes         TEXT,
  tags          TEXT NOT NULL DEFAULT '[]',
  redirect_type INTEGER NOT NULL DEFAULT 302,
  disabled      INTEGER NOT NULL DEFAULT 0,
  expires_at    INTEGER,
  max_clicks    INTEGER,
  fallback_url  TEXT,
  password_hash TEXT,
  passthrough   INTEGER NOT NULL DEFAULT 1,
  utm           TEXT,
  geo           TEXT,
  device        TEXT,
  source        TEXT NOT NULL DEFAULT 'manual',
  clicks        INTEGER NOT NULL DEFAULT 0,
  last_click_at INTEGER,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS links_created ON links(created_at);
CREATE INDEX IF NOT EXISTS links_clicks ON links(clicks);

CREATE TABLE IF NOT EXISTS aliases (
  slug    TEXT PRIMARY KEY,
  link_id INTEGER NOT NULL REFERENCES links(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS aliases_link ON aliases(link_id);

CREATE TABLE IF NOT EXISTS clicks (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  link_id  INTEGER NOT NULL,
  ts       INTEGER NOT NULL,
  country  TEXT,
  city     TEXT,
  referrer TEXT,
  device   TEXT,
  os       TEXT,
  browser  TEXT,
  bot      INTEGER NOT NULL DEFAULT 0,
  qr       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS clicks_link_ts ON clicks(link_id, ts);
CREATE INDEX IF NOT EXISTS clicks_ts ON clicks(ts);

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
