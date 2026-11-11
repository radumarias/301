//! Who is calling: a Google sign-in session (cookie), a Cloudflare Access JWT (admins and
//! service tokens, when Access is configured), or the local-dev bypass.
//!
//! Admins are the emails in `ADMIN_EMAILS`. Everyone else who signs in is a regular user
//! who only sees their own links.

use crate::account;
use crate::crypto::{b64url_decode, verify_rs256};
pub use crate::util::env_var as var;
use crate::util::now_ms;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use worker::{Env, Fetch, Request};

#[derive(Debug, Clone)]
pub struct Identity {
    pub user_id: i64,
    pub email: String,
    pub name: Option<String>,
    pub admin: bool,
}

#[derive(Debug, PartialEq)]
pub enum AuthError {
    /// Server is not configured (fail closed).
    NotConfigured(&'static str),
    /// No or invalid credentials.
    Denied(String),
}

#[derive(Deserialize, Debug)]
struct Header {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum Aud {
    One(String),
    Many(Vec<String>),
}

impl Aud {
    pub fn contains(&self, aud: &str) -> bool {
        match self {
            Aud::One(a) => a == aud,
            Aud::Many(v) => v.iter().any(|a| a == aud),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct Claims {
    aud: Aud,
    iss: String,
    exp: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    common_name: Option<String>,
}

#[derive(Default)]
struct JwksEntry {
    /// When the keys were fetched (ms) and the keys; empty until the first success.
    fetched_at: i64,
    keys: Vec<serde_json::Value>,
    /// When this isolate last started a fetch of this URL (successful or not).
    attempt: i64,
    /// True while a fetch is in flight; concurrent requests wait for it.
    fetching: bool,
}

thread_local! {
    /// JWKS by certs URL (Access certs, Google certs).
    static JWKS: RefCell<HashMap<String, JwksEntry>> = RefCell::new(HashMap::new());
}

const JWKS_TTL_MS: i64 = 3_600_000;
/// An unknown `kid` or a failed signature (key rotation) refetches early, but fetch
/// attempts, successful or not, start at most this often per isolate and URL, so forged
/// tokens cannot turn requests into fetches of the certs.
const JWKS_MIN_REFRESH_MS: i64 = 60_000;

pub fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn is_admin(env: &Env, email: &str) -> bool {
    list(&var(env, "ADMIN_EMAILS")).contains(&email.to_lowercase())
}

pub fn google_configured(env: &Env) -> bool {
    !var(env, "GOOGLE_CLIENT_ID").is_empty() && !var(env, "GOOGLE_CLIENT_SECRET").is_empty()
}

fn access_configured(env: &Env) -> bool {
    !var(env, "ACCESS_TEAM_DOMAIN").is_empty() && !var(env, "ACCESS_AUD").is_empty()
}

fn is_local(req: &Request) -> bool {
    req.url()
        .is_ok_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
}

/// Local development only (`.dev.vars`): ignored as soon as Access or Google is configured,
/// and only honoured on a loopback host, so it can never open a deployed Worker.
pub fn dev_email(req: &Request, env: &Env) -> Option<String> {
    let dev = var(env, "DEV_AUTH_EMAIL");
    (!dev.is_empty() && !access_configured(env) && !google_configured(env) && is_local(req))
        .then_some(dev)
}

fn access_token(req: &Request) -> Option<String> {
    if let Ok(Some(t)) = req.headers().get("Cf-Access-Jwt-Assertion") {
        return Some(t);
    }
    account::cookie(req, "CF_Authorization")
}

fn has_kid(keys: &[serde_json::Value], kid: Option<&str>) -> bool {
    kid.is_none_or(|kid| {
        keys.iter()
            .any(|k| k.get("kid").and_then(|v| v.as_str()) == Some(kid))
    })
}

fn with_entry<R>(url: &str, f: impl FnOnce(&mut JwksEntry) -> R) -> R {
    JWKS.with(|c| f(c.borrow_mut().entry(url.to_string()).or_default()))
}

async fn jwks(url: &str, kid: Option<&str>) -> worker::Result<Vec<serde_json::Value>> {
    let fresh = |e: &JwksEntry, now: i64| {
        !e.keys.is_empty() && now - e.fetched_at < JWKS_TTL_MS && has_kid(&e.keys, kid)
    };
    // Fresh keys that know this `kid` need no fetch, even while one is in flight.
    let start = now_ms();
    if let Some(k) = with_entry(url, |e| fresh(e, start).then(|| e.keys.clone())) {
        return Ok(k);
    }
    // Another request in this isolate is fetching: wait for its result instead of
    // failing or starting a second fetch (the UI sends requests in parallel).
    let mut waited = 0;
    while with_entry(url, |e| e.fetching) && waited < 10_000 {
        worker::Delay::from(std::time::Duration::from_millis(25)).await;
        waited += 25;
    }
    let now = now_ms();
    let recent_attempt = with_entry(url, |e| now - e.attempt < JWKS_MIN_REFRESH_MS);
    if let Some(k) = with_entry(url, |e| {
        (!e.keys.is_empty() && (recent_attempt || fresh(e, now))).then(|| e.keys.clone())
    }) {
        return Ok(k);
    }
    if recent_attempt {
        return Err(worker::Error::RustError(
            "certs fetch failed recently; retrying shortly".into(),
        ));
    }
    // Recorded before awaiting, so concurrent requests and failed fetches count too.
    with_entry(url, |e| {
        e.attempt = now;
        e.fetching = true;
    });
    let res = fetch_jwks(url).await;
    with_entry(url, |e| e.fetching = false);
    let keys = res?;
    with_entry(url, |e| {
        e.fetched_at = now;
        e.keys = keys.clone();
    });
    Ok(keys)
}

async fn fetch_jwks(url: &str) -> worker::Result<Vec<serde_json::Value>> {
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<serde_json::Value>,
    }
    let mut res = Fetch::Url(url.parse()?).send().await?;
    Ok(res.json::<Jwks>().await?.keys)
}

/// Verifies an RS256 JWT against the JWKS at `certs_url` and returns its claims.
/// Claim checks (issuer, audience, expiry) are the caller's job.
pub async fn verify_jwt<T: DeserializeOwned>(token: &str, certs_url: &str) -> Result<T, AuthError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(AuthError::Denied("malformed token".into()));
    }
    let header: Header = b64url_decode(parts[0])
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| AuthError::Denied("bad token header".into()))?;
    if header.alg != "RS256" {
        return Err(AuthError::Denied("unexpected token algorithm".into()));
    }
    let claims: T = b64url_decode(parts[1])
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| AuthError::Denied("bad token payload".into()))?;
    let sig =
        b64url_decode(parts[2]).ok_or_else(|| AuthError::Denied("bad token signature".into()))?;

