// End-to-end checks against a running Worker (`wrangler dev` with DEV_AUTH_EMAIL set).
// Usage: node tests/e2e.mjs http://127.0.0.1:8787
import assert from "node:assert/strict";

const BASE = (process.argv[2] || "http://127.0.0.1:8787").replace(/\/$/, "");
const run = Date.now().toString(36);
const UA = {
  desktop: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36",
  iphone: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
  slack: "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)",
};

async function api(method, path, body) {
  const res = await fetch(`${BASE}/api${path}`, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let data;
  try { data = JSON.parse(text); } catch { data = text; }
  return { status: res.status, data };
}
const visit = (path, ua = UA.desktop, init = {}) =>
  fetch(`${BASE}${path}`, { redirect: "manual", ...init, headers: { "User-Agent": ua, ...(init.headers || {}) } });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function eventually(fn, what, tries = 20) {
  for (let i = 0; i < tries; i++) {
    if (await fn()) return;
    await sleep(500);
  }
  assert.fail(`timed out waiting for ${what}`);
}

const tests = [];
const test = (name, fn) => tests.push([name, fn]);

test("create, redirect and passthrough", async () => {
  const { status, data } = await api("POST", "/links", { url: "https://example.com/page", slug: `e2e-${run}` });
  assert.equal(status, 201, JSON.stringify(data));
  const res = await visit(`/e2e-${run}?a=1`);
  assert.equal(res.status, 302);
  assert.equal(res.headers.get("location"), "https://example.com/page?a=1");
  await eventually(async () => (await api("GET", `/links/${data.id}`)).data.clicks === 1, "click counted");
  const st = await api("GET", `/links/${data.id}/stats?range=24h`);
  assert.equal(st.status, 200);
  assert.equal(st.data.clicks, 1);
});

test("unknown slug is a 404 and preview shows the destination", async () => {
  assert.equal((await visit(`/nope-${run}`)).status, 404);
  await api("POST", "/links", { url: "https://example.com/pv", slug: `pv-${run}` });
  const res = await visit(`/pv-${run}+`);
  assert.equal(res.status, 200);
  assert.match(await res.text(), /example\.com\/pv/);
});

test("password protection", async () => {
  await api("POST", "/links", { url: "https://example.com/secret", slug: `pw-${run}`, password: "hunter2" });
  assert.equal((await visit(`/pw-${run}`)).status, 200);
  const bad = await visit(`/pw-${run}`, UA.desktop, { method: "POST", body: new URLSearchParams({ password: "nope" }) });
  assert.equal(bad.status, 401);
  const ok = await visit(`/pw-${run}`, UA.desktop, { method: "POST", body: new URLSearchParams({ password: "hunter2" }) });
  assert.equal(ok.status, 303);
  assert.equal(ok.headers.get("location"), "https://example.com/secret");
});

test("max clicks is exact and then falls back", async () => {
  const { data } = await api("POST", "/links", { url: "https://example.com/limited", slug: `max-${run}`, max_clicks: 2, fallback_url: "https://example.com/over" });
  const locs = [];
  for (let i = 0; i < 4; i++) locs.push((await visit(`/max-${run}`)).headers.get("location"));
  assert.deepEqual(locs, ["https://example.com/limited", "https://example.com/limited", "https://example.com/over", "https://example.com/over"]);
  const link = (await api("GET", `/links/${data.id}`)).data;
  assert.equal(link.clicks, 2);
  assert.equal(link.status, "expired");
  // Raising the limit reopens the link.
  await api("PATCH", `/links/${data.id}`, { max_clicks: 3 });
  assert.equal((await visit(`/max-${run}`)).headers.get("location"), "https://example.com/limited");
});

test("bots do not use up click limits", async () => {
  await api("POST", "/links", { url: "https://example.com/b", slug: `botmax-${run}`, max_clicks: 1 });
  for (let i = 0; i < 3; i++) await visit(`/botmax-${run}`, "curl/8.5.0");
  assert.equal((await visit(`/botmax-${run}`)).headers.get("location"), "https://example.com/b");
});

test("social preview card for chat crawlers only", async () => {
  await api("POST", "/links", { url: "https://example.com/og", slug: `og-${run}`, og: { title: "Hello <world>", description: "A card", image: "https://example.com/card.png" } });
  const card = await visit(`/og-${run}`, UA.slack);
  assert.equal(card.status, 200);
  const html = await card.text();
  assert.match(html, /property="og:title" content="Hello &lt;world&gt;"/);
  assert.match(html, /og:image" content="https:\/\/example\.com\/card\.png"/);
  assert.match(html, /http-equiv="refresh" content="0;url=https:\/\/example\.com\/og"/);
  assert.equal((await visit(`/og-${run}`)).status, 302, "people still get the redirect");
});

test("A/B split sends traffic to variants and records them", async () => {
  const { status, data } = await api("POST", "/links", { url: "https://example.com/a", slug: `ab-${run}`, variants: [{ url: "https://example.com/b", weight: 99 }] });
  assert.equal(status, 201, JSON.stringify(data));
  const seen = new Set();
  for (let i = 0; i < 8; i++) seen.add((await visit(`/ab-${run}`)).headers.get("location"));
  assert.ok(seen.has("https://example.com/b"), [...seen].join());
  await eventually(async () => {
    const st = await api("GET", `/links/${data.id}/stats?range=24h`);
    return (st.data.top.variant || []).some((r) => r.k === "https://example.com/b" && r.n > 0);
  }, "variant stats");
  const bad = await api("POST", "/links", { url: "https://example.com/a", variants: [{ url: "https://example.com/b", weight: 100 }] });
  assert.equal(bad.status, 400);
});

test("app deep link page on phones", async () => {
  await api("POST", "/links", { url: "https://example.com/web", slug: `app-${run}`, app: { ios: "myapp://item/42" } });
  const res = await visit(`/app-${run}`, UA.iphone);
  assert.equal(res.status, 200);
  const html = await res.text();
  assert.match(html, /myapp:\/\/item\/42/);
  assert.match(res.headers.get("content-security-policy"), /script-src 'nonce-/);
  assert.equal((await visit(`/app-${run}`)).status, 302, "desktop is redirected");
  const bad = await api("POST", "/links", { url: "https://example.com/web", app: { ios: "javascript:alert(1)" } });
  assert.equal(bad.status, 400);
});

test("broken-link check and filter", async () => {
  const { data } = await api("POST", "/links", { url: "http://127.0.0.1:9/dead", slug: `dead-${run}` });
  const chk = await api("POST", `/links/${data.id}/check`);
  assert.equal(chk.status, 200);
  assert.equal(chk.data.broken, true);
  const list = await api("GET", `/links?status=broken&q=dead-${run}`);
  assert.equal(list.data.items.length, 1);
  assert.equal((await api("GET", "/stats?range=24h")).data.broken >= 1, true);
  // A new destination clears the old result.
  const upd = await api("PATCH", `/links/${data.id}`, { url: "https://example.com/alive" });
  assert.equal(upd.data.health, null);
});

test("webhooks", async () => {
  const bad = await api("POST", "/webhooks", { url: "https://example.com/hook", events: ["nope"] });
  assert.equal(bad.status, 400);
  const { status, data } = await api("POST", "/webhooks", { url: "http://127.0.0.1:9/hook", events: ["link.created"] });
  assert.equal(status, 201);
  assert.equal(data.secret.length, 48);
  const test = await api("POST", `/webhooks/${data.id}/test`);
  assert.equal(test.data.status, 0, "unreachable hook reports a network error");
  await api("POST", "/links", { url: "https://example.com/hooked" });
  await eventually(async () => (await api("GET", "/webhooks")).data.find((h) => h.id === data.id)?.last_at != null, "delivery recorded");
  assert.equal((await api("DELETE", `/webhooks/${data.id}`)).status, 200);
});

test("settings validation, status, backups and docs", async () => {
  const st = (await api("GET", "/settings")).data;
  assert.equal((await api("PUT", "/settings", { ...st, click_retention_days: 4000 })).status, 400);
  assert.equal((await api("PUT", "/settings", { ...st, click_retention_days: 0 })).status, 200);
  const status = (await api("GET", "/status")).data;
  assert.equal(typeof status.analytics_engine, "boolean");
  if (status.backups) {
    const b = await api("POST", "/backups");
    assert.equal(b.status, 201, JSON.stringify(b.data));
    const list = await api("GET", "/backups");
    assert.ok(list.data.some((x) => x.key === b.data.key));
    const dl = await fetch(`${BASE}/api/backups/download?key=${encodeURIComponent(b.data.key)}`);
    const json = await dl.json();
    assert.equal(json.format, "xorio-301-backup");
    assert.ok(json.links.length > 0);
  }
  const spec = await fetch(`${BASE}/_/openapi.yaml`);
  assert.equal(spec.status, 200);
  assert.match(await spec.text(), /^openapi: 3/);
});

test("cron jobs run (needs wrangler dev --test-scheduled)", async () => {
  for (const cron of ["17 3 * * *", "*/10 * * * *"]) {
    const res = await fetch(`${BASE}/__scheduled?cron=${encodeURIComponent(cron)}`);
    assert.match(await res.text(), /Ran scheduled event/);
  }
  const status = (await api("GET", "/status")).data;
  if (status.backups) assert.ok(status.last_backup && Date.now() - status.last_backup.at < 60_000, "nightly job wrote a backup");
  // The 10-minute job checks destinations nobody checked yet.
  await eventually(async () => (await api("GET", `/links?q=pv-${run}`)).data.items[0]?.health != null, "health checked by cron");
});

let failed = 0;
for (const [name, fn] of tests) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}\n     ${e.stack || e}`);
  }
}
console.log(`\n${tests.length - failed}/${tests.length} passed`);
process.exit(failed ? 1 : 0);
