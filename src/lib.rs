//! 301.xorio.rs: a URL shortener on Cloudflare Workers.
//!
//! * `/<slug>`  public redirect (KV on the hot path, D1 as source of truth)
//! * `/_/`      web app (embedded static files): anonymous shortening, or sign in
//! * `/auth/`   Sign in with Google
//! * `/api/`    JSON API: a few public routes, the rest for signed-in users

pub mod account;
pub mod auth;
pub mod crypto;
pub mod import;
pub mod model;
pub mod qr;
pub mod slug;
pub mod store;
pub mod ua;
pub mod util;

mod anon;
mod api;
mod google;
mod pages;
mod redirect;
pub mod safety;
mod ui;

use worker::{event, Context, Env, Request, Response, Result};

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let path = req.path();
    let res = if path == "/_" || path.starts_with("/_/") {
        // The app itself is public; the API decides what each caller may do.
        ui::serve(&path)
    } else if path.starts_with("/auth/") {
        google::handle(req, env).await
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
