// 301.xorio.rs web app. Vanilla JS, no build step. CSP: script-src 'self' (no inline handlers).
// Signed out: anonymous shortener. Signed in: your links. Admins also get import and moderation.
"use strict";

const ORIGIN = location.origin;
const $app = document.getElementById("app");
let ME = null; // { email, name, admin, user_id } once signed in
let CFG = { google: false, anon: { enabled: false, ttl_days: 30 } };
const isAdmin = () => !!(ME && ME.admin);

// ---------- tiny DOM helper ----------
function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "value") el.value = v;
    else if (k === "checked") el.checked = !!v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}
const svgNS = "http://www.w3.org/2000/svg";
function s(tag, attrs, ...kids) {
  const el = document.createElementNS(svgNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
  for (const kid of kids.flat()) if (kid != null) el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  return el;
}

function toast(msg, err) {
  const t = document.getElementById("toast");
  t.textContent = msg;
  t.className = "show" + (err ? " err" : "");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (t.className = ""), err ? 5000 : 2200);
}

async function api(method, path, body) {
  const res = await fetch("/api" + path, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
    credentials: "same-origin",
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (!res.ok) {
    const err = new Error((data && data.error) || `HTTP ${res.status}`);
    err.status = res.status;
    throw err;
  }
  return data;
}

const fmt = {
  n: (x) => (x ?? 0).toLocaleString(),
  date: (ms) => (ms ? new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "—"),
  dt: (ms) => (ms ? new Date(ms).toLocaleString() : "—"),
  ago(ms) {
    if (!ms) return "never";
    const s = (Date.now() - ms) / 1000;
    if (s < 60) return "just now";
    if (s < 3600) return Math.floor(s / 60) + " min ago";
    if (s < 86400) return Math.floor(s / 3600) + " h ago";
    return Math.floor(s / 86400) + " d ago";
  },
};
const shortUrl = (slug) => `${ORIGIN}/${slug}`;
const field = (lbl, el, hint) => h("div", { class: "field" }, h("label", {}, lbl), el, hint && h("div", { class: "hint" }, hint));
async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast("Copied " + text); }
  catch { toast("Copy failed", true); }
}
const toLocalInput = (ms) => {
  if (!ms) return "";
  const d = new Date(ms);
  d.setMinutes(d.getMinutes() - d.getTimezoneOffset());
  return d.toISOString().slice(0, 16);
};
const fromLocalInput = (v) => (v ? new Date(v).getTime() : null);

