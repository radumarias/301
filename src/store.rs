//! Persistence: D1 is the source of truth, KV is a read-through cache for redirects.

use crate::model::{validate_link, KvRecord, Link, LinkInput, LinkRow};
use crate::slug::{self, Style};
use crate::util::now_ms;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use worker::{D1Database, Env, KvStore};

pub const KV_CACHE_TTL: u64 = 60;

#[derive(Debug)]
pub enum StoreError {
    Bad(String),
    NotFound,
    Conflict(String),
    Internal(worker::Error),
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            root_url: None,
            not_found_url: None,
            default_redirect: 302,
            default_slug_style: "random".into(),
            default_slug_length: 7,
        }
    }
}

/// Settings for the redirect path: KV first (edge-cached), D1 as fallback.
pub async fn settings_cached(env: &Env) -> Settings {
    if let Ok(store) = kv(env) {
        if let Ok(Some(s)) = store
            .get("cfg")
            .cache_ttl(KV_CACHE_TTL)
            .json::<Settings>()
            .await
        {
            return s;
        }
    }
    settings(env).await.unwrap_or_default()
}

pub async fn settings(env: &Env) -> worker::Result<Settings> {
    #[derive(Deserialize)]
    struct Row {
        value: String,
    }
    let row = db(env)?
        .prepare("SELECT value FROM settings WHERE key = 'settings'")
        .first::<Row>(None)
        .await?;
    Ok(row
        .and_then(|r| serde_json::from_str(&r.value).ok())
        .unwrap_or_default())
}

pub async fn save_settings(env: &Env, st: &Settings) -> SResult<()> {
    for u in [&st.root_url, &st.not_found_url].into_iter().flatten() {
        crate::model::validate_url(u).map_err(StoreError::Bad)?;
    }
    if ![301, 302, 307, 308].contains(&st.default_redirect) {
        return Err(StoreError::Bad(
            "default_redirect must be 301, 302, 307 or 308".into(),
        ));
    }
    let json = serde_json::to_string(st).unwrap_or_default();
    db(env)?
        .prepare("INSERT INTO settings (key, value) VALUES ('settings', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(&[s(&json)])?
        .run()
        .await?;
    kv(env)?.put("cfg", json)?.execute().await?;
    Ok(())
}

// ---- Links ----

const LINK_COLS: &str = "id, slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, clicks, last_click_at, created_at, updated_at";

pub async fn aliases_of(env: &Env, id: i64) -> worker::Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Row {
        slug: String,
    }
    let rows = db(env)?
        .prepare("SELECT slug FROM aliases WHERE link_id = ?1 ORDER BY slug")
        .bind(&[n(id)])?
        .all()
        .await?
        .results::<Row>()?;
    Ok(rows.into_iter().map(|r| r.slug).collect())
}

pub async fn get_link(env: &Env, id: i64) -> worker::Result<Option<Link>> {
    let row = db(env)?
        .prepare(format!("SELECT {LINK_COLS} FROM links WHERE id = ?1"))
        .bind(&[n(id)])?
        .first::<LinkRow>(None)
        .await?;
    match row {
        Some(r) => {
            let mut l = Link::from(r);
            l.aliases = aliases_of(env, l.id).await?;
            Ok(Some(l))
        }
        None => Ok(None),
    }
}

/// Finds a link by its slug or one of its aliases.
pub async fn get_link_by_slug(env: &Env, slug: &str) -> worker::Result<Option<Link>> {
    let row = db(env)?
        .prepare(format!(
            "SELECT {LINK_COLS} FROM links WHERE slug = ?1 \
             UNION ALL SELECT {cols} FROM links l JOIN aliases a ON a.link_id = l.id WHERE a.slug = ?1 LIMIT 1",
            cols = LINK_COLS.split(", ").map(|c| format!("l.{c}")).collect::<Vec<_>>().join(", ")
        ))
        .bind(&[s(slug)])?
        .first::<LinkRow>(None)
        .await?;
    Ok(row.map(Link::from))
}

