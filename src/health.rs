//! Broken-link checker: a cron job re-checks every destination about once a day.

use crate::model::{is_broken, Health};
use crate::store::{self, n, on, os};
use crate::util::{self, now_ms};
use futures_util::future::join_all;
use serde::Deserialize;
use worker::{D1PreparedStatement, Env, Headers, Method, Request, RequestInit, RequestRedirect};

/// How often each destination is checked.
pub const INTERVAL_MS: i64 = 24 * 3_600_000;
/// Links per cron run. Each check is up to two subrequests, and the free plan allows
/// 50 subrequests per invocation.
pub const BATCH: usize = 15;
const TIMEOUT_MS: u64 = 10_000;
const UA: &str =
    "Mozilla/5.0 (compatible; 301-link-checker/1.0; +https://github.com/radumarias/301)";

async fn probe(url: &str, method: Method) -> worker::Result<u16> {
    let headers = Headers::new();
    headers.set("User-Agent", UA)?;
    headers.set("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")?;
    let mut init = RequestInit::new();
    init.with_method(method)
        .with_headers(headers)
        .with_redirect(RequestRedirect::Follow);
    let req = Request::new_with_init(url, &init)?;
    Ok(util::fetch_with_timeout(req, TIMEOUT_MS)
        .await?
        .status_code())
}

/// Checks one URL: HEAD first, GET when the server does not like HEAD.
pub async fn check_url(url: &str) -> (i64, Option<String>) {
    let res = match probe(url, Method::Head).await {
        Ok(code) if code == 403 || code == 404 || code == 405 || code >= 500 => {
            probe(url, Method::Get).await
        }
        other => other,
    };
    match res {
        Ok(code) => (code as i64, None),
        Err(e) => (0, Some(util::snippet(&e.to_string()))),
    }
}

fn result_stmt(env: &Env, id: i64, h: &Health) -> worker::Result<D1PreparedStatement> {
    store::db(env)?
        .prepare("UPDATE links SET health_code = ?2, health_error = ?3, health_checked_at = ?4 WHERE id = ?1")
        .bind(&[n(id), n(h.code), os(&h.error), on(Some(h.checked_at))])
}

async fn check(url: &str) -> Health {
    let (code, error) = check_url(url).await;
    Health {
        code,
        error,
        checked_at: now_ms(),
        broken: is_broken(code),
    }
}

/// Checks a link's main destination and stores the result.
pub async fn check_link(env: &Env, id: i64, url: &str) -> worker::Result<Health> {
    let h = check(url).await;
    result_stmt(env, id, &h)?.run().await?;
    Ok(h)
}

/// Checks the links that were never checked or are due, oldest first, and stores all
/// results in one D1 batch.
pub async fn run_batch(env: &Env) -> worker::Result<usize> {
    #[derive(Deserialize)]
    struct Row {
        id: i64,
        url: String,
    }
    let due = store::db(env)?
        .prepare(format!(
            "SELECT id, url FROM links WHERE disabled = 0 AND (health_checked_at IS NULL OR health_checked_at < ?1) \
             ORDER BY health_checked_at IS NOT NULL, health_checked_at LIMIT {BATCH}"
        ))
        .bind(&[n(now_ms() - INTERVAL_MS)])?
        .all()
        .await?
        .results::<Row>()?;
    if due.is_empty() {
        return Ok(0);
    }
    let results = join_all(due.iter().map(|r| check(&r.url))).await;
    let stmts = due
        .iter()
        .zip(&results)
        .map(|(r, h)| result_stmt(env, r.id, h))
        .collect::<worker::Result<Vec<_>>>()?;
    store::db(env)?.batch(stmts).await?;
    Ok(due.len())
}
