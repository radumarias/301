//! Small server-rendered pages for visitors (password form, errors, preview).

use crate::util::esc;
use worker::{Response, Result};

const STYLE: &str = r#"
:root{--bg:#f5f6f4;--fg:#161a18;--muted:#59625d;--card:#fff;--line:#d4d9d5;--accent:#1d5fa8;--ink:#fff;--bad:#b4233c}
@media (prefers-color-scheme:dark){:root{--bg:#101312;--fg:#e3e7e4;--muted:#9aa49e;--card:#171b19;--line:#2c3330;--accent:#7db3ee;--ink:#101312;--bad:#f08a95}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);
font:16px/1.55 "IBM Plex Sans","Segoe UI",system-ui,-apple-system,sans-serif;padding:16px}
main{background:var(--card);border:1px solid var(--line);border-radius:12px;box-shadow:0 14px 34px -26px rgba(0,0,0,.45);padding:28px;max-width:440px;width:100%}
h1{font:800 22px/1.15 "Schibsted Grotesk","Helvetica Neue",Arial,sans-serif;letter-spacing:-.01em;margin:0 0 8px}p{color:var(--muted);margin:0 0 16px;word-break:break-word}
.code{font:800 44px/1 "Schibsted Grotesk","Helvetica Neue",Arial,sans-serif;color:var(--accent);margin-bottom:12px}
input{width:100%;padding:11px 12px;border:1px solid var(--line);border-radius:8px;background:var(--bg);color:var(--fg);font-size:16px}
button,a.btn{display:inline-block;margin-top:12px;padding:10px 16px;border:0;border-radius:8px;background:var(--accent);color:var(--ink);font-weight:600;font-size:15px;text-decoration:none;cursor:pointer}
input:focus{outline:2px solid var(--accent);outline-offset:1px;border-color:var(--accent)}.err{color:var(--bad);margin:10px 0 0}small{color:var(--muted)}
"#;

pub fn page(status: u16, title: &str, body: &str) -> Result<Response> {
    let html = format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{t}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        t = esc(title)
    );
    let mut res = Response::from_html(html)?.with_status(status);
    let h = res.headers_mut();
    h.set("Cache-Control", "no-store")?;
    h.set("X-Frame-Options", "DENY")?;
    h.set(
        "Content-Security-Policy",
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
    )?;
    Ok(res)
}

pub fn not_found() -> Result<Response> {
    page(404, "Link not found", "<div class=code>404</div><h1>This short link does not exist</h1><p>Check the address for typos.</p>")
}

pub fn gone(reason: &str) -> Result<Response> {
    page(
        410,
        "Link unavailable",
        &format!(
            "<div class=code>410</div><h1>This link is no longer available</h1><p>{}</p>",
            esc(reason)
        ),
    )
}

/// `query` is the visitor's query string, kept on the form action so passthrough
/// parameters and the `qr` marker survive the POST.
pub fn too_many_requests(retry_after_secs: u32) -> Result<Response> {
    let mut res = page(
        429,
        "Too many requests",
        "<div class=code>429</div><h1>Slow down a little</h1><p>Too many requests from your network. Try again in a minute.</p>",
    )?;
    res.headers_mut()
        .set("Retry-After", &retry_after_secs.to_string())?;
    Ok(res)
}

pub fn password(slug: &str, query: Option<&str>, failed: bool) -> Result<Response> {
    let err = if failed {
        "<p class=err>Wrong password, try again.</p>"
    } else {
        ""
    };
    page(
        if failed { 401 } else { 200 },
        "Password required",
        &format!(
            "<h1>Password required</h1><p>The link <b>/{s}</b> is protected.</p>\
             <form method=post action=\"/{s}{q}\"><input type=password name=password autofocus required autocomplete=off placeholder=Password>\
             {err}<button type=submit>Continue</button></form>",
            s = esc(slug),
            q = query
                .filter(|q| !q.is_empty())
                .map(|q| format!("?{}", esc(q)))
                .unwrap_or_default()
        ),
    )
}

pub fn preview(slug: &str, dest: &str) -> Result<Response> {
    page(
        200,
        "Link preview",
        &format!(
            "<h1>/{s} goes to</h1><p>{d}</p><a class=btn href=\"{d}\" rel=\"noopener noreferrer\">Continue</a>",
            s = esc(slug),
            d = esc(dest)
        ),
    )
}

/// The card a link-preview crawler sees: Open Graph and Twitter tags, plus a refresh to
/// the destination for anything that renders it. Password-protected links get no
/// refresh, so the destination never leaks.
pub fn social_card(og: &crate::model::Og, dest: Option<&str>) -> Result<Response> {
    let title = og.title.as_deref().unwrap_or("");
    let mut meta = String::new();
    let mut tag = |prop: &str, v: &str, name: bool| {
        let attr = if name { "name" } else { "property" };
        meta.push_str(&format!("<meta {attr}=\"{prop}\" content=\"{}\">", esc(v)));
    };
    if let Some(t) = &og.title {
        tag("og:title", t, false);
        tag("twitter:title", t, true);
    }
    if let Some(d) = &og.description {
        tag("og:description", d, false);
        tag("description", d, true);
        tag("twitter:description", d, true);
    }
    if let Some(i) = &og.image {
        tag("og:image", i, false);
        tag("twitter:image", i, true);
        tag("twitter:card", "summary_large_image", true);
    } else {
        tag("twitter:card", "summary", true);
    }
    tag("og:type", "website", false);
    if let Some(d) = dest {
        tag("og:url", d, false);
        meta.push_str(&format!(
            "<meta http-equiv=\"refresh\" content=\"0;url={}\">",
            esc(d)
        ));
    }
    let html = format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><title>{t}</title>{meta}</head><body><h1>{t}</h1></body></html>",
        t = esc(title)
    );
    let mut res = Response::from_html(html)?;
    let h = res.headers_mut();
    h.set("Cache-Control", "public, max-age=300")?;
    // Same URL as the redirect: shared caches must not hand the card to people.
    h.set("Vary", "User-Agent")?;
    h.set("X-Robots-Tag", "noindex")?;
    h.set("Content-Security-Policy", "default-src 'none'; img-src *")?;
    Ok(res)
}

