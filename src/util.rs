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
            && !s[s.len() - 4..].contains(':')
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

/// A plain response with a content type, no-store caching and nosniff.
pub fn body(content: impl Into<String>, ctype: &str) -> Result<Response> {
    let mut res = Response::ok(content)?;
    let h = res.headers_mut();
    h.set("Content-Type", ctype)?;
    h.set("Cache-Control", "no-store")?;
    h.set("X-Content-Type-Options", "nosniff")?;
    Ok(res)
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

/// Builds a redirect without going through `url::Url` (keeps the URL exactly as stored).
pub fn redirect(location: &str, status: u16) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Location", location)?;
    headers.set("Cache-Control", "private, max-age=0")?;
    headers.set("Referrer-Policy", "unsafe-url")?;
    headers.set("X-Robots-Tag", "noindex")?;
    Ok(Response::empty()?.with_status(status).with_headers(headers))
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
    fn escapes_csv() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
    }
}