    let keys = jwks(certs_url, header.kid.as_deref())
        .await
        .map_err(|e| AuthError::Denied(format!("could not fetch signing keys: {e}")))?;
    let candidates: Vec<&serde_json::Value> = keys
        .iter()
        .filter(|k| match &header.kid {
            Some(kid) => k.get("kid").and_then(|v| v.as_str()) == Some(kid.as_str()),
            None => true,
        })
        .collect();
    let signed = format!("{}.{}", parts[0], parts[1]);
    for k in candidates {
        if verify_rs256(k, signed.as_bytes(), &sig)
            .await
            .unwrap_or(false)
        {
            return Ok(claims);
        }
    }
    // Keys may have rotated under a token without a `kid`: expire the cache so the next
    // request refetches (still subject to the attempt limit in `jwks`).
    with_entry(certs_url, |e| e.fetched_at = 0);
    Err(AuthError::Denied("invalid token signature".into()))
}

/// Pure Access claim checks, separated for unit testing. Returns the identity's email
/// (or `service:<client id>` for a service token).
pub fn check_claims(
    c: &Claims,
    aud: &str,
    team: &str,
    now_s: i64,
    emails: &[String],
    service_tokens: &[String],
) -> Result<String, AuthError> {
    if !c.aud.contains(aud) {
        return Err(AuthError::Denied("token audience mismatch".into()));
    }
    if c.iss.trim_end_matches('/') != format!("https://{team}") {
        return Err(AuthError::Denied("token issuer mismatch".into()));
    }
    if c.exp < now_s - 30 {
        return Err(AuthError::Denied("token expired".into()));
    }
    if c.nbf.is_some_and(|nbf| nbf > now_s + 30) {
        return Err(AuthError::Denied("token not yet valid".into()));
    }
    if let Some(email) = c
        .email
        .as_deref()
        .map(str::to_lowercase)
        .filter(|e| !e.is_empty())
    {
        if emails.contains(&email) {
            return Ok(email);
        }
        return Err(AuthError::Denied(format!("{email} is not an admin")));
    }
    if let Some(cn) = c.common_name.as_deref().map(str::to_lowercase) {
        if service_tokens.contains(&cn) {
            return Ok(format!("service:{cn}"));
        }
    }
    Err(AuthError::Denied("identity not allowed".into()))
}

