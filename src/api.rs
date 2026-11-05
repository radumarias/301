//! Admin JSON API under `/api`. Every route requires a verified Access identity.

use crate::auth::{self, AuthError, Identity};
use crate::model::{Link, LinkInput, LinkRow};
use crate::slug::Style;
use crate::store::{self, n, s, SResult, StoreError};
use crate::util::{csv_field, json, json_error, now_ms};
use crate::{import, qr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response, Result};

fn store_err(e: StoreError) -> Result<Response> {
    match e {
        StoreError::Bad(m) => json_error(m, 400),
        StoreError::NotFound => json_error("not found", 404),
        StoreError::Conflict(m) => json_error(m, 409),
        StoreError::Internal(e) => {
            worker::console_error!("internal: {e}");
            json_error("internal error", 500)
        }
    }
}

fn respond<T: Serialize>(r: SResult<T>, status: u16) -> Result<Response> {
    match r {
        Ok(v) => json(&v, status),
        Err(e) => store_err(e),
    }
}

pub async fn identity(req: &Request, env: &Env) -> std::result::Result<Identity, Response> {
    match auth::verify(req, env).await {
        Ok(id) => Ok(id),
        Err(AuthError::NotConfigured(m)) => Err(json_error(m, 503).unwrap()),
        Err(AuthError::Denied(m)) => Err(json_error(m, 403).unwrap()),
    }
}

/// Rejects cross-site writes: browsers always send Origin on POST/PATCH/DELETE.
fn same_origin(req: &Request) -> bool {
    if matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        return true;
    }
    let Ok(url) = req.url() else { return false };
    match req.headers().get("Origin").ok().flatten() {
        Some(o) => o == url.origin().ascii_serialization(),
        None => true, // non-browser clients (service tokens)
    }
}

