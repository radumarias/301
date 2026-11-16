//! Persistence: D1 is the source of truth, KV is a read-through cache for redirects.
//!
//! Every link mutation goes through this module, which keeps KV in sync in one place
//! (`sync_kv` / `retire_kv`).

use crate::model::{validate_redirect_type, validate_url, KvRecord, Link, LinkInput, LinkRow};
use crate::slug::{self, Style};
use crate::util::now_ms;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env, KvStore};

pub const KV_CACHE_TTL: u64 = 60;

pub const LINK_COLS: &str = "id, slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, created_by, clicks, last_click_at, created_at, updated_at";

#[derive(Debug)]
pub enum StoreError {
    Bad(String),
    NotFound,
    Conflict(String),
    Forbidden(String),
    Internal(worker::Error),
}

impl StoreError {
    /// Message safe to show to the admin (internal details are logged, not returned).
    pub fn public_message(&self) -> String {
        match self {
            StoreError::Bad(m) | StoreError::Conflict(m) | StoreError::Forbidden(m) => m.clone(),
            StoreError::NotFound => "not found".into(),
            StoreError::Internal(e) => {
                worker::console_error!("internal: {e}");
                "internal error".into()
            }
        }
    }
}

/// Message of the D1 triggers in migrations/0002 that guard retired slugs.
const RETIRED_TRIGGER_MSG: &str = "slug is permanently retired";

impl From<worker::Error> for StoreError {
    fn from(e: worker::Error) -> Self {
        // A write that lost a race to the database's own retired-slug guard.
        if e.to_string().contains(RETIRED_TRIGGER_MSG) {
            return StoreError::Conflict(RETIRED_MSG.into());
        }
        StoreError::Internal(e)
    }
}

/// What the admin sees when a slug is refused because it was used before.
pub const RETIRED_MSG: &str = "this slug was used before (by a deleted link or a removed alias) and is \
     permanently retired: it can never point anywhere else (restore the deleted link to use it again)";

impl From<worker::kv::KvError> for StoreError {
    fn from(e: worker::kv::KvError) -> Self {
        StoreError::Internal(worker::Error::RustError(e.to_string()))
    }
}

pub type SResult<T> = std::result::Result<T, StoreError>;

#[derive(Deserialize)]
pub struct IdRow {
    pub id: i64,
}

#[derive(Deserialize)]
pub struct CountRow {
    pub n: i64,
}

pub fn db(env: &Env) -> worker::Result<D1Database> {
    env.d1("DB")
}

pub fn kv(env: &Env) -> worker::Result<KvStore> {
    env.kv("LINKS")
}

// ---- D1 bind helpers (numbers go in as f64: D1 does not accept BigInt) ----

pub fn s(v: &str) -> JsValue {
    JsValue::from_str(v)
}
pub fn os(v: &Option<String>) -> JsValue {
    v.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL)
}
pub fn n(v: i64) -> JsValue {
    JsValue::from_f64(v as f64)
}
pub fn on(v: Option<i64>) -> JsValue {
    v.map(n).unwrap_or(JsValue::NULL)
}
fn b(v: bool) -> JsValue {
    n(v as i64)
}
fn json_or_null<T: Serialize>(v: &T, empty: bool) -> JsValue {
    if empty {
        JsValue::NULL
    } else {
        s(&serde_json::to_string(v).unwrap_or_default())
    }
}
pub fn tags_json(tags: &[String]) -> JsValue {
    s(&serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()))
}

pub fn kv_key(slug: &str) -> String {
    format!("s:{slug}")
}

// ---- Settings ----

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub root_url: Option<String>,
    pub not_found_url: Option<String>,
    pub default_redirect: u16,
    pub default_slug_style: String,
    pub default_slug_length: usize,
    /// Click events older than this are deleted by the hourly cron (0 keeps them forever).
    /// Per-link click totals are kept.
    pub click_retention_days: u32,
}

