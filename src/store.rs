//! Persistence: D1 is the source of truth, KV is a read-through cache for redirects.
//!
//! Every link mutation goes through this module, which keeps KV in sync in one place
//! (`sync_kv` / `forget_kv`).

use crate::model::{validate_redirect_type, validate_url, KvRecord, Link, LinkInput, LinkRow};
use crate::slug::{self, Style};
use crate::util::now_ms;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, Env, KvStore};

pub const KV_CACHE_TTL: u64 = 60;

pub const LINK_COLS: &str = "id, slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, clicks, last_click_at, created_at, updated_at, \
     owner_id, manage_hash, creator_ip, blocked";

/// Extra column for admin views: the owner's email (append to `LINK_COLS`).
pub const OWNER_EMAIL_COL: &str =
    "(SELECT email FROM users WHERE users.id = links.owner_id) AS owner_email";

#[derive(Debug)]
pub enum StoreError {
    Bad(String),
    NotFound,
    Conflict(String),
    Internal(worker::Error),
}

impl StoreError {
    /// Message safe to show to the admin (internal details are logged, not returned).
    pub fn public_message(&self) -> String {
        match self {
            StoreError::Bad(m) | StoreError::Conflict(m) => m.clone(),
            StoreError::NotFound => "not found".into(),
            StoreError::Internal(e) => {
                worker::console_error!("internal: {e}");
                "internal error".into()
            }
        }
    }
}

