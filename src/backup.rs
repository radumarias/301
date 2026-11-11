//! Nightly backups to R2 (click retention is `store::purge_old_clicks`, hourly).
//!
//! Each night writes `daily/<YYYY-MM-DD>.json` with every link (password hashes
//! included), alias, setting and webhook, so the whole admin state can be restored.
//! Raw click rows are not included (they can be large); link totals are.

use crate::store::{self, n};
use crate::util::{iso, now_ms};
use serde::Serialize;
use serde_json::Value;
use worker::Env;

pub const BINDING: &str = "BACKUPS";
const PREFIX: &str = "daily/";
const DAY_MS: i64 = 86_400_000;

pub fn enabled(env: &Env) -> bool {
    env.bucket(BINDING).is_ok()
}

async fn dump(env: &Env, table: &str, key: &str) -> worker::Result<Vec<Value>> {
    let d = store::db(env)?;
    let mut out: Vec<Value> = vec![];
    let mut after = Value::from(0);
    // Keyset paging keeps each D1 response small.
    loop {
        let page = d
            .prepare(format!(
                "SELECT * FROM {table} WHERE {key} > ?1 ORDER BY {key} LIMIT 1000"
            ))
            .bind(&[if key == "id" {
                n(after.as_i64().unwrap_or(0))
            } else {
                store::s(after.as_str().unwrap_or(""))
            }])?
            .all()
            .await?
            .results::<Value>()?;
        let done = page.len() < 1000;
        if let Some(last) = page.last().and_then(|r| r.get(key)) {
            after = last.clone();
        }
        out.extend(page);
        if done {
            return Ok(out);
        }
    }
}

#[derive(Serialize)]
pub struct BackupInfo {
    pub key: String,
    pub size: u64,
    pub uploaded: i64,
}

/// Writes today's backup and returns its key.
pub async fn run(env: &Env) -> worker::Result<String> {
    let bucket = env.bucket(BINDING)?;
    let now = now_ms();
    let (links, aliases, settings, webhooks) = futures_util::future::try_join4(
        dump(env, "links", "id"),
        dump(env, "aliases", "slug"),
        dump(env, "settings", "key"),
        dump(env, "webhooks", "id"),
    )
    .await?;
    let body = serde_json::json!({
        "format": "xorio-301-backup",
        "version": 1,
        "created_at": iso(now),
        "links": links,
        "aliases": aliases,
        "settings": settings,
        "webhooks": webhooks,
    });
    let key = format!("{PREFIX}{}.json", &iso(now)[..10]);
    bucket
        .put(&key, serde_json::to_vec(&body)?)
        .execute()
        .await?;
    store::set_state(
        env,
        "last_backup",
        &serde_json::json!({ "key": key, "at": now }).to_string(),
    )
    .await?;
    Ok(key)
}

pub async fn list(env: &Env) -> worker::Result<Vec<BackupInfo>> {
    let bucket = env.bucket(BINDING)?;
    let mut out = vec![];
    let mut cursor: Option<String> = None;
    loop {
        let mut req = bucket.list().prefix(PREFIX);
        if let Some(c) = &cursor {
            req = req.cursor(c.clone());
        }
        let page = req.execute().await?;
        for o in page.objects() {
            out.push(BackupInfo {
                key: o.key(),
                size: o.size(),
                uploaded: o.uploaded().as_millis() as i64,
            });
        }
        if !page.truncated() {
            break;
        }
        cursor = page.cursor();
    }
    out.sort_by(|a, b| b.key.cmp(&a.key));
    Ok(out)
}

/// Deletes backups older than `keep_days`.
pub async fn prune(env: &Env, keep_days: u32) -> worker::Result<usize> {
    let cutoff = now_ms() - keep_days as i64 * DAY_MS;
    let old: Vec<String> = list(env)
        .await?
        .into_iter()
        .filter(|b| b.uploaded < cutoff)
        .map(|b| b.key)
        .collect();
    let count = old.len();
    if count > 0 {
        env.bucket(BINDING)?.delete_multiple(old).await?;
    }
    Ok(count)
}

/// Only keys this module writes may be downloaded.
pub fn valid_key(key: &str) -> bool {
    key.strip_prefix(PREFIX)
        .and_then(|k| k.strip_suffix(".json"))
        .is_some_and(|d| d.len() == 10 && d.chars().all(|c| c.is_ascii_digit() || c == '-'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys() {
        assert!(super::valid_key("daily/2026-11-06.json"));
        assert!(!super::valid_key("daily/../x.json"));
        assert!(!super::valid_key("other/2026-11-06.json"));
    }
}
