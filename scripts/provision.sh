#!/usr/bin/env bash
# Idempotently provisions everything 301.xorio.rs needs on Cloudflare and writes the
# resulting ids into wrangler.toml (in the CI workspace; nothing secret is committed).
#
#   - Workers KV namespace  xorio-301-links
#   - D1 database           xorio-301
#   - R2 bucket             xorio-301-backups (nightly backups; skipped, and its
#                           binding removed, when R2 is not enabled on the account)
#   - Access application    "301 admin" on $DOMAIN/_ and $DOMAIN/api,
#                           allowing only ADMIN_EMAILS
#   - Custom domain route   when DOMAIN is set to something other than *.workers.dev
#
# Needs: CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID. Optional: ADMIN_EMAILS (defaults
# to the token owner's email), ACCESS_TEAM_DOMAIN (looked up when the token can read it),
# DOMAIN (default: the Worker's workers.dev host, xorio-301.<account subdomain>.workers.dev),
# ANALYTICS_API_TOKEN (keeps the Analytics Engine binding), and WORKER_NAME / KV_TITLE /
# D1_NAME / R2_BUCKET / AE_DATASET / APP_NAME / TOML to provision a second copy (staging).
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?set CLOUDFLARE_API_TOKEN}"
: "${CLOUDFLARE_ACCOUNT_ID:?set CLOUDFLARE_ACCOUNT_ID}"
WORKER_NAME="${WORKER_NAME:-xorio-301}"
KV_TITLE="${KV_TITLE:-xorio-301-links}"
D1_NAME="${D1_NAME:-xorio-301}"
R2_BUCKET="${R2_BUCKET:-xorio-301-backups}"
AE_DATASET="${AE_DATASET:-xorio_301_clicks}"
APP_NAME="${APP_NAME:-301 admin}"
API="https://api.cloudflare.com/client/v4"
ACC="$API/accounts/$CLOUDFLARE_ACCOUNT_ID"
TOML="${TOML:-wrangler.toml}"

