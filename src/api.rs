//! JSON API under `/api`. A few routes are public (config, anonymous links, reports);
//! the rest need a signed-in user. Admins see everything, users only their own links.

use crate::auth::{self, AuthError, Identity};
use crate::model::{Link, LinkInput};
use crate::slug::Style;
use crate::store::{self, n, s, CountRow, SResult, StoreError};
use crate::util::{self, csv_field, iso, json, json_error, now_ms};
use crate::{account, anon, safety};
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
pub(crate) async fn body<T: DeserializeOwned>(
    req: &mut Request,
) -> std::result::Result<T, Response> {
    req.json()
        .await
        .map_err(|e| json_error(format!("invalid JSON: {e}"), 400).unwrap())
}

#[macro_export]
macro_rules! body {
    ($req:expr) => {
        match body(&mut $req).await {
            Ok(v) => v,
            Err(r) => return Ok(r),
        }
    };
}

/// Rejects cross-site writes: browsers always send Origin on POST/PATCH/DELETE.
pub fn same_origin(req: &Request) -> bool {
    if matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        return true;
    }
    let Ok(url) = req.url() else { return false };
    let header = |k: &str| req.headers().get(k).ok().flatten();
    // Fetch metadata, where the browser sends it, must not say cross-site.
    if header("Sec-Fetch-Site").is_some_and(|v| v != "same-origin" && v != "none") {
        return false;
    }
    match header("Origin") {
        Some(o) => o == url.origin().ascii_serialization(),
        None => true, // non-browser clients (service tokens)
    }
}

/// Links a caller may see: admins see all, everyone else only their own.
pub fn visible(who: &Identity, owner_id: Option<i64>) -> bool {
    who.admin || owner_id == Some(who.user_id)
}

/// What a caller may do with a link.
#[derive(PartialEq)]
enum Access {
    Read,
    /// Change or delete: owners cannot touch a link an admin blocked.
    Write,
}

/// `Ok(Err(response))` when the caller may not access the link.
fn may(
    who: &Identity,
    owner_id: Option<i64>,
    blocked: bool,
    access: Access,
) -> Result<std::result::Result<(), Response>> {
    if !visible(who, owner_id) {
        // 404, so ids of other people's links are not confirmed.
        return Ok(Err(json_error("not found", 404)?));
    }
    if access == Access::Write && blocked && !who.admin {
        return Ok(Err(json_error(
            "This link was blocked by an admin and cannot be changed.",
            403,
        )?));
    }
    Ok(Ok(()))
}

/// Loads a link the caller may access.
async fn owned(
    env: &Env,
    who: &Identity,
    id: i64,
    access: Access,
) -> Result<std::result::Result<Link, Response>> {
    let Some(l) = store::get_link(env, id).await? else {
        return Ok(Err(json_error("not found", 404)?));
    };
    Ok(may(who, l.owner_id, l.blocked, access)?.map(|_| l))
}

/// Checks access without loading the whole link.
async fn check_access(
    env: &Env,
    who: &Identity,
    id: i64,
    access: Access,
) -> Result<std::result::Result<(), Response>> {
    match store::link_access(env, id).await? {
        Some((owner, blocked)) => may(who, owner, blocked, access),
        None => Ok(Err(json_error("not found", 404)?)),
    }
}

/// `owned!(env, who, id, Read|Write)` loads the link or returns the error response;
/// `owned!(check env, who, id, ...)` only checks.
macro_rules! owned {
    (check $env:expr, $who:expr, $id:expr, $access:ident) => {
        if let Err(r) = check_access($env, $who, $id, Access::$access).await? {
            return Ok(r);
        }
    };
    ($env:expr, $who:expr, $id:expr, $access:ident) => {
        match owned($env, $who, $id, Access::$access).await? {
            Ok(l) => l,
            Err(r) => return Ok(r),
        }
    };
}