/// Default retention for click events: 13 months.
pub const DEFAULT_CLICK_RETENTION_DAYS: u32 = 395;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            root_url: None,
            not_found_url: None,
            default_redirect: 302,
            default_slug_style: "random".into(),
            default_slug_length: 7,
            click_retention_days: DEFAULT_CLICK_RETENTION_DAYS,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        for u in [&self.root_url, &self.not_found_url].into_iter().flatten() {
            validate_url(u)?;
        }
        validate_redirect_type(self.default_redirect)?;
        if !(4..=32).contains(&self.default_slug_length) {
            return Err("default_slug_length must be between 4 and 32".into());
        }
        if self.click_retention_days > 3650 {
            return Err(
                "click_retention_days must be at most 3650 (0 keeps clicks forever)".into(),
            );
        }
        Ok(())
    }

    pub fn slug_style(&self) -> Style {
        Style::parse(&self.default_slug_style)
    }
}

/// Settings for the redirect path: KV first (edge-cached), D1 as fallback (and refill).
pub async fn settings_cached(env: &Env) -> Settings {
    let Ok(store) = kv(env) else {
        return Settings::default();
    };
    if let Ok(Some(st)) = store
        .get("cfg")
        .cache_ttl(KV_CACHE_TTL)
        .json::<Settings>()
        .await
    {
        return st;
    }
    match settings(env).await {
        Ok(st) => {
            if let Ok(put) = store.put("cfg", serde_json::to_string(&st).unwrap_or_default()) {
                let _ = put.execute().await;
            }
            st
        }
        // Do not cache defaults on a D1 error: they would stick until settings are re-saved.
        Err(_) => Settings::default(),
    }
}

async fn settings_row(env: &Env) -> worker::Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        value: String,
    }
    Ok(db(env)?
        .prepare("SELECT value FROM settings WHERE key = 'settings'")
        .first::<Row>(None)
        .await?
        .map(|r| r.value))
}

pub async fn settings(env: &Env) -> worker::Result<Settings> {
    Ok(settings_row(env)
        .await?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Like `settings`, but a stored value that does not parse is an error instead of the
/// defaults, so destructive jobs never act on a retention the owner did not choose.
async fn settings_strict(env: &Env) -> worker::Result<Settings> {
    match settings_row(env).await? {
        None => Ok(Settings::default()),
        Some(v) => serde_json::from_str(&v)
            .map_err(|e| worker::Error::RustError(format!("stored settings do not parse: {e}"))),
    }
}

pub async fn save_settings(env: &Env, st: &Settings) -> SResult<()> {
    st.validate().map_err(StoreError::Bad)?;
    let json = serde_json::to_string(st).unwrap_or_default();
    db(env)?
        .prepare("INSERT INTO settings (key, value) VALUES ('settings', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(&[s(&json)])?
        .run()
        .await?;
    kv(env)?.put("cfg", json)?.execute().await?;
    Ok(())
}

// ---- Reading links ----

/// Fills `aliases` for a set of links with one query per 90 links.
pub async fn with_aliases(env: &Env, mut links: Vec<Link>) -> worker::Result<Vec<Link>> {
    #[derive(Deserialize)]
    struct Row {
        slug: String,
        link_id: i64,
    }
    if links.is_empty() {
        return Ok(links);
    }
    let d = db(env)?;
    let mut by_link: HashMap<i64, Vec<String>> = HashMap::new();
    for chunk in links.chunks(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|l| n(l.id)).collect();
        let rows = d
            .prepare(format!(
                "SELECT slug, link_id FROM aliases WHERE link_id IN ({marks}) ORDER BY slug"
            ))
            .bind(&ids)?
            .all()
            .await?
            .results::<Row>()?;
        for r in rows {
            by_link.entry(r.link_id).or_default().push(r.slug);
        }
    }
    for l in &mut links {
        l.aliases = by_link.remove(&l.id).unwrap_or_default();
    }
    Ok(links)
}

/// Runs a `SELECT {LINK_COLS} FROM links ...` query (pass the part after `FROM links`).
pub async fn query_links(env: &Env, tail: &str, binds: &[JsValue]) -> worker::Result<Vec<Link>> {
    let rows = db(env)?
        .prepare(format!("SELECT {LINK_COLS} FROM links {tail}"))
        .bind(binds)?
        .all()
        .await?
        .results::<LinkRow>()?;
    Ok(rows.into_iter().map(Link::from).collect())
}

pub async fn get_link(env: &Env, id: i64) -> worker::Result<Option<Link>> {
    let links = query_links(env, "WHERE id = ?1", &[n(id)]).await?;
    Ok(with_aliases(env, links).await?.pop())
}

/// Why a slug cannot be given to a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// A live link (by its slug or an alias) uses it.
    Live(i64),
    /// It belonged to a link that was deleted (or to a removed alias) and stays bound to
    /// that link id forever, so old copies of the short URL can never be hijacked.
    Retired(i64),
}

/// The first hold on a slug by a link other than `owner` (live holds first).
///
/// Retired slugs always compare case-insensitively ("Promo" retired blocks "promo").
/// Live slugs do too when `live_nocase` is set (manual and generated slugs, so nobody
/// creates a look-alike of an existing link); imports keep exact matching, because the
/// services they come from have case-sensitive slugs that must survive the move.
pub async fn slug_held(
    env: &Env,
    slug: &str,
    owner: Option<i64>,
    live_nocase: bool,
) -> worker::Result<Option<Held>> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
        retired: i64,
    }
    let live = if live_nocase {
        "= ?1 COLLATE NOCASE"
    } else {
        "= ?1"
    };
    let row = db(env)?
        .prepare(format!(
            "SELECT id, 0 AS retired FROM links WHERE slug {live} AND id IS NOT ?2 \
             UNION ALL SELECT link_id, 0 FROM aliases WHERE slug {live} AND link_id IS NOT ?2 \
             UNION ALL SELECT link_id, 1 FROM retired_slugs WHERE slug = ?1 COLLATE NOCASE AND link_id IS NOT ?2 \
             ORDER BY retired LIMIT 1"
        ))
        .bind(&[s(slug), on(owner)])?
        .first::<Row>(None)
        .await?;
    Ok(row.map(|r| {
        if r.retired == 0 {
            Held::Live(r.id)
        } else {
            Held::Retired(r.id)
        }
    }))
}

