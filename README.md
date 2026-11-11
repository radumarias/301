# 301.xorio.rs

A URL shortener that runs entirely on Cloudflare's edge, written in Rust.

- **Redirects** are public and answered from the nearest Cloudflare location with a single Workers KV read.
- **Anyone can shorten a link** at `/_/` without an account: random name, expires after 30 days, a private manage link for stats and deleting.
- **Sign in with Google** to keep links, choose names and use every feature. Users only see their own links; admins (`ADMIN_EMAILS`) see everything, import, and moderate.
- **Everything is persisted** in D1 (users, links, aliases, click analytics, reports, settings); KV is a rebuildable read cache.

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
| Accounts | Google sign-in (OpenID Connect in the Worker, 30-day sessions), per-user links, links made anonymously on a device move to the account on sign-in |
| Abuse protection | Turnstile on anonymous creation, per-IP and per-user creation limits, Google Safe Browsing, no loops / private addresses / credentials in URLs, "Report this link" on every preview page (`slug+`), admin block (owner cannot undo), user bans |

## Layout

```
src/
  lib.rs        router: /_/ (UI), /auth/ (Google), /api/, everything else = redirect
  redirect.rs   the hot path: KV lookup, rules, password, click logging via waitUntil
  api.rs        JSON API (scoped by owner; admin-only routes)
  anon.rs       public API: config, anonymous links, reports
  auth.rs       who is calling: session cookie, Cloudflare Access JWT, local dev
  google.rs     Sign in with Google (authorization code + PKCE, ID token verified)
  account.rs    users, sessions, ownership, limits, reports
  safety.rs     URL checks, Safe Browsing, Turnstile
  store.rs      D1 + KV persistence (read-through KV cache)
  model.rs      link model, validation, UTM, KV record
  slug.rs       slug validation and generators
  import.rs     provider importers and commit logic
  ua.rs qr.rs crypto.rs pages.rs util.rs
ui/             admin SPA (vanilla JS, embedded with include_str!)
migrations/     D1 schema
scripts/provision.sh   creates KV and D1 idempotently (and the Access app, unless Google sign-in is configured)
```

## Deploy (GitHub Actions)

1. In the Cloudflare dashboard, create an API token from the **Edit Cloudflare Workers** template and add:
   **Account → D1: Edit**, **Account → Access: Apps and Policies: Edit**,
   **Account → Access: Organizations, Identity Providers, and Groups: Read**, plus **Zone (xorio.rs) → DNS: Edit** only if you use a custom domain.
2. In the GitHub repo, add **secrets** `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`, and optionally
   the **secret** `ADMIN_EMAILS` (defaults to the token owner's email; a secret so it stays out of public CI logs)
   and the **variable** `ACCESS_TEAM_DOMAIN`.
3. If you have never opened Zero Trust, visit [one.dash.cloudflare.com](https://one.dash.cloudflare.com) once to pick a team name.
4. Push to `main`. The workflow tests, provisions KV / D1 / Access, applies migrations and deploys to `xorio-301.<your-subdomain>.workers.dev` (set the `DOMAIN` variable and the route in `wrangler.toml` to use a custom domain such as 301.xorio.rs).

## Opening it to everyone (Google sign-in and anonymous links)

Until `GOOGLE_CLIENT_ID` is set, the app stays admin-only behind Cloudflare Access as before.

1. **Google OAuth client.** In [Google Cloud console](https://console.cloud.google.com/apis/credentials) create a project,
   set up the **OAuth consent screen** (External, scopes `openid`, `email`, `profile`; publish it so it is not limited to test users),
   then **Create credentials → OAuth client ID → Web application** with the authorized redirect URI
   `https://<your host>/auth/callback` (add `http://localhost:8787/auth/callback` for local testing).
2. **Turnstile.** In the Cloudflare dashboard, **Turnstile → Add widget**, hostname = your host, mode Managed.
3. **Safe Browsing (optional, recommended).** In the same Google Cloud project enable the **Safe Browsing API** and create an API key.
4. In the GitHub repo add **variables** `GOOGLE_CLIENT_ID`, `TURNSTILE_SITE_KEY` and **secrets** `GOOGLE_CLIENT_SECRET`,
   `TURNSTILE_SECRET`, `SAFE_BROWSING_KEY`, `ADMIN_EMAILS`. Make sure `ADMIN_EMAILS` holds the Google address you sign in with.
5. Re-run the deploy. The provision step deletes the "301 admin" Access application (the app has to be public now),
   the secrets are uploaded, and the first admin to sign in becomes the owner of every link made before accounts existed.
   Access service tokens (`ADMIN_SERVICE_TOKENS`) stop working at that point, since Access is gone.

Settings → Public access turns anonymous links off (a kill switch) and sets their lifetime and the creation limits.
Moderation lists reports and users: block a link (visitors get a 410 and the owner cannot re-enable it) or ban an account.

## Local development

```sh
npm ci
cargo install worker-build
echo 'DEV_AUTH_EMAIL=you@localhost' > .dev.vars   # signs you in as admin, only while no Access or Google config is set
npx wrangler d1 migrations apply xorio-301 --local
npx wrangler dev
open http://localhost:8787/_/
```

`cargo test` runs the unit tests (slugs, UA parsing, UTM, validation, JWT and Google claims, URL safety checks, importers, QR).

## Costs

On the Workers free plan the limiting factor is D1 writes (2 per human click), so roughly 50,000 clicks a day.
The $5/month Workers Paid plan covers about 25 million clicks a month. Imports write no KV entries; each
link's edge record is filled on its first click.

The public redirect path is rate limited per IP (100 requests a minute by default, set in the
`[[ratelimits]]` block of `wrangler.toml`). Over the limit a visitor gets a `429` before any KV read or
D1 write, which also slows down password guessing on protected links. The app and API (`/_/`, `/api/`) are not
limited by it; creating links has its own per-IP and per-user limits. The Worker request itself is still counted; to drop abusive traffic before it reaches the Worker,
add a WAF rate-limiting rule once the shortener is on a custom domain (WAF rules do not run on workers.dev).

Opening it to the public stays on the free tiers at small scale: Google sign-in, Turnstile and Safe Browsing
(non-commercial use) are free. Each link made by a signed-in user costs one KV write (1,000 a day on the free plan) and one D1 write
(anonymous links skip the KV write until their first click);
signed-in API calls add one D1 read for the session. If creation volume grows past that, Workers Paid ($5/month) lifts both limits.
