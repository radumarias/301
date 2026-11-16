-- Permanent slug reservation (anti-hijack).
--
-- A short link printed in a tweet, an email or a QR code lives forever. If its slug
-- could be claimed again after the link is deleted, whoever claims it controls every
-- copy already out there (the 2026 bit.ly back-half hijacks). So once a slug (or an
-- alias) has pointed somewhere, it is bound to that link id for good:
--
-- * deleting a link or an alias writes a tombstone row here (by trigger, so no code
--   path can forget it);
-- * the database refuses to give a retired slug to any other link id, compared
--   case-insensitively so "Promo" cannot come back as "promo";
-- * the only way to bring a retired slug back is to restore the same link id, with
--   its original destination (AUTOINCREMENT never hands that id to a new link).

ALTER TABLE links ADD COLUMN created_by TEXT;

CREATE TABLE IF NOT EXISTS retired_slugs (
  slug       TEXT PRIMARY KEY,
  link_id    INTEGER NOT NULL,
  was_alias  INTEGER NOT NULL DEFAULT 0,
  -- 1 when the slug went away together with its link (so a restore brings it back);
  -- 0 for an alias removed on its own, which stays removed.
  with_link  INTEGER NOT NULL DEFAULT 1,
  -- Primary rows only (alias rows read them from their link's primary row):
  url        TEXT,
  title      TEXT,
  created_by TEXT,
  created_at INTEGER,
  -- The deleted link's other settings (JSON), so a restore brings it back exactly,
  -- password and expiry included. NULL for alias rows.
  snapshot   TEXT,
  retired_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS retired_slugs_nocase ON retired_slugs(slug COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS retired_slugs_link ON retired_slugs(link_id, was_alias);
CREATE INDEX IF NOT EXISTS retired_slugs_recent ON retired_slugs(retired_at DESC, link_id DESC, was_alias, slug);
CREATE INDEX IF NOT EXISTS links_slug_nocase ON links(slug COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS aliases_slug_nocase ON aliases(slug COLLATE NOCASE);

-- Tombstones. store::delete_links deletes the link row first, so an alias whose link is
-- already gone was deleted with it (with_link = 1); otherwise it was removed on its own.
CREATE TRIGGER IF NOT EXISTS retire_link_slug AFTER DELETE ON links
BEGIN
  INSERT INTO retired_slugs (slug, link_id, was_alias, url, title, created_by, created_at, snapshot, retired_at)
  VALUES (OLD.slug, OLD.id, 0, OLD.url, OLD.title, OLD.created_by, OLD.created_at,
          json_object('notes', OLD.notes, 'tags', OLD.tags, 'redirect_type', OLD.redirect_type,
                      'disabled', OLD.disabled, 'expires_at', OLD.expires_at, 'max_clicks', OLD.max_clicks,
                      'fallback_url', OLD.fallback_url, 'password_hash', OLD.password_hash,
                      'passthrough', OLD.passthrough, 'utm', OLD.utm, 'geo', OLD.geo,
                      'device', OLD.device, 'source', OLD.source),
          CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER))
  ON CONFLICT(slug) DO UPDATE SET
    was_alias = 0, with_link = 1, url = excluded.url, title = excluded.title,
    created_by = excluded.created_by, created_at = excluded.created_at,
    snapshot = excluded.snapshot, retired_at = excluded.retired_at
  WHERE link_id = excluded.link_id;
END;

CREATE TRIGGER IF NOT EXISTS retire_alias AFTER DELETE ON aliases
BEGIN
  INSERT INTO retired_slugs (slug, link_id, was_alias, with_link, retired_at)
  VALUES (OLD.slug, OLD.link_id, 1,
          NOT EXISTS (SELECT 1 FROM links WHERE id = OLD.link_id),
          CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER))
  ON CONFLICT(slug) DO UPDATE SET
    was_alias = 1, with_link = excluded.with_link, url = NULL, title = NULL, created_by = NULL,
    created_at = NULL, snapshot = NULL, retired_at = excluded.retired_at
  WHERE link_id = excluded.link_id;
END;

-- Guards. `IS NOT` (not `<>`) so a NULL/auto-assigned NEW.id still counts as "another link".
CREATE TRIGGER IF NOT EXISTS links_insert_not_retired BEFORE INSERT ON links
WHEN EXISTS (SELECT 1 FROM retired_slugs WHERE slug = NEW.slug COLLATE NOCASE AND link_id IS NOT NEW.id)
BEGIN
  SELECT RAISE(ABORT, 'slug is permanently retired');
END;

CREATE TRIGGER IF NOT EXISTS links_rename_not_retired BEFORE UPDATE OF slug ON links
WHEN NEW.slug IS NOT OLD.slug
 AND EXISTS (SELECT 1 FROM retired_slugs WHERE slug = NEW.slug COLLATE NOCASE AND link_id IS NOT NEW.id)
BEGIN
  SELECT RAISE(ABORT, 'slug is permanently retired');
END;

CREATE TRIGGER IF NOT EXISTS links_id_immutable BEFORE UPDATE OF id ON links
WHEN NEW.id IS NOT OLD.id
BEGIN
  SELECT RAISE(ABORT, 'link ids are immutable');
END;

CREATE TRIGGER IF NOT EXISTS aliases_insert_not_retired BEFORE INSERT ON aliases
WHEN EXISTS (SELECT 1 FROM retired_slugs WHERE slug = NEW.slug COLLATE NOCASE AND link_id IS NOT NEW.link_id)
BEGIN
  SELECT RAISE(ABORT, 'slug is permanently retired');
END;

CREATE TRIGGER IF NOT EXISTS aliases_update_not_retired BEFORE UPDATE OF slug, link_id ON aliases
WHEN EXISTS (SELECT 1 FROM retired_slugs WHERE slug = NEW.slug COLLATE NOCASE AND link_id IS NOT NEW.link_id)
BEGIN
  SELECT RAISE(ABORT, 'slug is permanently retired');
END;

-- Tombstones are permanent: no statement may remove or rebind one.
CREATE TRIGGER IF NOT EXISTS retired_slugs_no_delete BEFORE DELETE ON retired_slugs
BEGIN
  SELECT RAISE(ABORT, 'retired slugs are permanent');
END;

CREATE TRIGGER IF NOT EXISTS retired_slugs_no_rebind BEFORE UPDATE ON retired_slugs
WHEN NEW.slug IS NOT OLD.slug OR NEW.link_id IS NOT OLD.link_id
BEGIN
  SELECT RAISE(ABORT, 'retired slugs are permanent');
END;