pub fn held_error(sl: &str, held: Held) -> StoreError {
    match held {
        Held::Live(_) => StoreError::Conflict(format!("slug '{sl}' is already taken")),
        Held::Retired(_) => StoreError::Conflict(format!("slug '{sl}': {RETIRED_MSG}")),
    }
}

pub async fn unique_slug(env: &Env, style: Style, length: usize) -> SResult<String> {
    for attempt in 0..12 {
        let candidate = slug::generate(style, length, attempt);
        if slug_held(env, &candidate, None, true).await?.is_none() {
            return Ok(candidate);
        }
    }
    Err(StoreError::Conflict(
        "could not find a free slug, try again".into(),
    ))
}

/// Validates a requested slug and checks it is free (or already held by `owner`, which
/// includes `owner`'s own retired slugs, such as an alias it had before).
pub async fn claim_slug(env: &Env, requested: &str, owner: Option<i64>) -> SResult<String> {
    let sl = slug::normalize(requested).ok_or_else(|| StoreError::Bad("slug is empty".into()))?;
    slug::validate(&sl).map_err(StoreError::Bad)?;
    match slug_held(env, &sl, owner, true).await? {
        Some(h) => Err(held_error(&sl, h)),
        None => Ok(sl),
    }
}

// ---- Writing links ----

async fn apply_password(link: &mut Link, input: &LinkInput) -> SResult<()> {
    if let Some(p) = &input.password {
        match p.as_deref().map(str::trim) {
            // Trimmed here and in the redirect path's check, so both sides agree.
            Some(pw) if !pw.is_empty() => {
                if pw.len() > 256 {
                    return Err(StoreError::Bad(
                        "password is longer than 256 characters".into(),
                    ));
                }
                link.password_hash = Some(crate::crypto::hash_password(pw).await?);
            }
            _ => link.password_hash = None,
        }
    }
    link.has_password = link.password_hash.is_some();
    Ok(())
}