/// Destination checks for links saved by non-admins (admins are trusted).
async fn check_destinations(
    env: &Env,
    req: &Request,
    link: &Link,
) -> std::result::Result<(), Response> {
    safety::check_link(env, req, link)
        .await
        .map_err(|e| json_error(e, 400).unwrap())
}

const DAY_MS: i64 = 24 * 3_600_000;

pub async fn handle(mut req: Request, env: Env) -> Result<Response> {
    let url = req.url()?;
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let q = |k: &str| query.get(k).map(String::as_str);
    let path = req.path();
    let segs: Vec<&str> = path
        .trim_start_matches("/api/")
        .trim_end_matches('/')
        .split('/')
        .collect();

    // Public endpoints: no sign-in needed.
    match (req.method(), segs.as_slice()) {
        (Method::Get, ["config"]) => return anon::config(&req, &env).await,
        (Method::Post, ["anon", "links"]) => return anon::create(req, &env).await,
        (Method::Get | Method::Delete, ["anon", token, rest @ ..]) => {
            let token = token.to_string();
            let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
            return anon::manage(&req, &env, &token, &rest, q("range")).await;
        }
        (Method::Post, ["report"]) => return anon::report(req, &env).await,
        _ => {}
    }

    let who = match auth::identify(&req, &env).await {
        Ok(Some(w)) => w,
        Ok(None) => return json_error("sign in required", 401),
        Err(AuthError::NotConfigured(m)) => return json_error(m, 503),
        Err(AuthError::Denied(m)) => return json_error(m, 403),
    };
    // Regular users only ever see their own links.
    let scope = (!who.admin).then_some(who.user_id);

    // `/api/links/<id>/...`: parse the id once.
    let link_id = match segs.as_slice() {
        ["links", id, ..] if *id != "bulk" => match id.parse::<i64>() {
            Ok(id) => Some(id),
            Err(_) => return json_error("bad id", 400),
        },
        _ => None,
    };
    let admin_only = matches!(
        (req.method(), segs.as_slice()),
        (Method::Put, ["settings"])
            | (_, ["import", ..])
            | (_, ["maintenance", ..])
            | (_, ["users", ..])
            | (_, ["reports", ..])
    );
    if admin_only && !who.admin {
        return json_error("admins only", 403);
    }

    match (req.method(), segs.as_slice(), link_id) {
        (Method::Get, ["me"], _) => json(
            &serde_json::json!({ "email": who.email, "name": who.name, "admin": who.admin, "user_id": who.user_id }),
            200,
        ),
        (Method::Post, ["claim"], _) => {
            #[derive(Deserialize)]
            struct Claim {
                tokens: Vec<String>,
            }
            let c: Claim = body!(req);
            let moved = account::claim_anonymous(&env, who.user_id, &c.tokens).await?;
            // Anonymous links have no aliases: forgetting the slug refreshes the record.
            let slugs: Vec<&str> = moved.iter().map(String::as_str).collect();
            if let Err(e) = store::forget_kv(&env, &slugs).await {
                return store_err(e);
            }
            json(&serde_json::json!({ "claimed": moved.len() }), 200)
        }

        (Method::Get, ["links"], _) => list_links(&env, &query, &who).await,
        (Method::Post, ["links"], _) => {
            let input: LinkInput = body!(req);
            let st = store::settings(&env).await.unwrap_or_default();
            let reservation = match scope {
                Some(uid) => match account::reserve_creation(
                    &env,
                    account::Creator::User(uid),
                    now_ms() - DAY_MS,
                    st.user_per_day,
                )
                .await?
                {
                    Some(r) => Some(r),
                    None => {
                        return json_error(
                            format!(
                                "You can create {} links a day. Try again tomorrow.",
                                st.user_per_day
                            ),
                            429,
                        )
                    }
                },
                None => None,
            };
            let res = create_link(&env, &req, &st, &input, &who, scope.is_some()).await;
            if let (Some(r), Err(_) | Ok(Err(_))) = (reservation, &res) {
                account::release_creation(&env, r).await?;
            }
            match res {
                Ok(Ok(link)) => json(&link, 201),
                Ok(Err(r)) => Ok(r),
                Err(e) => store_err(e),
            }
        }
        (Method::Post, ["links", "bulk"], _) => {
            let b: Bulk = body!(req);
            bulk(&env, b, &who).await
        }
        (Method::Get, ["links", _], Some(id)) => {
            let mut link = owned!(&env, &who, id, Read);
            if who.admin && link.owner_id.is_some() {
                link.owner_email = account::email_of(&env, link.owner_id).await?;
            }
            json(&link, 200)
        }
        (Method::Patch | Method::Put, ["links", _], Some(id)) => {
            let input: LinkInput = body!(req);
            let mut link = owned!(&env, &who, id, Write);
            let before: Vec<String> = link.destinations().into_iter().map(String::from).collect();
            let stmts = match store::prepare_update(&env, &mut link, &input).await {
                Ok(s) => s,
                Err(e) => return store_err(e),
            };
            // Only re-check (Safe Browsing is a network call) when a destination changed.
            if !who.admin
                && link
                    .destinations()
                    .into_iter()
                    .ne(before.iter().map(String::as_str))
            {
                if let Err(r) = check_destinations(&env, &req, &link).await {
                    return Ok(r);
                }
            }
            respond(store::save_update(&env, link, stmts).await, 200)
        }
        (Method::Delete, ["links", _], Some(id)) => {
            let link = owned!(&env, &who, id, Write);
            respond(
                store::delete_links(&env, &[link]).await.map(|_| ok_json()),
                200,
            )
        }
        (Method::Delete, ["links", _, "aliases", alias @ ..], Some(id)) => {
            owned!(check & env, &who, id, Write);
            respond(
                store::delete_alias(&env, id, &alias.join("/"))
                    .await
                    .map(|_| ok_json()),
                200,
            )
        }
        (Method::Get, ["links", _, "stats"], Some(id)) => {
            owned!(check & env, &who, id, Read);
            stats(&env, Some(id), q("range").unwrap_or("30d"), None).await
        }
        (Method::Get, ["links", _, "qr.svg"], Some(id)) => {
            let link = owned!(&env, &who, id, Read);
            qr_svg(&url, &link, &query)
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
            let mut taken = store::slug_owner(&env, slug).await?;
            // Do not reveal the ids of other people's links.
            if let (Some(id), false) = (taken, who.admin) {
                let mine = store::link_access(&env, id)
                    .await?
                    .is_some_and(|(owner, _)| visible(&who, owner));
                if !mine {
                    taken = Some(-1);
                }
            }
            json(
                &serde_json::json!({ "valid": valid.is_ok(), "reason": valid.err(), "taken_by": taken }),
                200,
            )
        }
        (Method::Get, ["stats"], _) => stats(&env, None, q("range").unwrap_or("30d"), scope).await,
        (Method::Get, ["tags"], _) => tags(&env, scope).await,
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
            respond(import::commit(&env, b, Some(who.user_id)).await, 200)
        }
        (Method::Get, ["export"], _) => export(&env, q("format").unwrap_or("csv"), scope).await,
        (Method::Post, ["maintenance", "rebuild-kv"], _) => {
            rebuild_kv(&env, q("after").and_then(|v| v.parse().ok()).unwrap_or(0)).await
        }
        (Method::Get, ["users"], _) => json(&account::list_users(&env).await?, 200),
        (Method::Post, ["users", id], _) => {
            #[derive(Deserialize)]
            struct Ban {
                banned: bool,
            }
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            if id == who.user_id {
                return json_error("you cannot ban yourself", 400);
            }
            let b: Ban = body!(req);
            account::set_banned(&env, id, b.banned).await?;
            json(&ok_json(), 200)
        }
        (Method::Get, ["reports"], _) => json(&account::open_reports(&env).await?, 200),
        (Method::Post, ["reports", id, "resolve"], _) => {
            let Ok(id) = id.parse::<i64>() else {
                return json_error("bad id", 400);
            };
            account::resolve_report(&env, id, q("all") == Some("1")).await?;
            json(&ok_json(), 200)
        }
        _ => json_error("no such endpoint", 404),
    }
}

