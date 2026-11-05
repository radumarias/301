//! Admin JSON API under `/api`. Every route requires a verified Access identity.

use crate::auth::{self, AuthError};
use crate::model::{Link, LinkInput};
use crate::slug::Style;
use crate::store::{self, n, s, CountRow, SResult, StoreError};
use crate::util::{self, csv_field, iso, json, json_error, now_ms};
use crate::{import, qr};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wasm_bindgen::JsValue;
use worker::{Env, Method, Request, Response, Result};

fn store_err(e: StoreError) -> Result<Response> {
    let status = match e {
        StoreError::Bad(_) => 400,
        StoreError::NotFound => 404,
        StoreError::Conflict(_) => 409,
        StoreError::Internal(_) => 500,
    };
    json_error(e.public_message(), status)
}

fn respond<T: Serialize>(r: SResult<T>, status: u16) -> Result<Response> {
    match r {
        Ok(v) => json(&v, status),
        Err(e) => store_err(e),
    }
}

fn ok_json() -> serde_json::Value {
    serde_json::json!({ "ok": true })
}

/// Parses a JSON request body, or produces the 400 response to return.
async fn body<T: DeserializeOwned>(req: &mut Request) -> std::result::Result<T, Response> {
    req.json()
        .await
        .map_err(|e| json_error(format!("invalid JSON: {e}"), 400).unwrap())
}

