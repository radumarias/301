-- Social previews, A/B split, app deep links, link health checks, webhooks.

-- {"title","description","image"} shown to link-preview crawlers (Slack, X, iMessage...).
ALTER TABLE links ADD COLUMN og TEXT;
-- [{"url","weight"}]: extra destinations for an A/B split; the main URL gets the rest.
ALTER TABLE links ADD COLUMN variants TEXT;
-- {"ios","android"}: app URIs tried first on phones, the web destination is the fallback.
ALTER TABLE links ADD COLUMN app TEXT;

-- Broken-link checker: last HTTP status (0 = network error), its error text, and when.
ALTER TABLE links ADD COLUMN health_code INTEGER;
ALTER TABLE links ADD COLUMN health_error TEXT;
ALTER TABLE links ADD COLUMN health_checked_at INTEGER;
CREATE INDEX IF NOT EXISTS links_health ON links(health_checked_at);

-- Which A/B variant a click went to (0 = main URL), NULL when the link has no split.
ALTER TABLE clicks ADD COLUMN variant INTEGER;

CREATE TABLE IF NOT EXISTS webhooks (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  url         TEXT NOT NULL,
  events      TEXT NOT NULL DEFAULT '[]',
  secret      TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  last_status INTEGER,
  last_error  TEXT,
  last_at     INTEGER
);
