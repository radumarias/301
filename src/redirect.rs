//! The public redirect path: one KV read on the hot path, analytics after the response.

use crate::model::{passthrough_query, KvRecord};
use crate::store::{self, Click};
use crate::{pages, ua, util, webhooks};
use std::cell::RefCell;
use std::collections::HashMap;
use worker::{Context, Env, Method, Request, Response, Result};

fn decode_path(path: &str) -> String {
    let p = path.trim_start_matches('/');
    percent_encoding::percent_decode_str(p)
        .decode_utf8_lossy()
        .into_owned()
}

fn referrer_host(req: &Request) -> Option<String> {
    let r = req.headers().get("Referer").ok().flatten()?;
    url::Url::parse(&r)
        .ok()?
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_string())
}

/// Name of the Workers Rate Limiting binding in wrangler.toml (`[[ratelimits]]`).
const LIMITER: &str = "REDIRECT_LIMITER";

/// Seconds a client is told to wait after a 429; matches the binding's `period`.
const RETRY_AFTER_SECS: u32 = 60;

/// The key the limiter counts against: the visitor's IP as seen by Cloudflare.
fn client_key(req: &Request) -> Option<String> {
    req.headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .filter(|ip| !ip.is_empty())
}

/// True when this visitor is over the per-IP limit. Fails open: a missing binding or a
/// limiter error never blocks a redirect.
async fn over_limit(req: &Request, env: &Env) -> bool {
    let Some(key) = client_key(req) else {
        return false;
    };
    let Ok(limiter) = env.rate_limiter(LIMITER) else {
        return false;
    };
    match limiter.limit(key).await {
        Ok(outcome) => !outcome.success,
        Err(e) => {
            worker::console_error!("rate limiter failed: {e}");
            false
        }
    }
}

