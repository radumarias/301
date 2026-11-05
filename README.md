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
| Rules | Expiry by date or max clicks with fallback URL, password protection (PBKDF2), country and device (iOS / Android / desktop) targeting |
| Marketing | UTM builder with live preview, tags, notes, QR codes (SVG/PNG, colours, error correction, scans counted separately) |
| Analytics | Clicks over time, countries, cities, referrers, devices, OS, browsers; bots and link-preview crawlers counted separately; no IPs stored |
| Import | Bitly, Rebrandly, Short.io, TinyURL (API) and CSV (auto-detects Bitly/Rebrandly/Short.io/TinyURL/Dub exports) with skip / rename / overwrite on conflicts |
| Admin | Search, tag and status filters, bulk tag / disable / delete, CSV and JSON export, settings for root and 404 redirects, light and dark themes, `n` to create, paste a URL anywhere to shorten |

## Layout

```
src/
  lib.rs        router: /_/ (UI), /api/ (admin), everything else = redirect
  redirect.rs   the hot path: KV lookup, rules, password, click logging via waitUntil
  api.rs        admin JSON API
  auth.rs       Cloudflare Access JWT verification (WebCrypto RS256)
  store.rs      D1 + KV persistence (read-through KV cache)
  model.rs      link model, validation, UTM, KV record
  slug.rs       slug validation and generators
  import.rs     provider importers and commit logic
  ua.rs qr.rs crypto.rs pages.rs util.rs
ui/             admin SPA (vanilla JS, embedded with include_str!)
migrations/     D1 schema
scripts/provision.sh   creates KV, D1 and the Access app idempotently
```

## Deploy (GitHub Actions)

1. In the Cloudflare dashboard, create an API token from the **Edit Cloudflare Workers** template and add:
   **Account → D1: Edit**, **Account → Access: Apps and Policies: Edit**,
   **Account → Access: Organizations, Identity Providers, and Groups: Read**, **Zone (xorio.rs) → DNS: Edit**.
2. In the GitHub repo, add **secrets** `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`, and optionally
   **variables** `ADMIN_EMAILS` (defaults to the token owner's email) and `ACCESS_TEAM_DOMAIN`.
3. If you have never opened Zero Trust, visit [one.dash.cloudflare.com](https://one.dash.cloudflare.com) once to pick a team name.
4. Push to `main`. The workflow tests, provisions KV / D1 / Access, applies migrations and deploys to the `301.xorio.rs` custom domain.

## Local development

```sh
npm ci
cargo install worker-build
echo 'DEV_AUTH_EMAIL=you@localhost' > .dev.vars   # bypasses Access only while ACCESS_* vars are empty
npx wrangler d1 migrations apply xorio-301 --local
npx wrangler dev
open http://localhost:8787/_/
```

`cargo test` runs the unit tests (slugs, UA parsing, UTM, validation, JWT claims, importers, QR).

## Costs

On the Workers free plan the limiting factor is D1 writes (2 per human click), so roughly 50,000 clicks a day.
The $5/month Workers Paid plan covers about 25 million clicks a month. Imports write no KV entries; each
link's edge record is filled on its first click.