/// The 17 column values shared by INSERT and UPDATE, as ?1..?17.
fn link_binds(l: &Link) -> Vec<JsValue> {
    vec![
        s(&l.slug),
        s(&l.url),
        os(&l.title),
        os(&l.notes),
        tags_json(&l.tags),
        n(l.redirect_type as i64),
        b(l.disabled),
        on(l.expires_at),
        on(l.max_clicks),
        os(&l.fallback_url),
        os(&l.password_hash),
        b(l.passthrough),
        json_or_null(&l.utm, l.utm.is_empty()),
        json_or_null(&l.geo, l.geo.is_empty()),
        json_or_null(&l.device, l.device.is_empty()),
        s(&l.source),
        n(l.updated_at),
    ]
}

pub fn insert_stmt(db: &D1Database, l: &Link) -> worker::Result<D1PreparedStatement> {
    let mut binds = link_binds(l);
    binds.push(n(l.created_at));
    binds.push(os(&l.created_by));
    db.prepare(
        "INSERT INTO links (slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, updated_at, created_at, created_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19) RETURNING id",
    )
    .bind(&binds)
}

pub fn update_stmt(db: &D1Database, l: &Link) -> worker::Result<D1PreparedStatement> {
    let mut binds = link_binds(l);
    binds.push(n(l.id));
    db.prepare(
        "UPDATE links SET slug = ?1, url = ?2, title = ?3, notes = ?4, tags = ?5, redirect_type = ?6, disabled = ?7, expires_at = ?8, \
         max_clicks = ?9, fallback_url = ?10, password_hash = ?11, passthrough = ?12, utm = ?13, geo = ?14, device = ?15, source = ?16, updated_at = ?17 \
         WHERE id = ?18",
    )
    .bind(&binds)
}

/// Builds a new (unsaved) link from an input, with defaults from settings.
pub async fn new_link(
    env: &Env,
    st: &Settings,
    input: &LinkInput,
    source: &str,
    created_by: &str,
) -> SResult<Link> {
    let mut link = Link::new(st.default_redirect, source);
    link.created_by = Some(created_by.to_string());
    input.apply(&mut link).map_err(StoreError::Bad)?;
    apply_password(&mut link, input).await?;
    link.slug = match input.slug.as_deref().and_then(slug::normalize) {
        Some(sl) => claim_slug(env, &sl, None).await?,
        None => {
            let style = input
                .slug_style
                .as_deref()
                .map(Style::parse)
                .unwrap_or(st.slug_style());
            unique_slug(env, style, st.default_slug_length).await?
        }
    };
    let now = now_ms();
    link.created_at = now;
    link.updated_at = now;
    Ok(link)
}

pub async fn create_link(env: &Env, input: &LinkInput, created_by: &str) -> SResult<Link> {
    let st = settings(env).await.unwrap_or_default();
    let mut link = new_link(env, &st, input, "manual", created_by).await?;
    let row = insert_stmt(&db(env)?, &link)?.first::<IdRow>(None).await?;
    link.id = row
        .ok_or_else(|| {
            StoreError::Internal(worker::Error::RustError("insert returned no id".into()))
        })?
        .id;
    sync_kv(env, &link).await?;
    Ok(link.with_status())
}