pub async fn handle(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    if over_limit(&req, &env).await {
        return pages::too_many_requests(RETRY_AFTER_SECS);
    }

    let slug = decode_path(&req.path());

    match slug.as_str() {
        "" => {
            let st = store::settings_cached(&env).await;
            return match st.root_url {
                Some(u) => util::redirect(&u, 302),
                None => pages::landing(),
            };
        }
        "robots.txt" => {
            return Response::ok("User-agent: *\nDisallow: /_/\nDisallow: /api/\n");
        }
        "favicon.ico" | "favicon.svg" => {
            let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#1d5fa8"/><text x="32" y="42" font-family="monospace" font-size="26" font-weight="700" fill="#fff" text-anchor="middle">301</text></svg>"##;
            let mut res = Response::ok(svg)?;
            res.headers_mut().set("Content-Type", "image/svg+xml")?;
            res.headers_mut()
                .set("Cache-Control", "public, max-age=86400")?;
            return Ok(res);
        }
        _ => {}
    }

    let (slug, preview) = match slug.strip_suffix('+') {
        Some(s) => (s.to_string(), true),
        None => (slug, false),
    };

    let Some((rec, needs_fill)) = store::lookup(&env, &slug).await? else {
        let st = store::settings_cached(&env).await;
        return match st.not_found_url {
            Some(u) => util::redirect(&u, 302),
            None => pages::not_found(),
        };
    };

    if needs_fill {
        // Read-through: cache the D1 record in KV after the response is sent.
        let (fill_env, fill_slug, fill_rec) = (env.clone(), slug.clone(), rec.clone());
        ctx.wait_until(async move {
            let _ = store::fill_kv(&fill_env, &fill_slug, &fill_rec).await;
        });
    }

    if rec.disabled {
        return pages::gone("It was disabled by its owner.");
    }
    let now = util::now_ms();
    if rec.expires_at.is_some_and(|e| now >= e) || rec.exhausted {
        return expired(&rec);
    }

    let ua_str = req
        .headers()
        .get("User-Agent")
        .ok()
        .flatten()
        .unwrap_or_default();
    let info = ua::parse(&ua_str);
    let is_get = req.method() == Method::Get;
    let url = req.url()?;
    let cf = req.cf();
    let country = cf.as_ref().and_then(|c| c.country());
    // Everything the click log needs, gathered once; the caller fills in the rest.
    let click = Click {
        link_id: rec.id,
        slug: slug.clone(),
        ts: now,
        country: country.clone(),
        city: cf.as_ref().and_then(|c| c.city()),
        referrer: referrer_host(&req),
        device: info.device,
        os: info.os,
        browser: info.browser,
        bot: info.bot || req.method() == Method::Head,
        qr: url.query_pairs().any(|(k, _)| k == "qr"),
        variant: None,
        counted: false,
    };

    // Chat apps and social networks get the custom preview card instead of a redirect.
    if is_get && !preview && !rec.og.is_empty() && ua::is_preview_crawler(&ua_str) {
        let dest = rec.pw.is_none().then_some(rec.url.as_str());
        log_click(&env, &ctx, Click { bot: true, ..click });
        return pages::social_card(&rec.og, dest);
    }

    if let Some(hash) = &rec.pw {
        if req.method() != Method::Post {
            return pages::password(&slug, url.query(), false);
        }
        let form = req.form_data().await.ok();
        let given = form
            .and_then(|f| f.get_field("password"))
            .unwrap_or_default();
        if !crate::crypto::verify_password(given.trim(), hash).await {
            return pages::password(&slug, url.query(), true);
        }
    }

    // Rejection sampling: 256 is not a multiple of 100, so a plain `% 100` would favour
    // rolls 0..56 and skew the split towards the variants.
    let roll = || loop {
        let b = crate::crypto::random_bytes(1)[0];
        if b < 200 {
            break b % 100;
        }
    };
    let (picked, split) = rec.destination(info.os, info.device, country.as_deref(), roll);
    let variant = split.then(|| picked.to_string());
    let dest = if rec.passthrough {
        passthrough_query(picked, url.query())
    } else {
        picked.to_string()
    };

    if preview {
        return pages::preview(&slug, &dest);
    }

    // Click limits: count atomically before redirecting, so the limit is exact. Once it
    // is used up the KV record says so and later visitors cost no D1 access at all.
    let mut counted = false;
    if rec.max_clicks.is_some() && !click.bot {
        match store::consume_click(&env, rec.id, now).await {
            Ok(store::Consume::Counted { last }) => {
                counted = true;
                if last {
                    resync_later(&env, &ctx, rec.id);
                }
            }
            Ok(store::Consume::Exhausted) => {
                // Edge caches may still serve the old record for a minute; one resync
                // per isolate is enough to fix it.
                if first_resync(rec.id, now) {
                    resync_later(&env, &ctx, rec.id);
                }
                return expired(&rec);
            }
            // Fail open: a D1 hiccup should not break the link.
            Err(e) => worker::console_error!("click limit check failed: {e}"),
        }
    }

    let bot = click.bot;
    log_click(
        &env,
        &ctx,
        Click {
            variant,
            counted,
            ..click
        },
    );

    // Phones with an app link try the app first and fall back to the web destination.
    if is_get && !bot {
        if let Some(app) = rec.app.for_os(info.os) {
            return pages::open_app(app, &dest);
        }
    }

    // A password form POST must become a GET at the destination.
    let status = if rec.pw.is_some() {
        303
    } else {
        rec.redirect_type
    };
    util::redirect(&dest, status)
}

fn expired(rec: &KvRecord) -> Result<Response> {
    match &rec.fallback {
        Some(f) => util::redirect(f, 302),
        None => pages::gone("It has expired."),
    }
}

thread_local! {
    /// Links this isolate resynced recently, with when.
    static RESYNCED: RefCell<HashMap<i64, i64>> = RefCell::new(HashMap::new());
}

/// True the first time in a minute this isolate sees `id` used up.
fn first_resync(id: i64, now: i64) -> bool {
    RESYNCED.with(|m| {
        let mut m = m.borrow_mut();
        m.retain(|_, at| now - *at < 60_000);
        // Keep the first time: refreshing it on every hit would never let it expire.
        match m.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(now);
                true
            }
        }
    })
}

/// Rewrites the link's KV records from D1 after the response (e.g. limit reached).
fn resync_later(env: &Env, ctx: &Context, id: i64) {
    let env = env.clone();
    ctx.wait_until(async move {
        if let Err(e) = store::resync(&env, id).await {
            worker::console_error!("kv resync failed: {}", e.public_message());
        }
    });
}

/// Logs the click (and fires click webhooks) after the response is sent.
fn log_click(env: &Env, ctx: &Context, click: Click) {
    let env = env.clone();
    ctx.wait_until(async move {
        if let Err(e) = store::record_click(&env, &click).await {
            worker::console_error!("click write failed: {e}");
        }
        if !click.bot {
            webhooks::fire_click(&env, &click).await;
        }
    });
}