pub async fn handle(mut req: Request, env: Env) -> Result<Response> {
    let who = match identity(&req, &env).await {
        Ok(w) => w,
        Err(r) => return Ok(r),
    };
    if !same_origin(&req) {
        return json_error("cross-origin request refused", 403);
    }
    let url = req.url()?;
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let path = req.path();
    let segs: Vec<&str> = path
        .trim_start_matches("/api/")
        .trim_end_matches('/')
        .split('/')
        .collect();
    let method = req.method();

    match (method, segs.as_slice()) {
        (Method::Get, ["me"]) => json(&serde_json::json!({ "email": who.email }), 200),

        (Method::Get, ["links"]) => list_links(&env, &query).await,
        (Method::Post, ["links"]) => {
            let input: LinkInput = match req.json().await {
                Ok(i) => i,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            respond(store::create_link(&env, &input).await, 201)
        }
        (Method::Post, ["links", "bulk"]) => {
            let body: Bulk = match req.json().await {
                Ok(b) => b,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            bulk(&env, body).await
        }
        (Method::Get, ["links", id]) => match id.parse::<i64>() {
            Ok(id) => match store::get_link(&env, id).await? {
                Some(l) => json(&l, 200),
                None => json_error("not found", 404),
            },
            Err(_) => json_error("bad id", 400),
        },
        (Method::Patch, ["links", id]) | (Method::Put, ["links", id]) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            let input: LinkInput = match req.json().await {
                Ok(i) => i,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            respond(store::update_link(&env, id, &input).await, 200)
        }
        (Method::Delete, ["links", id]) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            respond(
                store::delete_link(&env, id)
                    .await
                    .map(|_| serde_json::json!({"ok": true})),
                200,
            )
        }
        (Method::Delete, ["links", id, "aliases", alias @ ..]) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            let alias = alias.join("/");
            respond(
                store::delete_alias(&env, id, &alias)
                    .await
                    .map(|_| serde_json::json!({"ok": true})),
                200,
            )
        }
        (Method::Get, ["links", id, "stats"]) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            stats(
                &env,
                Some(id),
                query.get("range").map(String::as_str).unwrap_or("30d"),
            )
            .await
        }
        (Method::Get, ["links", id, "qr.svg"]) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            let Some(link) = store::get_link(&env, id).await? else {
                return json_error("not found", 404);
            };
            let track = query.get("track").map(String::as_str) != Some("0");
            let data = format!(
                "{}/{}{}",
                url.origin().ascii_serialization(),
                link.slug,
                if track { "?qr=1" } else { "" }
            );
            let size = query
                .get("size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(512);
            match qr::svg(
                &data,
                query.get("fg").map(String::as_str),
                query.get("bg").map(String::as_str),
                query.get("ecc").map(String::as_str),
                size,
            ) {
                Ok(svg) => {
                    let mut res = Response::ok(svg)?;
                    res.headers_mut().set("Content-Type", "image/svg+xml")?;
                    res.headers_mut().set("Cache-Control", "no-store")?;
                    Ok(res)
                }
                Err(e) => json_error(e, 400),
            }
        }
        (Method::Get, ["slug"]) => {
            let st = store::settings(&env).await.unwrap_or_default();
            let style = Style::parse(
                query
                    .get("style")
                    .map(String::as_str)
                    .unwrap_or(&st.default_slug_style),
            );
            let len = query
                .get("length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(st.default_slug_length);
            respond(
                store::unique_slug(&env, style, len)
                    .await
                    .map(|s| serde_json::json!({ "slug": s })),
                200,
            )
        }
        (Method::Get, ["slug", "check"]) => {
            let slug = query.get("slug").cloned().unwrap_or_default();
            let valid = crate::slug::validate(&slug);
            let taken = store::slug_owner(&env, &slug).await?;
            json(
                &serde_json::json!({ "valid": valid.is_ok(), "reason": valid.err(), "taken_by": taken }),
                200,
            )
        }
        (Method::Get, ["stats"]) => {
            stats(
                &env,
                None,
                query.get("range").map(String::as_str).unwrap_or("30d"),
            )
            .await
        }
        (Method::Get, ["tags"]) => tags(&env).await,
        (Method::Get, ["settings"]) => json(&store::settings(&env).await?, 200),
        (Method::Put, ["settings"]) => {
            let st: store::Settings = match req.json().await {
                Ok(s) => s,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            respond(store::save_settings(&env, &st).await.map(|_| st), 200)
        }
        (Method::Post, ["import", "fetch"]) => {
            let body: import::FetchRequest = match req.json().await {
                Ok(b) => b,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            match import::fetch_page(&body).await {
                Ok(page) => json(&page, 200),
                Err(e) => json_error(e, 502),
            }
        }
        (Method::Post, ["import", "commit"]) => {
            let body: import::CommitRequest = match req.json().await {
                Ok(b) => b,
                Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
            };
            respond(import::commit(&env, body).await, 200)
        }
        (Method::Get, ["export"]) => {
            export(
                &env,
                query.get("format").map(String::as_str).unwrap_or("csv"),
            )
            .await
        }
        (Method::Post, ["maintenance", "rebuild-kv"]) => {
            let offset = query
                .get("offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0i64);
            rebuild_kv(&env, offset).await
        }
        _ => json_error("no such endpoint", 404),
    }
}

// ---- Listing ----

#[derive(Serialize)]
struct Page<T> {
    items: Vec<T>,
    total: i64,
}

#[derive(Deserialize)]
struct Count {
    n: i64,
}

async fn list_links(env: &Env, q: &HashMap<String, String>) -> Result<Response> {
    let mut wheres: Vec<&str> = vec![];
    let mut binds: Vec<JsValue> = vec![];
    if let Some(text) = q.get("q").map(|t| t.trim()).filter(|t| !t.is_empty()) {
        wheres.push("(slug LIKE ? OR url LIKE ? OR title LIKE ? OR notes LIKE ?)");
        let like = format!("%{text}%");
        for _ in 0..4 {
            binds.push(s(&like));
        }
    }
    if let Some(tag) = q.get("tag").filter(|t| !t.is_empty()) {
        wheres.push("EXISTS (SELECT 1 FROM json_each(links.tags) WHERE value = ?)");
        binds.push(s(tag));
    }
    let now = now_ms();
    match q.get("status").map(String::as_str) {
        Some("disabled") => wheres.push("disabled = 1"),
        Some("expired") => {
            wheres.push("(expires_at IS NOT NULL AND expires_at <= ?)");
            binds.push(n(now));
        }
        Some("active") => {
            wheres.push("disabled = 0 AND (expires_at IS NULL OR expires_at > ?)");
            binds.push(n(now));
        }
        Some("protected") => wheres.push("password_hash IS NOT NULL"),
        _ => {}
    }
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wheres.join(" AND "))
    };
    let sort = match q.get("sort").map(String::as_str) {
        Some("clicks") => "clicks",
        Some("slug") => "slug",
        Some("updated") => "updated_at",
        Some("last_click") => "last_click_at",
        _ => "created_at",
    };
    let order = if q.get("order").map(String::as_str) == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let offset = q
        .get("offset")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let d = store::db(env)?;
    let total = d
        .prepare(format!("SELECT COUNT(*) AS n FROM links {where_sql}"))
        .bind(&binds)?
        .first::<Count>(None)
        .await?
        .map(|c| c.n)
        .unwrap_or(0);
    let mut page_binds = binds.clone();
    page_binds.push(n(limit));
    page_binds.push(n(offset));
    let rows = d
        .prepare(format!(
            "SELECT id, slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, clicks, last_click_at, created_at, updated_at \
             FROM links {where_sql} ORDER BY {sort} {order}, id DESC LIMIT ? OFFSET ?"
        ))
        .bind(&page_binds)?
        .all()
        .await?
        .results::<LinkRow>()?;
    let items: Vec<Link> = rows.into_iter().map(Link::from).collect();
    json(&Page { items, total }, 200)
}

async fn tags(env: &Env) -> Result<Response> {
    #[derive(Deserialize, Serialize)]
    struct Row {
        tag: String,
        n: i64,
    }
    let rows = store::db(env)?
        .prepare("SELECT j.value AS tag, COUNT(*) AS n FROM links, json_each(links.tags) j GROUP BY j.value ORDER BY n DESC, tag")
        .all()
        .await?
        .results::<Row>()?;
    json(&rows, 200)
}

// ---- Bulk ----

#[derive(Deserialize)]
struct Bulk {
    ids: Vec<i64>,
    action: String,
    #[serde(default)]
    tags: Vec<String>,
}

async fn bulk(env: &Env, body: Bulk) -> Result<Response> {
    if body.ids.len() > 200 {
        return json_error("at most 200 links per bulk action", 400);
    }
    let mut done = 0;
    let mut errors = vec![];
    for id in body.ids {
        let r = match body.action.as_str() {
            "delete" => store::delete_link(env, id).await,
            "disable" | "enable" => {
                let input = LinkInput {
                    disabled: Some(body.action == "disable"),
                    ..Default::default()
                };
                store::update_link(env, id, &input).await.map(|_| ())
            }
            "tag" | "untag" => match store::get_link(env, id).await {
                Ok(Some(l)) => {
                    let mut tags = l.tags.clone();
                    if body.action == "tag" {
                        tags.extend(body.tags.iter().cloned());
                    } else {
                        let rm: Vec<String> =
                            body.tags.iter().map(|t| t.trim().to_lowercase()).collect();
                        tags.retain(|t| !rm.contains(t));
                    }
                    let input = LinkInput {
                        tags: Some(tags),
                        ..Default::default()
                    };
                    store::update_link(env, id, &input).await.map(|_| ())
                }
                Ok(None) => Err(StoreError::NotFound),
                Err(e) => Err(e.into()),
            },
            _ => return json_error("action must be tag, untag, disable, enable or delete", 400),
        };
        match r {
            Ok(()) => done += 1,
            Err(e) => errors.push(serde_json::json!({ "id": id, "error": format!("{e:?}") })),
        }
    }
    json(&serde_json::json!({ "done": done, "errors": errors }), 200)
}

// ---- Stats ----

fn range(r: &str) -> (i64, i64) {
    const H: i64 = 3_600_000;
    const D: i64 = 24 * H;
    let now = now_ms();
    match r {
        "24h" => (now - D, H),
        "7d" => (now - 7 * D, D),
        "90d" => (now - 90 * D, D),
        "all" => (0, D),
        _ => (now - 30 * D, D),
    }
}

#[derive(Deserialize, Serialize)]
struct KV {
    k: Option<String>,
    n: i64,
}

#[derive(Deserialize, Serialize)]
struct Bucket {
    t: i64,
    n: i64,
}

async fn stats(env: &Env, link: Option<i64>, r: &str) -> Result<Response> {
    let (since, bucket) = range(r);
    let d = store::db(env)?;
    let (filter, mut binds): (&str, Vec<JsValue>) = match link {
        Some(id) => ("link_id = ? AND ts >= ?", vec![n(id), n(since)]),
        None => ("ts >= ?", vec![n(since)]),
    };
    let base = binds.clone();

    #[derive(Deserialize)]
    struct Totals {
        humans: Option<i64>,
        bots: Option<i64>,
        qr: Option<i64>,
    }
    let totals = d
        .prepare(format!(
            "SELECT SUM(CASE WHEN bot = 0 THEN 1 ELSE 0 END) AS humans, SUM(bot) AS bots, SUM(CASE WHEN bot = 0 THEN qr ELSE 0 END) AS qr FROM clicks WHERE {filter}"
        ))
        .bind(&base)?
        .first::<Totals>(None)
        .await?;

    binds.insert(0, n(bucket));
    let series = d
        .prepare(format!(
            "SELECT CAST(ts / ?1 AS INTEGER) * ?1 AS t, COUNT(*) AS n FROM clicks WHERE {} AND bot = 0 GROUP BY t ORDER BY t",
            // Shift placeholders by one because ?1 is the bucket size.
            if link.is_some() { "link_id = ?2 AND ts >= ?3" } else { "ts >= ?2" }
        ))
        .bind(&binds)?
        .all()
        .await?
        .results::<Bucket>()?;

    let mut dims = serde_json::Map::new();
    for col in ["country", "referrer", "device", "os", "browser", "city"] {
        let rows = d
            .prepare(format!(
                "SELECT {col} AS k, COUNT(*) AS n FROM clicks WHERE {filter} AND bot = 0 GROUP BY {col} ORDER BY n DESC LIMIT 10"
            ))
            .bind(&base)?
            .all()
            .await?
            .results::<KV>()?;
        dims.insert(
            col.to_string(),
            serde_json::to_value(rows).unwrap_or_default(),
        );
    }

    let mut out = serde_json::json!({
        "range": r,
        "bucket_ms": bucket,
        "since": since,
        "clicks": totals.as_ref().and_then(|t| t.humans).unwrap_or(0),
        "bots": totals.as_ref().and_then(|t| t.bots).unwrap_or(0),
        "qr": totals.as_ref().and_then(|t| t.qr).unwrap_or(0),
        "series": series,
        "top": dims,
    });

    if link.is_none() {
        #[derive(Deserialize, Serialize)]
        struct TopLink {
            id: i64,
            slug: String,
            url: String,
            title: Option<String>,
            n: i64,
        }
        let top = d
            .prepare(
                "SELECT l.id, l.slug, l.url, l.title, COUNT(*) AS n FROM clicks c JOIN links l ON l.id = c.link_id \
                 WHERE c.ts >= ? AND c.bot = 0 GROUP BY l.id ORDER BY n DESC LIMIT 10",
            )
            .bind(&[n(since)])?
            .all()
            .await?
            .results::<TopLink>()?;
        let links = d
            .prepare("SELECT COUNT(*) AS n FROM links")
            .first::<Count>(None)
            .await?
            .map(|c| c.n)
            .unwrap_or(0);
        out["top_links"] = serde_json::to_value(top).unwrap_or_default();
        out["links"] = links.into();
    }
    json(&out, 200)
}

// ---- Export ----

async fn all_links(env: &Env) -> Result<Vec<Link>> {
    let d = store::db(env)?;
    let mut out = vec![];
    let mut offset = 0;
    loop {
        let rows = d
            .prepare("SELECT id, slug, url, title, notes, tags, redirect_type, disabled, expires_at, max_clicks, fallback_url, password_hash, passthrough, utm, geo, device, source, clicks, last_click_at, created_at, updated_at FROM links ORDER BY id LIMIT 1000 OFFSET ?")
            .bind(&[n(offset)])?
            .all()
            .await?
            .results::<LinkRow>()?;
        let len = rows.len();
        out.extend(rows.into_iter().map(Link::from));
        if len < 1000 {
            break;
        }
        offset += 1000;
    }
    Ok(out)
}

fn iso(ms: i64) -> String {
    js_date_iso(ms)
}

#[cfg(target_arch = "wasm32")]
fn js_date_iso(ms: i64) -> String {
    js_sys::Date::new(&JsValue::from_f64(ms as f64))
        .to_iso_string()
        .into()
}

#[cfg(not(target_arch = "wasm32"))]
fn js_date_iso(ms: i64) -> String {
    ms.to_string()
}

async fn export(env: &Env, format: &str) -> Result<Response> {
    let links = all_links(env).await?;
    let (body, ctype, ext) = if format == "json" {
        (
            serde_json::to_string_pretty(&links).unwrap_or_default(),
            "application/json",
            "json",
        )
    } else {
        let mut csv = String::from("slug,url,title,tags,notes,redirect_type,disabled,expires_at,clicks,created_at,source\n");
        for l in &links {
            let row = [
                csv_field(&l.slug),
                csv_field(&l.url),
                csv_field(l.title.as_deref().unwrap_or("")),
                csv_field(&l.tags.join(";")),
                csv_field(l.notes.as_deref().unwrap_or("")),
                l.redirect_type.to_string(),
                l.disabled.to_string(),
                l.expires_at.map(iso).unwrap_or_default(),
                l.clicks.to_string(),
                iso(l.created_at),
                csv_field(&l.source),
            ];
            csv.push_str(&row.join(","));
            csv.push('\n');
        }
        (csv, "text/csv; charset=utf-8", "csv")
    };
    let mut res = Response::ok(body)?;
    let h = res.headers_mut();
    h.set("Content-Type", ctype)?;
    h.set(
        "Content-Disposition",
        &format!("attachment; filename=\"301-links.{ext}\""),
    )?;
    h.set("Cache-Control", "no-store")?;
    Ok(res)
}

async fn rebuild_kv(env: &Env, offset: i64) -> Result<Response> {
    const PAGE: i64 = 40;
    let rows = store::db(env)?
        .prepare("SELECT id FROM links ORDER BY id LIMIT ? OFFSET ?")
        .bind(&[n(PAGE), n(offset)])?
        .all()
        .await?
        .results::<serde_json::Value>()?;
    let mut written = 0;
    for r in &rows {
        if let Some(id) = r
            .get("id")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        {
            if let Some(l) = store::get_link(env, id).await? {
                if store::write_kv(env, &l).await.is_ok() {
                    written += 1;
                }
            }
        }
    }
    let next = if rows.len() as i64 == PAGE {
        Some(offset + PAGE)
    } else {
        None
    };
    json(
        &serde_json::json!({ "written": written, "next_offset": next }),
        200,
    )
}