/// Applies `input` to an already-loaded link and returns the statements that persist it.
/// The caller runs them (alone or in a batch) and then calls `sync_kv`.
pub async fn prepare_update(
    env: &Env,
    link: &mut Link,
    input: &LinkInput,
) -> SResult<Vec<D1PreparedStatement>> {
    let old_slug = link.slug.clone();
    input.apply(link).map_err(StoreError::Bad)?;
    apply_password(link, input).await?;
    let d = db(env)?;
    let mut stmts = vec![];

    if let Some(requested) = input
        .slug
        .as_deref()
        .and_then(slug::normalize)
        .filter(|s| *s != old_slug)
    {
        let new_slug = claim_slug(env, &requested, Some(link.id)).await?;
        // Renaming back to one of our own aliases: drop that alias.
        if link.aliases.contains(&new_slug) {
            stmts.push(
                d.prepare("DELETE FROM aliases WHERE slug = ?1")
                    .bind(&[s(&new_slug)])?,
            );
            link.aliases.retain(|a| *a != new_slug);
        }
        // The old slug keeps working as an alias.
        stmts.push(
            d.prepare("INSERT OR IGNORE INTO aliases (slug, link_id) VALUES (?1, ?2)")
                .bind(&[s(&old_slug), n(link.id)])?,
        );
        link.aliases.push(old_slug);
        link.aliases.sort();
        link.slug = new_slug;
    }
    link.updated_at = now_ms();
    stmts.insert(0, update_stmt(&d, link)?);
    Ok(stmts)
}

pub async fn update_loaded(env: &Env, mut link: Link, input: &LinkInput) -> SResult<Link> {
    let stmts = prepare_update(env, &mut link, input).await?;
    db(env)?.batch(stmts).await?;
    sync_kv(env, &link).await?;
    Ok(link.with_status())
}

pub async fn update_link(env: &Env, id: i64, input: &LinkInput) -> SResult<Link> {
    let link = get_link(env, id).await?.ok_or(StoreError::NotFound)?;
    update_loaded(env, link, input).await
}

/// Deletes links (with their aliases and clicks) and their KV records.
pub async fn delete_links(env: &Env, links: &[Link]) -> SResult<()> {
    let d = db(env)?;
    let mut stmts = vec![];
    for chunk in links.chunks(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|l| n(l.id)).collect();
        // Links before aliases: the alias tombstone trigger reads "link already gone" as
        // "deleted with the link" (a restore brings those aliases back).
        for table in [
            "clicks WHERE link_id",
            "links WHERE id",
            "aliases WHERE link_id",
        ] {
            stmts.push(
                d.prepare(format!("DELETE FROM {table} IN ({marks})"))
                    .bind(&ids)?,
            );
        }
    }
    d.batch(stmts).await?;
    let slugs: Vec<&str> = links
        .iter()
        .flat_map(|l| std::iter::once(l.slug.as_str()).chain(l.aliases.iter().map(String::as_str)))
        .collect();
    retire_kv(env, &slugs).await
}

pub async fn delete_link(env: &Env, id: i64) -> SResult<()> {
    let link = get_link(env, id).await?.ok_or(StoreError::NotFound)?;
    delete_links(env, &[link]).await
}

pub async fn delete_alias(env: &Env, id: i64, alias: &str) -> SResult<()> {
    let res = db(env)?
        .prepare("DELETE FROM aliases WHERE slug = ?1 AND link_id = ?2")
        .bind(&[s(alias), n(id)])?
        .run()
        .await?;
    if res.meta()?.and_then(|m| m.changes).unwrap_or(0) == 0 {
        return Err(StoreError::NotFound);
    }
    retire_kv(env, &[alias]).await
}

// ---- Retired slugs (see migrations/0002_retired_slugs.sql) ----

/// A retired slug; alias rows show their link's destination from its primary row.
#[derive(Serialize, Debug)]
pub struct Retired {
    pub slug: String,
    pub link_id: i64,
    pub was_alias: bool,
    pub url: Option<String>,
    pub title: Option<String>,
    pub created_by: Option<String>,
    pub retired_at: i64,
}

#[derive(Deserialize)]
struct RetiredRow {
    slug: String,
    link_id: i64,
    was_alias: i64,
    url: Option<String>,
    title: Option<String>,
    created_by: Option<String>,
    retired_at: i64,
}

impl From<RetiredRow> for Retired {
    fn from(r: RetiredRow) -> Self {
        Retired {
            slug: r.slug,
            link_id: r.link_id,
            was_alias: r.was_alias != 0,
            url: r.url,
            title: r.title,
            created_by: r.created_by,
            retired_at: r.retired_at,
        }
    }
}

