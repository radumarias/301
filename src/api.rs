//! Admin JSON API under `/api`. Every route requires a verified Access identity.

use crate::auth::{self, AuthError};
use crate::model::{Link, LinkInput};
use crate::slug::Style;
use crate::store::{self, n, s, CountRow, SResult, StoreError};
use crate::util::{self, csv_field, iso, json, json_error, now_ms};
use crate::{analytics, backup, health, import, qr, webhooks};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use wasm_bindgen::JsValue;
use worker::{Context, Env, Method, Request, Response, Result};

/// Sends a link event for some links to webhooks after the response.
fn fire_links(env: &Env, ctx: &Context, event: &'static str, links: &[Link]) {
    let env = env.clone();
    let data = links
        .iter()
        .map(|l| serde_json::to_value(l).unwrap_or_default())
        .collect();
    ctx.wait_until(async move { webhooks::fire_links(&env, event, data).await });
}

/// Responds with a stored link and, on success, fires the matching webhook event.
fn fired(
    env: &Env,
    ctx: &Context,
    event: &'static str,
    r: SResult<Link>,
    status: u16,
) -> Result<Response> {
    if let Ok(l) = &r {
        fire_links(env, ctx, event, std::slice::from_ref(l));
    }
    respond(r, status)
}

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

pub async fn handle(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
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
    let hook_id = match segs.as_slice() {
        ["webhooks", id, ..] => id.parse::<i64>().ok(),
        _ => None,
    };

    match (req.method(), segs.as_slice(), link_id) {
        (Method::Get, ["me"], _) => json(&serde_json::json!({ "email": who.email }), 200),

        (Method::Get, ["links"], _) => list_links(&env, &query).await,
        (Method::Post, ["links"], _) => {
            let input: LinkInput = body!(req);
            fired(
                &env,
                &ctx,
                "link.created",
                store::create_link(&env, &input).await,
                201,
            )
        }
        (Method::Post, ["links", "bulk"], _) => {
            let b: Bulk = body!(req);
            bulk(&env, &ctx, b).await
        }
        (Method::Get, ["links", _], Some(id)) => match store::get_link(&env, id).await? {
            Some(l) => json(&l, 200),
            None => json_error("not found", 404),
        },
        (Method::Patch | Method::Put, ["links", _], Some(id)) => {
            let input: LinkInput = body!(req);
            fired(
                &env,
                &ctx,
                "link.updated",
                store::update_link(&env, id, &input).await,
                200,
            )
        }
        (Method::Delete, ["links", _], Some(id)) => match store::delete_link(&env, id).await {
            Ok(link) => {
                fire_links(&env, &ctx, "link.deleted", &[link]);
                json(&ok_json(), 200)
            }
            Err(e) => store_err(e),
        },
        (Method::Post, ["links", _, "check"], Some(id)) => {
            let Some(link) = store::get_link(&env, id).await? else {
                return json_error("not found", 404);
            };
            json(&health::check_link(&env, id, &link.url).await?, 200)
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
        (Method::Get, ["status"], _) => status(&env).await,

        (Method::Get, ["webhooks"], _) => json(&webhooks::list(&env).await?, 200),
        (Method::Post, ["webhooks"], _) => {
            let input: webhooks::HookInput = body!(req);
            respond(webhooks::create(&env, &input).await, 201)
        }
        (Method::Delete, ["webhooks", _], _) => match hook_id {
            Some(id) => respond(webhooks::delete(&env, id).await.map(|_| ok_json()), 200),
            None => json_error("bad id", 400),
        },
        (Method::Post, ["webhooks", _, "test"], _) => {
            let hooks = webhooks::list(&env).await?;
            let Some(hook) = hooks.iter().find(|h| Some(h.id) == hook_id) else {
                return json_error("not found", 404);
            };
            let (status, error) =
                webhooks::deliver(hook, "test", &serde_json::json!({ "hello": "from 301" })).await;
            json(
                &serde_json::json!({ "status": status, "error": error }),
                200,
            )
        }

        (_, ["backups", ..], _) if !backup::enabled(&env) => {
            json_error("backups need an R2 bucket bound as BACKUPS", 503)
        }
        (Method::Get, ["backups"], _) => json(&backup::list(&env).await?, 200),
        (Method::Post, ["backups"], _) => {
            json(&serde_json::json!({ "key": backup::run(&env).await? }), 201)
        }
        (Method::Get, ["backups", "download"], _) => {
            let key = q("key").unwrap_or_default();
            if !backup::valid_key(key) {
                return json_error("not found", 404);
            }
            let Some(obj) = env.bucket(backup::BINDING)?.get(key).execute().await? else {
                return json_error("not found", 404);
            };
            let Some(b) = obj.body() else {
                return json_error("empty backup", 500);
            };
            let mut res = util::body(b.text().await?, "application/json")?;
            let name = key.trim_start_matches("daily/");
            res.headers_mut().set(
                "Content-Disposition",
                &format!("attachment; filename=\"301-backup-{name}\""),
            )?;
            Ok(res)
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
        Some("broken") => wheres.push(crate::model::BROKEN_SQL.into()),
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

async fn bulk(env: &Env, ctx: &Context, body: Bulk) -> Result<Response> {
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
    // D1 allows at most 100 bound parameters per statement.
    let mut links = vec![];
    for chunk in body.ids.chunks(90) {
        let marks = vec!["?"; chunk.len()].join(",");
        let ids: Vec<JsValue> = chunk.iter().map(|id| n(*id)).collect();
        links.extend(store::query_links(env, &format!("WHERE id IN ({marks})"), &ids).await?);
    }
    let links = store::with_aliases(env, links).await?;

    let Some(input) = input else {
        if let Err(e) = store::delete_links(env, &links).await {
            return store_err(e);
        }
        fire_links(env, ctx, "link.deleted", &links);
        return json(
            &serde_json::json!({ "done": links.len(), "errors": [] }),
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
    fire_links(env, ctx, "link.updated", &updated);
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

/// Click aggregates from one source (D1 rows or Analytics Engine), merged before output.
#[derive(Default)]
struct Agg {
    humans: i64,
    bots: i64,
    qr: i64,
    series: BTreeMap<i64, i64>,
    dims: HashMap<String, HashMap<Option<String>, i64>>,
    top_links: HashMap<i64, i64>,
}

impl Agg {
    fn merge(&mut self, o: Agg) {
        self.humans += o.humans;
        self.bots += o.bots;
        self.qr += o.qr;
        for (t, n) in o.series {
            *self.series.entry(t).or_default() += n;
        }
        for (d, m) in o.dims {
            let e = self.dims.entry(d).or_default();
            for (k, n) in m {
                *e.entry(k).or_default() += n;
            }
        }
        for (id, n) in o.top_links {
            *self.top_links.entry(id).or_default() += n;
        }
    }
}

fn top_n<K: Clone + Ord>(m: &HashMap<K, i64>, n: usize) -> Vec<(K, i64)> {
    let mut v: Vec<(K, i64)> = m.iter().map(|(k, n)| (k.clone(), *n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

fn dims_for(link: Option<i64>) -> Vec<&'static str> {
    let mut d = DIMENSIONS.to_vec();
    if link.is_some() {
        d.push("variant");
    }
    d
}

async fn d1_stats(env: &Env, link: Option<i64>, since: i64, bucket: i64) -> Result<Agg> {
    let d = store::db(env)?;
    let (filter, binds): (&str, Vec<JsValue>) = match link {
        Some(id) => ("link_id = ?1 AND ts >= ?2", vec![n(id), n(since)]),
        None => ("ts >= ?1", vec![n(since)]),
    };
    let dims = dims_for(link);
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
    for col in &dims {
        let (expr, extra) = if *col == "variant" {
            ("variant", " AND variant IS NOT NULL")
        } else {
            (*col, "")
        };
        stmts.push(
            d.prepare(format!(
                "SELECT {expr} AS k, COUNT(*) AS n FROM clicks WHERE {filter} AND bot = 0{extra} GROUP BY k ORDER BY n DESC LIMIT 10"
            ))
            .bind(&binds)?,
        );
    }
    if link.is_none() {
        stmts.push(
            d.prepare(
                "SELECT CAST(link_id AS TEXT) AS k, COUNT(*) AS n FROM clicks WHERE ts >= ?1 AND bot = 0 GROUP BY link_id ORDER BY n DESC LIMIT 10",
            )
            .bind(&binds)?,
        );
    }
    let res = d.batch(stmts).await?;

    #[derive(Deserialize)]
    struct Totals {
        humans: Option<i64>,
        bots: Option<i64>,
        qr: Option<i64>,
    }
    let totals = res[0].results::<Totals>()?.into_iter().next();
    let mut agg = Agg {
        humans: totals.as_ref().and_then(|t| t.humans).unwrap_or(0),
        bots: totals.as_ref().and_then(|t| t.bots).unwrap_or(0),
        qr: totals.as_ref().and_then(|t| t.qr).unwrap_or(0),
        series: res[1]
            .results::<Bucket>()?
            .into_iter()
            .map(|b| (b.t, b.n))
            .collect(),
        ..Default::default()
    };
    for (i, col) in dims.iter().enumerate() {
        agg.dims.insert(
            col.to_string(),
            res[2 + i]
                .results::<KV>()?
                .into_iter()
                .map(|r| (r.k, r.n))
                .collect(),
        );
    }
    if link.is_none() {
        agg.top_links = res[2 + dims.len()]
            .results::<KV>()?
            .into_iter()
            .filter_map(|r| r.k.and_then(|k| k.parse().ok()).map(|id| (id, r.n)))
            .collect();
    }
    Ok(agg)
}

async fn ae_stats(
    cfg: &analytics::Config,
    link: Option<i64>,
    since: i64,
    bucket: i64,
) -> Result<Agg> {
    let dims = dims_for(link);
    let sql = analytics::stats_sql(&cfg.dataset, since, bucket, link, &dims);
    let rows = futures_util::future::join_all(sql.iter().map(|q| analytics::query(cfg, q)))
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let first = rows[0].first();
    let mut agg = Agg {
        humans: analytics::num(first.and_then(|r| r.get("humans"))),
        bots: analytics::num(first.and_then(|r| r.get("bots"))),
        qr: analytics::num(first.and_then(|r| r.get("qr"))),
        // Seconds in Analytics Engine, milliseconds in the API.
        series: rows[1]
            .iter()
            .map(|r| {
                (
                    analytics::num(r.get("t")) * 1000,
                    analytics::num(r.get("n")),
                )
            })
            .collect(),
        ..Default::default()
    };
    for (i, col) in dims.iter().enumerate() {
        agg.dims.insert(
            col.to_string(),
            rows[2 + i]
                .iter()
                .map(|r| (analytics::text(r.get("k")), analytics::num(r.get("n"))))
                .collect(),
        );
    }
    if link.is_none() {
        agg.top_links = rows[2 + dims.len()]
            .iter()
            .filter_map(|r| {
                analytics::text(r.get("k"))
                    .and_then(|k| k.parse().ok())
                    .map(|id| (id, analytics::num(r.get("n"))))
            })
            .collect();
    }
    Ok(agg)
}

async fn stats(env: &Env, link: Option<i64>, r: &str) -> Result<Response> {
    let (since, bucket) = range(r);
    // D1 holds clicks from before Analytics Engine was switched on (and all clicks when it
    // is off); both are added up.
    let cfg = analytics::config(env);
    let ae = async {
        match &cfg {
            Some(c) => Some(ae_stats(c, link, since, bucket).await),
            None => None,
        }
    };
    let (d1, ae) = futures_util::future::join(d1_stats(env, link, since, bucket), ae).await;
    let mut agg = d1?;
    let mut analytics_error = None;
    match ae {
        Some(Ok(a)) => agg.merge(a),
        Some(Err(e)) => {
            worker::console_error!("analytics engine stats failed: {e}");
            analytics_error = Some(
                "Analytics Engine could not be queried; showing database clicks only".to_string(),
            );
        }
        None => {}
    }

    let mut dims = serde_json::Map::new();
    for (col, m) in &agg.dims {
        let rows: Vec<KV> = top_n(m, 10).into_iter().map(|(k, n)| KV { k, n }).collect();
        dims.insert(col.clone(), serde_json::to_value(rows).unwrap_or_default());
    }
    let series: Vec<Bucket> = agg
        .series
        .iter()
        .map(|(t, n)| Bucket { t: *t, n: *n })
        .collect();
    let mut out = serde_json::json!({
        "range": r,
        "bucket_ms": bucket,
        "since": since,
        "clicks": agg.humans,
        "bots": agg.bots,
        "qr": agg.qr,
        "series": series,
        "top": dims,
        "analytics_engine": cfg.is_some(),
        "analytics_error": analytics_error,
    });
    if link.is_none() {
        // Extra candidates: deleted links (still in Analytics Engine) are skipped below.
        let top = top_n(&agg.top_links, 20);
        let mut info: HashMap<i64, serde_json::Value> = HashMap::new();
        if !top.is_empty() {
            let marks = vec!["?"; top.len()].join(",");
            let ids: Vec<JsValue> = top.iter().map(|(id, _)| n(*id)).collect();
            for row in store::db(env)?
                .prepare(format!(
                    "SELECT id, slug, url, title FROM links WHERE id IN ({marks})"
                ))
                .bind(&ids)?
                .all()
                .await?
                .results::<serde_json::Value>()?
            {
                if let Some(id) = row.get("id").and_then(|v| v.as_f64()) {
                    info.insert(id as i64, row);
                }
            }
        }
        out["top_links"] = top
            .into_iter()
            .filter_map(|(id, cnt)| {
                info.remove(&id).map(|mut row| {
                    row["n"] = cnt.into();
                    row
                })
            })
            .take(10)
            .collect::<Vec<_>>()
            .into();
        let d = store::db(env)?;
        let res = d
            .batch(vec![
                d.prepare("SELECT COUNT(*) AS n FROM links"),
                d.prepare(format!(
                    "SELECT COUNT(*) AS n FROM links WHERE disabled = 0 AND {}",
                    crate::model::BROKEN_SQL
                )),
            ])
            .await?;
        let count = |i: usize| -> Result<i64> {
            Ok(res[i]
                .results::<CountRow>()?
                .first()
                .map(|c| c.n)
                .unwrap_or(0))
        };
        out["links"] = count(0)?.into();
        out["broken"] = count(1)?.into();
    }
    json(&out, 200)
}

/// What is switched on, for the settings page.
async fn status(env: &Env) -> Result<Response> {
    let last_backup: Option<serde_json::Value> = store::get_state(env, "last_backup")
        .await?
        .and_then(|v| serde_json::from_str(&v).ok());
    json(
        &serde_json::json!({
            "analytics_engine": analytics::enabled(env),
            "backups": backup::enabled(env),
            "last_backup": last_backup,
            "webhook_events": webhooks::EVENTS,
        }),
        200,
    )
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
