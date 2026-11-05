//! Small server-rendered pages for visitors (password form, errors, preview).

use crate::util::esc;
use worker::{Response, Result};

const STYLE: &str = r#"
:root{--bg:#f7f7f5;--fg:#1d1d1b;--muted:#6b6b66;--card:#fff;--line:#e4e4df;--accent:#e8590c}
@media (prefers-color-scheme:dark){:root{--bg:#141413;--fg:#ececea;--muted:#9a9a94;--card:#1f1f1d;--line:#33332f;--accent:#ff8a3d}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);
font:16px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;padding:16px}
main{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:28px;max-width:440px;width:100%}
h1{font-size:20px;margin:0 0 8px}p{color:var(--muted);margin:0 0 16px;word-break:break-word}
.code{font:600 44px/1 ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--accent);margin-bottom:12px}
input{width:100%;padding:11px 12px;border:1px solid var(--line);border-radius:9px;background:var(--bg);color:var(--fg);font-size:16px}
button,a.btn{display:inline-block;margin-top:12px;padding:10px 16px;border:0;border-radius:9px;background:var(--accent);color:#fff;font-weight:600;font-size:15px;text-decoration:none;cursor:pointer}
.err{color:#d9480f;margin:10px 0 0}small{color:var(--muted)}
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

pub fn password(slug: &str, failed: bool) -> Result<Response> {
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
             <form method=post action=\"/{s}\"><input type=password name=password autofocus required autocomplete=off placeholder=Password>\
             {err}<button type=submit>Continue</button></form>",
            s = esc(slug)
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