/// Retired slugs of deleted links, newest first, optionally filtered by slug or URL.
pub async fn list_retired(
    env: &Env,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> worker::Result<(Vec<Retired>, i64)> {
    let mut wh = "r.link_id NOT IN (SELECT id FROM links)".to_string();
    let mut binds = vec![];
    if let Some(q) = q.map(str::trim).filter(|q| !q.is_empty()) {
        binds.push(s(&format!("%{q}%")));
        wh.push_str(" AND (r.slug LIKE ?1 OR p.url LIKE ?1)");
    }
    let from = format!(
        "FROM retired_slugs r LEFT JOIN retired_slugs p ON p.link_id = r.link_id AND p.was_alias = 0 WHERE {wh}"
    );
    let d = db(env)?;
    let count = d
        .prepare(format!("SELECT COUNT(*) AS n {from}"))
        .bind(&binds)?;
    let i = binds.len();
    binds.push(n(limit));
    binds.push(n(offset));
    let page = d
        .prepare(format!(
            "SELECT r.slug, r.link_id, r.was_alias, p.url, p.title, p.created_by, r.retired_at {from} \
             ORDER BY r.retired_at DESC, r.link_id DESC, r.was_alias, r.slug LIMIT ?{} OFFSET ?{}",
            i + 1,
            i + 2
        ))
        .bind(&binds)?;
    let res = d.batch(vec![count, page]).await?;
    let total = res[0]
        .results::<CountRow>()?
        .first()
        .map(|r| r.n)
        .unwrap_or(0);
    let rows = res[1].results::<RetiredRow>()?;
    Ok((rows.into_iter().map(Retired::from).collect(), total))
}

/// Rebuilds a deleted link from its primary tombstone. Every `links` column except the
/// click counters must appear here and in the snapshot of the `retire_link_slug` trigger
/// (a unit test checks both against `LINK_COLS`). A no-op when the link is already live,
/// so a second, racing restore succeeds instead of failing on the `links.id` key.
const RESTORE_LINK_SQL: &str = "INSERT INTO links (id, slug, url, title, created_by, created_at, updated_at, \
     notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, \
     utm, geo, device, source, clicks, last_click_at) \
     SELECT link_id, slug, url, title, created_by, COALESCE(created_at, ?2), ?2, \
     json_extract(snapshot, '$.notes'), COALESCE(json_extract(snapshot, '$.tags'), '[]'), \
     COALESCE(json_extract(snapshot, '$.redirect_type'), 302), COALESCE(json_extract(snapshot, '$.disabled'), 0), \
     json_extract(snapshot, '$.expires_at'), json_extract(snapshot, '$.max_clicks'), \
     json_extract(snapshot, '$.fallback_url'), json_extract(snapshot, '$.password_hash'), \
     COALESCE(json_extract(snapshot, '$.passthrough'), 1), json_extract(snapshot, '$.utm'), \
     json_extract(snapshot, '$.geo'), json_extract(snapshot, '$.device'), \
     COALESCE(json_extract(snapshot, '$.source'), 'manual'), COALESCE(json_extract(snapshot, '$.clicks'), 0), \
     json_extract(snapshot, '$.last_click_at') \
     FROM retired_slugs WHERE link_id = ?1 AND was_alias = 0 \
     AND NOT EXISTS (SELECT 1 FROM links WHERE id = ?1) ORDER BY retired_at DESC LIMIT 1";

/// Aliases that went away with the link come back, unless their slug is live again
/// (removed-on-their-own aliases stay removed; other links' slugs are guarded by triggers).
const RESTORE_ALIASES_SQL: &str = "INSERT OR IGNORE INTO aliases (slug, link_id) \
     SELECT slug, link_id FROM retired_slugs r WHERE link_id = ?1 AND was_alias = 1 AND with_link = 1 \
     AND NOT EXISTS (SELECT 1 FROM links WHERE slug = r.slug)";

/// Brings a deleted link back under its original id, slug, aliases, destination and
/// settings (the click total is kept, click details are not). Only the admin who created it may do this; links
/// created before creators were recorded can be restored by any admin, since the
/// destination is the original one either way.
pub async fn restore_link(env: &Env, id: i64, who: &str) -> SResult<Link> {
    #[derive(Deserialize)]
    struct Row {
        created_by: Option<String>,
        live: i64,
    }
    let d = db(env)?;
    let row = d
        .prepare(
            "SELECT created_by, EXISTS (SELECT 1 FROM links WHERE id = ?1) AS live \
             FROM retired_slugs WHERE link_id = ?1 AND was_alias = 0 ORDER BY retired_at DESC LIMIT 1",
        )
        .bind(&[n(id)])?
        .first::<Row>(None)
        .await?
        .ok_or(StoreError::NotFound)?;
    if row.live != 0 {
        return Err(StoreError::Conflict("this link is not deleted".into()));
    }
    if let Some(owner) = &row.created_by {
        if !owner.eq_ignore_ascii_case(who) {
            return Err(StoreError::Forbidden(format!(
                "only {owner}, who created this link, can restore it"
            )));
        }
    }
    d.batch(vec![
        d.prepare(RESTORE_LINK_SQL).bind(&[n(id), n(now_ms())])?,
        d.prepare(RESTORE_ALIASES_SQL).bind(&[n(id)])?,
    ])
    .await?;
    let link = get_link(env, id).await?.ok_or(StoreError::NotFound)?;
    sync_kv(env, &link).await?;
    Ok(link.with_status())
}

// ---- KV sync (the only place KV is written for links) ----

/// Writes the KV record for a link's slug and all its aliases, concurrently.
pub async fn sync_kv(env: &Env, link: &Link) -> SResult<()> {
    let rec = serde_json::to_string(&KvRecord::from_link(link)).unwrap_or_default();
    let store = kv(env)?;
    let keys: Vec<String> = std::iter::once(&link.slug)
        .chain(&link.aliases)
        .map(|s| kv_key(s))
        .collect();
    let mut puts = vec![];
    for k in &keys {
        puts.push(store.put(k, rec.clone())?.execute());
    }
    for r in join_all(puts).await {
        r?;
    }
    Ok(())
}

/// Replaces the KV records of retired slugs with permanent "gone" records, so the
/// redirect path answers 410 from the edge without reading D1 (a restore overwrites them).
pub async fn retire_kv(env: &Env, slugs: &[&str]) -> SResult<()> {
    let store = kv(env)?;
    let rec = serde_json::to_string(&KvRecord::gone()).unwrap_or_default();
    let mut puts = vec![];
    for sl in slugs {
        puts.push(store.put(&kv_key(sl), rec.clone())?.execute());
    }
    for r in join_all(puts).await {
        r?;
    }
    Ok(())
}

/// Redirect-path lookup: KV first; on a miss, D1 (the link and, failing that, a retired
/// slug, in one round trip). The bool says the KV record should be filled (the caller
/// does it after responding). A retired slug comes back as `KvRecord::gone()`.
pub async fn lookup(env: &Env, slug: &str) -> worker::Result<Option<(KvRecord, bool)>> {
    if let Ok(Some(rec)) = kv(env)?
        .get(&kv_key(slug))
        .cache_ttl(KV_CACHE_TTL)
        .json::<KvRecord>()
        .await
    {
        return Ok(Some((rec, false)));
    }
    let d = db(env)?;
    let res = d
        .batch(vec![
            d.prepare(format!(
                "SELECT {LINK_COLS} FROM links WHERE slug = ?1 OR id = (SELECT link_id FROM aliases WHERE slug = ?1) LIMIT 1"
            ))
            .bind(&[s(slug)])?,
            d.prepare("SELECT link_id AS id FROM retired_slugs WHERE slug = ?1 LIMIT 1")
                .bind(&[s(slug)])?,
        ])
        .await?;
    if let Some(row) = res[0].results::<LinkRow>()?.pop() {
        return Ok(Some((KvRecord::from_link(&Link::from(row)), true)));
    }
    Ok((!res[1].results::<IdRow>()?.is_empty()).then(|| (KvRecord::gone(), true)))
}

/// Read-through fills expire: a fill racing an admin update/delete would otherwise write
/// a stale record (or resurrect a deleted link) in KV permanently.
pub const KV_FILL_TTL: u64 = 3600;

pub async fn fill_kv(env: &Env, slug: &str, rec: &KvRecord) -> SResult<()> {
    let json = serde_json::to_string(rec).unwrap_or_default();
    kv(env)?
        .put(&kv_key(slug), json)?
        .expiration_ttl(KV_FILL_TTL)
        .execute()
        .await?;
    Ok(())
}

pub async fn click_count(env: &Env, id: i64) -> worker::Result<i64> {
    #[derive(Deserialize)]
    struct Row {
        clicks: i64,
    }
    Ok(db(env)?
        .prepare("SELECT clicks FROM links WHERE id = ?1")
        .bind(&[n(id)])?
        .first::<Row>(None)
        .await?
        .map(|r| r.clicks)
        .unwrap_or(0))
}

pub struct Click<'a> {
    pub link_id: i64,
    pub ts: i64,
    pub country: Option<String>,
    pub city: Option<String>,
    pub referrer: Option<String>,
    pub device: &'a str,
    pub os: &'a str,
    pub browser: &'a str,
    pub bot: bool,
    pub qr: bool,
}

