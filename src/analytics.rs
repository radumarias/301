//! Click analytics on Workers Analytics Engine.
//!
//! When the `CLICKS_AE` binding, the `ANALYTICS_API_TOKEN` secret and the
//! `CLOUDFLARE_ACCOUNT_ID` var are all present, clicks are written to Analytics Engine
//! instead of D1 (zero D1 writes per click), stats are read back through the SQL API,
//! and a cron job rolls the counts into `links.clicks`. Without them the Worker keeps
//! logging to the D1 `clicks` table, so nothing breaks before the token is set.
//!
//! Data point layout (keep in sync with the SQL below):
//!   index1  link id
//!   blob1..blob8  country, city, referrer, device, os, browser, slug,
//!                 A/B destination ('' = no split)
//!   double1 bot (0/1), double2 qr (0/1), double3 already counted in D1 (0/1)

use crate::store::{self, Click};
use crate::util::{self, now_ms};
use serde_json::Value;
use worker::{AnalyticsEngineDataPointBuilder, Env, Headers, Method, Request, RequestInit};

pub const BINDING: &str = "CLICKS_AE";
pub const DEFAULT_DATASET: &str = "xorio_301_clicks";
const ROLLUP_CURSOR: &str = "ae_rollup_cursor";
/// Analytics Engine ingests within seconds; stay this far behind "now" when rolling up.
const ROLLUP_LAG_SECS: i64 = 120;

pub struct Config {
    pub account: String,
    pub token: String,
    pub dataset: String,
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|v| v.to_string())
        .filter(|v| !v.trim().is_empty())
}

/// Only lowercase letters, digits and `_`: the name is interpolated into SQL.
pub fn valid_dataset(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn config(env: &Env) -> Option<Config> {
    env.analytics_engine(BINDING).ok()?;
    let token = env.secret("ANALYTICS_API_TOKEN").ok()?.to_string();
    let account = var(env, "CLOUDFLARE_ACCOUNT_ID")?;
    let dataset = var(env, "AE_DATASET").unwrap_or_else(|| DEFAULT_DATASET.into());
    (valid_dataset(&dataset) && !token.is_empty()).then_some(Config {
        account,
        token,
        dataset,
    })
}

pub fn enabled(env: &Env) -> bool {
    config(env).is_some()
}

pub fn write(env: &Env, c: &Click) -> worker::Result<()> {
    let ds = env.analytics_engine(BINDING)?;
    let id = c.link_id.to_string();
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();
    AnalyticsEngineDataPointBuilder::new()
        .indexes([id.as_str()])
        .blobs([
            opt(&c.country),
            opt(&c.city),
            opt(&c.referrer),
            c.device.to_string(),
            c.os.to_string(),
            c.browser.to_string(),
            c.slug.clone(),
            opt(&c.variant),
        ])
        .doubles([
            c.bot as u8 as f64,
            c.qr as u8 as f64,
            c.counted as u8 as f64,
        ])
        .write_to(&ds)
}

/// Runs a query against the Analytics Engine SQL API and returns its `data` rows.
pub async fn query(cfg: &Config, sql: &str) -> worker::Result<Vec<Value>> {
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/analytics_engine/sql",
        cfg.account
    );
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {}", cfg.token))?;
    headers.set("Content-Type", "text/plain")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(format!("{sql} FORMAT JSON").into()));
    let req = Request::new_with_init(&url, &init)?;
    let mut res = util::fetch_with_timeout(req, 15_000).await?;
    let text = res.text().await?;
    if res.status_code() != 200 {
        return Err(worker::Error::RustError(format!(
            "analytics engine query failed ({}): {}",
            res.status_code(),
            util::snippet(&text)
        )));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(v.get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Analytics Engine returns 64-bit numbers as strings in JSON; accept both.
pub fn num(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0).round() as i64,
        Some(Value::String(s)) => s.parse::<f64>().map(|f| f.round() as i64).unwrap_or(0),
        _ => 0,
    }
}

pub fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Column holding each stats dimension in the data point.
pub fn dim_column(dim: &str) -> Option<&'static str> {
    Some(match dim {
        "country" => "blob1",
        "city" => "blob2",
        "referrer" => "blob3",
        "device" => "blob4",
        "os" => "blob5",
        "browser" => "blob6",
        "variant" => "blob8",
        _ => return None,
    })
}

