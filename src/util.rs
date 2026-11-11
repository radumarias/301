//! Small helpers shared by the redirect path and the admin API.

use serde::Serialize;
use worker::{Headers, Response, Result};

/// Current time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// ISO-8601 string for a unix-ms timestamp.
pub fn iso(ms: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64))
            .to_iso_string()
            .into()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        ms.to_string()
    }
}

/// Parses a date string (ISO-8601, RFC 2822, "+0000" offsets) to unix ms.
pub fn parse_date_str(s: &str) -> Option<i64> {
    #[cfg(target_arch = "wasm32")]
    {
        // JS accepts "+0000" offsets poorly; normalise to "+00:00".
        let b = s.as_bytes();
        let fixed = if s.len() > 5
            && matches!(b[s.len() - 5], b'+' | b'-')
            && b[s.len() - 4..].iter().all(u8::is_ascii_digit)
        {
            format!("{}:{}", &s[..s.len() - 2], &s[s.len() - 2..])
        } else {
            s.to_string()
        };
        let t = js_sys::Date::parse(&fixed);
        (!t.is_nan()).then_some(t as i64)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = s;
        None
    }
}

/// A plain response with a content type and no-store caching.
pub fn body(content: impl Into<String>, ctype: &str) -> Result<Response> {
    let mut res = Response::ok(content)?;
    let h = res.headers_mut();
    h.set("Content-Type", ctype)?;
    h.set("Cache-Control", "no-store")?;
    Ok(res)
}

/// Security headers for every response, applied once in `lib.rs`. Pages that need a
/// looser policy (the admin UI, visitor pages) set their own; everything else (API
/// data, SVGs, redirects, errors) gets an inert default: no script, no framing.
pub fn harden(res: &mut Response) -> Result<()> {
    const DEFAULTS: [(&str, &str); 3] = [
        (
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; sandbox",
        ),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
    ];
    let h = res.headers_mut();
    for (k, v) in DEFAULTS {
        if !h.has(k)? {
            h.set(k, v)?;
        }
    }
    h.set("X-Content-Type-Options", "nosniff")?;
    h.set("Strict-Transport-Security", "max-age=31536000")?;
    Ok(())
}

/// A trimmed environment variable, empty when unset.
pub fn env_var(env: &worker::Env, name: &str) -> String {
    env.var(name)
        .map(|v| v.to_string())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// JSON response with a status code and no-store caching.
pub fn json<T: Serialize>(value: &T, status: u16) -> Result<Response> {
    let mut res = Response::from_json(value)?.with_status(status);
    res.headers_mut().set("Cache-Control", "no-store")?;
    Ok(res)
}

/// `{"error": msg}` with the given status.
pub fn json_error(msg: impl Into<String>, status: u16) -> Result<Response> {
    json(&serde_json::json!({ "error": msg.into() }), status)
}

/// Percent-encodes non-ASCII and control characters so the value is a valid header
/// (`Headers.set` rejects non-ByteString values). ASCII is left exactly as stored.
pub fn header_safe_url(location: &str) -> String {
    // Controls, non-ASCII and the characters WHATWG URL parsing would encode in a path.
    const UNSAFE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'<')
        .add(b'>')
        .add(b'`');
    percent_encoding::utf8_percent_encode(location, UNSAFE).to_string()
}

/// Builds a redirect without going through `url::Url` (keeps the URL as stored, apart
/// from percent-encoding non-ASCII characters).
pub fn redirect(location: &str, status: u16) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Location", &header_safe_url(location))?;
    headers.set("Cache-Control", "private, max-age=0")?;
    // The browser default: a cross-origin destination sees only the origin of the page
    // the link was clicked on, not its path or query (same-origin requests keep it).
    headers.set("Referrer-Policy", "strict-origin-when-cross-origin")?;
    headers.set("X-Robots-Tag", "noindex")?;
    Ok(Response::empty()?.with_status(status).with_headers(headers))
}

/// `fetch` with a deadline (Workers has no default timeout for subrequests).
pub async fn fetch_with_timeout(req: worker::Request, ms: u64) -> Result<Response> {
    use futures_util::future::{select, Either};
    let fetch = Box::pin(async move { worker::Fetch::Request(req).send().await });
    let timeout = Box::pin(worker::Delay::from(std::time::Duration::from_millis(ms)));
    match select(fetch, timeout).await {
        Either::Left((res, _)) => res,
        Either::Right(_) => Err(worker::Error::RustError(format!("timed out after {ms} ms"))),
    }
}

/// First 300 characters of an error message, for storing and showing.
pub fn snippet(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Lowercase hex of some bytes.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Escapes text for safe inclusion in HTML.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a CSV field (RFC 4180) and neutralises spreadsheet formula injection.
pub fn csv_field(s: &str) -> String {
    let mut v = s.to_string();
    if v.starts_with(['=', '+', '-', '@']) {
        v.insert(0, '\'');
    }
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn location_is_header_safe() {
        assert_eq!(
            header_safe_url("https://e.com/a?b=1"),
            "https://e.com/a?b=1"
        );
        assert_eq!(header_safe_url("https://e.com/ü"), "https://e.com/%C3%BC");
        assert_eq!(
            header_safe_url("https://e.com/a b\"c"),
            "https://e.com/a%20b%22c"
        );
    }

    #[test]
    fn escapes_csv() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
    }
}