/// Deletes click events older than the retention setting, in bounded chunks.
/// Returns the number of rows deleted.
pub async fn purge_old_clicks(env: &Env) -> worker::Result<usize> {
    let days = settings_strict(env).await?.click_retention_days;
    if days == 0 {
        return Ok(0);
    }
    let cutoff = crate::util::now_ms() - i64::from(days) * 86_400_000;
    let d = db(env)?;
    let mut total = 0;
    const CHUNK: usize = 5000;
    // A few bounded chunks per run keep each statement short; the next run continues.
    for _ in 0..20 {
        let res = d
            .prepare(
                "DELETE FROM clicks WHERE id IN (SELECT id FROM clicks WHERE ts < ?1 LIMIT ?2)",
            )
            .bind(&[n(cutoff), n(CHUNK as i64)])?
            .run()
            .await?;
        let changed = res.meta()?.and_then(|m| m.changes).unwrap_or(0);
        total += changed;
        if changed < CHUNK {
            break;
        }
    }
    Ok(total)
}

pub async fn record_click(env: &Env, c: Click<'_>) -> worker::Result<()> {
    let d = db(env)?;
    let mut stmts = vec![d
        .prepare("INSERT INTO clicks (link_id, ts, country, city, referrer, device, os, browser, bot, qr) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)")
        .bind(&[
            n(c.link_id),
            n(c.ts),
            os(&c.country),
            os(&c.city),
            os(&c.referrer),
            s(c.device),
            s(c.os),
            s(c.browser),
            b(c.bot),
            b(c.qr),
        ])?];
    if !c.bot {
        stmts.push(
            d.prepare("UPDATE links SET clicks = clicks + 1, last_click_at = ?2 WHERE id = ?1")
                .bind(&[n(c.link_id), n(c.ts)])?,
        );
    }
    d.batch(stmts).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column added to `links` but forgotten in the restore path would silently drop
    /// that setting (a password or an expiry) when a deleted link is restored.
    #[test]
    fn restore_covers_every_link_column() {
        let migration = include_str!("../migrations/0002_retired_slugs.sql");
        let insert_cols = RESTORE_LINK_SQL
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(cols, _)| cols)
            .unwrap();
        let kept = ["slug", "url", "title", "created_by", "created_at"];
        for col in LINK_COLS.split(',').map(str::trim) {
            if ["id", "updated_at"].contains(&col) {
                continue;
            }
            assert!(
                insert_cols.split(',').any(|c| c.trim() == col),
                "restore INSERT misses {col}"
            );
            if !kept.contains(&col) {
                assert!(
                    RESTORE_LINK_SQL.contains(&format!("'$.{col}'")),
                    "restore misses {col}"
                );
                assert!(
                    migration.contains(&format!("'{col}', OLD.{col}")),
                    "snapshot misses {col}"
                );
            }
        }
    }
}