cf() { # cf METHOD URL [JSON]
  local out
  if [[ $# -ge 3 ]]; then
    out=$(curl -sS -X "$1" "$2" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H "Content-Type: application/json" --data "$3")
  else
    out=$(curl -sS -X "$1" "$2" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN")
  fi
  if [[ "$(jq -r '.success' <<<"$out")" != "true" ]]; then
    echo "Cloudflare API $1 $2 failed: $(jq -c '.errors' <<<"$out")" >&2
    return 1
  fi
  printf '%s' "$out"
}

echo "== KV namespace $KV_TITLE"
KV_ID=$(cf GET "$ACC/storage/kv/namespaces?per_page=100" | jq -r --arg t "$KV_TITLE" '.result[] | select(.title==$t) | .id' | head -1)
if [[ -z "$KV_ID" ]]; then
  KV_ID=$(cf POST "$ACC/storage/kv/namespaces" "$(jq -nc --arg t "$KV_TITLE" '{title:$t}')" | jq -r '.result.id')
  echo "   created $KV_ID"
else
  echo "   exists  $KV_ID"
fi

echo "== D1 database $D1_NAME"
D1_ID=$(cf GET "$ACC/d1/database?name=$D1_NAME&per_page=100" | jq -r --arg n "$D1_NAME" '.result[] | select(.name==$n) | .uuid' | head -1)
if [[ -z "$D1_ID" ]]; then
  D1_ID=$(cf POST "$ACC/d1/database" "$(jq -nc --arg n "$D1_NAME" '{name:$n}')" | jq -r '.result.uuid')
  echo "   created $D1_ID"
else
  echo "   exists  $D1_ID"
fi

echo "== R2 bucket $R2_BUCKET"
R2_OK=1
if curl -sS "$ACC/r2/buckets/$R2_BUCKET" -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" | jq -e '.success' >/dev/null; then
  echo "   exists"
elif cf POST "$ACC/r2/buckets" "$(jq -nc --arg n "$R2_BUCKET" '{name:$n}')" >/dev/null; then
  echo "   created"
else
  R2_OK=0
  echo "   R2 is not available (enable it in the dashboard and give the token R2 Edit); backups stay off" >&2
fi

echo "== Admin emails"
if [[ -z "${ADMIN_EMAILS:-}" ]]; then
  ADMIN_EMAILS=$(cf GET "$API/user" | jq -r '.result.email')
fi
# CI logs of a public repo are public: mask the addresses and never print them.
if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
  for e in ${ADMIN_EMAILS//,/ }; do echo "::add-mask::$e"; done
fi
echo "   $(awk -F, '{print NF}' <<<"$ADMIN_EMAILS") address(es)"

echo "== Access team domain"
if [[ -z "${ACCESS_TEAM_DOMAIN:-}" ]]; then
  ACCESS_TEAM_DOMAIN=$(cf GET "$ACC/access/organizations" | jq -r '.result.auth_domain // empty') || true
fi
if [[ -z "${ACCESS_TEAM_DOMAIN:-}" ]]; then
  echo "Could not read the Zero Trust team domain. Open one.dash.cloudflare.com once to create your team," >&2
  echo "then set the ACCESS_TEAM_DOMAIN repo variable (e.g. myteam.cloudflareaccess.com)." >&2
  exit 1
fi
echo "   $ACCESS_TEAM_DOMAIN"

if [[ -z "${DOMAIN:-}" ]]; then
  SUB=$(cf GET "$ACC/workers/subdomain" | jq -r '.result.subdomain // empty') || true
  if [[ -z "$SUB" ]]; then
    echo "No workers.dev subdomain on this account yet. Open Workers & Pages in the dashboard once to pick one." >&2
    exit 1
  fi
  DOMAIN="$WORKER_NAME.$SUB.workers.dev"
fi
echo "== Host: $DOMAIN"
[[ -n "${GITHUB_ENV:-}" ]] && echo "DOMAIN=$DOMAIN" >> "$GITHUB_ENV"

echo "== Access application '$APP_NAME'"
include=$(jq -nc --arg e "$ADMIN_EMAILS" '[$e | split(",")[] | gsub("^\\s+|\\s+$";"") | select(length>0) | {email:{email:.}}]')
APP=$(cf GET "$ACC/access/apps?per_page=100" | jq -c --arg n "$APP_NAME" '[.result[] | select(.name==$n)][0] // empty')
app_body=$(jq -nc --arg n "$APP_NAME" --arg d "$DOMAIN" '{
  name: $n, type: "self_hosted", domain: ($d + "/_"),
  destinations: [{type:"public", uri:($d + "/_")}, {type:"public", uri:($d + "/api")}],
  session_duration: "24h", app_launcher_visible: true, auto_redirect_to_identity: false,
  http_only_cookie_attribute: true, same_site_cookie_attribute: "lax"
}')
if [[ -z "$APP" ]]; then
  APP=$(cf POST "$ACC/access/apps" "$app_body" | jq -c '.result')
  echo "   created $(jq -r '.id' <<<"$APP")"
else
  APP=$(cf PUT "$ACC/access/apps/$(jq -r '.id' <<<"$APP")" "$app_body" | jq -c '.result')
  echo "   updated $(jq -r '.id' <<<"$APP")"
fi
APP_ID=$(jq -r '.id' <<<"$APP")
ACCESS_AUD=$(jq -r '.aud' <<<"$APP")

echo "== Access policy 'Owner'"
POL_ID=$(cf GET "$ACC/access/apps/$APP_ID/policies" | jq -r '[.result[] | select(.name=="Owner")][0].id // empty')
pol_body=$(jq -nc --argjson inc "$include" '{name:"Owner", decision:"allow", include:$inc, precedence:1}')
if [[ -z "$POL_ID" ]]; then
  cf POST "$ACC/access/apps/$APP_ID/policies" "$pol_body" >/dev/null && echo "   created"
else
  cf PUT "$ACC/access/apps/$APP_ID/policies/$POL_ID" "$pol_body" >/dev/null && echo "   updated"
fi

echo "== Writing $TOML"
if [[ -z "${ANALYTICS_API_TOKEN:-}" ]]; then
  echo "   no ANALYTICS_API_TOKEN: clicks are logged to D1 (Analytics Engine binding removed)"
  sed -i.bak '/^# >>> ae/,/^# <<< ae/d' "$TOML"
fi
if [[ "$R2_OK" != 1 ]]; then
  sed -i.bak '/^# >>> r2/,/^# <<< r2/d' "$TOML"
fi
if [[ "$DOMAIN" != *.workers.dev ]]; then
  # Serve on the custom domain instead of workers.dev (DNS record and cert are automatic
  # when the zone is on this account).
  sed -i.bak "s|^workers_dev = true|workers_dev = false\nroutes = [{ pattern = \"$DOMAIN\", custom_domain = true }]|" "$TOML"
fi
# A free-form value as a TOML string body, safe inside the sed replacement below.
toml_sed() { local v=${1//\\/\\\\}; v=${v//\"/\\\"}; printf '%s' "$v" | sed -e 's/[\\&|]/\\&/g'; }
PRIVACY_CONTACT_ESC=$(toml_sed "${PRIVACY_CONTACT:-}")
sed -i.bak \
  -e "s|^id = \"REPLACE_KV_ID\"|id = \"$KV_ID\"|" \
  -e "s|^database_id = \"REPLACE_D1_ID\"|database_id = \"$D1_ID\"|" \
  -e "s|^ACCESS_TEAM_DOMAIN = \".*\"|ACCESS_TEAM_DOMAIN = \"$ACCESS_TEAM_DOMAIN\"|" \
  -e "s|^ACCESS_AUD = \".*\"|ACCESS_AUD = \"$ACCESS_AUD\"|" \
  -e "s|^ADMIN_EMAILS = \".*\"|ADMIN_EMAILS = \"$ADMIN_EMAILS\"|" \
  -e "s|^ADMIN_SERVICE_TOKENS = \".*\"|ADMIN_SERVICE_TOKENS = \"${ADMIN_SERVICE_TOKENS:-}\"|" \
  -e "s|^CLOUDFLARE_ACCOUNT_ID = \".*\"|CLOUDFLARE_ACCOUNT_ID = \"$CLOUDFLARE_ACCOUNT_ID\"|" \
  -e "s|^AE_DATASET = \".*\"|AE_DATASET = \"$AE_DATASET\"|" \
  -e "s|^dataset = \"xorio_301_clicks\"|dataset = \"$AE_DATASET\"|" \
  -e "s|^bucket_name = \"xorio-301-backups\"|bucket_name = \"$R2_BUCKET\"|" \
  -e "s|^name = \"xorio-301\"|name = \"$WORKER_NAME\"|" \
  -e "s|^PRIVACY_CONTACT = \".*\"|PRIVACY_CONTACT = \"$PRIVACY_CONTACT_ESC\"|" \
  "$TOML"
rm -f "$TOML.bak"
grep -nE '^(name|id|database_id|bucket_name|dataset|routes|workers_dev|ACCESS_TEAM_DOMAIN|ACCESS_AUD) =' "$TOML"
echo "Provisioned."
