//! "Sign in with Google" (OpenID Connect authorization code flow with PKCE), handled by
//! the Worker itself. After a verified login the Worker sets its own session cookie.
//!
//! * `GET  /auth/login?next=/_/`  redirects to Google
//! * `GET  /auth/callback`        exchanges the code, verifies the ID token, starts a session
//! * `POST /auth/logout`          ends the session

use crate::account::{self, SESSION_COOKIE};
use crate::auth::{self, Aud, AuthError};
use crate::crypto::{b64url, b64url_decode, constant_time_eq, random_token, sha256};
use crate::util::now_ms;
use crate::{pages, util};
use serde::Deserialize;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, Response, Result};

const AUTHORIZE: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const CERTS: &str = "https://www.googleapis.com/oauth2/v3/certs";
const FLOW_COOKIE: &str = "__Host-301o";

#[derive(Deserialize, Debug)]
pub struct GoogleClaims {
    iss: String,
    aud: Aud,
    exp: i64,
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    name: Option<String>,
}

/// Pure ID-token claim checks. Returns (sub, email, name).
pub fn check_google_claims(
    c: &GoogleClaims,
    client_id: &str,
    now_s: i64,
) -> std::result::Result<(String, String, Option<String>), String> {
    if c.iss != "https://accounts.google.com" && c.iss != "accounts.google.com" {
        return Err("token issuer mismatch".into());
    }
    if !c.aud.contains(client_id) {
        return Err("token audience mismatch".into());
    }
    if c.exp < now_s - 30 {
        return Err("token expired".into());
    }
    let email = c
        .email
        .as_deref()
        .map(str::to_lowercase)
        .filter(|e| !e.is_empty())
        .ok_or("your Google account has no email address")?;
    if c.email_verified != Some(true) {
        return Err("your Google email address is not verified".into());
    }
    if c.sub.is_empty() {
        return Err("token has no subject".into());
    }
    Ok((c.sub.clone(), email, c.name.clone()))
}

/// Only same-site paths, so the login cannot be turned into an open redirect.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.to_string(),
        _ => "/_/".into(),
    }
}

fn redirect_uri(req: &Request) -> Result<String> {
    Ok(format!(
        "{}/auth/callback",
        req.url()?.origin().ascii_serialization()
    ))
}

fn set_cookie(res: &mut Response, value: &str) -> Result<()> {
    res.headers_mut().append("Set-Cookie", value)?;
    Ok(())
}

fn flow_cookie(value: &str, max_age: i64) -> String {
    format!("{FLOW_COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age}")
}

pub async fn handle(req: Request, env: Env) -> Result<Response> {
    match (req.method(), req.path().as_str()) {
        (Method::Get, "/auth/login") => login(req, &env).await,
        (Method::Get, "/auth/callback") => match callback(&req, &env).await {
            Ok(res) => Ok(res),
            Err(msg) => {
                let mut res = pages::auth_failed(&msg)?;
                set_cookie(&mut res, &flow_cookie("", 0))?;
                Ok(res)
            }
        },
        (Method::Post, "/auth/logout") => logout(req, &env).await,
        _ => pages::not_found(),
    }
}

async fn login(req: Request, env: &Env) -> Result<Response> {
    if !auth::google_configured(env) {
        return pages::auth_failed("Google sign-in is not configured on this server.");
    }
    let url = req.url()?;
    let next = safe_next(
        url.query_pairs()
            .find(|(k, _)| k == "next")
            .map(|(_, v)| v.into_owned())
            .as_deref(),
    );
    let state = random_token(16);
    let verifier = random_token(32);
    let challenge = b64url(&sha256(verifier.as_bytes()).await?);
    let mut target = url::Url::parse(AUTHORIZE)?;
    target
        .query_pairs_mut()
        .append_pair("client_id", &auth::var(env, "GOOGLE_CLIENT_ID"))
        .append_pair("redirect_uri", &redirect_uri(&req)?)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    let mut res = util::redirect(target.as_str(), 302)?;
    // state.verifier.next, short-lived; checked on the way back.
    let value = format!("{state}.{verifier}.{}", b64url(next.as_bytes()));
    set_cookie(&mut res, &flow_cookie(&value, 600))?;
    Ok(res)
}