/// Tries to open the native app and falls back to the web destination when the app is
/// not installed (the page stays visible, so the timer fires).
pub fn open_app(app_uri: &str, web: &str) -> Result<Response> {
    let nonce = crate::util::hex(&crate::crypto::random_bytes(16));
    // JSON-encode for the script, then escape `<` so `</script>` cannot appear.
    let js = |v: &str| {
        serde_json::to_string(v)
            .unwrap_or_default()
            .replace('<', "\\u003c")
    };
    let body = format!(
        "<h1>Opening the app…</h1><p>If nothing happens, continue in the browser.</p>\
         <a class=btn href=\"{a}\">Open app</a> <a class=btn href=\"{w}\" rel=\"noopener noreferrer\">Continue in browser</a>\
         <script nonce=\"{nonce}\">(function(){{var app={ja},web={jw},t=setTimeout(function(){{location.replace(web)}},1500);\
         document.addEventListener('visibilitychange',function(){{if(document.hidden)clearTimeout(t)}});location.href=app}})();</script>",
        a = esc(app_uri),
        w = esc(web),
        ja = js(app_uri),
        jw = js(web),
    );
    let mut res = page(200, "Opening…", &body)?;
    res.headers_mut().set(
        "Content-Security-Policy",
        &format!("default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-{nonce}'; frame-ancestors 'none'"),
    )?;
    Ok(res)
}

pub fn landing() -> Result<Response> {
    page(
        200,
        "301.xorio.rs",
        "<div class=code>301</div><h1>301.xorio.rs</h1><p>A personal link shortener.</p>",
    )
}

pub fn forbidden(msg: &str) -> Result<Response> {
    page(
        403,
        "Forbidden",
        &format!(
            "<div class=code>403</div><h1>Admin access only</h1><p>{}</p>",
            esc(msg)
        ),
    )
}

pub fn unconfigured(msg: &str) -> Result<Response> {
    page(
        503,
        "Not configured",
        &format!(
            "<div class=code>503</div><h1>Admin is not configured</h1><p>{}</p>",
            esc(msg)
        ),
    )
}
