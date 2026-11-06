//! Abuse protection for links created by anyone other than an admin: local URL checks,
//! Google Safe Browsing, and Cloudflare Turnstile for anonymous creation.

use crate::auth::var;
use crate::model::Link;
use serde::Deserialize;
use std::net::IpAddr;
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

/// Checks that need no network: no credentials in the URL, no loops back to this
/// shortener, no private or local hosts.
pub fn local_check(u: &str, own_host: &str) -> Result<(), String> {
    let p = url::Url::parse(u).map_err(|e| format!("invalid URL {u}: {e}"))?;
    if !p.username().is_empty() || p.password().is_some() {
        return Err("URLs with a username or password are not allowed".into());
    }
    let host = p
        .host_str()
        .ok_or("URL has no host")?
        .trim_end_matches('.')
        .to_lowercase();
    if host == own_host.to_lowercase() {
        return Err("a short link cannot point to this shortener".into());
    }
    let local_name = host == "localhost"
        || [".localhost", ".local", ".internal", ".lan", ".home.arpa"]
            .iter()
            .any(|s| host.ends_with(s))
        || !host.contains('.') && !host.starts_with('[');
    if local_name {
        return Err("local or private addresses are not allowed".into());
    }
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip.parse::<IpAddr>() {
        let private = match ip {
            IpAddr::V4(v) => {
                v.is_private()
                    || v.is_loopback()
                    || v.is_link_local()
                    || v.is_unspecified()
                    || v.is_broadcast()
                    || v.octets()[0] == 100 && (64..128).contains(&v.octets()[1]) // CGNAT
                    || v.octets()[0] == 0
            }
            IpAddr::V6(v) => {
                v.is_loopback()
                    || v.is_unspecified()
                    || (v.segments()[0] & 0xfe00) == 0xfc00 // unique local
                    || (v.segments()[0] & 0xffc0) == 0xfe80 // link local
                    || v.to_ipv4_mapped().is_some()
            }
        };
        if private {
            return Err("local or private addresses are not allowed".into());
        }
    }
    Ok(())
}

/// Google Safe Browsing (v4 Lookup API). Returns the first flagged URL, if any.
/// Skipped when `SAFE_BROWSING_KEY` is not set; fails open on API errors (logged).
pub async fn flagged(env: &Env, urls: &[String]) -> Option<String> {
    let key = var(env, "SAFE_BROWSING_KEY");
    if key.is_empty() || urls.is_empty() {
        return None;
    }
    let body = serde_json::json!({
        "client": { "clientId": "xorio-301", "clientVersion": "1.0" },
        "threatInfo": {
            "threatTypes": ["MALWARE", "SOCIAL_ENGINEERING", "UNWANTED_SOFTWARE", "POTENTIALLY_HARMFUL_APPLICATION"],
            "platformTypes": ["ANY_PLATFORM"],
            "threatEntryTypes": ["URL"],
            "threatEntries": urls.iter().map(|u| serde_json::json!({ "url": u })).collect::<Vec<_>>(),
        }
    });
    #[derive(Deserialize)]
    struct Match {
        threat: Threat,
    }
    #[derive(Deserialize)]
    struct Threat {
        url: String,
    }
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        matches: Vec<Match>,
    }
    let url = format!("https://safebrowsing.googleapis.com/v4/threatMatches:find?key={key}");
    match post_json::<Resp>(&url, &body).await {
        Ok(r) => r.matches.into_iter().next().map(|m| m.threat.url),
        Err(e) => {
            worker::console_error!("safe browsing lookup failed: {e}");
            None
        }
    }
}

/// The host this request was made to (the shortener's own host).
pub fn own_host(req: &Request) -> String {
    req.url()
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Checks every destination of a link saved by a non-admin: local checks for each URL,
/// then one Safe Browsing lookup for all of them.
pub async fn check_link(env: &Env, req: &Request, link: &Link) -> Result<(), String> {
    let host = own_host(req);
    let mut urls: Vec<String> = link.destinations().into_iter().map(String::from).collect();
    urls.sort();
    urls.dedup();
    for u in &urls {
        local_check(u, &host)?;
    }
    if let Some(bad) = flagged(env, &urls).await {
        return Err(format!(
            "{bad} is flagged as unsafe by Google Safe Browsing"
        ));
    }
    Ok(())
}

pub fn turnstile_configured(env: &Env) -> bool {
    !var(env, "TURNSTILE_SITE_KEY").is_empty() && !var(env, "TURNSTILE_SECRET").is_empty()
}

/// Verifies a Turnstile token (fails closed).
pub async fn turnstile_ok(env: &Env, token: &str, ip: Option<&str>) -> bool {
    if token.is_empty() || token.len() > 2048 {
        return false;
    }
    let mut body = serde_json::json!({ "secret": var(env, "TURNSTILE_SECRET"), "response": token });
    if let Some(ip) = ip {
        body["remoteip"] = ip.into();
    }
    #[derive(Deserialize)]
    struct Resp {
        success: bool,
    }
    match post_json::<Resp>(
        "https://challenges.cloudflare.com/turnstile/v0/siteverify",
        &body,
    )
    .await
    {
        Ok(r) => r.success,
        Err(e) => {
            worker::console_error!("turnstile verify failed: {e}");
            false
        }
    }
}

async fn post_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    body: &serde_json::Value,
) -> worker::Result<T> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));
    let mut res = Fetch::Request(Request::new_with_init(url, &init)?)
        .send()
        .await?;
    if res.status_code() >= 400 {
        return Err(worker::Error::RustError(format!(
            "HTTP {} from {}",
            res.status_code(),
            url.split('?').next().unwrap_or(url)
        )));
    }
    res.json().await
}

pub fn client_ip(req: &Request) -> Option<String> {
    req.headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .filter(|ip| !ip.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_checks() {
        let own = "xorio-301.42dev.workers.dev";
        assert!(local_check("https://example.com/a", own).is_ok());
        assert!(local_check("https://8.8.8.8/", own).is_ok());
        assert!(local_check("https://xorio-301.42dev.workers.dev/abc", own).is_err());
        assert!(local_check("https://XORIO-301.42dev.workers.dev./abc", own).is_err());
        assert!(local_check("http://localhost:8080/", own).is_err());
        assert!(local_check("http://intranet/", own).is_err());
        assert!(local_check("http://printer.local/", own).is_err());
        assert!(local_check("http://192.168.1.1/", own).is_err());
        assert!(local_check("http://10.0.0.5/", own).is_err());
        assert!(local_check("http://127.0.0.1/", own).is_err());
        assert!(local_check("http://169.254.169.254/latest", own).is_err());
        assert!(local_check("http://[::1]/", own).is_err());
        assert!(local_check("http://[fd00::1]/", own).is_err());
        assert!(local_check("https://user:pw@example.com/", own).is_err());
        assert!(local_check("https://paypal.com@evil.example/", own).is_err());
    }
}
