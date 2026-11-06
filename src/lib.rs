//! 301.xorio.rs: a personal URL shortener on Cloudflare Workers.
//!
//! * `/<slug>`  public redirect (KV on the hot path, D1 as source of truth)
//! * `/_/`      admin UI (embedded static files), behind Cloudflare Access
//! * `/api/`    admin JSON API, behind Cloudflare Access + in-Worker JWT check

pub mod analytics;
pub mod auth;
pub mod backup;
pub mod crypto;
pub mod health;
pub mod import;
pub mod model;
pub mod qr;
pub mod slug;
pub mod store;
pub mod ua;
pub mod util;
pub mod webhooks;

mod api;
mod pages;
mod redirect;
mod ui;

use worker::{event, Context, Env, Request, Response, Result, ScheduleContext, ScheduledEvent};

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
        api::handle(req, env, ctx).await
    } else {
        redirect::handle(req, env, ctx).await
    };
    res.or_else(|e| {
        worker::console_error!("unhandled: {e}");
        Response::error("Internal error", 500)
    })
}

/// Cron jobs (see `[triggers]` in wrangler.toml):
/// * every 10 minutes: roll Analytics Engine clicks into link totals, check a batch of
///   destinations for broken links
/// * nightly: back up to R2, prune old backups, delete raw clicks past retention
#[event(scheduled)]
async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if event.cron() == NIGHTLY {
        let st = store::settings(&env).await.unwrap_or_default();
        if backup::enabled(&env) {
            match backup::run(&env).await {
                Ok(key) => worker::console_log!("backup written: {key}"),
                Err(e) => worker::console_error!("backup failed: {e}"),
            }
            if let Err(e) = backup::prune(&env, st.backup_keep_days).await {
                worker::console_error!("backup prune failed: {e}");
            }
        }
        match backup::apply_retention(&env, &st).await {
            Ok(n) if n > 0 => worker::console_log!("deleted {n} old click rows"),
            Ok(_) => {}
            Err(e) => worker::console_error!("click retention failed: {e}"),
        }
        return;
    }
    if let Err(e) = analytics::rollup(&env).await {
        worker::console_error!("analytics rollup failed: {e}");
    }
    match health::run_batch(&env).await {
        Ok(n) if n > 0 => worker::console_log!("checked {n} destinations"),
        Ok(_) => {}
        Err(e) => worker::console_error!("link check failed: {e}"),
    }
}

/// Must match the nightly entry in wrangler.toml `[triggers] crons`.
pub const NIGHTLY: &str = "17 3 * * *";
