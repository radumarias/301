//! The admin single-page app, compiled into the Worker.

use worker::{Response, Result};

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const OPENAPI: &str = include_str!("../docs/openapi.yaml");

pub fn serve(path: &str) -> Result<Response> {
    let (body, ctype) = match path {
        "/_/app.js" => (APP_JS, "text/javascript; charset=utf-8"),
        "/_/app.css" => (APP_CSS, "text/css; charset=utf-8"),
        "/_/openapi.yaml" => (OPENAPI, "text/yaml; charset=utf-8"),
        "/_" => return crate::util::redirect("/_/", 302),
        _ => (INDEX, "text/html; charset=utf-8"),
    };
    let mut res = Response::ok(body)?;
    let h = res.headers_mut();
    h.set("Content-Type", ctype)?;
    h.set("Cache-Control", "no-store")?;
    h.set("Referrer-Policy", "same-origin")?;
    h.set("Cross-Origin-Opener-Policy", "same-origin")?;
    h.set(
        "Permissions-Policy",
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    )?;
    h.set(
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' data: blob: https:; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    )?;
    Ok(res)
}
