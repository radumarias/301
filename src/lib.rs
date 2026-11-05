//! 301.xorio.rs: a personal URL shortener on Cloudflare Workers.
//!
//! * `/<slug>`  public redirect (KV on the hot path, D1 as source of truth)
//! * `/_/`      admin UI (embedded static files), behind Cloudflare Access
//! * `/api/`    admin JSON API, behind Cloudflare Access + in-Worker JWT check

pub mod auth;
pub mod crypto;
pub mod import;
pub mod model;
pub mod qr;
pub mod slug;
pub mod store;
pub mod ua;
pub mod util;

mod api;
mod pages;
mod redirect;
mod ui;

use worker::{event, Context, Env, Request, Response, Result};

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let path = req.path();
    let res = if path == "/_" || path.starts_with("/_/") {
        match auth::verify(&req, &env).await {
            Ok(_) => ui::serve(&path),
            Err(auth::AuthError::NotConfigured(m)) => pages::unconfigured(m),
            Err(auth::AuthError::Denied(m)) => pages::forbidden(&m),
        }
    } else if path.starts_with("/api/") {
        api::handle(req, env).await
    } else {
        redirect::handle(req, env, ctx).await
    };
    res.or_else(|e| {
        worker::console_error!("unhandled: {e}");
        Response::error("Internal error", 500)
    })
}
