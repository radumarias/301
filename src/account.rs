//! Accounts: users, sessions, the server secret, link ownership and creation limits.

use crate::crypto::{b64url, hmac_sha256, random_bytes, random_token, token_hash};
use crate::store::{db, n, os, s, CountRow, IdRow};
use crate::util::now_ms;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use worker::Env;

/// Sessions last 30 days.
pub const SESSION_TTL_MS: i64 = 30 * 24 * 3_600_000;
pub const SESSION_COOKIE: &str = "__Host-301s";

thread_local! {
    static SECRET: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    static USER_IDS: RefCell<HashMap<String, i64>> = RefCell::new(HashMap::new());
}

/// A per-deployment random key, generated on first use and kept in D1 (`settings`).
/// Used to hash IPs; never leaves the Worker.
pub async fn server_secret(env: &Env) -> worker::Result<Vec<u8>> {
    if let Some(k) = SECRET.with(|c| c.borrow().clone()) {
        return Ok(k);
    }
    #[derive(Deserialize)]
    struct Row {
        value: String,
    }
    let d = db(env)?;
    let res = d
        .batch(vec![
            d.prepare("INSERT OR IGNORE INTO settings (key, value) VALUES ('secret', ?1)")
                .bind(&[s(&b64url(&random_bytes(32)))])?,
            d.prepare("SELECT value FROM settings WHERE key = 'secret'"),
        ])
        .await?;
    let row = res[1]
        .results::<Row>()?
        .pop()
        .ok_or_else(|| worker::Error::RustError("server secret missing".into()))?;
    let key = row.value.into_bytes();
    SECRET.with(|c| *c.borrow_mut() = Some(key.clone()));
    Ok(key)
}

/// Keyed hash of a visitor IP, so limits work without storing IPs.
pub async fn ip_hash(env: &Env, ip: &str) -> worker::Result<String> {
    let key = server_secret(env).await?;
    Ok(b64url(&hmac_sha256(&key, ip.as_bytes()).await?[..18]))
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: Option<String>,
    pub banned: i64,
}

/// Creates or refreshes the account for a verified Google identity.
pub async fn upsert_google_user(
    env: &Env,
    sub: &str,
    email: &str,
    name: Option<&str>,
) -> worker::Result<User> {
    let d = db(env)?;
    let now = now_ms();
    let name = name.map(str::to_string);
    let res = d
        .batch(vec![
            // An account first seen through Cloudflare Access (no Google id yet) is linked.
            d.prepare("UPDATE users SET google_sub = ?1 WHERE email = ?2 AND google_sub IS NULL")
                .bind(&[s(sub), s(email)])?,
            d.prepare(
                "INSERT INTO users (google_sub, email, name, created_at, last_login_at) VALUES (?1, ?2, ?3, ?4, ?4) \
                 ON CONFLICT(google_sub) DO UPDATE SET email = excluded.email, name = excluded.name, last_login_at = excluded.last_login_at \
                 RETURNING id, email, name, banned",
            )
            .bind(&[s(sub), s(email), os(&name), n(now)])?,
        ])
        .await?;
    res[1]
        .results::<User>()?
        .pop()
        .ok_or_else(|| worker::Error::RustError("user upsert returned nothing".into()))
}

/// The account id for an email (Cloudflare Access / local dev identities), created on first use.
pub async fn user_id_for_email(env: &Env, email: &str) -> worker::Result<i64> {
    if let Some(id) = USER_IDS.with(|c| c.borrow().get(email).copied()) {
        return Ok(id);
    }
    let row = db(env)?
        .prepare(
            "INSERT INTO users (email, created_at) VALUES (?1, ?2) \
             ON CONFLICT(email) DO UPDATE SET email = excluded.email RETURNING id",
        )
        .bind(&[s(email), n(now_ms())])?
        .first::<IdRow>(None)
        .await?
        .ok_or_else(|| worker::Error::RustError("user insert returned nothing".into()))?;
    USER_IDS.with(|c| c.borrow_mut().insert(email.to_string(), row.id));
    Ok(row.id)
}

