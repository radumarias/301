-- One row per link creation by a non-admin, for the creation limits. Unlike `links`,
-- rows survive deleting or claiming the link, so neither resets an allowance.
-- Rows older than two days are purged by the hourly cron.
CREATE TABLE IF NOT EXISTS creations (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id   INTEGER,
  ip         TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS creations_owner ON creations(owner_id, created_at) WHERE owner_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS creations_ip ON creations(ip, created_at) WHERE ip IS NOT NULL;