/// Who owns a slug: Some(link_id) when taken (as slug or alias).
pub async fn slug_owner(env: &Env, slug: &str) -> worker::Result<Option<i64>> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
    }
    let row = db(env)?
        .prepare("SELECT id FROM links WHERE slug = ?1 UNION ALL SELECT link_id AS id FROM aliases WHERE slug = ?1 LIMIT 1")
        .bind(&[s(slug)])?
        .first::<Row>(None)
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

async fn apply_password(link: &mut Link, input: &LinkInput) -> SResult<()> {
    if let Some(p) = &input.password {
        match p.as_deref().map(str::trim) {
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

pub async fn create_link(env: &Env, input: &LinkInput) -> SResult<Link> {
    let st = settings(env).await.unwrap_or_default();
    let mut link = Link {
        redirect_type: st.default_redirect,
        passthrough: true,
        source: "manual".into(),
        ..Default::default()
    };
    input.apply(&mut link).map_err(StoreError::Bad)?;
    apply_password(&mut link, input).await?;

    link.slug = match input
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sl) => {
            slug::validate(sl).map_err(StoreError::Bad)?;
            if slug_owner(env, sl).await?.is_some() {
                return Err(StoreError::Conflict(format!(
                    "slug '{sl}' is already taken"
                )));
            }
            sl.to_string()
        }
        None => {
            let style = Style::parse(
                input
                    .slug_style
                    .as_deref()
                    .unwrap_or(&st.default_slug_style),
            );
            unique_slug(env, style, st.default_slug_length).await?
        }
    };
    let now = now_ms();
    link.created_at = now;
    link.updated_at = now;
    link.id = insert_link(env, &link).await?;
    write_kv(env, &link).await?;
    Ok(link)
}

fn insert_stmt(db: &D1Database, l: &Link) -> worker::Result<worker::D1PreparedStatement> {
    db.prepare(
        "INSERT INTO links (slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18) RETURNING id",
    )
    .bind(&[
        s(&l.slug),
        s(&l.url),
        os(&l.title),
        os(&l.notes),
        s(&serde_json::to_string(&l.tags).unwrap_or_else(|_| "[]".into())),
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
        n(l.created_at),
        n(l.updated_at),
    ])
}

pub fn insert_statement(db: &D1Database, l: &Link) -> worker::Result<worker::D1PreparedStatement> {
    insert_stmt(db, l)
}

async fn insert_link(env: &Env, l: &Link) -> SResult<i64> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
    }
    let d = db(env)?;
    let row = insert_stmt(&d, l)?.first::<Row>(None).await?;
    row.map(|r| r.id).ok_or_else(|| {
        StoreError::Internal(worker::Error::RustError("insert returned no id".into()))
    })
}

pub fn update_statement(db: &D1Database, l: &Link) -> worker::Result<worker::D1PreparedStatement> {
    db.prepare(
        "UPDATE links SET slug = ?2, url = ?3, title = ?4, notes = ?5, tags = ?6, redirect_type = ?7, disabled = ?8, expires_at = ?9, \
         max_clicks = ?10, fallback_url = ?11, password_hash = ?12, passthrough = ?13, utm = ?14, geo = ?15, device = ?16, source = ?17, updated_at = ?18 \
         WHERE id = ?1",
    )
    .bind(&[
        n(l.id),
        s(&l.slug),
        s(&l.url),
        os(&l.title),
        os(&l.notes),
        s(&serde_json::to_string(&l.tags).unwrap_or_else(|_| "[]".into())),
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
    ])
}