pub async fn email_of(env: &Env, user_id: Option<i64>) -> worker::Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        email: String,
    }
    let Some(id) = user_id else { return Ok(None) };
    Ok(db(env)?
        .prepare("SELECT email FROM users WHERE id = ?1")
        .bind(&[n(id)])?
        .first::<Row>(None)
        .await?
        .map(|r| r.email))
}

/// Starts a session and returns the cookie value (only its hash is stored).
pub async fn create_session(env: &Env, user_id: i64) -> worker::Result<String> {
    let token = random_token(32);
    let now = now_ms();
    let d = db(env)?;
    d.batch(vec![
        d.prepare("DELETE FROM sessions WHERE user_id = ?1 AND expires_at <= ?2")
            .bind(&[n(user_id), n(now)])?,
        d.prepare(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&[
            s(&token_hash(&token).await?),
            n(user_id),
            n(now),
            n(now + SESSION_TTL_MS),
        ])?,
    ])
    .await?;
    Ok(token)
}

pub async fn session_user(env: &Env, token: &str) -> worker::Result<Option<User>> {
    db(env)?
        .prepare(
            "SELECT u.id, u.email, u.name, u.banned FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ?1 AND s.expires_at > ?2",
        )
        .bind(&[s(&token_hash(token).await?), n(now_ms())])?
        .first::<User>(None)
        .await
}

pub async fn end_session(env: &Env, token: &str) -> worker::Result<()> {
    db(env)?
        .prepare("DELETE FROM sessions WHERE token_hash = ?1")
        .bind(&[s(&token_hash(token).await?)])?
        .run()
        .await?;
    Ok(())
}

/// A `__Host-` cookie: HTTPS only, not readable by scripts, sent on top-level navigation.
pub fn cookie_header(name: &str, value: &str, max_age_s: i64) -> String {
    format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age_s}")
}

pub fn session_cookie(token: &str, max_age_s: i64) -> String {
    cookie_header(SESSION_COOKIE, token, max_age_s)
}

/// Reads a cookie from a request.
pub fn cookie(req: &worker::Request, name: &str) -> Option<String> {
    let cookies = req.headers().get("Cookie").ok().flatten()?;
    cookies
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Gives links from before accounts existed to an admin (idempotent; cheap once done).
pub async fn claim_legacy_links(env: &Env, admin_id: i64) -> worker::Result<()> {
    db(env)?
        .prepare("UPDATE links SET owner_id = ?1 WHERE owner_id IS NULL AND manage_hash IS NULL")
        .bind(&[n(admin_id)])?
        .run()
        .await?;
    Ok(())
}

/// Moves anonymous links (by their manage tokens) to an account. Returns how many moved.
pub async fn claim_anonymous(env: &Env, user_id: i64, tokens: &[String]) -> worker::Result<i64> {
    let d = db(env)?;
    let mut stmts = vec![];
    for t in tokens.iter().take(50) {
        stmts.push(
            d.prepare(
                "UPDATE links SET owner_id = ?1, manage_hash = NULL, creator_ip = NULL WHERE manage_hash = ?2",
            )
            .bind(&[n(user_id), s(&token_hash(t).await?)])?,
        );
    }
    if stmts.is_empty() {
        return Ok(0);
    }
    let mut moved = 0;
    for r in d.batch(stmts).await? {
        moved += r.meta()?.and_then(|m| m.changes).unwrap_or(0) as i64;
    }
    Ok(moved)
}

/// Links created since `since` by an owner or (anonymous) by an IP hash.
pub async fn created_since(
    env: &Env,
    owner_id: Option<i64>,
    ip: Option<&str>,
    since: i64,
) -> worker::Result<i64> {
    let (sql, binds) = match (owner_id, ip) {
        (Some(o), _) => (
            "SELECT COUNT(*) AS n FROM links WHERE owner_id = ?1 AND created_at >= ?2",
            vec![n(o), n(since)],
        ),
        (None, Some(ip)) => (
            "SELECT COUNT(*) AS n FROM links WHERE creator_ip = ?1 AND created_at >= ?2",
            vec![s(ip), n(since)],
        ),
        _ => return Ok(0),
    };
    Ok(db(env)?
        .prepare(sql)
        .bind(&binds)?
        .first::<CountRow>(None)
        .await?
        .map(|c| c.n)
        .unwrap_or(0))
}

#[derive(Deserialize, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub email: String,
    pub name: Option<String>,
    pub banned: i64,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
    pub links: i64,
}