macro_rules! body {
    ($req:expr) => {
        match body(&mut $req).await {
            Ok(v) => v,
            Err(r) => return Ok(r),
        }
    };
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
    let who = match auth::verify(&req, &env).await {
        Ok(w) => w,
        Err(AuthError::NotConfigured(m)) => return json_error(m, 503),
        Err(AuthError::Denied(m)) => return json_error(m, 403),
    };
    if !same_origin(&req) {
        return json_error("cross-origin request refused", 403);
    }
    let url = req.url()?;
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let q = |k: &str| query.get(k).map(String::as_str);
    let path = req.path();
    let segs: Vec<&str> = path
        .trim_start_matches("/api/")
        .trim_end_matches('/')
        .split('/')
        .collect();
    // `/api/links/<id>/...`: parse the id once.
    let link_id = match segs.as_slice() {
        ["links", id, ..] if *id != "bulk" => match id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => return json_error("bad id", 400),
        },
        _ => None,
    };

    match (req.method(), segs.as_slice(), link_id) {
        (Method::Get, ["me"], _) => json(&serde_json::json!({ "email": who.email }), 200),

        (Method::Get, ["links"], _) => list_links(&env, &query).await,
        (Method::Post, ["links"], _) => {
            let input: LinkInput = body!(req);
            respond(store::create_link(&env, &input).await, 201)
        }
        (Method::Post, ["links", "bulk"], _) => {
            let b: Bulk = body!(req);
            bulk(&env, b).await
        }
        (Method::Get, ["links", _], Some(id)) => match store::get_link(&env, id).await? {
            Some(l) => json(&l, 200),
            None => json_error("not found", 404),
        },
        (Method::Patch | Method::Put, ["links", _], Some(id)) => {
            let input: LinkInput = body!(req);
            respond(store::update_link(&env, id, &input).await, 200)
        }
        (Method::Delete, ["links", _], Some(id)) => {
            respond(store::delete_link(&env, id).await.map(|_| ok_json()), 200)
        }
        (Method::Delete, ["links", _, "aliases", alias @ ..], Some(id)) => respond(
            store::delete_alias(&env, id, &alias.join("/"))
                .await
                .map(|_| ok_json()),
            200,
        ),
        (Method::Get, ["links", _, "stats"], Some(id)) => {
            stats(&env, Some(id), q("range").unwrap_or("30d")).await
        }
        (Method::Get, ["links", _, "qr.svg"], Some(id)) => {
            let Some(link) = store::get_link(&env, id).await? else {
                return json_error("not found", 404);
            };
            let track = q("track") != Some("0");
            let data = format!(
                "{}/{}{}",
                url.origin().ascii_serialization(),
                link.slug,
                if track { "?qr=1" } else { "" }
            );
            let size = q("size").and_then(|v| v.parse().ok()).unwrap_or(512);
            match qr::svg(&data, q("fg"), q("bg"), q("ecc"), size) {
                Ok(svg) => util::body(svg, "image/svg+xml"),
                Err(e) => json_error(e, 400),
            }
        }
        (Method::Get, ["slug"], _) => {
            let st = store::settings(&env).await.unwrap_or_default();
            let style = q("style").map(Style::parse).unwrap_or(st.slug_style());
            let len = q("length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(st.default_slug_length);
            respond(
                store::unique_slug(&env, style, len)
                    .await
                    .map(|s| serde_json::json!({ "slug": s })),
                200,
            )
        }
        (Method::Get, ["slug", "check"], _) => {
            let slug = q("slug").unwrap_or_default();
            let valid = crate::slug::validate(slug);
            let taken = store::slug_owner(&env, slug).await?;
            json(
                &serde_json::json!({ "valid": valid.is_ok(), "reason": valid.err(), "taken_by": taken }),
                200,
            )
        }
        (Method::Get, ["stats"], _) => stats(&env, None, q("range").unwrap_or("30d")).await,
        (Method::Get, ["tags"], _) => tags(&env).await,
        (Method::Get, ["settings"], _) => json(&store::settings(&env).await?, 200),
        (Method::Put, ["settings"], _) => {
            let st: store::Settings = body!(req);
            respond(store::save_settings(&env, &st).await.map(|_| st), 200)
        }
        (Method::Post, ["import", "fetch"], _) => {
            let b: import::FetchRequest = body!(req);
            match import::fetch_page(&b).await {
                Ok(page) => json(&page, 200),
                Err(e) => json_error(e, 502),
            }
        }
        (Method::Post, ["import", "commit"], _) => {
            let b: import::CommitRequest = body!(req);
            respond(import::commit(&env, b).await, 200)
        }
        (Method::Get, ["export"], _) => export(&env, q("format").unwrap_or("csv")).await,
        (Method::Post, ["maintenance", "rebuild-kv"], _) => {
            rebuild_kv(&env, q("after").and_then(|v| v.parse().ok()).unwrap_or(0)).await
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

const EXPIRED_SQL: &str = "((expires_at IS NOT NULL AND expires_at <= ?) OR (max_clicks IS NOT NULL AND clicks >= max_clicks))";

async fn list_links(env: &Env, q: &HashMap<String, String>) -> Result<Response> {
    let mut wheres: Vec<String> = vec![];
    let mut binds: Vec<JsValue> = vec![];
    if let Some(text) = q.get("q").map(|t| t.trim()).filter(|t| !t.is_empty()) {
        wheres.push("(slug LIKE ?1 OR url LIKE ?1 OR title LIKE ?1 OR notes LIKE ?1)".into());
        binds.push(s(&format!("%{text}%")));
    }
    if let Some(tag) = q.get("tag").filter(|t| !t.is_empty()) {
        wheres.push(format!(
            "EXISTS (SELECT 1 FROM json_each(links.tags) WHERE value = ?{})",
            binds.len() + 1
        ));
        binds.push(s(tag));
    }
    let now_bind = format!("?{}", binds.len() + 1);
    let expired = EXPIRED_SQL.replace('?', &now_bind);
    match q.get("status").map(String::as_str) {
        Some("disabled") => wheres.push("disabled = 1".into()),
        Some("expired") => wheres.push(format!("disabled = 0 AND {expired}")),
        Some("active") => wheres.push(format!("disabled = 0 AND NOT {expired}")),
        Some("protected") => wheres.push("password_hash IS NOT NULL".into()),
        _ => {}
    }
    if wheres.iter().any(|w| w.contains(&now_bind)) {
        binds.push(n(now_ms()));
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
    let res = d
        .batch(vec![
            d.prepare(format!("SELECT COUNT(*) AS n FROM links {where_sql}")).bind(&binds)?,
            d.prepare(format!(
                "SELECT {} FROM links {where_sql} ORDER BY {sort} {order}, id DESC LIMIT {limit} OFFSET {offset}",
                store::LINK_COLS
            ))
            .bind(&binds)?,
        ])
        .await?;
    let total = res[0]
        .results::<CountRow>()?
        .first()
        .map(|c| c.n)
        .unwrap_or(0);
    let items: Vec<Link> = res[1]
        .results::<crate::model::LinkRow>()?
        .into_iter()
        .map(Link::from)
        .collect();
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
    let input = match body.action.as_str() {
        "delete" => None,
        "disable" | "enable" => Some(LinkInput {
            disabled: Some(body.action == "disable"),
            ..Default::default()
        }),
        "tag" => Some(LinkInput {
            add_tags: Some(body.tags),
            ..Default::default()
        }),
        "untag" => Some(LinkInput {
            remove_tags: Some(body.tags),
            ..Default::default()
        }),
        _ => return json_error("action must be tag, untag, disable, enable or delete", 400),
    };
    if body.ids.is_empty() {
        return json(&serde_json::json!({ "done": 0, "errors": [] }), 200);
    }
    let marks = vec!["?"; body.ids.len()].join(",");
    let ids: Vec<JsValue> = body.ids.iter().map(|id| n(*id)).collect();
    let links = store::with_aliases(
        env,
        store::query_links(env, &format!("WHERE id IN ({marks})"), &ids).await?,
    )
    .await?;

    let Some(input) = input else {
        return respond(
            store::delete_links(env, &links)
                .await
                .map(|_| serde_json::json!({ "done": links.len(), "errors": [] })),
            200,
        );
    };
    // One D1 batch for all updates, then one concurrent KV sync per link.
    let mut stmts = vec![];
    let mut updated = vec![];
    let mut errors = vec![];
    for mut l in links {
        match store::prepare_update(env, &mut l, &input).await {
            Ok(st) => {
                stmts.extend(st);
                updated.push(l);
            }
            Err(e) => errors.push(serde_json::json!({ "id": l.id, "error": e.public_message() })),
        }
    }
    if !stmts.is_empty() {
        store::db(env)?.batch(stmts).await?;
    }
    for r in futures_util::future::join_all(updated.iter().map(|l| store::sync_kv(env, l))).await {
        if let Err(e) = r {
            return store_err(e);
        }
    }
    json(
        &serde_json::json!({ "done": updated.len(), "errors": errors }),
        200,
    )
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

const DIMENSIONS: [&str; 6] = ["country", "referrer", "device", "os", "browser", "city"];

async fn stats(env: &Env, link: Option<i64>, r: &str) -> Result<Response> {
    let (since, bucket) = range(r);
    let d = store::db(env)?;
    let (filter, binds): (&str, Vec<JsValue>) = match link {
        Some(id) => ("link_id = ?1 AND ts >= ?2", vec![n(id), n(since)]),
        None => ("ts >= ?1", vec![n(since)]),
    };

    // Everything in one D1 round trip.
    let mut stmts = vec![
        d.prepare(format!(
            "SELECT SUM(CASE WHEN bot = 0 THEN 1 ELSE 0 END) AS humans, SUM(bot) AS bots, SUM(CASE WHEN bot = 0 THEN qr ELSE 0 END) AS qr FROM clicks WHERE {filter}"
        ))
        .bind(&binds)?,
        // `bucket` is a constant from `range`, not user input.
        d.prepare(format!(
            "SELECT CAST(ts / {bucket} AS INTEGER) * {bucket} AS t, COUNT(*) AS n FROM clicks WHERE {filter} AND bot = 0 GROUP BY t ORDER BY t"
        ))
        .bind(&binds)?,
    ];
    for col in DIMENSIONS {
        stmts.push(
            d.prepare(format!(
                "SELECT {col} AS k, COUNT(*) AS n FROM clicks WHERE {filter} AND bot = 0 GROUP BY {col} ORDER BY n DESC LIMIT 10"
            ))
            .bind(&binds)?,
        );
    }
    if link.is_none() {
        stmts.push(
            d.prepare(
                "SELECT l.id, l.slug, l.url, l.title, COUNT(*) AS n FROM clicks c JOIN links l ON l.id = c.link_id \
                 WHERE c.ts >= ?1 AND c.bot = 0 GROUP BY l.id ORDER BY n DESC LIMIT 10",
            )
            .bind(&binds)?,
        );
        stmts.push(d.prepare("SELECT COUNT(*) AS n FROM links"));
    }
    let res = d.batch(stmts).await?;

    #[derive(Deserialize)]
    struct Totals {
        humans: Option<i64>,
        bots: Option<i64>,
        qr: Option<i64>,
    }
    let totals = res[0].results::<Totals>()?.into_iter().next();
    let mut dims = serde_json::Map::new();
    for (i, col) in DIMENSIONS.iter().enumerate() {
        dims.insert(
            col.to_string(),
            serde_json::to_value(res[2 + i].results::<KV>()?).unwrap_or_default(),
        );
    }
    let mut out = serde_json::json!({
        "range": r,
        "bucket_ms": bucket,
        "since": since,
        "clicks": totals.as_ref().and_then(|t| t.humans).unwrap_or(0),
        "bots": totals.as_ref().and_then(|t| t.bots).unwrap_or(0),
        "qr": totals.as_ref().and_then(|t| t.qr).unwrap_or(0),
        "series": res[1].results::<Bucket>()?,
        "top": dims,
    });
    if link.is_none() {
        out["top_links"] = serde_json::Value::Array(res[8].results::<serde_json::Value>()?);
        out["links"] = res[9]
            .results::<CountRow>()?
            .first()
            .map(|c| c.n)
            .unwrap_or(0)
            .into();
    }
    json(&out, 200)
}

// ---- Export / maintenance ----

/// One page of links (with aliases) after `after_id`, ordered by id (keyset paging).
async fn links_after(env: &Env, after_id: i64, limit: i64) -> Result<Vec<Link>> {
    let page = store::query_links(
        env,
        &format!("WHERE id > ?1 ORDER BY id LIMIT {limit}"),
        &[n(after_id)],
    )
    .await?;
    store::with_aliases(env, page).await
}

async fn export(env: &Env, format: &str) -> Result<Response> {
    let mut links = vec![];
    loop {
        let page = links_after(env, links.last().map(|l: &Link| l.id).unwrap_or(0), 1000).await?;
        let done = page.len() < 1000;
        links.extend(page);
        if done {
            break;
        }
    }
    let (content, ctype, ext) = if format == "json" {
        (
            serde_json::to_string_pretty(&links).unwrap_or_default(),
            "application/json",
            "json",
        )
    } else {
        let mut csv = String::from("slug,url,title,tags,notes,redirect_type,disabled,expires_at,clicks,created_at,source,aliases\n");
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
                csv_field(&l.aliases.join(";")),
            ];
            csv.push_str(&row.join(","));
            csv.push('\n');
        }
        (csv, "text/csv; charset=utf-8", "csv")
    };
    let mut res = util::body(content, ctype)?;
    res.headers_mut().set(
        "Content-Disposition",
        &format!("attachment; filename=\"301-links.{ext}\""),
    )?;
    Ok(res)
}

async fn rebuild_kv(env: &Env, after: i64) -> Result<Response> {
    const PAGE: i64 = 100;
    let links = links_after(env, after, PAGE).await?;
    let results =
        futures_util::future::join_all(links.iter().map(|l| store::sync_kv(env, l))).await;
    let written = results.iter().filter(|r| r.is_ok()).count();
    let next = (links.len() as i64 == PAGE)
        .then(|| links.last().map(|l| l.id))
        .flatten();
    json(
        &serde_json::json!({ "written": written, "next_after": next }),
        200,
    )
}