fn qr_svg(url: &url::Url, link: &Link, query: &HashMap<String, String>) -> Result<Response> {
    let q = |k: &str| query.get(k).map(String::as_str);
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

// ---- Listing ----

#[derive(Serialize)]
struct Page<T> {
    items: Vec<T>,
    total: i64,
}

const EXPIRED_SQL: &str = "((expires_at IS NOT NULL AND expires_at <= ?) OR (max_clicks IS NOT NULL AND clicks >= max_clicks))";

async fn list_links(env: &Env, q: &HashMap<String, String>, who: &Identity) -> Result<Response> {
    let mut wheres: Vec<String> = vec![];
    let mut binds: Vec<JsValue> = vec![];
    // Admins can filter by owner; everyone else is pinned to their own links.
    let owner = if who.admin {
        q.get("owner").map(String::as_str).unwrap_or("")
    } else {
        "me"
    };
    match owner {
        "me" => {
            binds.push(n(who.user_id));
            wheres.push(format!("owner_id = ?{}", binds.len()));
        }
        "anon" => wheres.push("manage_hash IS NOT NULL".into()),
        id => {
            if let Ok(id) = id.parse::<i64>() {
                binds.push(n(id));
                wheres.push(format!("owner_id = ?{}", binds.len()));
            }
        }
    }
    if let Some(text) = q.get("q").map(|t| t.trim()).filter(|t| !t.is_empty()) {
        binds.push(s(&format!("%{text}%")));
        let i = binds.len();
        wheres.push(format!(
            "(slug LIKE ?{i} OR url LIKE ?{i} OR title LIKE ?{i} OR notes LIKE ?{i})"
        ));
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
        // Mirrors `Link::status`: blocked wins over disabled and expired.
        Some("disabled") => wheres.push("blocked = 0 AND disabled = 1".into()),
        Some("blocked") => wheres.push("blocked = 1".into()),
        Some("reported") => wheres.push(
            "EXISTS (SELECT 1 FROM reports WHERE reports.link_id = links.id AND reports.resolved = 0)".into(),
        ),
        Some("expired") => wheres.push(format!("blocked = 0 AND disabled = 0 AND {expired}")),
        Some("active") => wheres.push(format!("blocked = 0 AND disabled = 0 AND NOT {expired}")),
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
                "SELECT {}{} FROM links {where_sql} ORDER BY {sort} {order}, id DESC LIMIT {limit} OFFSET {offset}",
                store::LINK_COLS,
                if who.admin { format!(", {}", store::OWNER_EMAIL_COL) } else { String::new() }
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

async fn tags(env: &Env, scope: Option<i64>) -> Result<Response> {
    #[derive(Deserialize, Serialize)]
    struct Row {
        tag: String,
        n: i64,
    }
    let (filter, binds) = match scope {
        Some(uid) => ("WHERE links.owner_id = ?1", vec![n(uid)]),
        None => ("", vec![]),
    };
    let rows = store::db(env)?
        .prepare(format!("SELECT j.value AS tag, COUNT(*) AS n FROM links, json_each(links.tags) j {filter} GROUP BY j.value ORDER BY n DESC, tag"))
        .bind(&binds)?
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

async fn bulk(env: &Env, body: Bulk, who: &Identity) -> Result<Response> {
    if body.ids.len() > 200 {
        return json_error("at most 200 links per bulk action", 400);
    }
    if matches!(body.action.as_str(), "block" | "unblock") && !who.admin {
        return json_error("admins only", 403);
    }
    let input = match body.action.as_str() {
        "delete" => None,
        "block" | "unblock" => Some(LinkInput {
            blocked: Some(body.action == "block"),
            ..Default::default()
        }),
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
    // D1 allows at most 100 bound parameters per statement.
    let mut links = vec![];
    for chunk in body.ids.chunks(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|id| n(*id)).collect();
        links.extend(store::query_links(env, &format!("WHERE id IN ({marks})"), &ids).await?);
    }
    // Other people's links are silently left out; blocked ones stay as they are for users.
    links.retain(|l| visible(who, l.owner_id) && (who.admin || !l.blocked));
    if links.is_empty() {
        return json(&serde_json::json!({ "done": 0, "errors": [] }), 200);
    }
    let links = store::with_aliases(env, links).await?;

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
        if let Err(e) = store::refresh_blocked(env, &mut updated).await {
            return store_err(e);
        }
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

/// Builds, checks and saves a new link. `Ok(Err(response))` is a refusal (unsafe URL).
async fn create_link(
    env: &Env,
    req: &Request,
    st: &store::Settings,
    input: &LinkInput,
    who: &Identity,
    check: bool,
) -> SResult<std::result::Result<Link, Response>> {
    let mut link = store::new_link(env, st, input, "manual").await?;
    if check {
        if let Err(r) = check_destinations(env, req, &link).await {
            return Ok(Err(r));
        }
    }
    link.owner_id = Some(who.user_id);
    store::insert_link(env, link, true).await.map(Ok)
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

/// Click stats for one link, or across all links (`owner`: only that user's links).
pub async fn stats(env: &Env, link: Option<i64>, r: &str, owner: Option<i64>) -> Result<Response> {
    let (since, bucket) = range(r);
    let d = store::db(env)?;
    let (filter, binds): (&str, Vec<JsValue>) = match (link, owner) {
        (Some(id), _) => ("link_id = ?1 AND ts >= ?2", vec![n(id), n(since)]),
        (None, Some(uid)) => (
            "ts >= ?1 AND link_id IN (SELECT id FROM links WHERE owner_id = ?2)",
            vec![n(since), n(uid)],
        ),
        (None, None) => ("ts >= ?1", vec![n(since)]),
    };
    let owner_and = if owner.is_some() {
        " AND l.owner_id = ?2"
    } else {
        ""
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
            d.prepare(format!(
                "SELECT l.id, l.slug, l.url, l.title, COUNT(*) AS n FROM clicks c JOIN links l ON l.id = c.link_id \
                 WHERE c.ts >= ?1 AND c.bot = 0{owner_and} GROUP BY l.id ORDER BY n DESC LIMIT 10"
            ))
            .bind(&binds)?,
        );
        stmts.push(match owner {
            Some(uid) => d
                .prepare("SELECT COUNT(*) AS n FROM links WHERE owner_id = ?1")
                .bind(&[n(uid)])?,
            None => d.prepare("SELECT COUNT(*) AS n FROM links"),
        });
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
async fn links_after(
    env: &Env,
    after_id: i64,
    limit: i64,
    owner: Option<i64>,
) -> Result<Vec<Link>> {
    let mut binds = vec![n(after_id)];
    binds.extend(owner.map(n));
    let mine = if owner.is_some() {
        " AND owner_id = ?2"
    } else {
        ""
    };
    let page = store::query_links(
        env,
        &format!("WHERE id > ?1{mine} ORDER BY id LIMIT {limit}"),
        &binds,
    )
    .await?;
    store::with_aliases(env, page).await
}

async fn export(env: &Env, format: &str, owner: Option<i64>) -> Result<Response> {
    let mut links = vec![];
    loop {
        let page = links_after(
            env,
            links.last().map(|l: &Link| l.id).unwrap_or(0),
            1000,
            owner,
        )
        .await?;
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
            // Slugs/aliases are validated to [A-Za-z0-9-_./]: written raw so a leading '-'
            // is not prefixed with "'" (which would break re-importing the export).
            let row = [
                l.slug.clone(),
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
                l.aliases.join(";"),
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
    let links = links_after(env, after, PAGE, None).await?;
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