pub async fn list_users(env: &Env) -> worker::Result<Vec<UserRow>> {
    db(env)?
        .prepare(
            "SELECT u.id, u.email, u.name, u.banned, u.created_at, u.last_login_at, \
             (SELECT COUNT(*) FROM links WHERE owner_id = u.id) AS links \
             FROM users u ORDER BY u.last_login_at DESC NULLS LAST, u.id DESC LIMIT 500",
        )
        .all()
        .await?
        .results::<UserRow>()
}

/// Bans or unbans an account; a ban also signs it out everywhere.
pub async fn set_banned(env: &Env, user_id: i64, banned: bool) -> worker::Result<()> {
    let d = db(env)?;
    let mut stmts = vec![d
        .prepare("UPDATE users SET banned = ?1 WHERE id = ?2")
        .bind(&[n(banned as i64), n(user_id)])?];
    if banned {
        stmts.push(
            d.prepare("DELETE FROM sessions WHERE user_id = ?1")
                .bind(&[n(user_id)])?,
        );
    }
    d.batch(stmts).await?;
    Ok(())
}

// ---- Abuse reports ----

pub async fn add_report(
    env: &Env,
    link_id: i64,
    reason: &str,
    reporter: Option<&str>,
) -> worker::Result<()> {
    db(env)?
        .prepare(
            "INSERT INTO reports (link_id, reason, reporter, created_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&[
            n(link_id),
            s(reason),
            reporter.map(s).unwrap_or(wasm_bindgen::JsValue::NULL),
            n(now_ms()),
        ])?
        .run()
        .await?;
    Ok(())
}

pub async fn reports_since(env: &Env, reporter: &str, since: i64) -> worker::Result<i64> {
    Ok(db(env)?
        .prepare("SELECT COUNT(*) AS n FROM reports WHERE reporter = ?1 AND created_at >= ?2")
        .bind(&[s(reporter), n(since)])?
        .first::<CountRow>(None)
        .await?
        .map(|c| c.n)
        .unwrap_or(0))
}

#[derive(Deserialize, Serialize)]
pub struct ReportRow {
    pub id: i64,
    pub link_id: i64,
    pub slug: Option<String>,
    pub url: Option<String>,
    pub blocked: Option<i64>,
    pub owner_email: Option<String>,
    pub reason: String,
    pub created_at: i64,
}

pub async fn open_reports(env: &Env) -> worker::Result<Vec<ReportRow>> {
    db(env)?
        .prepare(
            "SELECT r.id, r.link_id, l.slug, l.url, l.blocked, u.email AS owner_email, r.reason, r.created_at \
             FROM reports r LEFT JOIN links l ON l.id = r.link_id LEFT JOIN users u ON u.id = l.owner_id \
             WHERE r.resolved = 0 ORDER BY r.created_at DESC LIMIT 200",
        )
        .all()
        .await?
        .results::<ReportRow>()
}

/// Resolves one report, or all open reports for its link when `whole_link` is set.
pub async fn resolve_report(env: &Env, id: i64, whole_link: bool) -> worker::Result<()> {
    let sql = if whole_link {
        "UPDATE reports SET resolved = 1 WHERE link_id = (SELECT link_id FROM reports WHERE id = ?1)"
    } else {
        "UPDATE reports SET resolved = 1 WHERE id = ?1"
    };
    db(env)?.prepare(sql).bind(&[n(id)])?.run().await?;
    Ok(())
}