async fn callback(req: &Request, env: &Env) -> std::result::Result<Response, String> {
    let fail = |e: worker::Error| {
        worker::console_error!("google callback: {e}");
        "Something went wrong while signing you in. Try again.".to_string()
    };
    let url = req.url().map_err(fail)?;
    let q = |k: &str| {
        url.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(e) = q("error") {
        return Err(if e == "access_denied" {
            "Sign-in was cancelled.".into()
        } else {
            format!("Google returned an error: {e}")
        });
    }
    let (code, state) = q("code")
        .zip(q("state"))
        .ok_or("The sign-in response is incomplete. Try again.")?;
    let flow = account::cookie(req, FLOW_COOKIE)
        .ok_or("Your sign-in took too long or cookies are blocked. Try again.")?;
    let mut parts = flow.splitn(3, '.');
    let (want_state, verifier, next) = match (parts.next(), parts.next(), parts.next()) {
        (Some(s), Some(v), Some(n)) => (s, v, n),
        _ => return Err("Your sign-in session is invalid. Try again.".into()),
    };
    if !constant_time_eq(want_state.as_bytes(), state.as_bytes()) {
        return Err("Your sign-in session does not match. Try again.".into());
    }
    let next = safe_next(
        b64url_decode(next)
            .and_then(|b| String::from_utf8(b).ok())
            .as_deref(),
    );

    // Exchange the code for an ID token.
    let client_id = auth::var(env, "GOOGLE_CLIENT_ID");
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", &code)
        .append_pair("client_id", &client_id)
        .append_pair("client_secret", &auth::var(env, "GOOGLE_CLIENT_SECRET"))
        .append_pair("redirect_uri", &redirect_uri(req).map_err(fail)?)
        .append_pair("grant_type", "authorization_code")
        .append_pair("code_verifier", verifier)
        .finish();
    let headers = Headers::new();
    headers
        .set("Content-Type", "application/x-www-form-urlencoded")
        .map_err(fail)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(form.into()));
    let token_req = Request::new_with_init(TOKEN, &init).map_err(fail)?;
    let mut res = Fetch::Request(token_req).send().await.map_err(fail)?;
    #[derive(Deserialize)]
    struct TokenResponse {
        id_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }
    let body: TokenResponse = res.json().await.map_err(fail)?;
    let Some(id_token) = body.id_token else {
        worker::console_error!(
            "google token exchange failed: {:?} {:?}",
            body.error,
            body.error_description
        );
        return Err("Google did not accept the sign-in. Try again.".into());
    };

    // The token came straight from Google over TLS; its signature is checked anyway.
    let claims: GoogleClaims = auth::verify_jwt(&id_token, CERTS)
        .await
        .map_err(|e| match e {
            AuthError::Denied(m) => m,
            AuthError::NotConfigured(m) => m.to_string(),
        })?;
    let (sub, email, name) = check_google_claims(&claims, &client_id, now_ms() / 1000)?;

    let user = account::upsert_google_user(env, &sub, &email, name.as_deref())
        .await
        .map_err(fail)?;
    if user.banned != 0 {
        return Err("This account is suspended.".into());
    }
    if auth::is_admin(env, &user.email) {
        account::claim_legacy_links(env, user.id)
            .await
            .map_err(fail)?;
    }
    let token = account::create_session(env, user.id).await.map_err(fail)?;
    let mut res = util::redirect(&next, 302).map_err(fail)?;
    set_cookie(
        &mut res,
        &account::session_cookie(&token, account::SESSION_TTL_MS / 1000),
    )
    .map_err(fail)?;
    set_cookie(&mut res, &flow_cookie("", 0)).map_err(fail)?;
    Ok(res)
}

async fn logout(req: Request, env: &Env) -> Result<Response> {
    if !crate::api::same_origin(&req) {
        return util::json_error("cross-origin request refused", 403);
    }
    if let Some(token) = account::cookie(&req, SESSION_COOKIE) {
        account::end_session(env, &token).await?;
    }
    let mut res = util::json(&serde_json::json!({ "ok": true }), 200)?;
    set_cookie(&mut res, &account::session_cookie("", 0))?;
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(json: &str) -> GoogleClaims {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn google_claim_checks() {
        let ok = claims(
            r#"{"iss":"https://accounts.google.com","aud":"CID","exp":2000,"sub":"123","email":"Ann@Example.com","email_verified":true,"name":"Ann"}"#,
        );
        assert_eq!(
            check_google_claims(&ok, "CID", 1000).unwrap(),
            ("123".into(), "ann@example.com".into(), Some("Ann".into()))
        );
        assert!(check_google_claims(&ok, "OTHER", 1000).is_err());
        assert!(check_google_claims(&ok, "CID", 3000).is_err());

        let unverified = claims(
            r#"{"iss":"accounts.google.com","aud":"CID","exp":2000,"sub":"1","email":"a@b.c","email_verified":false}"#,
        );
        assert!(check_google_claims(&unverified, "CID", 1000).is_err());

        let bad_iss = claims(
            r#"{"iss":"https://evil.example","aud":"CID","exp":2000,"sub":"1","email":"a@b.c","email_verified":true}"#,
        );
        assert!(check_google_claims(&bad_iss, "CID", 1000).is_err());
    }

    #[test]
    fn next_is_same_site() {
        assert_eq!(safe_next(Some("/_/#/links")), "/_/#/links");
        assert_eq!(safe_next(Some("//evil.com")), "/_/");
        assert_eq!(safe_next(Some("https://evil.com")), "/_/");
        assert_eq!(safe_next(Some("/\\evil.com")), "/_/");
        assert_eq!(safe_next(None), "/_/");
    }
}
