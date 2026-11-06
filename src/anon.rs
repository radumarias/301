//! Public endpoints: client config, anonymous links (no account) and abuse reports.
//!
//! Anonymous links get a random slug, expire after `anon_ttl_days`, and come with a secret
//! manage token (only its hash is stored) that shows stats and deletes the link. Creating
//! one needs a Turnstile pass, is limited per IP, and every destination is checked.

use crate::account;
use crate::auth;
use crate::crypto::{random_token, token_hash};
use crate::model::{validate_url, LinkInput};
use crate::safety;
use crate::store::{self, s, Settings};
use crate::util::{json, json_error, now_ms};
use serde::Deserialize;
use worker::{Env, Method, Request, Response, Result};

const HOUR_MS: i64 = 3_600_000;
const REPORTS_PER_HOUR: i64 = 5;

fn anon_available(env: &Env, st: &Settings) -> bool {
    st.anon_enabled && (safety::turnstile_configured(env) || auth::dev_email(env).is_some())
}

pub async fn config(env: &Env) -> Result<Response> {
    let st = store::settings(env).await.unwrap_or_default();
    let site_key = auth::var(env, "TURNSTILE_SITE_KEY");
    json(
        &serde_json::json!({
            "google": auth::google_configured(env),
            "dev": auth::dev_email(env).is_some(),
            "turnstile_site_key": (safety::turnstile_configured(env)).then_some(site_key),
            "anon": { "enabled": anon_available(env, &st), "ttl_days": st.anon_ttl_days },
        }),
        200,
    )
}

#[derive(Deserialize)]
struct Create {
    url: String,
    #[serde(default)]
    slug_style: Option<String>,
    #[serde(default)]
    turnstile: String,
}

pub async fn create(mut req: Request, env: &Env) -> Result<Response> {
    let st = store::settings(env).await.unwrap_or_default();
    if !anon_available(env, &st) {
        return json_error(
            "Creating links without an account is turned off. Sign in to create links.",
            403,
        );
    }
    let body: Create = match req.json().await {
        Ok(b) => b,
        Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
    };
    let ip = safety::client_ip(&req);
    if safety::turnstile_configured(env)
        && !safety::turnstile_ok(env, &body.turnstile, ip.as_deref()).await
    {
        return json_error(
            "The human check failed. Reload the page and try again.",
            403,
        );
    }
    let ip_hash = account::ip_hash(env, ip.as_deref().unwrap_or("local")).await?;
    let made = account::created_since(env, None, Some(&ip_hash), now_ms() - HOUR_MS).await?;
    if made >= st.anon_per_hour {
        return json_error(
            "Too many links from your network in the last hour. Sign in, or try again later.",
            429,
        );
    }

    let url = body.url.trim().to_string();
    if let Err(e) = validate_url(&url) {
        return json_error(e, 400);
    }
    let host = req
        .url()?
        .host_str()
        .map(str::to_string)
        .unwrap_or_default();
    if let Err(e) = safety::check_urls(env, &host, std::slice::from_ref(&url)).await {
        return json_error(e, 400);
    }

    let input = LinkInput {
        url: Some(url),
        slug_style: body
            .slug_style
            .filter(|s| ["random", "words", "names"].contains(&s.as_str())),
        ..Default::default()
    };
    let mut link = match store::new_link(env, &st, &input, "anonymous").await {
        Ok(l) => l,
        Err(e) => return json_error(e.public_message(), 400),
    };
    let token = random_token(24);
    link.expires_at = Some(link.created_at + st.anon_ttl_days * 24 * HOUR_MS);
    link.manage_hash = Some(token_hash(&token).await?);
    link.creator_ip = Some(ip_hash);
    link.anonymous = true;
    match store::insert_link(env, link).await {
        Ok(link) => json(
            &serde_json::json!({ "link": link, "manage_token": token }),
            201,
        ),
        Err(e) => json_error(e.public_message(), 500),
    }
}

/// `GET /api/anon/<token>`, `GET /api/anon/<token>/stats`, `DELETE /api/anon/<token>`.
pub async fn manage(
    req: &Request,
    env: &Env,
    token: &str,
    rest: &[String],
    range: Option<&str>,
) -> Result<Response> {
    let links = store::query_links(
        env,
        "WHERE manage_hash = ?1",
        &[s(&token_hash(token).await?)],
    )
    .await?;
    let Some(link) = store::with_aliases(env, links).await?.pop() else {
        return json_error(
            "This manage link is not valid. The link may have been deleted or moved to an account.",
            404,
        );
    };
    match (req.method(), rest) {
        (Method::Get, []) => json(&link, 200),
        (Method::Get, [s]) if s == "stats" => {
            crate::api::stats(env, Some(link.id), range.unwrap_or("30d"), None).await
        }
        (Method::Delete, []) => match store::delete_links(env, &[link]).await {
            Ok(()) => json(&serde_json::json!({ "ok": true }), 200),
            Err(e) => json_error(e.public_message(), 500),
        },
        _ => json_error("no such endpoint", 404),
    }
}

#[derive(Deserialize)]
struct Report {
    slug: String,
    reason: String,
}

pub async fn report(mut req: Request, env: &Env) -> Result<Response> {
    let body: Report = match req.json().await {
        Ok(b) => b,
        Err(e) => return json_error(format!("invalid JSON: {e}"), 400),
    };
    let reason = body.reason.trim();
    if reason.is_empty() || reason.len() > 1000 {
        return json_error(
            "Tell us briefly what is wrong (up to 1000 characters).",
            400,
        );
    }
    let ip = safety::client_ip(&req);
    let reporter = account::ip_hash(env, ip.as_deref().unwrap_or("local")).await?;
    if account::reports_since(env, &reporter, now_ms() - HOUR_MS).await? >= REPORTS_PER_HOUR {
        return json_error("Too many reports from your network. Try again later.", 429);
    }
    let slug = body.slug.trim().trim_start_matches('/');
    let Some(link) = store::get_link_by_slug(env, slug).await? else {
        return json_error("No short link with that name.", 404);
    };
    account::add_report(env, link.id, reason, Some(&reporter)).await?;
    json(&serde_json::json!({ "ok": true }), 200)
}
