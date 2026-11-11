//! Outgoing webhooks: `link.created`, `link.updated`, `link.deleted` and `click`.
//!
//! Each delivery is a JSON POST signed with the hook's secret:
//! `X-301-Signature: sha256=<hex HMAC-SHA256 of the raw body>`.

use crate::model::validate_url;
use crate::store::{self, n, on, os, s, SResult, StoreError};
use crate::util::{self, hex, now_ms};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use worker::{Env, Headers, Method, Request, RequestInit};

pub const EVENTS: [&str; 4] = ["link.created", "link.updated", "link.deleted", "click"];
const TIMEOUT_MS: u64 = 5_000;
/// The redirect path reads hooks from an in-isolate cache refreshed this often, so a
/// click costs no extra KV or D1 read.
const CACHE_MS: i64 = 60_000;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hook {
    pub id: i64,
    pub url: String,
    /// Stored as a JSON array in a TEXT column.
    #[serde(deserialize_with = "json_text")]
    pub events: Vec<String>,
    pub secret: String,
    pub created_at: i64,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    pub last_at: Option<i64>,
}

fn json_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let s = String::deserialize(d)?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

impl Hook {
    fn wants(&self, event: &str) -> bool {
        self.events.iter().any(|e| e == event)
    }
}

#[derive(Deserialize)]
pub struct HookInput {
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
}

thread_local! {
    static CACHE: RefCell<Option<(i64, Rc<Vec<Hook>>)>> = const { RefCell::new(None) };
}

fn invalidate() {
    CACHE.with(|c| *c.borrow_mut() = None);
}

const COLS: &str = "id, url, events, secret, created_at, last_status, last_error, last_at";

pub async fn list(env: &Env) -> worker::Result<Vec<Hook>> {
    store::db(env)?
        .prepare(format!("SELECT {COLS} FROM webhooks ORDER BY id"))
        .all()
        .await?
        .results::<Hook>()
}

async fn cached(env: &Env) -> Rc<Vec<Hook>> {
    let now = now_ms();
    if let Some(hooks) = CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(at, _)| now - at < CACHE_MS)
            .map(|(_, h)| h.clone())
    }) {
        return hooks;
    }
    // A missing table (migration not applied yet) just means "no hooks".
    let hooks = Rc::new(list(env).await.unwrap_or_default());
    CACHE.with(|c| *c.borrow_mut() = Some((now, hooks.clone())));
    hooks
}

pub fn validate(input: &HookInput) -> Result<Vec<String>, String> {
    validate_url(&input.url)?;
    let mut events: Vec<String> = input.events.iter().map(|e| e.trim().to_string()).collect();
    events.sort();
    events.dedup();
    if events.is_empty() {
        return Err("pick at least one event".into());
    }
    if let Some(bad) = events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
        return Err(format!("unknown event {bad}"));
    }
    Ok(events)
}

pub async fn create(env: &Env, input: &HookInput) -> SResult<Hook> {
    let events = validate(input).map_err(StoreError::Bad)?;
    let hook = store::db(env)?
        .prepare(format!(
            "INSERT INTO webhooks (url, events, secret, created_at) VALUES (?1, ?2, ?3, ?4) RETURNING {COLS}"
        ))
        .bind(&[
            s(input.url.trim()),
            s(&serde_json::to_string(&events).unwrap_or_default()),
            s(&hex(&crate::crypto::random_bytes(24))),
            n(now_ms()),
        ])?
        .first::<Hook>(None)
        .await?
        .ok_or(StoreError::NotFound)?;
    invalidate();
    Ok(hook)
}

pub async fn delete(env: &Env, id: i64) -> SResult<()> {
    let res = store::db(env)?
        .prepare("DELETE FROM webhooks WHERE id = ?1")
        .bind(&[n(id)])?
        .run()
        .await?;
    invalidate();
    if res.meta()?.and_then(|m| m.changes).unwrap_or(0) == 0 {
        return Err(StoreError::NotFound);
    }
    Ok(())
}

