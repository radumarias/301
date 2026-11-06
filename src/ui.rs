//! The single-page app (anonymous shortener, user and admin views), compiled into the Worker.

use worker::{Response, Result};

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

pub fn serve(path: &str) -> Result<Response> {
    let (body, ctype) = match path {
        "/_/app.js" => (APP_JS, "text/javascript; charset=utf-8"),
        "/_/app.css" => (APP_CSS, "text/css; charset=utf-8"),
        "/_" => return crate::util::redirect("/_/", 302),
        _ => (INDEX, "text/html; charset=utf-8"),
    };
    let mut res = Response::ok(body)?;
    let h = res.headers_mut();
    h.set("Content-Type", ctype)?;
    h.set("Cache-Control", "no-store")?;
    h.set("X-Frame-Options", "DENY")?;
    h.set("X-Content-Type-Options", "nosniff")?;
    h.set("Referrer-Policy", "same-origin")?;
    h.set(
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
         script-src 'self' https://challenges.cloudflare.com; frame-src https://challenges.cloudflare.com; connect-src 'self'; frame-ancestors 'none'",
    )?;
    Ok(res)
}
