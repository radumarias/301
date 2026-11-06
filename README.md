# 301.xorio.rs

A personal URL shortener that runs entirely on Cloudflare's edge, written in Rust.

- **Redirects** are public and answered from the nearest Cloudflare location with a single Workers KV read.
- **Admin UI** (`/_/`) and **API** (`/api/`) sit behind Cloudflare Access, so only you can sign in; the Worker verifies the Access JWT again on every request.
- **Everything is persisted** in D1 (links, aliases, click analytics, settings); KV is a rebuildable read cache.

Docs: [Requirements](https://claude.ai/artifact/Vjvrkrzm6cYzkx13JHaWG8) · [Technical Spec](https://claude.ai/artifact/M1jxmfngKUaauraybg69aq)

## Features

| Area | What you get |
| --- | --- |
| Slugs | Custom (case-sensitive, `/` allowed) or generated: random chars (`k7RmQ2x`), words (`brave-otter-42`), names (`curious-lovelace`) |
| Redirects | 301 / 302 / 307 / 308 per link, query-string passthrough, `slug+` preview page, renamed slugs keep working as aliases |
| Rules | Expiry by date or exact max clicks (counted atomically) with fallback URL, password protection (PBKDF2), country and device (iOS / Android / desktop) targeting |
| A/B split | Up to four extra destinations with weights in %; per-variant click stats |
| Social previews | Custom title, description and image for Slack, X, iMessage, WhatsApp and other preview crawlers; people still get the redirect |
| Open in app | iOS / Android app links tried first on phones, falling back to the web destination |
| Marketing | UTM builder with live preview, tags, notes, QR codes (SVG/PNG, colours, error correction, scans counted separately) |
| Analytics | Clicks over time, countries, cities, referrers, devices, OS, browsers; bots and link-preview crawlers counted separately; no IPs stored |
| Import | Bitly, Rebrandly, Short.io, TinyURL (API) and CSV (auto-detects Bitly/Rebrandly/Short.io/TinyURL/Dub exports) with skip / rename / overwrite on conflicts |
| Health | Daily broken-link checker (404, 410, 5xx, no answer), dashboard notice, "broken" filter, check now |
| Webhooks | `link.created`, `link.updated`, `link.deleted`, `click`, signed with `X-301-Signature: sha256=<HMAC>` |
| Backups | Nightly JSON backup of links, aliases, settings and webhooks to R2, pruning, raw-click retention |
| Capture | Bookmarklet, iOS Share Sheet shortcut, service-token API with an [OpenAPI spec](docs/openapi.yaml) (also served at `/_/openapi.yaml`) |
| Admin | **Features** button with a full-screen tour of all of this, search, tag and status filters, bulk tag / disable / delete, CSV and JSON export, settings for root and 404 redirects, light and dark themes, `n` to create, paste a URL anywhere to shorten |

## Layout

```
src/
  lib.rs        router: /_/ (UI), /api/ (admin), everything else = redirect
  redirect.rs   the hot path: KV lookup, rules, password, A/B, previews, app links, click logging via waitUntil
  analytics.rs  Analytics Engine writes, SQL API stats, cron rollup into link totals
  health.rs     broken-link checker (cron)
  backup.rs     nightly R2 backups and click retention (cron)
  webhooks.rs   signed outgoing webhooks
  api.rs        admin JSON API
  auth.rs       Cloudflare Access JWT verification (WebCrypto RS256)
  store.rs      D1 + KV persistence (read-through KV cache)
  model.rs      link model, validation, UTM, KV record
  slug.rs       slug validation and generators
  import.rs     provider importers and commit logic
  ua.rs qr.rs crypto.rs pages.rs util.rs
ui/             admin SPA (vanilla JS, embedded with include_str!)
docs/openapi.yaml  admin API reference
tests/e2e.mjs   end-to-end tests against `wrangler dev`
migrations/     D1 schema
scripts/provision.sh   creates KV, D1 and the Access app idempotently
```

## Deploy (GitHub Actions)

1. In the Cloudflare dashboard, create an API token from the **Edit Cloudflare Workers** template and add:
   **Account → D1: Edit**, **Account → Access: Apps and Policies: Edit**,
   **Account → Access: Organizations, Identity Providers, and Groups: Read**, plus **Zone (xorio.rs) → DNS: Edit** only if you use a custom domain.
2. In the GitHub repo, add **secrets** `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`, and optionally
   **variables** `ADMIN_EMAILS` (defaults to the token owner's email) and `ACCESS_TEAM_DOMAIN`.
3. If you have never opened Zero Trust, visit [one.dash.cloudflare.com](https://one.dash.cloudflare.com) once to pick a team name.
4. Optional extras:
   - **Analytics Engine** (removes the D1 write per click): create a token with **Account → Account Analytics: Read**
     and add it as the `ANALYTICS_API_TOKEN` secret. Without it clicks keep going to D1 and the binding is left out.
   - **Backups**: enable R2 on the account and add **Account → Workers R2 Storage: Edit** to the deploy token.
     Without R2 the bucket binding is left out and backups stay off.
   - **Custom domain**: set the `DOMAIN` variable (e.g. `301.xorio.rs`); the provisioner switches `wrangler.toml`
     from workers.dev to a custom-domain route.
   - **Staging**: run the workflow manually with target `staging` for a full second copy (own Worker, KV, D1, R2,
     Access app) on `xorio-301-staging.<subdomain>.workers.dev`, or on the `STAGING_DOMAIN` variable.
5. Push to `main`. The workflow tests, provisions KV / D1 / Access, applies migrations and deploys to `xorio-301.<your-subdomain>.workers.dev`.

## Local development

```sh
npm ci
cargo install worker-build
echo 'DEV_AUTH_EMAIL=you@localhost' > .dev.vars   # bypasses Access only while ACCESS_* vars are empty
npx wrangler d1 migrations apply xorio-301 --local
npx wrangler dev
open http://localhost:8787/_/
```

`cargo test` runs the unit tests (slugs, UA parsing, UTM, validation, JWT claims, importers, QR, A/B split,
webhooks, backups). With `npx wrangler dev --test-scheduled` running, `node tests/e2e.mjs http://localhost:8787`
runs the end-to-end tests (redirects, passwords, click limits, previews, A/B, app links, health checks,
webhooks, backups, both cron jobs); CI runs them before every deploy.

## Costs

With the `ANALYTICS_API_TOKEN` secret set, clicks go to Workers Analytics Engine and cost no D1 writes (a cron
job adds them to each link's total every 10 minutes), so the free plan is limited by Workers requests
(100,000 a day) instead. Links with a click limit still count each click in D1, so the limit stays exact.

On the Workers free plan the limiting factor is D1 writes (2 per human click), so roughly 50,000 clicks a day.
The $5/month Workers Paid plan covers about 25 million clicks a month. Imports write no KV entries; each
link's edge record is filled on its first click.

The public redirect path is rate limited per IP (100 requests a minute by default, set in the
`[[ratelimits]]` block of `wrangler.toml`). Over the limit a visitor gets a `429` before any KV read or
D1 write, which also slows down password guessing on protected links. The admin (`/_/`, `/api/`) is not
limited. The Worker request itself is still counted; to drop abusive traffic before it reaches the Worker,
add a WAF rate-limiting rule once the shortener is on a custom domain (WAF rules do not run on workers.dev).
