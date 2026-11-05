//! Cloudflare Access JWT verification for the admin UI and API.
//!
//! Access protects `/_/*` and `/api/*` at the edge; the Worker verifies the
//! `Cf-Access-Jwt-Assertion` token again so a missing or wrong Access policy
//! fails closed instead of exposing the admin.

use crate::crypto::{b64url_decode, verify_rs256};
use crate::util::now_ms;
use serde::Deserialize;
use std::cell::RefCell;
use worker::{Env, Fetch, Request};

#[derive(Debug, Clone)]
pub struct Identity {
    pub email: String,
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
enum Aud {
    One(String),
    Many(Vec<String>),
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

thread_local! {
    static JWKS: RefCell<Option<(i64, String, Vec<serde_json::Value>)>> = const { RefCell::new(None) };
}

fn var(env: &Env, name: &str) -> String {
    env.var(name)
        .map(|v| v.to_string())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn token_from(req: &Request) -> Option<String> {
    if let Ok(Some(t)) = req.headers().get("Cf-Access-Jwt-Assertion") {
        return Some(t);
    }
    let cookies = req.headers().get("Cookie").ok().flatten()?;
    cookies
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == "CF_Authorization")
        .map(|(_, v)| v.to_string())
}

async fn jwks(team: &str) -> worker::Result<Vec<serde_json::Value>> {
    let now = now_ms();
    let cached = JWKS.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(at, t, _)| t == team && now - at < 3_600_000)
            .map(|(_, _, k)| k.clone())
    });
    if let Some(k) = cached {
        return Ok(k);
    }
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<serde_json::Value>,
    }
    let url = format!("https://{team}/cdn-cgi/access/certs");
    let mut res = Fetch::Url(url.parse()?).send().await?;
    let body: Jwks = res.json().await?;
    JWKS.with(|c| *c.borrow_mut() = Some((now, team.to_string(), body.keys.clone())));
    Ok(body.keys)
}

/// Pure claim checks, separated for unit testing.
pub fn check_claims(
    c: &Claims,
    aud: &str,
    team: &str,
    now_s: i64,
    emails: &[String],
    service_tokens: &[String],
) -> Result<Identity, AuthError> {
    let aud_ok = match &c.aud {
        Aud::One(a) => a == aud,
        Aud::Many(v) => v.iter().any(|a| a == aud),
    };
    if !aud_ok {
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
            return Ok(Identity { email });
        }
        return Err(AuthError::Denied(format!("{email} is not an admin")));
    }
    if let Some(cn) = c.common_name.as_deref().map(str::to_lowercase) {
        if service_tokens.contains(&cn) {
            return Ok(Identity {
                email: format!("service:{cn}"),
            });
        }
    }
    Err(AuthError::Denied("identity not allowed".into()))
}

pub async fn verify(req: &Request, env: &Env) -> Result<Identity, AuthError> {
    let team = var(env, "ACCESS_TEAM_DOMAIN");
    let aud = var(env, "ACCESS_AUD");

    // Local development only (`.dev.vars`). Ignored as soon as Access is configured,
    // so it can never open a deployed Worker.
    let dev = var(env, "DEV_AUTH_EMAIL");
    if !dev.is_empty() && aud.is_empty() && team.is_empty() {
        return Ok(Identity { email: dev });
    }

    let emails = list(&var(env, "ADMIN_EMAILS"));
    let service_tokens = list(&var(env, "ADMIN_SERVICE_TOKENS"));
    if team.is_empty() || aud.is_empty() {
        return Err(AuthError::NotConfigured(
            "ACCESS_TEAM_DOMAIN and ACCESS_AUD must be set",
        ));
    }
    if emails.is_empty() && service_tokens.is_empty() {
        return Err(AuthError::NotConfigured("ADMIN_EMAILS must be set"));
    }
    let team = team
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();

    let token = token_from(req)
        .ok_or_else(|| AuthError::Denied("missing Cloudflare Access token".into()))?;
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
    let claims: Claims = b64url_decode(parts[1])
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| AuthError::Denied("bad token payload".into()))?;
    let sig =
        b64url_decode(parts[2]).ok_or_else(|| AuthError::Denied("bad token signature".into()))?;

    let keys = jwks(&team)
        .await
        .map_err(|e| AuthError::Denied(format!("could not fetch Access certs: {e}")))?;
    let candidates: Vec<&serde_json::Value> = keys
        .iter()
        .filter(|k| match &header.kid {
            Some(kid) => k.get("kid").and_then(|v| v.as_str()) == Some(kid.as_str()),
            None => true,
        })
        .collect();
    let signed = format!("{}.{}", parts[0], parts[1]);
    let mut valid = false;
    for k in candidates {
        if verify_rs256(k, signed.as_bytes(), &sig)
            .await
            .unwrap_or(false)
        {
            valid = true;
            break;
        }
    }
    if !valid {
        // Keys may have rotated: drop the cache so the next request refetches.
        JWKS.with(|c| *c.borrow_mut() = None);
        return Err(AuthError::Denied("invalid token signature".into()));
    }
    check_claims(
        &claims,
        &aud,
        &team,
        now_ms() / 1000,
        &emails,
        &service_tokens,
    )
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
            check_claims(&ok, "AUD", "t.cloudflareaccess.com", 1000, &emails, &[])
                .unwrap()
                .email,
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
