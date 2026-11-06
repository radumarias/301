//! Outgoing webhooks: `link.created`, `link.updated`, `link.deleted` and `click`.
//!
//! Each delivery is a JSON POST signed with the hook's secret:
//! `X-301-Signature: sha256=<hex HMAC-SHA256 of the raw body>`.

use crate::model::validate_url;
use crate::store::{self, n, on, os, s, SResult, StoreError};
use crate::util::{self, hex, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
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
    pub events: Vec<String>,
    pub secret: String,
    pub created_at: i64,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    pub last_at: Option<i64>,
}

#[derive(Deserialize)]
struct Row {
    id: i64,
    url: String,
    events: String,
    secret: String,
    created_at: i64,
    last_status: Option<i64>,
    last_error: Option<String>,
    last_at: Option<i64>,
}

impl From<Row> for Hook {
    fn from(r: Row) -> Self {
        Hook {
            id: r.id,
            url: r.url,
            events: serde_json::from_str(&r.events).unwrap_or_default(),
            secret: r.secret,
            created_at: r.created_at,
            last_status: r.last_status,
            last_error: r.last_error,
            last_at: r.last_at,
        }
    }
}

#[derive(Deserialize)]
pub struct HookInput {
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
}

thread_local! {
    static CACHE: RefCell<Option<(i64, Vec<Hook>)>> = const { RefCell::new(None) };
}

fn invalidate() {
    CACHE.with(|c| *c.borrow_mut() = None);
}

pub async fn list(env: &Env) -> worker::Result<Vec<Hook>> {
    let rows = store::db(env)?
        .prepare("SELECT id, url, events, secret, created_at, last_status, last_error, last_at FROM webhooks ORDER BY id")
        .all()
        .await?
        .results::<Row>()?;
    Ok(rows.into_iter().map(Hook::from).collect())
}

async fn cached(env: &Env) -> Vec<Hook> {
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
    let hooks = list(env).await.unwrap_or_default();
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
    let secret = hex(&crate::crypto::random_bytes(24));
    let now = now_ms();
    let row = store::db(env)?
        .prepare("INSERT INTO webhooks (url, events, secret, created_at) VALUES (?1, ?2, ?3, ?4) RETURNING id")
        .bind(&[
            s(input.url.trim()),
            s(&serde_json::to_string(&events).unwrap_or_default()),
            s(&secret),
            n(now),
        ])?
        .first::<store::IdRow>(None)
        .await?
        .ok_or(StoreError::NotFound)?;
    invalidate();
    Ok(Hook {
        id: row.id,
        url: input.url.trim().to_string(),
        events,
        secret,
        created_at: now,
        last_status: None,
        last_error: None,
        last_at: None,
    })
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
        Err(e) => (0, Some(e.to_string().chars().take(300).collect())),
    }
}

async fn record(env: &Env, id: i64, status: i64, error: &Option<String>) {
    if let Ok(d) = store::db(env) {
        if let Ok(stmt) = d
            .prepare(
                "UPDATE webhooks SET last_status = ?2, last_error = ?3, last_at = ?4 WHERE id = ?1",
            )
            .bind(&[n(id), n(status), os(error), on(Some(now_ms()))])
        {
            let _ = stmt.run().await;
        }
    }
}

/// Delivers an event to every hook subscribed to it. Call it from `wait_until`.
/// The delivery status is stored for link events; for clicks only failures are, so
/// a busy link does not turn every click back into a D1 write.
pub async fn fire(env: &Env, event: &str, data: Value) {
    let hooks: Vec<Hook> = if event == "click" {
        cached(env).await
    } else {
        list(env).await.unwrap_or_default()
    };
    let targets: Vec<&Hook> = hooks
        .iter()
        .filter(|h| h.events.iter().any(|e| e == event))
        .collect();
    let results =
        futures_util::future::join_all(targets.iter().map(|h| deliver(h, event, &data))).await;
    for (h, (status, error)) in targets.iter().zip(results) {
        if event != "click" || error.is_some() {
            record(env, h.id, status, &error).await;
        }
    }
}

/// True when some hook wants click events (checked from the in-isolate cache).
pub async fn wants_clicks(env: &Env) -> bool {
    cached(env)
        .await
        .iter()
        .any(|h| h.events.iter().any(|e| e == "click"))
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