pub async fn update_link(env: &Env, id: i64, input: &LinkInput) -> SResult<Link> {
    let mut link = get_link(env, id).await?.ok_or(StoreError::NotFound)?;
    let old_slug = link.slug.clone();
    input.apply(&mut link).map_err(StoreError::Bad)?;
    apply_password(&mut link, input).await?;
    let d = db(env)?;
    let mut stmts = vec![];

    if let Some(new_slug) = input
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != old_slug)
    {
        slug::validate(new_slug).map_err(StoreError::Bad)?;
        match slug_owner(env, new_slug).await? {
            Some(owner) if owner != id => {
                return Err(StoreError::Conflict(format!(
                    "slug '{new_slug}' is already taken"
                )));
            }
            Some(_) => {
                // Renaming back to one of our own aliases: drop that alias.
                stmts.push(
                    d.prepare("DELETE FROM aliases WHERE slug = ?1")
                        .bind(&[s(new_slug)])?,
                );
                link.aliases.retain(|a| a != new_slug);
            }
            None => {}
        }
        // The old slug keeps working as an alias.
        stmts.push(
            d.prepare("INSERT OR IGNORE INTO aliases (slug, link_id) VALUES (?1, ?2)")
                .bind(&[s(&old_slug), n(id)])?,
        );
        link.aliases.push(old_slug.clone());
        link.aliases.sort();
        link.slug = new_slug.to_string();
    }
    validate_link(&link).map_err(StoreError::Bad)?;
    link.updated_at = now_ms();
    stmts.insert(0, update_statement(&d, &link)?);
    d.batch(stmts).await?;
    write_kv(env, &link).await?;
    Ok(link)
}

pub async fn delete_link(env: &Env, id: i64) -> SResult<()> {
    let link = get_link(env, id).await?.ok_or(StoreError::NotFound)?;
    let d = db(env)?;
    d.batch(vec![
        d.prepare("DELETE FROM clicks WHERE link_id = ?1")
            .bind(&[n(id)])?,
        d.prepare("DELETE FROM aliases WHERE link_id = ?1")
            .bind(&[n(id)])?,
        d.prepare("DELETE FROM links WHERE id = ?1")
            .bind(&[n(id)])?,
    ])
    .await?;
    let store = kv(env)?;
    store.delete(&kv_key(&link.slug)).await?;
    for a in &link.aliases {
        store.delete(&kv_key(a)).await?;
    }
    Ok(())
}

pub async fn delete_alias(env: &Env, id: i64, alias: &str) -> SResult<()> {
    let res = db(env)?
        .prepare("DELETE FROM aliases WHERE slug = ?1 AND link_id = ?2")
        .bind(&[s(alias), n(id)])?
        .run()
        .await?;
    let changed = res.meta()?.and_then(|m| m.changes).unwrap_or(0);
    if changed == 0 {
        return Err(StoreError::NotFound);
    }
    kv(env)?.delete(&kv_key(alias)).await?;
    Ok(())
}

/// Writes the KV record for a link's slug and all its aliases.
pub async fn write_kv(env: &Env, link: &Link) -> SResult<()> {
    let rec = serde_json::to_string(&KvRecord::from_link(link)).unwrap_or_default();
    let store = kv(env)?;
    store
        .put(&kv_key(&link.slug), rec.clone())?
        .execute()
        .await?;
    for a in &link.aliases {
        store.put(&kv_key(a), rec.clone())?.execute().await?;
    }
    Ok(())
}

/// Redirect-path lookup: KV, then D1 (read-through fill).
pub async fn lookup(env: &Env, slug: &str) -> worker::Result<Option<KvRecord>> {
    let store = kv(env)?;
    let key = kv_key(slug);
    if let Ok(Some(rec)) = store
        .get(&key)
        .cache_ttl(KV_CACHE_TTL)
        .json::<KvRecord>()
        .await
    {
        return Ok(Some(rec));
    }
    let Some(link) = get_link_by_slug(env, slug).await? else {
        return Ok(None);
    };
    let rec = KvRecord::from_link(&link);
    if let Ok(json) = serde_json::to_string(&rec) {
        if let Ok(put) = store.put(&key, json) {
            let _ = put.execute().await;
        }
    }
    Ok(Some(rec))
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