// ---------- router ----------
// [pattern, page, nav item, public?]
const routes = [
  [/^#\/m\/([A-Za-z0-9_-]+)$/, (m) => managePage(m[1]), "", true],
  [/^#\/report\/(.+)$/, (m) => reportPage(decodeURIComponent(m[1])), "", true],
  [/^#?\/?$/, dashboard, "dash"],
  [/^#\/links\/?$/, linksPage, "links"],
  [/^#\/links\/new$/, (m) => editPage(null), "links"],
  [/^#\/links\/(\d+)$/, (m) => editPage(Number(m[1])), "links"],
  [/^#\/import$/, importPage, "import"],
  [/^#\/moderation$/, moderationPage, "moderation"],
  [/^#\/settings$/, settingsPage, "settings"],
];
async function render() {
  const hash = location.hash || "#/";
  for (let [re, fn, nav, pub] of routes) {
    const m = hash.match(re);
    if (m) {
      if (!ME && !pub) fn = publicHome;
      document.querySelectorAll("#nav a").forEach((a) => a.classList.toggle("active", a.dataset.nav === nav));
      $app.replaceChildren(h("p", { class: "muted" }, "Loading…"));
      try { await fn(m); } catch (e) { $app.replaceChildren(h("div", { class: "panel" }, h("h2", {}, "Something went wrong"), h("p", { class: "bad" }, e.message))); }
      window.scrollTo(0, 0);
      return;
    }
  }
  location.hash = "#/";
}
window.addEventListener("hashchange", render);
document.getElementById("new-link").addEventListener("click", () => (location.hash = "#/links/new"));
document.addEventListener("keydown", (e) => {
  if (ME && e.key === "n" && !e.metaKey && !e.ctrlKey && !["INPUT", "TEXTAREA", "SELECT"].includes(document.activeElement.tagName)) {
    e.preventDefault();
    location.hash = "#/links/new";
  }
});
// Paste a URL anywhere (outside inputs) to start a new link with it.
document.addEventListener("paste", (e) => {
  if (!ME || ["INPUT", "TEXTAREA"].includes(document.activeElement.tagName)) return;
  const text = (e.clipboardData.getData("text") || "").trim();
  if (/^https?:\/\/\S+$/i.test(text)) {
    sessionStorage.setItem("prefill-url", text);
    location.hash = "#/links/new";
    if (location.hash === "#/links/new") render();
  }
});

// ---------- charts ----------
function lineChart(series, bucketMs, since) {
  const W = 720, H = 200, P = { l: 36, r: 12, t: 12, b: 26 };
  const now = Date.now();
  const start = since > 0 ? Math.floor(since / bucketMs) * bucketMs : (series[0]?.t ?? now - 30 * 864e5);
  const byT = new Map(series.map((p) => [p.t, p.n]));
  const pts = [];
  for (let t = start; t <= now; t += bucketMs) pts.push({ t, n: byT.get(t) || 0 });
  if (pts.length > 400) pts.splice(0, pts.length - 400);
  const max = Math.max(1, ...pts.map((p) => p.n));
  const x = (i) => P.l + (i / Math.max(1, pts.length - 1)) * (W - P.l - P.r);
  const y = (v) => H - P.b - (v / max) * (H - P.t - P.b);
  const line = pts.map((p, i) => `${i ? "L" : "M"}${x(i).toFixed(1)} ${y(p.n).toFixed(1)}`).join("");
  const area = `${line}L${x(pts.length - 1)} ${H - P.b}L${x(0)} ${H - P.b}Z`;
  const ticks = [0, Math.round(max / 2), max];
  const label = (t) => new Date(t).toLocaleDateString(undefined, bucketMs < 864e5 ? { hour: "2-digit" } : { month: "short", day: "numeric" });
  const xt = [0, Math.floor((pts.length - 1) / 2), pts.length - 1].filter((v, i, a) => a.indexOf(v) === i);
  return h("div", { class: "chart" },
    s("svg", { viewBox: `0 0 ${W} ${H}`, role: "img", "aria-label": "Clicks over time" },
      ticks.map((v) => [
        s("line", { x1: P.l, x2: W - P.r, y1: y(v), y2: y(v), stroke: "var(--line)" }),
        s("text", { x: P.l - 6, y: y(v) + 4, "text-anchor": "end", "font-size": 11, fill: "var(--muted)" }, v),
      ]),
      s("path", { d: area, fill: "var(--accent)", "fill-opacity": 0.12 }),
      s("path", { d: line, fill: "none", stroke: "var(--accent)", "stroke-width": 2, "stroke-linejoin": "round" }),
      xt.map((i) => s("text", { x: x(i), y: H - 6, "text-anchor": i === 0 ? "start" : i === pts.length - 1 ? "end" : "middle", "font-size": 11, fill: "var(--muted)" }, label(pts[i].t))),
    ));
}
function bars(title, rows, empty = "No data yet") {
  const total = rows.reduce((a, r) => a + r.n, 0) || 1;
  return h("div", { class: "panel bars" }, h("h2", {}, title),
    rows.length ? rows.map((r) => h("div", { class: "b" },
      h("span", { class: "k", title: r.k || "(none)" }, r.k || h("span", { class: "muted" }, "(direct / unknown)")),
      h("div", { class: "track" }, h("div", { class: "fill", style: `width:${((r.n / total) * 100).toFixed(1)}%` })),
      h("span", { class: "num" }, fmt.n(r.n)))) : h("p", { class: "muted small" }, empty));
}
function rangePicker(current, onPick) {
  return h("div", { class: "seg" }, ["24h", "7d", "30d", "90d", "all"].map((r) =>
    h("button", { class: r === current ? "on" : "", onclick: () => onPick(r) }, r)));
}
function statsBlock(st) {
  const t = st.top || {};
  return [
    h("div", { class: "panel" }, lineChart(st.series || [], st.bucket_ms, st.since)),
    h("div", { class: "grid g3" },
      bars("Countries", t.country || []), bars("Referrers", t.referrer || []), bars("Devices", t.device || []),
      bars("Operating systems", t.os || []), bars("Browsers", t.browser || []), bars("Cities", t.city || [])),
  ];
}

// ---------- dashboard ----------
async function dashboard() {
  const range = sessionStorage.getItem("range") || "30d";
  const st = await api("GET", `/stats?range=${range}`);
  const pick = (r) => { sessionStorage.setItem("range", r); render(); };
  const [chart, dims] = statsBlock(st);
  $app.replaceChildren(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, isAdmin() ? "Dashboard" : "Your links"), h("div", { class: "muted" }, "Human clicks; bots and link previews are counted separately.")), rangePicker(range, pick)),
    h("div", { class: "grid g4", style: "margin-bottom:16px" },
      stat("Clicks", st.clicks), stat("Links", st.links), stat("QR scans", st.qr), stat("Bot hits", st.bots)),
    h("div", { class: "grid", style: "gap:16px" },
      chart,
      h("div", { class: "panel" }, h("h2", {}, "Top links"),
        (st.top_links || []).length ? h("table", {}, h("tbody", {}, st.top_links.map((l) =>
          h("tr", { class: "link-row", onclick: () => (location.hash = `#/links/${l.id}`) },
            h("td", {}, h("div", { class: "slug" }, "/" + l.slug), h("div", { class: "dest" }, l.title || l.url)),
            h("td", { class: "num" }, fmt.n(l.n)))))) : h("p", { class: "muted" }, "No clicks in this period.")),
      dims),
  );
}
const stat = (l, v) => h("div", { class: "panel stat" }, h("div", { class: "v" }, fmt.n(v)), h("div", { class: "l" }, l));

// ---------- links list ----------
const listState = { q: "", tag: "", sort: "created", order: "desc", status: "", owner: "", offset: 0, limit: 50, selected: new Set() };
async function linksPage() {
  const qs = new URLSearchParams({ q: listState.q, tag: listState.tag, sort: listState.sort, order: listState.order, status: listState.status, owner: listState.owner, offset: listState.offset, limit: listState.limit });
  const [page, tags] = await Promise.all([api("GET", `/links?${qs}`), api("GET", "/tags")]);
  const search = h("input", { type: "search", placeholder: "Search slug, URL, title, notes", value: listState.q });
  let timer;
  search.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => { listState.q = search.value; listState.offset = 0; linksPage().then(() => { const el = $app.querySelector("input[type=search]"); el.focus(); el.setSelectionRange(el.value.length, el.value.length); }); }, 250); });
  const setFilter = (k) => (e) => { listState[k] = e.target.value; listState.offset = 0; linksPage(); };
  const tagSel = h("select", { onchange: setFilter("tag") },
    h("option", { value: "" }, "All tags"), tags.map((t) => h("option", { value: t.tag, selected: t.tag === listState.tag }, `${t.tag} (${t.n})`)));
  const statusSel = h("select", { onchange: setFilter("status") },
    [["", "Any status"], ["active", "Active"], ["disabled", "Disabled"], ["expired", "Expired"], ["protected", "Password"], ["blocked", "Blocked"], ...(isAdmin() ? [["reported", "Reported"]] : [])].map(([v, l]) => h("option", { value: v, selected: v === listState.status }, l)));
  const ownerOpts = [["", "Everyone's links"], ["me", "My links"], ["anon", "Anonymous"]];
  if (listState.owner && !ownerOpts.some(([v]) => v === listState.owner)) ownerOpts.push([listState.owner, `User #${listState.owner}`]);
  const ownerSel = isAdmin() && h("select", { onchange: setFilter("owner") }, ownerOpts.map(([v, l]) => h("option", { value: v, selected: v === listState.owner }, l)));
  const sortSel = h("select", { onchange: (e) => { const [s1, o] = e.target.value.split(":"); listState.sort = s1; listState.order = o; linksPage(); } },
    [["created:desc", "Newest"], ["created:asc", "Oldest"], ["clicks:desc", "Most clicks"], ["last_click:desc", "Recently clicked"], ["slug:asc", "Slug A→Z"]].map(([v, l]) =>
      h("option", { value: v, selected: v === `${listState.sort}:${listState.order}` }, l)));

  const sel = listState.selected;
  const selCount = h("span", { class: "muted small" });
  const bulkBar = h("div", { class: "row" }, selCount,
    h("button", { class: "btn sm", onclick: () => bulk("tag") }, "Add tag"),
    h("button", { class: "btn sm", onclick: () => bulk("untag") }, "Remove tag"),
    h("button", { class: "btn sm", onclick: () => bulk("disable") }, "Disable"),
    h("button", { class: "btn sm", onclick: () => bulk("enable") }, "Enable"),
    isAdmin() && h("button", { class: "btn sm", onclick: () => bulk("block"), title: "Owners cannot re-enable a blocked link" }, "Block"),
    isAdmin() && h("button", { class: "btn sm", onclick: () => bulk("unblock") }, "Unblock"),
    h("button", { class: "btn sm danger", onclick: () => bulk("delete") }, "Delete"));
  const syncBar = () => { bulkBar.style.display = sel.size ? "" : "none"; selCount.textContent = `${sel.size} selected`; };
  syncBar();
  async function bulk(action) {
    let tags = [];
    if (action === "tag" || action === "untag") {
      const t = prompt(action === "tag" ? "Tags to add (comma separated)" : "Tags to remove (comma separated)");
      if (!t) return;
      tags = t.split(",").map((x) => x.trim()).filter(Boolean);
    }
    if (action === "delete" && !confirm(`Delete ${sel.size} links and their analytics? This cannot be undone.`)) return;
    const r = await api("POST", "/links/bulk", { ids: [...sel], action, tags });
    toast(`${r.done} updated${r.errors.length ? `, ${r.errors.length} failed` : ""}`, r.errors.length > 0);
    sel.clear();
    linksPage();
  }

  const rows = page.items.map((l) => {
    const cb = h("input", { type: "checkbox", checked: sel.has(l.id), onclick: (e) => e.stopPropagation(), onchange: (e) => { e.target.checked ? sel.add(l.id) : sel.delete(l.id); syncBar(); } });
    return h("tr", { class: "link-row", onclick: () => (location.hash = `#/links/${l.id}`) },
      h("td", {}, cb),
      h("td", {},
        h("div", {}, h("span", { class: "slug" }, "/" + l.slug),
          l.status === "disabled" && h("span", { class: "badge off" }, "OFF"),
          l.blocked && h("span", { class: "badge blocked" }, "BLOCKED"),
          l.anonymous && h("span", { class: "badge anon" }, "ANON"),
          l.has_password && h("span", { class: "badge lock" }, "PW"),
          l.status === "expired" && h("span", { class: "badge exp" }, "EXPIRED")),
        h("div", { class: "dest" }, l.title ? `${l.title} · ${l.url}` : l.url),
        isAdmin() && l.owner_id !== ME.user_id && h("div", { class: "small muted" }, l.owner_email ? `by ${l.owner_email}` : l.anonymous ? "anonymous" : ""),
        h("div", {}, l.tags.map((t) => h("span", { class: "tag" }, t)))),
      h("td", { class: "num" }, fmt.n(l.clicks), h("div", { class: "muted small" }, fmt.ago(l.last_click_at))),
      h("td", { class: "small muted" }, fmt.date(l.created_at)),
      h("td", {}, h("button", { class: "btn sm", onclick: (e) => { e.stopPropagation(); copy(shortUrl(l.slug)); } }, "Copy")));
  });
  const allBox = h("input", { type: "checkbox", checked: page.items.length > 0 && page.items.every((l) => sel.has(l.id)),
    onchange: (e) => {
      page.items.forEach((l) => (e.target.checked ? sel.add(l.id) : sel.delete(l.id)));
      $app.querySelectorAll("tbody input[type=checkbox]").forEach((c) => (c.checked = e.target.checked));
      syncBar();
    } });

  $app.replaceChildren(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Links"), h("div", { class: "muted" }, `${fmt.n(page.total)} links · press `, h("span", { class: "kbd" }, "n"), " for a new one, or paste a URL anywhere")), bulkBar),
    h("div", { class: "panel" },
      h("div", { class: "grid", style: `grid-template-columns: 2fr ${ownerSel ? "1fr " : ""}1fr 1fr 1fr; gap: 8px; margin-bottom: 8px` }, search, ownerSel, tagSel, statusSel, sortSel),
      page.items.length ? h("div", { class: "table-wrap" }, h("table", {},
        h("thead", {}, h("tr", {}, h("th", {}, allBox), h("th", {}, "Link"), h("th", { class: "num" }, "Clicks"), h("th", {}, "Created"), h("th", {}))),
        h("tbody", {}, rows))) : h("div", { class: "empty" }, listState.q || listState.tag || listState.status || listState.owner ? "No links match." : isAdmin() ? "No links yet. Create one or import from another service." : "No links yet. Create your first one."),
      h("div", { class: "pager" },
        h("button", { class: "btn sm", disabled: listState.offset === 0, onclick: () => { listState.offset = Math.max(0, listState.offset - listState.limit); linksPage(); } }, "← Prev"),
        h("span", { class: "muted small" }, page.total ? `${listState.offset + 1}–${Math.min(page.total, listState.offset + listState.limit)} of ${fmt.n(page.total)}` : ""),
        h("button", { class: "btn sm", disabled: listState.offset + listState.limit >= page.total, onclick: () => { listState.offset += listState.limit; linksPage(); } }, "Next →"))),
  );
}

// ---------- create / edit ----------
function applyUtm(url, utm) {
  try {
    const u = new URL(url);
    const set = ["source", "medium", "campaign", "term", "content"].map((k) => ["utm_" + k, (utm[k] || "").trim()]).filter(([, v]) => v);
    set.forEach(([k]) => u.searchParams.delete(k));
    set.forEach(([k, v]) => u.searchParams.append(k, v));
    return u.toString();
  } catch { return url; }
}

async function editPage(id) {
  const [link, settings] = await Promise.all([id ? api("GET", `/links/${id}`) : null, api("GET", "/settings")]);
  const isNew = !link;
  const L = link || { url: sessionStorage.getItem("prefill-url") || "", slug: "", tags: [], redirect_type: settings.default_redirect, passthrough: true, utm: {}, geo: {}, device: {}, aliases: [] };
  sessionStorage.removeItem("prefill-url");

  const f = {};
  const inp = (key, attrs) => (f[key] = h("input", attrs));

  inp("url", { type: "url", required: true, placeholder: "https://example.com/very/long/page", value: L.url });
  inp("slug", { type: "text", placeholder: isNew ? "leave empty to generate" : "", value: L.slug, autocomplete: "off", spellcheck: "false" });
  const styleSel = h("select", { style: "width:auto" }, [["random", "Random chars"], ["words", "Words"], ["names", "Names"]].map(([v, l]) => h("option", { value: v, selected: v === settings.default_slug_style }, l)));
  const slugStatus = h("div", { class: "hint" }, "Letters, digits, - _ . / ; case-sensitive.");
  const genBtn = h("button", { class: "btn", type: "button", onclick: async () => {
    const r = await api("GET", `/slug?style=${styleSel.value}`);
    f.slug.value = r.slug; checkSlug();
  } }, "Generate");
  let slugTimer;
  async function checkSlug() {
    const v = f.slug.value.trim();
    if (!v || v === L.slug) { slugStatus.textContent = isNew ? "Empty: one is generated in the default style." : "Current slug."; slugStatus.className = "hint"; return; }
    const r = await api("GET", `/slug/check?slug=${encodeURIComponent(v)}`);
    if (!r.valid) { slugStatus.textContent = r.reason; slugStatus.className = "hint bad"; }
    else if (r.taken_by && r.taken_by !== L.id) { slugStatus.textContent = "Already taken."; slugStatus.className = "hint bad"; }
    else { slugStatus.textContent = `Available: ${shortUrl(v)}`; slugStatus.className = "hint ok"; }
  }
  f.slug.addEventListener("input", () => { clearTimeout(slugTimer); slugTimer = setTimeout(checkSlug, 300); });

  inp("title", { type: "text", value: L.title || "", placeholder: "Optional, for your own reference" });
  inp("tags", { type: "text", value: (L.tags || []).join(", "), placeholder: "blog, launch" });
  f.notes = h("textarea", { placeholder: "Private notes" }, L.notes || "");
  f.type = h("select", {}, [[302, "302 Found (temporary, default)"], [301, "301 Moved Permanently"], [307, "307 Temporary, keep method"], [308, "308 Permanent, keep method"]].map(([v, l]) => h("option", { value: v, selected: v === L.redirect_type }, l)));
  inp("passthrough", { type: "checkbox", checked: L.passthrough });
  inp("disabled", { type: "checkbox", checked: L.disabled });
  inp("expires", { type: "datetime-local", value: toLocalInput(L.expires_at) });
  inp("max", { type: "number", min: 1, value: L.max_clicks || "", placeholder: "unlimited" });
  inp("fallback", { type: "url", value: L.fallback_url || "", placeholder: "Where to send visitors after expiry (else 410 page)" });
  inp("password", { type: "password", autocomplete: "new-password", placeholder: L.has_password ? "•••••• (unchanged)" : "No password" });
  inp("removePw", { type: "checkbox" });
  const utm = {};
  for (const k of ["source", "medium", "campaign", "term", "content"]) utm[k] = h("input", { type: "text", value: (L.utm || {})[k] || "", placeholder: { source: "newsletter, x, linkedin", medium: "email, social, cpc", campaign: "spring-launch", term: "keyword", content: "variant-a" }[k] });
  const preview = h("div", { class: "preview" });
  const updPreview = () => { preview.textContent = applyUtm(f.url.value || "https://…", Object.fromEntries(Object.entries(utm).map(([k, e]) => [k, e.value]))); };
  [f.url, ...Object.values(utm)].forEach((e) => e.addEventListener("input", updPreview));
  updPreview();

  const geoBox = h("div");
  const addGeo = (cc = "", url = "") => {
    const row = h("div", { class: "rule" },
      h("input", { type: "text", maxlength: 2, placeholder: "RO", value: cc, class: "geo-cc", style: "text-transform:uppercase" }),
      h("input", { type: "url", placeholder: "https://example.com/ro", value: url, class: "geo-url" }),
      h("button", { class: "btn sm", type: "button", onclick: () => row.remove() }, "✕"));
    geoBox.append(row);
  };
  Object.entries(L.geo || {}).forEach(([c, u]) => addGeo(c, u));
  const dev = {};
  for (const k of ["ios", "android", "desktop"]) dev[k] = h("input", { type: "url", value: (L.device || {})[k] || "", placeholder: { ios: "https://apps.apple.com/app/…", android: "https://play.google.com/store/apps/details?id=…", desktop: "https://example.com/desktop" }[k] });

  const hasAdvanced = (o) => Object.values(o || {}).some(Boolean);
  const notice = L.blocked
    ? h("div", { class: "notice" }, isAdmin() ? "Blocked by an admin: visitors get a 410 page and the owner cannot edit it. Unblock it from the Links list." : "An admin blocked this link because it broke the rules of this service. It cannot be edited.")
    : null;
  const form = h("form", { class: "panel", onsubmit: save },
    notice,
    field("Destination URL", f.url),
    h("div", { class: "field" }, h("label", {}, "Short link"),
      h("div", { class: "row slug-row" }, h("span", { class: "muted mono small", style: "white-space:nowrap" }, ORIGIN.replace(/^https?:\/\//, "") + "/"), f.slug, styleSel, genBtn), slugStatus),
    h("div", { class: "grid g2" }, field("Title", f.title), field("Tags", f.tags, "Comma separated")),
    field("Notes", f.notes),
    h("details", { class: "section", open: hasAdvanced(L.utm) || undefined }, h("summary", {}, "UTM builder"),
      h("div", { class: "grid g3" }, Object.entries(utm).map(([k, e]) => field("utm_" + k, e))),
      h("label", {}, "Final destination"), preview),
    h("details", { class: "section", open: !!(L.expires_at || L.max_clicks || L.has_password || L.disabled) || undefined }, h("summary", {}, "Expiry, password and status"),
      h("div", { class: "grid g2" }, field("Expires at", f.expires, "Local time"), field("Max clicks", f.max, "Counts human clicks only")),
      field("After expiry, redirect to", f.fallback),
      h("div", { class: "grid g2" }, field("Password", f.password, "Visitors must enter it before redirecting"),
        h("div", { class: "field" }, h("label", {}, " "), L.has_password && h("label", { class: "check" }, f.removePw, "Remove password"))),
      h("label", { class: "check" }, f.disabled, "Disabled (visitors get a 410 page)")),
    h("details", { class: "section", open: hasAdvanced(L.geo) || hasAdvanced(L.device) || undefined }, h("summary", {}, "Targeting: country and device"),
      h("label", {}, "Country rules"), h("div", { class: "hint", style: "margin:-2px 0 8px" }, "ISO country code from Cloudflare (e.g. US, RO, DE). Device rules win over country rules."),
      geoBox, h("button", { class: "btn sm", type: "button", onclick: () => addGeo() }, "+ Add country"),
      h("div", { class: "grid g3", style: "margin-top:14px" }, field("iOS", dev.ios), field("Android", dev.android), field("Desktop", dev.desktop))),
    h("details", { class: "section" }, h("summary", {}, "Redirect behaviour"),
      field("Redirect type", f.type),
      h("label", { class: "check" }, f.passthrough, "Pass the visitor's query string through to the destination")),
    h("div", { class: "row", style: "margin-top:18px; justify-content:space-between" },
      h("div", { class: "row" }, h("button", { class: "btn primary", type: "submit" }, isNew ? "Create link" : "Save changes"), h("a", { class: "btn", href: "#/links" }, "Cancel")),
      !isNew && h("button", { class: "btn danger", type: "button", onclick: del }, "Delete")),
  );

  async function save(e) {
    e.preventDefault();
    const geo = {};
    geoBox.querySelectorAll(".rule").forEach((r) => {
      const cc = r.querySelector(".geo-cc").value.trim().toUpperCase(), u = r.querySelector(".geo-url").value.trim();
      if (cc && u) geo[cc] = u;
    });
    const body = {
      url: f.url.value.trim(),
      title: f.title.value.trim() || null,
      notes: f.notes.value.trim() || null,
      tags: f.tags.value.split(",").map((t) => t.trim()).filter(Boolean),
      redirect_type: Number(f.type.value),
      passthrough: f.passthrough.checked,
      disabled: f.disabled.checked,
      expires_at: fromLocalInput(f.expires.value),
      max_clicks: f.max.value ? Number(f.max.value) : null,
      fallback_url: f.fallback.value.trim() || null,
      utm: Object.fromEntries(Object.entries(utm).map(([k, e]) => [k, e.value.trim() || null])),
      geo,
      device: Object.fromEntries(Object.entries(dev).map(([k, e]) => [k, e.value.trim() || null])),
    };
    const slugV = f.slug.value.trim();
    if (slugV) body.slug = slugV; else if (isNew) body.slug_style = styleSel.value;
    if (f.password.value) body.password = f.password.value;
    else if (f.removePw.checked) body.password = null;
    try {
      const saved = isNew ? await api("POST", "/links", body) : await api("PATCH", `/links/${L.id}`, body);
      toast(isNew ? "Created " + shortUrl(saved.slug) : "Saved");
      if (isNew) { copy(shortUrl(saved.slug)); location.hash = `#/links/${saved.id}`; } else editPage(saved.id);
    } catch (err) { toast(err.message, true); }
  }
  async function del() {
    if (!confirm(`Delete /${L.slug} and all of its analytics? This cannot be undone.`)) return;
    try { await api("DELETE", `/links/${L.id}`); toast("Deleted"); location.hash = "#/links"; }
    catch (err) { toast(err.message, true); }
  }

  if (isNew) {
    $app.replaceChildren(h("div", { class: "page-head" }, h("h1", {}, "New link")), form);
    f.url.focus();
    return;
  }

  // Existing link: header, QR, aliases, stats.
  const qr = qrPanel(L);
  const aliasPanel = h("div", { class: "panel" }, h("h2", {}, "Aliases"),
    L.aliases.length ? L.aliases.map((a) => h("div", { class: "row", style: "justify-content:space-between; margin-bottom:6px" },
      h("span", { class: "slug small" }, "/" + a),
      h("button", { class: "btn sm", onclick: async () => { await api("DELETE", `/links/${L.id}/aliases/${a.split("/").map(encodeURIComponent).join("/")}`); toast("Alias removed"); editPage(L.id); } }, "Remove")))
      : h("p", { class: "muted small" }, "Renaming the slug keeps the old one working as an alias."));
  const statsWrap = h("div", { class: "grid", style: "gap:16px; margin-top:16px" });
  const range = sessionStorage.getItem("range") || "30d";
  async function loadStats(r) {
    sessionStorage.setItem("range", r);
    const st = await api("GET", `/links/${L.id}/stats?range=${r}`);
    statsWrap.replaceChildren(
      h("div", { class: "page-head", style: "margin:8px 0 0" }, h("h2", { style: "margin:0" }, "Analytics"), rangePicker(r, loadStats)),
      h("div", { class: "grid g4" }, stat("Clicks", st.clicks), stat("QR scans", st.qr), stat("Bot hits", st.bots), stat("All-time clicks", L.clicks)),
      ...statsBlock(st));
  }
  $app.replaceChildren(
    h("div", { class: "page-head" },
      h("div", {}, h("div", { class: "shorturl" }, shortUrl(L.slug)), h("div", { class: "muted small" }, `Created ${fmt.dt(L.created_at)} · source: ${L.source} · last click ${fmt.ago(L.last_click_at)}`,
        isAdmin() && L.owner_id !== ME.user_id ? ` · owner: ${L.owner_email || (L.anonymous ? "anonymous" : "none")}` : "")),
      h("div", { class: "row" }, h("button", { class: "btn primary", onclick: () => copy(shortUrl(L.slug)) }, "Copy"), h("a", { class: "btn", href: shortUrl(L.slug) + "+", target: "_blank", rel: "noopener" }, "Preview"))),
    h("div", { class: "grid side" }, form, h("div", { class: "grid" }, qr, aliasPanel)),
    statsWrap);
  loadStats(range);
}

function qrPanel(L) {
  const fg = h("input", { type: "color", value: "#000000" }), bg = h("input", { type: "color", value: "#ffffff" });
  const ecc = h("select", {}, [["L", "Low (7%)"], ["M", "Medium (15%)"], ["Q", "Quartile (25%)"], ["H", "High (30%)"]].map(([v, l]) => h("option", { value: v, selected: v === "M" }, l)));
  const track = h("input", { type: "checkbox", checked: true });
  const img = h("img", { alt: `QR code for ${shortUrl(L.slug)}` });
  const src = (size = 512) => `/api/links/${L.id}/qr.svg?fg=${fg.value.slice(1)}&bg=${bg.value.slice(1)}&ecc=${ecc.value}&size=${size}${track.checked ? "" : "&track=0"}`;
  const refresh = () => (img.src = src());
  [fg, bg, ecc, track].forEach((e) => e.addEventListener("change", refresh));
  refresh();
  async function download(kind) {
    const svg = await (await fetch(src(1024), { credentials: "same-origin" })).text();
    const name = `qr-${L.slug.replace(/\//g, "_")}`;
    if (kind === "svg") return save(new Blob([svg], { type: "image/svg+xml" }), name + ".svg");
    const url = URL.createObjectURL(new Blob([svg], { type: "image/svg+xml" }));
    const im = new Image();
    im.onload = () => {
      const c = document.createElement("canvas");
      c.width = c.height = 1024;
      c.getContext("2d").drawImage(im, 0, 0, 1024, 1024);
      URL.revokeObjectURL(url);
      c.toBlob((b) => save(b, name + ".png"), "image/png");
    };
    im.src = url;
  }
  function save(blob, name) {
    const a = h("a", { href: URL.createObjectURL(blob), download: name });
    document.body.append(a); a.click(); a.remove();
  }
  return h("div", { class: "panel" }, h("h2", {}, "QR code"), h("div", { class: "qr-box" }, img),
    h("div", { class: "grid g2", style: "margin-top:12px; gap:8px" }, h("div", {}, h("label", {}, "Foreground"), fg), h("div", {}, h("label", {}, "Background"), bg)),
    h("div", { class: "field", style: "margin-top:8px" }, h("label", {}, "Error correction"), ecc),
    h("label", { class: "check" }, track, "Count scans separately (adds ?qr=1)"),
    h("div", { class: "row", style: "margin-top:10px" }, h("button", { class: "btn sm", onclick: () => download("svg") }, "Download SVG"), h("button", { class: "btn sm", onclick: () => download("png") }, "Download PNG")));
}

// ---------- import ----------
const SOURCES = {
  bitly: { name: "Bitly", token: "Access token (Settings → API)", help: "Generate a token at app.bitly.com → Settings → Developer settings → API." },
  rebrandly: { name: "Rebrandly", token: "API key", domain: "Domain (optional, e.g. rebrand.ly)", help: "Create an API key at app.rebrandly.com → Account → API keys. Tags are not included in Rebrandly's link list." },
  shortio: { name: "Short.io", token: "Secret API key", domain: "Short.io domain (required, e.g. go.example.com)", help: "Create a secret key at app.short.io → Integrations & API." },
  tinyurl: { name: "TinyURL", token: "API token", help: "Create a token at tinyurl.com/app/dev. Each link needs one extra lookup, so this one is slower." },
  csv: { name: "CSV file", help: "Works with exports from Bitly, Rebrandly, Short.io, TinyURL, Dub and most tools. Columns are detected from the header row." },
};

function parseCSV(text) {
  const rows = []; let row = [], cell = "", q = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (q) {
      if (c === '"') { if (text[i + 1] === '"') { cell += '"'; i++; } else q = false; }
      else cell += c;
    } else if (c === '"') q = true;
    else if (c === ",") { row.push(cell); cell = ""; }
    else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i++;
      row.push(cell); cell = "";
      if (row.some((x) => x !== "")) rows.push(row);
      row = [];
    } else cell += c;
  }
  row.push(cell);
  if (row.some((x) => x !== "")) rows.push(row);
  return rows;
}
const COLS = {
  slug: ["slug", "alias", "back-half", "backhalf", "back_half", "slashtag", "path", "key", "short_code", "shortcode", "custom_alias"],
  short: ["link", "short_url", "shorturl", "short link", "short_link", "bitlink", "shortlink", "tiny_url", "secureshorturl", "shortURL"],
  url: ["url", "long_url", "longurl", "long url", "destination", "original_url", "originalurl", "target", "target_url", "redirect", "destination url"],
  title: ["title", "name", "description"],
  tags: ["tags", "tag", "labels"],
  notes: ["notes", "note", "comments"],
  expires_at: ["expires_at", "expiresat", "expiration", "expires", "expiry"],
};
function detectColumns(header) {
  const norm = header.map((x) => x.trim().toLowerCase());
  const map = {};
  for (const [k, names] of Object.entries(COLS)) {
    const i = norm.findIndex((n) => names.map((x) => x.toLowerCase()).includes(n));
    if (i >= 0) map[k] = i;
  }
  return map;
}
function slugFromShort(v) {
  try { const u = new URL(/^https?:\/\//.test(v) ? v : "https://" + v); return u.pathname.replace(/^\/+|\/+$/g, "") || null; } catch { return null; }
}

async function importPage() {
  let source = "bitly", rows = [];
  const srcSel = h("select", {}, Object.entries(SOURCES).map(([k, v]) => h("option", { value: k }, v.name)));
  const token = h("input", { type: "password", autocomplete: "off" });
  const domain = h("input", { type: "text" });
  const file = h("input", { type: "file", accept: ".csv,text/csv" });
  const help = h("p", { class: "muted small" });
  const tokenField = h("div", { class: "field" }, h("label", {}), token, h("div", { class: "hint" }, "Used once for this import and never stored."));
  const domainField = h("div", { class: "field" }, h("label", {}), domain);
  const fileField = h("div", { class: "field" }, h("label", {}, "CSV file"), file);
  const mapBox = h("div");
  const progress = h("div", { class: "progress" }, h("div"));
  const status = h("p", { class: "muted small" });
  const review = h("div");
  const fetchBtn = h("button", { class: "btn primary", onclick: run }, "Fetch links");

  function sync() {
    source = srcSel.value;
    const S = SOURCES[source];
    help.textContent = S.help;
    tokenField.style.display = S.token ? "" : "none";
    tokenField.querySelector("label").textContent = S.token || "";
    domainField.style.display = S.domain ? "" : "none";
    domainField.querySelector("label").textContent = S.domain || "";
    fileField.style.display = source === "csv" ? "" : "none";
    fetchBtn.style.display = source === "csv" ? "none" : "";
  }
  srcSel.addEventListener("change", sync);

  file.addEventListener("change", async () => {
    const text = await file.files[0].text();
    const table = parseCSV(text.replace(/^﻿/, ""));
    if (table.length < 2) return toast("The CSV has no data rows", true);
    const header = table[0];
    const map = detectColumns(header);
    const sels = {};
    mapBox.replaceChildren(h("h2", {}, "Columns"), h("div", { class: "grid g4" }, Object.keys(COLS).map((k) => {
      sels[k] = h("select", {}, h("option", { value: "" }, "—"), header.map((c, i) => h("option", { value: i, selected: map[k] === i }, c)));
      sels[k].addEventListener("change", build);
      return h("div", { class: "field" }, h("label", {}, { slug: "Slug", short: "Short URL (slug taken from path)", url: "Destination URL", title: "Title", tags: "Tags", notes: "Notes", expires_at: "Expires at" }[k]), sels[k]);
    })));
    function build() {
      const col = (k, r) => (sels[k].value === "" ? "" : (r[Number(sels[k].value)] || "").trim());
      rows = table.slice(1).map((r) => {
        const exp = col("expires_at", r);
        return {
          slug: col("slug", r) || slugFromShort(col("short", r)) || null,
          url: col("url", r),
          title: col("title", r) || null,
          notes: col("notes", r) || null,
          tags: col("tags", r).split(/[;,|]/).map((t) => t.trim()).filter(Boolean),
          expires_at: exp ? (isNaN(Date.parse(exp)) ? null : Date.parse(exp)) : null,
          source: "csv",
        };
      }).filter((r) => r.url);
      showReview();
    }
    build();
  });

  async function run() {
    rows = []; review.replaceChildren(); fetchBtn.disabled = true;
    let cursor = null, pages = 0;
    try {
      do {
        status.textContent = `Fetching page ${pages + 1}… ${rows.length} links so far`;
        progress.firstChild.style.width = Math.min(95, 10 + pages * 8) + "%";
        const r = await api("POST", "/import/fetch", { source, token: token.value, domain: domain.value || null, cursor });
        rows.push(...r.rows);
        cursor = r.next_cursor;
        pages++;
      } while (cursor && pages < 500);
      progress.firstChild.style.width = "100%";
      status.textContent = `Fetched ${rows.length} links from ${SOURCES[source].name}.`;
      showReview();
    } catch (e) { status.textContent = ""; toast(e.message, true); }
    fetchBtn.disabled = false;
  }

  function showReview() {
    const conflict = h("select", { style: "width:auto" }, [["skip", "Skip existing slugs"], ["rename", "Rename to slug-2"], ["overwrite", "Overwrite existing"]].map(([v, l]) => h("option", { value: v }, l)));
    const extraTag = h("input", { type: "text", value: `import-${source}`, style: "width:auto" });
    const keep = rows.map(() => true);
    const body = h("tbody", {}, rows.slice(0, 500).map((r, i) => h("tr", {},
      h("td", {}, h("input", { type: "checkbox", checked: true, onchange: (e) => (keep[i] = e.target.checked) })),
      h("td", { class: "slug small" }, r.slug ? "/" + r.slug : h("span", { class: "muted" }, "(generate)")),
      h("td", { class: "dest" }, r.url || h("span", { class: "bad" }, "missing URL"), r.warning && h("div", { class: "warn small" }, r.warning)),
      h("td", { class: "small" }, r.title || ""),
      h("td", {}, (r.tags || []).map((t) => h("span", { class: "tag" }, t))))));
    const go = h("button", { class: "btn primary", onclick: commit }, `Import ${rows.length} links`);
    review.replaceChildren(h("div", { class: "panel", style: "margin-top:16px" },
      h("div", { class: "page-head" }, h("h2", { style: "margin:0" }, `Review (${rows.length})`),
        h("div", { class: "row" }, h("span", { class: "small muted" }, "Also tag with"), extraTag, conflict, go)),
      rows.length > 500 && h("p", { class: "muted small" }, `Showing the first 500 of ${rows.length}; all will be imported.`),
      h("div", { class: "table-wrap" }, h("table", {}, h("thead", {}, h("tr", {}, h("th", {}), h("th", {}, "Slug"), h("th", {}, "Destination"), h("th", {}, "Title"), h("th", {}, "Tags"))), body))));

    async function commit() {
      go.disabled = true;
      const chosen = rows.filter((_, i) => keep[i] !== false && rows[i].url);
      const total = { created: 0, updated: 0, skipped: 0, errors: [], renamed: [] };
      try {
        const CHUNK = 25; // stays under Workers Free's 50 D1 queries per request
        for (let i = 0; i < chosen.length; i += CHUNK) {
          status.textContent = `Importing ${Math.min(i + CHUNK, chosen.length)} of ${chosen.length}…`;
          progress.firstChild.style.width = ((i / chosen.length) * 100).toFixed(0) + "%";
          const r = await api("POST", "/import/commit", { rows: chosen.slice(i, i + CHUNK), on_conflict: conflict.value, tags: extraTag.value.trim() ? [extraTag.value.trim()] : [] });
          total.created += r.created; total.updated += r.updated; total.skipped += r.skipped;
          total.errors.push(...r.errors); total.renamed.push(...r.renamed);
        }
        progress.firstChild.style.width = "100%";
        status.textContent = "";
        review.replaceChildren(h("div", { class: "panel", style: "margin-top:16px" }, h("h2", {}, "Import finished"),
          h("p", {}, `${total.created} created, ${total.updated} overwritten, ${total.skipped} skipped, ${total.renamed.length} renamed, ${total.errors.length} errors.`),
          total.errors.length > 0 && h("details", {}, h("summary", {}, "Errors"), total.errors.slice(0, 200).map((e) => h("div", { class: "small bad" }, `${e.slug || "row " + e.row}: ${e.error}`))),
          total.renamed.length > 0 && h("details", {}, h("summary", {}, "Renamed"), total.renamed.slice(0, 200).map((e) => h("div", { class: "small" }, `/${e.from} → /${e.to}`))),
          h("a", { class: "btn", href: "#/links" }, "View links")));
      } catch (e) { toast(e.message, true); go.disabled = false; }
    }
  }

  $app.replaceChildren(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Import"), h("div", { class: "muted" }, "Bring your links over. Click history stays with the old service."))),
    h("div", { class: "panel" },
      h("div", { class: "field" }, h("label", {}, "Source"), srcSel), help, tokenField, domainField, fileField, mapBox,
      h("div", { class: "row" }, fetchBtn), h("div", { style: "margin-top:12px" }, progress), status),
    review);
  sync();
}

// ---------- settings ----------
async function settingsPage() {
  const st = await api("GET", "/settings");
  const theme = h("select", {}, [["", "System"], ["light", "Light"], ["dark", "Dark"]].map(([v, l]) => h("option", { value: v, selected: v === (localStorage.getItem("theme") || "") }, l)));
  theme.addEventListener("change", () => { try { localStorage.setItem("theme", theme.value); } catch {} applyTheme(); });
  const exportPanel = h("div", { class: "panel" }, h("h2", {}, "Export"), h("p", { class: "muted small" }, isAdmin() ? "All links with their settings and click counts." : "Your links with their settings and click counts."),
    h("div", { class: "row" }, h("a", { class: "btn", href: "/api/export?format=csv" }, "Download CSV"), h("a", { class: "btn", href: "/api/export?format=json" }, "Download JSON")));
  const appearance = h("div", { class: "panel" }, h("h2", {}, "Appearance"), field("Theme", theme));
  const accountPanel = h("div", { class: "panel" }, h("h2", {}, "Account"),
    h("p", { class: "small" }, ME.name ? `${ME.name} · ${ME.email}` : ME.email),
    h("p", { class: "muted small" }, isAdmin() ? "You are an admin." : `You can create up to ${st.user_per_day} links a day.`),
    h("button", { class: "btn", onclick: signOut }, "Sign out"));
  if (!isAdmin()) {
    $app.replaceChildren(h("div", { class: "page-head" }, h("h1", {}, "Settings")), h("div", { class: "grid g2" }, accountPanel, h("div", { class: "grid" }, exportPanel, appearance)));
    return;
  }

  const root = h("input", { type: "url", value: st.root_url || "", placeholder: "Empty: a small landing page" });
  const nf = h("input", { type: "url", value: st.not_found_url || "", placeholder: "Empty: a 404 page" });
  const red = h("select", {}, [301, 302, 307, 308].map((v) => h("option", { value: v, selected: v === st.default_redirect }, v)));
  const style = h("select", {}, [["random", "Random chars (k7RmQ2x)"], ["words", "Words (brave-otter-42)"], ["names", "Names (curious-lovelace)"]].map(([v, l]) => h("option", { value: v, selected: v === st.default_slug_style }, l)));
  const len = h("input", { type: "number", min: 4, max: 32, value: st.default_slug_length });
  const keep = h("input", { type: "number", min: 0, max: 3650, step: 1, required: true, value: st.click_retention_days });
  const anonOn = h("input", { type: "checkbox", checked: st.anon_enabled });
  const anonTtl = h("input", { type: "number", min: 1, max: 365, value: st.anon_ttl_days });
  const anonRate = h("input", { type: "number", min: 0, max: 1000, value: st.anon_per_hour });
  const userRate = h("input", { type: "number", min: 0, max: 10000, value: st.user_per_day });
  async function save(e) {
    e.preventDefault();
    try {
      await api("PUT", "/settings", {
        root_url: root.value.trim() || null, not_found_url: nf.value.trim() || null, default_redirect: Number(red.value), default_slug_style: style.value, default_slug_length: Number(len.value),
        click_retention_days: Number(keep.value),
        anon_enabled: anonOn.checked, anon_ttl_days: Number(anonTtl.value), anon_per_hour: Number(anonRate.value), user_per_day: Number(userRate.value),
      });
      toast("Settings saved");
    } catch (err) { toast(err.message, true); }
  }
  async function rebuild(btn) {
    btn.disabled = true;
    let after = 0, total = 0;
    try {
      while (after != null) {
        const r = await api("POST", `/maintenance/rebuild-kv?after=${after}`);
        total += r.written; after = r.next_after;
        btn.textContent = `Rebuilding… ${total}`;
      }
      toast(`Rewrote ${total} KV records`);
    } catch (e) { toast(e.message, true); }
    btn.disabled = false; btn.textContent = "Rebuild KV cache";
  }
  const rebuildBtn = h("button", { class: "btn", onclick: (e) => rebuild(e.target) }, "Rebuild KV cache");
  $app.replaceChildren(
    h("div", { class: "page-head" }, h("h1", {}, "Settings")),
    h("div", { class: "grid g2" },
      h("form", { class: "panel", onsubmit: save }, h("h2", {}, "Redirects"),
        field("Root URL (301.xorio.rs/)", root), field("Unknown slug redirect", nf),
        field("Default redirect type", red), field("Default slug style", style), field("Random slug length", len),
        field("Keep click details for (days, 0 = forever)", keep),
        h("h2", { style: "margin-top:22px" }, "Public access"),
        h("label", { class: "check", style: "margin-bottom:12px" }, anonOn, "Anyone can create links without an account"),
        !CFG.turnstile_site_key && !CFG.dev && h("div", { class: "notice" }, "Anonymous links stay off until Turnstile is configured (TURNSTILE_SITE_KEY and TURNSTILE_SECRET)."),
        h("div", { class: "grid g2" }, field("Anonymous links expire after (days)", anonTtl), field("Anonymous links per IP per hour", anonRate)),
        field("Links per signed-in user per day", userRate, "Admins have no limit."),
        h("button", { class: "btn primary", type: "submit" }, "Save")),
      h("div", { class: "grid" }, accountPanel, exportPanel,
        h("div", { class: "panel" }, h("h2", {}, "Maintenance"), h("p", { class: "muted small" }, "Rewrites every link's edge record from the database. Only needed after manual database edits."), rebuildBtn),
        appearance)),
  );
}

// ---------- moderation (admins) ----------
async function moderationPage() {
  const [reports, users] = await Promise.all([api("GET", "/reports"), api("GET", "/users")]);
  async function block(r) {
    try { await api("POST", "/links/bulk", { ids: [r.link_id], action: "block" }); await api("POST", `/reports/${r.id}/resolve?all=1`); toast(`Blocked /${r.slug}`); moderationPage(); }
    catch (e) { toast(e.message, true); }
  }
  async function dismiss(r) {
    try { await api("POST", `/reports/${r.id}/resolve`); moderationPage(); } catch (e) { toast(e.message, true); }
  }
  async function ban(u, banned) {
    if (banned && !confirm(`Ban ${u.email}? They are signed out and cannot sign in again. Their links keep working until you block them.`)) return;
    try { await api("POST", `/users/${u.id}`, { banned }); toast(banned ? "Banned" : "Unbanned"); moderationPage(); } catch (e) { toast(e.message, true); }
  }
  const viewLinks = (owner) => { Object.assign(listState, { owner: String(owner), offset: 0, status: "", q: "", tag: "" }); location.hash = "#/links"; };
  $app.replaceChildren(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Moderation"), h("div", { class: "muted" }, "Reports from visitors, and everyone who has signed in."))),
    h("div", { class: "panel", style: "margin-bottom:16px" }, h("h2", {}, `Open reports (${reports.length})`),
      reports.length ? h("div", { class: "table-wrap" }, h("table", {},
        h("thead", {}, h("tr", {}, h("th", {}, "Link"), h("th", {}, "Reason"), h("th", {}, "Reported"), h("th", {}))),
        h("tbody", {}, reports.map((r) => h("tr", {},
          h("td", {}, r.slug ? h("a", { class: "slug", href: `#/links/${r.link_id}` }, "/" + r.slug) : h("span", { class: "muted" }, "(deleted)"),
            r.blocked ? h("span", { class: "badge blocked" }, "BLOCKED") : null,
            h("div", { class: "dest" }, r.url || ""), h("div", { class: "small muted" }, r.owner_email || "anonymous")),
          h("td", { class: "small" }, r.reason),
          h("td", { class: "small muted" }, fmt.ago(r.created_at)),
          h("td", {}, h("div", { class: "row" },
            r.slug && !r.blocked && h("button", { class: "btn sm danger", onclick: () => block(r) }, "Block link"),
            h("button", { class: "btn sm", onclick: () => dismiss(r) }, "Dismiss")))))))) : h("p", { class: "muted small" }, "Nothing reported. Visitors can report a link from its preview page (add + to any short link).")),
    h("div", { class: "panel" }, h("h2", {}, `Users (${users.length})`),
      h("div", { class: "table-wrap" }, h("table", {},
        h("thead", {}, h("tr", {}, h("th", {}, "User"), h("th", { class: "num" }, "Links"), h("th", {}, "Last sign-in"), h("th", {}))),
        h("tbody", {}, users.map((u) => h("tr", {},
          h("td", {}, h("div", {}, u.email, u.banned ? h("span", { class: "badge blocked" }, "BANNED") : null), h("div", { class: "small muted" }, u.name || "")),
          h("td", { class: "num" }, h("a", { href: "#/links", onclick: (e) => { e.preventDefault(); viewLinks(u.id); } }, fmt.n(u.links))),
          h("td", { class: "small muted" }, fmt.ago(u.last_login_at)),
          h("td", {}, u.id !== ME.user_id && h("button", { class: "btn sm" + (u.banned ? "" : " danger"), onclick: () => ban(u, !u.banned) }, u.banned ? "Unban" : "Ban")))))))),
  );
}

// ---------- signed out: anonymous shortener ----------
const ANON_KEY = "anon-links";
function anonList() { try { return JSON.parse(localStorage.getItem(ANON_KEY) || "[]"); } catch { return []; } }
function anonSave(list) { try { localStorage.setItem(ANON_KEY, JSON.stringify(list.slice(0, 200))); } catch {} }
const forgetAnon = (token) => anonSave(anonList().filter((a) => a.token !== token));
const manageUrl = (token) => `${ORIGIN}/_/#/m/${token}`;
const signInUrl = (next = "/_/") => `/auth/login?next=${encodeURIComponent(next)}`;

let turnstileReady;
function loadTurnstile() {
  turnstileReady ||= new Promise((resolve, reject) => {
    const sc = document.createElement("script");
    sc.src = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";
    sc.async = true;
    sc.onload = () => resolve(window.turnstile);
    sc.onerror = () => reject(new Error("Could not load the human check. Disable blockers for this page and reload."));
    document.head.append(sc);
  });
  return turnstileReady;
}

async function publicHome() {
  const signIn = CFG.google && h("a", { class: "btn primary", href: signInUrl(location.hash ? "/_/" + location.hash : "/_/") }, "Sign in with Google");
  const perks = h("div", { class: "panel" }, h("h2", {}, "With an account"),
    h("ul", { class: "small", style: "margin:0 0 14px; padding-left:18px" },
      h("li", {}, "Links that never expire, with your own names"),
      h("li", {}, "Analytics, QR codes, UTM builder, passwords, country and device rules"),
      h("li", {}, "Edit or delete your links any time")),
    signIn || h("p", { class: "muted small" }, "Sign-in is not available on this server."));

  if (!CFG.anon.enabled) {
    $app.replaceChildren(h("div", { class: "hero" }, h("h1", {}, "Short links"),
      h("p", { class: "muted" }, "Sign in to create and manage short links."), perks));
    return;
  }

  const url = h("input", { type: "url", required: true, placeholder: "Paste a long URL", autocomplete: "off" });
  const style = h("select", { "aria-label": "Slug style" }, [["random", "k7RmQ2x"], ["words", "brave-otter-42"], ["names", "curious-lovelace"]].map(([v, l]) => h("option", { value: v }, l)));
  const go = h("button", { class: "btn primary", type: "submit" }, "Shorten");
  const ts = h("div", { class: "ts" });
  const result = h("div");
  let tsToken = "", tsWidget = null;
  if (CFG.turnstile_site_key) {
    loadTurnstile().then((T) => {
      tsWidget = T.render(ts, { sitekey: CFG.turnstile_site_key, callback: (v) => (tsToken = v), "expired-callback": () => (tsToken = ""), "error-callback": () => (tsToken = "") });
    }).catch((e) => toast(e.message, true));
  }
  async function submit(e) {
    e.preventDefault();
    if (CFG.turnstile_site_key && !tsToken) return toast("Wait for the human check to finish, then try again.", true);
    go.disabled = true;
    try {
      const r = await api("POST", "/anon/links", { url: url.value.trim(), slug_style: style.value, turnstile: tsToken });
      const l = r.link;
      anonSave([{ token: r.manage_token, slug: l.slug, url: l.url, expires_at: l.expires_at, created_at: l.created_at }, ...anonList()]);
      copy(shortUrl(l.slug));
      url.value = "";
      result.replaceChildren(h("div", { class: "result" },
        h("div", { class: "shorturl" }, shortUrl(l.slug)),
        h("div", { class: "row", style: "margin:8px 0" }, h("button", { class: "btn sm primary", type: "button", onclick: () => copy(shortUrl(l.slug)) }, "Copy"),
          h("a", { class: "btn sm", href: `#/m/${r.manage_token}` }, "Stats and delete")),
        h("p", { class: "muted small", style: "margin:0" }, `Expires ${fmt.date(l.expires_at)}. Keep this private manage link to see clicks or delete it: `,
          h("span", { class: "mono" }, manageUrl(r.manage_token)))));
      renderMine();
    } catch (err) { toast(err.message, true); }
    go.disabled = false;
    if (tsWidget != null && window.turnstile) { window.turnstile.reset(tsWidget); tsToken = ""; }
  }
  const mine = h("div");
  function renderMine() {
    const list = anonList();
    mine.replaceChildren(list.length ? h("div", { class: "panel", style: "margin-top:16px" }, h("h2", {}, "Links made on this device"),
      h("div", { class: "table-wrap" }, h("table", {}, h("tbody", {}, list.map((a) => h("tr", { class: "link-row", onclick: () => (location.hash = `#/m/${a.token}`) },
        h("td", {}, h("div", { class: "slug" }, "/" + a.slug), h("div", { class: "dest" }, a.url)),
        h("td", { class: "small muted" }, a.expires_at && a.expires_at < Date.now() ? "expired" : `expires ${fmt.date(a.expires_at)}`),
        h("td", {}, h("button", { class: "btn sm", onclick: (e) => { e.stopPropagation(); copy(shortUrl(a.slug)); } }, "Copy"))))))),
      CFG.google && h("p", { class: "muted small", style: "margin:10px 0 0" }, "Sign in to move these links to your account.")) : "");
  }
  renderMine();
  $app.replaceChildren(h("div", { class: "hero" },
    h("h1", {}, "Shorten a link"),
    h("p", { class: "muted" }, `No account needed. Links get a random name and expire after ${CFG.anon.ttl_days} days.`),
    h("form", { class: "panel", onsubmit: submit }, h("div", { class: "shorten" }, url, style, go), ts, result),
    h("p", { class: "muted small" }, "By creating a link you agree not to use it for spam, phishing or malware. Links are checked with Google Safe Browsing and can be reported."),
    mine, h("div", { style: "margin-top:16px" }, perks)));
  url.focus();
}

async function managePage(token) {
  let link;
  try { link = await api("GET", `/anon/${token}`); }
  catch (e) {
    // Only a 404 means the token is dead; keep it on network errors, 429s or 5xx.
    if (e.status === 404) forgetAnon(token);
    $app.replaceChildren(h("div", { class: "hero panel" }, h("h2", {}, e.status === 404 ? "Link not found" : "Could not load the link"), h("p", { class: "muted" }, e.message), h("a", { class: "btn", href: "#/" }, "Back")));
    return;
  }
  async function del() {
    if (!confirm(`Delete /${link.slug}? It stops working right away.`)) return;
    try { await api("DELETE", `/anon/${token}`); forgetAnon(token); toast("Deleted"); location.hash = "#/"; }
    catch (e) { toast(e.message, true); }
  }
  async function claim() {
    try {
      const r = await api("POST", "/claim", { tokens: [token] });
      forgetAnon(token);
      toast(r.claimed ? "Moved to your account" : "Nothing to move");
      location.hash = "#/links";
    } catch (e) { toast(e.message, true); }
  }
  const statsWrap = h("div", { class: "grid", style: "gap:16px; margin-top:16px" });
  async function loadStats(r) {
    const st = await api("GET", `/anon/${token}/stats?range=${r}`);
    statsWrap.replaceChildren(
      h("div", { class: "page-head", style: "margin:8px 0 0" }, h("h2", { style: "margin:0" }, "Clicks"), rangePicker(r, loadStats)),
      h("div", { class: "grid g4" }, stat("Clicks", st.clicks), stat("QR scans", st.qr), stat("Bot hits", st.bots), stat("All-time clicks", link.clicks)),
      ...statsBlock(st));
  }
  $app.replaceChildren(
    h("div", { class: "page-head" },
      h("div", {}, h("div", { class: "shorturl" }, shortUrl(link.slug)), h("div", { class: "dest" }, link.url),
        h("div", { class: "muted small" }, `Created ${fmt.dt(link.created_at)} · ${link.status === "expired" ? "expired" : "expires " + fmt.dt(link.expires_at)}`)),
      h("div", { class: "row" },
        h("button", { class: "btn primary", onclick: () => copy(shortUrl(link.slug)) }, "Copy"),
        ME ? h("button", { class: "btn", onclick: claim }, "Move to my account") : CFG.google && h("a", { class: "btn", href: signInUrl("/_/#/m/" + token) }, "Sign in to keep it"),
        h("button", { class: "btn danger", onclick: del }, "Delete"))),
    link.blocked && h("div", { class: "notice" }, "This link was blocked by an admin because it broke the rules of this service."),
    statsWrap);
  loadStats("30d");
}

async function reportPage(slug) {
  const name = h("input", { type: "text", value: slug.replace(/\+$/, ""), required: true });
  const reason = h("textarea", { required: true, maxlength: 1000, placeholder: "Phishing, malware, spam, or something else? Anything that helps." });
  async function send(e) {
    e.preventDefault();
    try { await api("POST", "/report", { slug: name.value.trim(), reason: reason.value.trim() }); $app.replaceChildren(h("div", { class: "hero panel" }, h("h2", {}, "Thanks"), h("p", { class: "muted" }, "An admin will look at it."))); }
    catch (err) { toast(err.message, true); }
  }
  $app.replaceChildren(h("div", { class: "hero" }, h("h1", {}, "Report a link"),
    h("form", { class: "panel", onsubmit: send }, field("Short link", name, ORIGIN.replace(/^https?:\/\//, "") + "/…"), field("What is wrong with it?", reason),
      h("button", { class: "btn primary", type: "submit" }, "Send report"))));
}

// ---------- account ----------
async function signOut() {
  try { await fetch("/auth/logout", { method: "POST", credentials: "same-origin" }); } catch {}
  location.href = "/_/";
}

function renderHeader() {
  const signedIn = !!ME;
  document.getElementById("nav").hidden = !signedIn;
  document.getElementById("new-link").hidden = !signedIn;
  document.querySelectorAll("#nav [data-admin]").forEach((a) => (a.hidden = !isAdmin()));
  document.getElementById("me").textContent = signedIn ? ME.name || ME.email : "";
  const acc = document.getElementById("account");
  acc.replaceChildren(
    signedIn
      ? (CFG.dev ? "" : h("button", { class: "btn sm", onclick: signOut }, "Sign out"))
      : CFG.google ? h("a", { class: "btn sm", href: signInUrl("/_/" + (location.hash || "")) }, "Sign in") : "");
}

/// Anonymous links made on this device before signing in move to the account.
async function claimDeviceLinks() {
  // The server takes at most 50 tokens per call: send them in chunks and only forget
  // the ones that were sent, so a failure keeps the rest for the next sign-in.
  let list = anonList();
  const sent = [];
  let claimed = 0;
  try {
    while (list.length) {
      const chunk = list.slice(0, 50).map((a) => a.token);
      const r = await api("POST", "/claim", { tokens: chunk });
      sent.push(...chunk);
      claimed += r.claimed || 0;
      list = list.slice(50);
      anonSave(list);
    }
  } catch {}
  if (claimed) toast(`Moved ${claimed} link${claimed === 1 ? "" : "s"} you made without an account to your account`);
  return sent;
}

function applyTheme() {
  let t = "";
  try { t = localStorage.getItem("theme") || ""; } catch {}
  if (t) document.documentElement.dataset.theme = t; else delete document.documentElement.dataset.theme;
}

async function boot() {
  applyTheme();
  const [cfg, me] = await Promise.allSettled([api("GET", "/config"), api("GET", "/me")]);
  if (cfg.status === "fulfilled") CFG = cfg.value;
  ME = me.status === "fulfilled" ? me.value : null;
  if (me.status === "rejected" && [403, 503].includes(me.reason.status)) toast(me.reason.message, true);
  renderHeader();
  if (ME) {
    // Signed in from a manage page: that token was just claimed, so show the account's links.
    const sent = await claimDeviceLinks();
    const m = location.hash.match(/^#\/m\/([^/?]+)/);
    if (m && sent.includes(m[1])) history.replaceState(null, "", "#/links");
  }
  render();
}
boot();