/// Sends one delivery; returns the HTTP status (0 on a network error) and error text.
pub async fn deliver(hook: &Hook, event: &str, data: &Value) -> (i64, Option<String>) {
    let body = serde_json::json!({
        "event": event,
        "at": util::iso(now_ms()),
        "data": data,
    })
    .to_string();
    let send = async {
        let sig = crate::crypto::hmac_sha256(hook.secret.as_bytes(), body.as_bytes()).await?;
        let headers = Headers::new();
        headers.set("Content-Type", "application/json")?;
        headers.set("User-Agent", "301-webhooks/1.0")?;
        headers.set("X-301-Event", event)?;
        headers.set("X-301-Signature", &format!("sha256={}", hex(&sig)))?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(body.clone().into()));
        let req = Request::new_with_init(&hook.url, &init)?;
        util::fetch_with_timeout(req, TIMEOUT_MS)
            .await
            .map(|r| r.status_code())
    };
    match send.await {
        Ok(code) if (200..300).contains(&code) => (code as i64, None),
        Ok(code) => (code as i64, Some(format!("HTTP {code}"))),
        Err(e) => (0, Some(util::snippet(&e.to_string()))),
    }
}

/// Delivers to every hook in `hooks` that wants `event`, all at once.
async fn deliver_all(hooks: &[Hook], event: &str, data: &Value) -> Vec<(i64, i64, Option<String>)> {
    let targets: Vec<&Hook> = hooks.iter().filter(|h| h.wants(event)).collect();
    let results = join_all(targets.iter().map(|h| deliver(h, event, data))).await;
    targets
        .iter()
        .zip(results)
        .map(|(h, (status, error))| (h.id, status, error))
        .collect()
}

/// Stores the delivery outcomes in one D1 batch.
async fn record(env: &Env, outcomes: &[(i64, i64, Option<String>)]) -> worker::Result<()> {
    if outcomes.is_empty() {
        return Ok(());
    }
    let d = store::db(env)?;
    let now = now_ms();
    let mut stmts = vec![];
    for (id, status, error) in outcomes {
        stmts.push(
            d.prepare(
                "UPDATE webhooks SET last_status = ?2, last_error = ?3, last_at = ?4 WHERE id = ?1",
            )
            .bind(&[n(*id), n(*status), os(error), on(Some(now))])?,
        );
    }
    d.batch(stmts).await?;
    Ok(())
}

/// Sends a link event (`link.created` ...) with one delivery per hook: `data` is the
/// link for single changes and an array of links for bulk actions, so a 200-link bulk
/// action stays within the subrequest limit. Call it from `wait_until`.
pub async fn fire_links(env: &Env, event: &str, mut links: Vec<Value>) {
    let hooks = list(env).await.unwrap_or_default();
    if links.is_empty() || !hooks.iter().any(|h| h.wants(event)) {
        return;
    }
    let data = if links.len() == 1 {
        links.remove(0)
    } else {
        Value::Array(links)
    };
    let outcomes = deliver_all(&hooks, event, &data).await;
    if let Err(e) = record(env, &outcomes).await {
        worker::console_error!("webhook status write failed: {e}");
    }
}

/// Sends a `click` event. Hooks come from the in-isolate cache, and only failures (and
/// the first success after one) are stored, so a busy link does not turn every click
/// back into a D1 write.
pub async fn fire_click(env: &Env, click: &crate::store::Click) {
    let hooks = cached(env).await;
    if !hooks.iter().any(|h| h.wants("click")) {
        return;
    }
    let mut data = serde_json::to_value(click).unwrap_or_default();
    data["link"] = serde_json::json!({ "id": click.link_id, "slug": click.slug });
    let failed: Vec<_> = deliver_all(&hooks, "click", &data)
        .await
        .into_iter()
        .filter(|o| o.2.is_some() || hooks.iter().any(|h| h.id == o.0 && h.last_error.is_some()))
        .collect();
    if !failed.is_empty() {
        let _ = record(env, &failed).await;
        // The stored error state changed: reload it so the next success is not dropped.
        invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_events() {
        let ok = HookInput {
            url: "https://example.com/hook".into(),
            events: vec!["click".into(), "link.created".into(), "click".into()],
        };
        assert_eq!(validate(&ok).unwrap(), vec!["click", "link.created"]);
        let bad = HookInput {
            url: "https://example.com/hook".into(),
            events: vec!["nope".into()],
        };
        assert!(validate(&bad).is_err());
        let none = HookInput {
            url: "https://example.com/hook".into(),
            events: vec![],
        };
        assert!(validate(&none).is_err());
    }
}