/// Cloudflare Access: admins and service tokens only.
async fn verify_access(token: &str, env: &Env) -> Result<String, AuthError> {
    let emails = list(&var(env, "ADMIN_EMAILS"));
    let service_tokens = list(&var(env, "ADMIN_SERVICE_TOKENS"));
    if emails.is_empty() && service_tokens.is_empty() {
        return Err(AuthError::NotConfigured("ADMIN_EMAILS must be set"));
    }
    let aud = var(env, "ACCESS_AUD");
    let team = var(env, "ACCESS_TEAM_DOMAIN")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
    let claims: Claims = verify_jwt(token, &format!("https://{team}/cdn-cgi/access/certs")).await?;
    check_claims(
        &claims,
        &aud,
        &team,
        now_ms() / 1000,
        &emails,
        &service_tokens,
    )
}

fn internal(e: worker::Error) -> AuthError {
    worker::console_error!("auth: {e}");
    AuthError::Denied("could not check your session, try again".into())
}

async fn identity_for_email(env: &Env, email: String, admin: bool) -> Result<Identity, AuthError> {
    let user_id = account::user_id_for_email(env, &email)
        .await
        .map_err(internal)?;
    Ok(Identity {
        user_id,
        email,
        name: None,
        admin,
    })
}

/// The caller's identity, `Ok(None)` when not signed in.
pub async fn identify(req: &Request, env: &Env) -> Result<Option<Identity>, AuthError> {
    if let Some(token) = account::cookie(req, account::SESSION_COOKIE) {
        if let Some(u) = account::session_user(env, &token).await.map_err(internal)? {
            if u.banned != 0 {
                return Err(AuthError::Denied("This account is suspended.".into()));
            }
            return Ok(Some(Identity {
                user_id: u.id,
                admin: is_admin(env, &u.email),
                email: u.email,
                name: u.name,
            }));
        }
    }
    if let Some(dev) = dev_email(req, env) {
        return identity_for_email(env, dev, true).await.map(Some);
    }
    if access_configured(env) {
        if let Some(token) = access_token(req) {
            let email = verify_access(&token, env).await?;
            return identity_for_email(env, email, true).await.map(Some);
        }
    }
    if !google_configured(env) && !access_configured(env) {
        return Err(AuthError::NotConfigured(
            "Sign-in is not configured: set GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET (or Cloudflare Access).",
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(json: &str) -> Claims {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn claim_checks() {
        let emails = vec!["radu@example.com".to_string()];
        let ok = claims(
            r#"{"aud":["AUD"],"iss":"https://t.cloudflareaccess.com","exp":2000,"email":"Radu@example.com"}"#,
        );
        assert_eq!(
            check_claims(&ok, "AUD", "t.cloudflareaccess.com", 1000, &emails, &[]).unwrap(),
            "radu@example.com"
        );

        let wrong_aud = claims(
            r#"{"aud":"OTHER","iss":"https://t.cloudflareaccess.com","exp":2000,"email":"radu@example.com"}"#,
        );
        assert!(check_claims(
            &wrong_aud,
            "AUD",
            "t.cloudflareaccess.com",
            1000,
            &emails,
            &[]
        )
        .is_err());

        let expired = claims(
            r#"{"aud":"AUD","iss":"https://t.cloudflareaccess.com","exp":100,"email":"radu@example.com"}"#,
        );
        assert!(check_claims(
            &expired,
            "AUD",
            "t.cloudflareaccess.com",
            1000,
            &emails,
            &[]
        )
        .is_err());

        let stranger = claims(
            r#"{"aud":"AUD","iss":"https://t.cloudflareaccess.com","exp":2000,"email":"eve@example.com"}"#,
        );
        assert!(check_claims(
            &stranger,
            "AUD",
            "t.cloudflareaccess.com",
            1000,
            &emails,
            &[]
        )
        .is_err());

        let bad_iss = claims(
            r#"{"aud":"AUD","iss":"https://evil.cloudflareaccess.com","exp":2000,"email":"radu@example.com"}"#,
        );
        assert!(check_claims(
            &bad_iss,
            "AUD",
            "t.cloudflareaccess.com",
            1000,
            &emails,
            &[]
        )
        .is_err());

        let svc = claims(
            r#"{"aud":"AUD","iss":"https://t.cloudflareaccess.com","exp":2000,"common_name":"abc.access"}"#,
        );
        assert!(check_claims(
            &svc,
            "AUD",
            "t.cloudflareaccess.com",
            1000,
            &emails,
            &["abc.access".into()]
        )
        .is_ok());
        assert!(check_claims(&svc, "AUD", "t.cloudflareaccess.com", 1000, &emails, &[]).is_err());
    }
}