/// SQL `WHERE` for a time range (seconds) and an optional link.
pub fn filter(since_ms: i64, link: Option<i64>) -> String {
    let mut f = format!("timestamp >= toDateTime({})", since_ms / 1000);
    if let Some(id) = link {
        f.push_str(&format!(" AND index1 = '{id}'"));
    }
    f
}

/// The stats queries for one range, in the order `Stats::from_ae` reads them.
pub fn stats_sql(
    ds: &str,
    since_ms: i64,
    bucket_ms: i64,
    link: Option<i64>,
    dims: &[&str],
) -> Vec<String> {
    let f = filter(since_ms, link);
    let b = bucket_ms / 1000;
    let mut q = vec![
        format!("SELECT SUM(_sample_interval * (1 - double1)) AS humans, SUM(_sample_interval * double1) AS bots, SUM(_sample_interval * (1 - double1) * double2) AS qr FROM {ds} WHERE {f}"),
        format!("SELECT intDiv(toUInt32(timestamp), {b}) * {b} AS t, SUM(_sample_interval) AS n FROM {ds} WHERE {f} AND double1 = 0 GROUP BY t ORDER BY t"),
    ];
    for d in dims {
        let col = dim_column(d).unwrap_or("blob1");
        let extra = if *d == "variant" {
            " AND blob8 != ''"
        } else {
            ""
        };
        q.push(format!(
            "SELECT {col} AS k, SUM(_sample_interval) AS n FROM {ds} WHERE {f} AND double1 = 0{extra} GROUP BY k ORDER BY n DESC LIMIT 10"
        ));
    }
    if link.is_none() {
        q.push(format!(
            "SELECT index1 AS k, SUM(_sample_interval) AS n FROM {ds} WHERE {f} AND double1 = 0 GROUP BY k ORDER BY n DESC LIMIT 10"
        ));
    }
    q
}

/// Rolls human clicks logged since the last run into `links.clicks` / `last_click_at`.
/// Clicks on max-clicks links are skipped: those are counted in D1 when they happen.
pub async fn rollup(env: &Env) -> worker::Result<usize> {
    let Some(cfg) = config(env) else {
        return Ok(0);
    };
    let from: i64 = store::get_state(env, ROLLUP_CURSOR)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let until = now_ms() / 1000 - ROLLUP_LAG_SECS;
    if until <= from {
        return Ok(0);
    }
    let ds = &cfg.dataset;
    let rows = query(
        &cfg,
        &format!(
            "SELECT index1 AS id, SUM(_sample_interval) AS n, MAX(toUInt32(timestamp)) AS last FROM {ds} \
             WHERE double1 = 0 AND double3 = 0 AND timestamp >= toDateTime({from}) AND timestamp < toDateTime({until}) GROUP BY id"
        ),
    )
    .await?;
    let d = store::db(env)?;
    let mut stmts = vec![];
    for r in &rows {
        let Some(id) = text(r.get("id")).and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        stmts.push(
            d.prepare("UPDATE links SET clicks = clicks + ?2, last_click_at = MAX(COALESCE(last_click_at, 0), ?3) WHERE id = ?1")
                .bind(&[store::n(id), store::n(num(r.get("n"))), store::n(num(r.get("last")) * 1000)])?,
        );
    }
    let updated = stmts.len();
    // Counts and the cursor move together, so a failed run is simply retried.
    stmts.push(store::set_state_stmt(
        &d,
        ROLLUP_CURSOR,
        &until.to_string(),
    )?);
    d.batch(stmts).await?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_names() {
        assert!(valid_dataset("xorio_301_clicks"));
        assert!(!valid_dataset("x; DROP"));
        assert!(!valid_dataset(""));
    }

    #[test]
    fn numbers_as_strings() {
        assert_eq!(num(Some(&Value::String("12".into()))), 12);
        assert_eq!(num(Some(&serde_json::json!(3.0))), 3);
        assert_eq!(num(None), 0);
    }

    #[test]
    fn sql_shape() {
        let q = stats_sql(
            "ds",
            86_400_000,
            3_600_000,
            Some(7),
            &["country", "variant"],
        );
        assert_eq!(q.len(), 4);
        assert!(q[0].contains("index1 = '7'") && q[0].contains("toDateTime(86400)"));
        assert!(q[1].contains("intDiv(toUInt32(timestamp), 3600) * 3600"));
        assert!(q[3].contains("blob8 != ''"));
        assert_eq!(stats_sql("ds", 0, 86_400_000, None, &[]).len(), 3);
    }
}