impl From<worker::Error> for StoreError {
    fn from(e: worker::Error) -> Self {
        StoreError::Internal(e)
    }
}

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
pub fn b(v: bool) -> JsValue {
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
    /// Anyone can create links without signing in (also needs Turnstile configured).
    pub anon_enabled: bool,
    /// Anonymous links expire after this many days.
    pub anon_ttl_days: i64,
    /// Anonymous links one IP may create per hour.
    pub anon_per_hour: i64,
    /// Links a signed-in (non-admin) user may create per day.
    pub user_per_day: i64,
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
            anon_enabled: true,
            anon_ttl_days: 30,
            anon_per_hour: 10,
            user_per_day: 100,
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
        if !(1..=365).contains(&self.anon_ttl_days) {
            return Err("anonymous links must expire after 1 to 365 days".into());
        }
        if !(0..=1000).contains(&self.anon_per_hour) || !(0..=10_000).contains(&self.user_per_day) {
            return Err("creation limits must be between 0 and 1000 (anonymous, per hour) and 0 and 10000 (per user, per day)".into());
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

/// Finds a link by its slug or one of its aliases (aliases included).
pub async fn get_link_by_slug(env: &Env, slug: &str) -> worker::Result<Option<Link>> {
    let links = query_links(
        env,
        "WHERE slug = ?1 OR id = (SELECT link_id FROM aliases WHERE slug = ?1) LIMIT 1",
        &[s(slug)],
    )
    .await?;
    Ok(with_aliases(env, links).await?.pop())
}

/// Who owns a slug: Some(link_id) when taken (as slug or alias).
pub async fn slug_owner(env: &Env, slug: &str) -> worker::Result<Option<i64>> {
    let row = db(env)?
        .prepare("SELECT id FROM links WHERE slug = ?1 UNION ALL SELECT link_id AS id FROM aliases WHERE slug = ?1 LIMIT 1")
        .bind(&[s(slug)])?
        .first::<IdRow>(None)
        .await?;
    Ok(row.map(|r| r.id))
}

pub async fn unique_slug(env: &Env, style: Style, length: usize) -> SResult<String> {
    for attempt in 0..12 {
        let candidate = slug::generate(style, length, attempt);
        if slug_owner(env, &candidate).await?.is_none() {
            return Ok(candidate);
        }
    }
    Err(StoreError::Conflict(
        "could not find a free slug, try again".into(),
    ))
}

/// Validates a requested slug and checks it is free (or already owned by `owner`).
pub async fn claim_slug(env: &Env, requested: &str, owner: Option<i64>) -> SResult<String> {
    let sl = slug::normalize(requested).ok_or_else(|| StoreError::Bad("slug is empty".into()))?;
    slug::validate(&sl).map_err(StoreError::Bad)?;
    match slug_owner(env, &sl).await? {
        Some(o) if Some(o) != owner => Err(StoreError::Conflict(format!(
            "slug '{sl}' is already taken"
        ))),
        _ => Ok(sl),
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
    binds.extend([
        n(l.created_at),
        on(l.owner_id),
        os(&l.manage_hash),
        os(&l.creator_ip),
        b(l.blocked),
    ]);
    db.prepare(
        "INSERT INTO links (slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, updated_at, \
         created_at, owner_id, manage_hash, creator_ip, blocked) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22) RETURNING id",
    )
    .bind(&binds)
}

/// Ownership columns are not written here: see `claim_*` in `account`. `blocked` is only
/// written for a moderation action (`block`), so an edit that loaded the link earlier can
/// never undo a block made in the meantime.
pub fn update_stmt(
    db: &D1Database,
    l: &Link,
    block: Option<bool>,
) -> worker::Result<D1PreparedStatement> {
    let mut binds = link_binds(l);
    binds.push(block.map(b).unwrap_or(JsValue::NULL));
    binds.push(n(l.id));
    db.prepare(
        "UPDATE links SET slug = ?1, url = ?2, title = ?3, notes = ?4, tags = ?5, redirect_type = ?6, disabled = ?7, expires_at = ?8, \
         max_clicks = ?9, fallback_url = ?10, password_hash = ?11, passthrough = ?12, utm = ?13, geo = ?14, device = ?15, source = ?16, updated_at = ?17, \
         blocked = COALESCE(?18, blocked) WHERE id = ?19",
    )
    .bind(&binds)
}

/// Builds a new (unsaved) link from an input, with defaults from settings.
pub async fn new_link(env: &Env, st: &Settings, input: &LinkInput, source: &str) -> SResult<Link> {
    let mut link = Link::new(st.default_redirect, source);
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

/// Saves a link built with `new_link`. With `write_kv` its KV record is written now;
/// otherwise the first click fills it (read-through), which costs no KV write for
/// links nobody clicks.
pub async fn insert_link(env: &Env, mut link: Link, write_kv: bool) -> SResult<Link> {
    let row = insert_stmt(&db(env)?, &link)?.first::<IdRow>(None).await?;
    link.id = row
        .ok_or_else(|| {
            StoreError::Internal(worker::Error::RustError("insert returned no id".into()))
        })?
        .id;
    if write_kv {
        sync_kv(env, &link).await?;
    }
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
    stmts.insert(0, update_stmt(&d, link, input.blocked)?);
    Ok(stmts)
}

/// Runs the statements from `prepare_update` and refreshes the link's KV records.
pub async fn save_update(
    env: &Env,
    mut link: Link,
    stmts: Vec<D1PreparedStatement>,
) -> SResult<Link> {
    db(env)?.batch(stmts).await?;
    refresh_blocked(env, std::slice::from_mut(&mut link)).await?;
    sync_kv(env, &link).await?;
    Ok(link.with_status())
}

/// Re-reads `blocked` after an update, so the KV record written next carries a block an
/// admin made while the edit was in progress.
pub async fn refresh_blocked(env: &Env, links: &mut [Link]) -> SResult<()> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
        blocked: i64,
    }
    let d = db(env)?;
    for chunk in links.chunks_mut(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|l| n(l.id)).collect();
        let rows = d
            .prepare(format!(
                "SELECT id, blocked FROM links WHERE id IN ({marks})"
            ))
            .bind(&ids)?
            .all()
            .await?
            .results::<Row>()?;
        let blocked: HashMap<i64, bool> =
            rows.into_iter().map(|r| (r.id, r.blocked != 0)).collect();
        for l in chunk {
            if let Some(&b) = blocked.get(&l.id) {
                l.blocked = b;
            }
        }
    }
    Ok(())
}

/// Owner and blocked flag of a link, without loading the whole row.
pub async fn link_access(env: &Env, id: i64) -> worker::Result<Option<(Option<i64>, bool)>> {
    #[derive(Deserialize)]
    struct Row {
        owner_id: Option<i64>,
        blocked: i64,
    }
    Ok(db(env)?
        .prepare("SELECT owner_id, blocked FROM links WHERE id = ?1")
        .bind(&[n(id)])?
        .first::<Row>(None)
        .await?
        .map(|r| (r.owner_id, r.blocked != 0)))
}

/// Deletes links (with their aliases and clicks) and their KV records.
pub async fn delete_links(env: &Env, links: &[Link]) -> SResult<()> {
    let d = db(env)?;
    let mut stmts = vec![];
    for chunk in links.chunks(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|l| n(l.id)).collect();
        for table in [
            "clicks WHERE link_id",
            "aliases WHERE link_id",
            "links WHERE id",
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
    forget_kv(env, &slugs).await
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
    forget_kv(env, &[alias]).await
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

/// Removes KV records so the next redirect refills them from D1.
pub async fn forget_kv(env: &Env, slugs: &[&str]) -> SResult<()> {
    let store = kv(env)?;
    let keys: Vec<String> = slugs.iter().map(|s| kv_key(s)).collect();
    for r in join_all(keys.iter().map(|k| store.delete(k))).await {
        r?;
    }
    Ok(())
}

/// Redirect-path lookup: KV first; on a miss, D1. The bool says the KV record should be
/// filled (the caller does it after responding).
pub async fn lookup(env: &Env, slug: &str) -> worker::Result<Option<(KvRecord, bool)>> {
    if let Ok(Some(rec)) = kv(env)?
        .get(&kv_key(slug))
        .cache_ttl(KV_CACHE_TTL)
        .json::<KvRecord>()
        .await
    {
        return Ok(Some((rec, false)));
    }
    Ok(get_link_by_slug(env, slug)
        .await?
        .map(|l| (KvRecord::from_link(&l), true)))
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
