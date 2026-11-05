//! Importers for Bitly, Rebrandly, Short.io, TinyURL (API) and CSV (parsed in the browser).
//!
//! Provider fetches are page-at-a-time so the browser drives pagination and no single
//! Worker request runs into the subrequest limit. Tokens are never stored.

use crate::model::{Link, LinkInput};
use crate::slug;
use crate::store::{self, s, SResult, StoreError};
use crate::util::now_ms;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

#[derive(Deserialize, Debug)]
pub struct FetchRequest {
    pub source: String,
    pub token: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ImportRow {
    #[serde(default)]
    pub slug: Option<String>,
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Serialize, Debug, Default)]
pub struct FetchPage {
    pub rows: Vec<ImportRow>,
    pub next_cursor: Option<String>,
}

type R<T> = std::result::Result<T, String>;

async fn get_json(url: &str, headers: &[(&str, String)]) -> R<Value> {
    let h = Headers::new();
    h.set("Accept", "application/json")
        .map_err(|e| e.to_string())?;
    h.set("User-Agent", "301.xorio.rs importer")
        .map_err(|e| e.to_string())?;
    for (k, v) in headers {
        h.set(k, v).map_err(|e| e.to_string())?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(h);
    let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
    let mut res = Fetch::Request(req)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status_code();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("{url} answered {status}: {snippet}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("bad JSON from {url}: {e}"))
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn tags_of(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string).or_else(|| str_of(t, "name")))
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_date(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| {
            if f < 1e11 {
                (f * 1000.0) as i64
            } else {
                f as i64
            }
        }),
        Value::String(s) if !s.is_empty() => crate::util::parse_date_str(s),
        _ => None,
    }
}

fn path_of(u: &str) -> Option<String> {
    let with_scheme = if u.contains("://") {
        u.to_string()
    } else {
        format!("https://{u}")
    };
    let p = url::Url::parse(&with_scheme).ok()?;
    let path = p.path().trim_matches('/').to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

pub async fn fetch_page(req: &FetchRequest) -> R<FetchPage> {
    let token = req.token.trim().to_string();
    if token.is_empty() {
        return Err("an API token is required".into());
    }
    match req.source.as_str() {
        "bitly" => bitly(&token, req.cursor.as_deref()).await,
        "rebrandly" => rebrandly(&token, req.domain.as_deref(), req.cursor.as_deref()).await,
        "shortio" => shortio(&token, req.domain.as_deref(), req.cursor.as_deref()).await,
        "tinyurl" => tinyurl(&token, req.cursor.as_deref()).await,
        other => Err(format!("unknown source '{other}'")),
    }
}

async fn bitly(token: &str, cursor: Option<&str>) -> R<FetchPage> {
    let auth = [("Authorization", format!("Bearer {token}"))];
    let url = match cursor {
        Some(c) if c.starts_with("https://api-ssl.bitly.com/") => c.to_string(),
        Some(_) => return Err("bad cursor".into()),
        None => {
            let user = get_json("https://api-ssl.bitly.com/v4/user", &auth).await?;
            let guid =
                str_of(&user, "default_group_guid").ok_or("Bitly user has no default group")?;
            format!("https://api-ssl.bitly.com/v4/groups/{guid}/bitlinks?size=100")
        }
    };
    let body = get_json(&url, &auth).await?;
    let rows = body
        .get("links")
        .and_then(Value::as_array)
        .map(|links| links.iter().filter_map(bitly_row).collect())
        .unwrap_or_default();
    let next = body
        .pointer("/pagination/next")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(FetchPage {
        rows,
        next_cursor: next,
    })
}

pub fn bitly_row(l: &Value) -> Option<ImportRow> {
    let url = str_of(l, "long_url")?;
    let custom = l
        .get("custom_bitlinks")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().rev().find_map(|v| v.as_str()))
        .and_then(path_of);
    let slug = custom.or_else(|| str_of(l, "id").and_then(|id| path_of(&id)));
    Some(ImportRow {
        slug,
        url,
        title: str_of(l, "title"),
        tags: tags_of(l, "tags"),
        disabled: l.get("archived").and_then(Value::as_bool).unwrap_or(false),
        created_at: l.get("created_at").and_then(parse_date),
        source: Some("bitly".into()),
        ..Default::default()
    })
}

async fn rebrandly(token: &str, domain: Option<&str>, cursor: Option<&str>) -> R<FetchPage> {
    let mut params = vec![
        ("limit", "25"),
        ("orderBy", "createdAt"),
        ("orderDir", "desc"),
    ];
    if let Some(c) = cursor {
        params.push(("last", c));
    }
    if let Some(d) = domain.map(str::trim).filter(|d| !d.is_empty()) {
        params.push(("domain.fullName", d));
    }
    let url = url::Url::parse_with_params("https://api.rebrandly.com/v1/links", &params)
        .map_err(|e| e.to_string())?;
    let body = get_json(url.as_str(), &[("apikey", token.to_string())]).await?;
    let links = body.as_array().cloned().unwrap_or_default();
    let rows = links.iter().filter_map(rebrandly_row).collect();
    let next = if links.len() == 25 {
        links.last().and_then(|l| str_of(l, "id"))
    } else {
        None
    };
    Ok(FetchPage {
        rows,
        next_cursor: next,
    })
}

pub fn rebrandly_row(l: &Value) -> Option<ImportRow> {
    Some(ImportRow {
        slug: str_of(l, "slashtag"),
        url: str_of(l, "destination")?,
        title: str_of(l, "title"),
        created_at: l.get("createdAt").and_then(parse_date),
        source: Some("rebrandly".into()),
        ..Default::default()
    })
}

async fn shortio(token: &str, domain: Option<&str>, cursor: Option<&str>) -> R<FetchPage> {
    let auth = [("Authorization", token.to_string())];
    let (domain_id, page_token) = match cursor {
        Some(c) => {
            let (id, tok) = c.split_once('|').ok_or("bad cursor")?;
            (id.to_string(), Some(tok.to_string()))
        }
        None => {
            let host = domain
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .ok_or("Short.io needs the domain hostname, e.g. go.example.com")?;
            let domains = get_json("https://api.short.io/api/domains", &auth).await?;
            let id = domains
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|d| str_of(d, "hostname").as_deref() == Some(host))
                })
                .and_then(|d| d.get("id"))
                .map(|v| v.to_string().trim_matches('"').to_string())
                .ok_or_else(|| format!("domain {host} not found in this Short.io account"))?;
            (id, None)
        }
    };
    if !domain_id.chars().all(|c| c.is_ascii_digit()) {
        return Err("bad domain id".into());
    }
    let mut params = vec![("domain_id", domain_id.as_str()), ("limit", "150")];
    if let Some(t) = &page_token {
        params.push(("pageToken", t));
    }
    let url = url::Url::parse_with_params("https://api.short.io/api/links", &params)
        .map_err(|e| e.to_string())?;
    let body = get_json(url.as_str(), &auth).await?;
    let rows = body
        .get("links")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(shortio_row).collect())
        .unwrap_or_default();
    let next = body
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|t| format!("{domain_id}|{t}"));
    Ok(FetchPage {
        rows,
        next_cursor: next,
    })
}

pub fn shortio_row(l: &Value) -> Option<ImportRow> {
    let has_pw = l
        .get("hasPassword")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(ImportRow {
        slug: str_of(l, "path"),
        url: str_of(l, "originalURL")?,
        title: str_of(l, "title"),
        tags: tags_of(l, "tags"),
        expires_at: l.get("expiresAt").and_then(parse_date),
        disabled: l.get("archived").and_then(Value::as_bool).unwrap_or(false),
        created_at: l.get("createdAt").and_then(parse_date),
        source: Some("shortio".into()),
        warning: has_pw
            .then(|| "password-protected on Short.io; set a new password after import".into()),
        ..Default::default()
    })
}

async fn tinyurl(token: &str, cursor: Option<&str>) -> R<FetchPage> {
    const PER_PAGE: usize = 40;
    let auth = [("Authorization", format!("Bearer {token}"))];
    let start: usize = cursor
        .map(|c| c.parse().map_err(|_| "bad cursor"))
        .transpose()?
        .unwrap_or(0);
    let list = get_json("https://api.tinyurl.com/urls/available", &auth).await?;
    let items: Vec<Value> = list
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let page: Vec<(&Value, String, String)> = items
        .iter()
        .skip(start)
        .take(PER_PAGE)
        .filter_map(|item| Some((item, str_of(item, "domain")?, str_of(item, "alias")?)))
        .collect();
    // The list has no long URLs: fetch each link's detail, concurrently.
    let details = join_all(page.iter().map(|(_, domain, alias)| {
        let mut u = url::Url::parse("https://api.tinyurl.com/alias/").expect("static url");
        u.path_segments_mut()
            .expect("base")
            .pop_if_empty()
            .push(domain)
            .push(alias);
        let auth = &auth;
        async move { get_json(u.as_str(), auth).await }
    }))
    .await;
    let rows = page
        .into_iter()
        .zip(details)
        .map(|((item, domain, alias), detail)| {
            let url = detail
                .ok()
                .and_then(|d| d.get("data").and_then(|v| str_of(v, "url")));
            ImportRow {
                warning: url
                    .is_none()
                    .then(|| format!("could not read the long URL for {domain}/{alias}")),
                slug: Some(alias),
                url: url.unwrap_or_default(),
                tags: tags_of(item, "tags"),
                expires_at: item.get("expires_at").and_then(parse_date),
                created_at: item.get("created_at").and_then(parse_date),
                source: Some("tinyurl".into()),
                ..Default::default()
            }
        })
        .collect();
    let next = (start + PER_PAGE < items.len()).then(|| (start + PER_PAGE).to_string());
    Ok(FetchPage {
        rows,
        next_cursor: next,
    })
}

// ---- Commit ----

#[derive(Deserialize)]
pub struct CommitRequest {
    pub rows: Vec<ImportRow>,
    #[serde(default = "default_conflict")]
    pub on_conflict: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_conflict() -> String {
    "skip".into()
}

#[derive(Serialize, Default)]
pub struct CommitResult {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub errors: Vec<Value>,
    pub renamed: Vec<Value>,
}

impl ImportRow {
    /// The fields an import sets, as a regular link input (same validation as the form).
    fn to_input(&self, extra_tags: &[String]) -> LinkInput {
        LinkInput {
            url: Some(self.url.clone()),
            title: self.title.clone().map(Some),
            notes: self.notes.clone().map(Some),
            add_tags: Some(self.tags.iter().chain(extra_tags).cloned().collect()),
            expires_at: Some(self.expires_at),
            disabled: Some(self.disabled),
            source: Some(self.source.clone().unwrap_or_else(|| "csv".into())),
            ..Default::default()
        }
    }
}

async fn taken(env: &Env, slugs: &[String]) -> SResult<HashMap<String, i64>> {
    #[derive(Deserialize)]
    struct Row {
        slug: String,
        id: i64,
    }
    let d = store::db(env)?;
    let mut stmts = vec![];
    for chunk in slugs.chunks(40) {
        let marks = vec!["?"; chunk.len()].join(",");
        let binds: Vec<_> = chunk.iter().chain(chunk).map(|c| s(c)).collect();
        stmts.push(
            d.prepare(format!(
                "SELECT slug, id FROM links WHERE slug IN ({marks}) UNION ALL SELECT slug, link_id AS id FROM aliases WHERE slug IN ({marks})"
            ))
            .bind(&binds)?,
        );
    }
    let mut out = HashMap::new();
    if stmts.is_empty() {
        return Ok(out);
    }
    for res in d.batch(stmts).await? {
        out.extend(res.results::<Row>()?.into_iter().map(|r| (r.slug, r.id)));
    }
    Ok(out)
}

pub async fn commit(env: &Env, req: CommitRequest) -> SResult<CommitResult> {
    if req.rows.len() > 100 {
        return Err(StoreError::Bad(
            "at most 100 rows per commit; send them in chunks".into(),
        ));
    }
    if !["skip", "overwrite", "rename"].contains(&req.on_conflict.as_str()) {
        return Err(StoreError::Bad(
            "on_conflict must be skip, overwrite or rename".into(),
        ));
    }
    let st = store::settings(env).await.unwrap_or_default();
    let mut result = CommitResult::default();
    let slugs: Vec<Option<String>> = req
        .rows
        .iter()
        .map(|r| r.slug.as_deref().and_then(slug::normalize))
        .collect();
    // Candidates for rows without a slug, checked in the same batched lookup instead of
    // one D1 query per row (which runs into the per-invocation query limit).
    let generated: Vec<Option<String>> = slugs
        .iter()
        .map(|s| {
            s.is_none()
                .then(|| slug::generate(st.slug_style(), st.default_slug_length, 0))
        })
        .collect();
    let lookup: Vec<String> = slugs.iter().chain(&generated).flatten().cloned().collect();
    let existing = taken(env, &lookup).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let d = store::db(env)?;
    let mut stmts = vec![];
    let mut overwritten: Vec<Link> = vec![];
    let now = now_ms();

    for (i, (row, slug_n)) in req.rows.iter().zip(slugs).enumerate() {
        let mut fail = |slug: Option<&str>, e: String| {
            result
                .errors
                .push(serde_json::json!({ "row": i, "slug": slug, "error": e }))
        };
        let input = row.to_input(&req.tags);
        let mut slug_v = match slug_n {
            Some(sl) => match slug::validate(&sl) {
                Ok(()) => sl,
                Err(e) => {
                    fail(Some(&sl), e);
                    continue;
                }
            },
            None => {
                // A generated slug that collides is regenerated, never skipped or renamed.
                let mut c = generated[i].clone().unwrap_or_default();
                let mut tries = 0;
                while c.is_empty() || existing.contains_key(&c) || seen.contains(&c) {
                    tries += 1;
                    if tries > 5 {
                        break;
                    }
                    c = store::unique_slug(env, st.slug_style(), st.default_slug_length).await?;
                }
                if tries > 5 {
                    fail(None, "could not find a free slug, try again".into());
                    continue;
                }
                c
            }
        };
        let conflict = existing.get(&slug_v).copied();
        let dup = seen.contains(&slug_v);

        if conflict.is_some() || dup {
            match req.on_conflict.as_str() {
                "skip" => {
                    result.skipped += 1;
                    continue;
                }
                "overwrite" if !dup => {
                    // Same write path as an edit in the admin UI.
                    let Some(mut link) = store::get_link(env, conflict.unwrap_or_default()).await?
                    else {
                        continue;
                    };
                    match store::prepare_update(env, &mut link, &input).await {
                        Ok(st) => {
                            stmts.extend(st);
                            seen.insert(slug_v);
                            overwritten.push(link);
                            result.updated += 1;
                        }
                        Err(e) => fail(Some(&slug_v), e.public_message()),
                    }
                    continue;
                }
                _ => {
                    let base = slug_v.clone();
                    let mut found = None;
                    for k in 2..50 {
                        let cand = format!("{base}-{k}");
                        if !seen.contains(&cand)
                            && !existing.contains_key(&cand)
                            && store::slug_owner(env, &cand).await?.is_none()
                        {
                            found = Some(cand);
                            break;
                        }
                    }
                    let Some(newslug) = found else {
                        fail(Some(&base), "no free renamed slug".into());
                        continue;
                    };
                    result
                        .renamed
                        .push(serde_json::json!({ "from": base, "to": newslug }));
                    slug_v = newslug;
                }
            }
        }

        let mut link = Link::new(st.default_redirect, "csv");
        if let Err(e) = input.apply(&mut link) {
            fail(Some(&slug_v), e);
            continue;
        }
        link.slug = slug_v.clone();
        link.created_at = row.created_at.unwrap_or(now);
        link.updated_at = now;
        stmts.push(store::insert_stmt(&d, &link)?);
        seen.insert(slug_v);
        result.created += 1;
    }

    // One batch (one transaction): on failure nothing is written, so the reported counts
    // never disagree with the database and the client can retry the chunk as-is.
    if !stmts.is_empty() {
        d.batch(stmts).await?;
    }
    // New links get their KV record on first click; overwritten ones are refreshed now.
    for r in join_all(overwritten.iter().map(|l| store::sync_kv(env, l))).await {
        r?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_bitly() {
        let r = bitly_row(&json!({
            "id": "bit.ly/3abcXYZ", "long_url": "https://example.com/a", "title": "A",
            "tags": ["x"], "archived": false,
            "custom_bitlinks": ["https://bit.ly/my-custom"]
        }))
        .unwrap();
        assert_eq!(r.slug.as_deref(), Some("my-custom"));
        assert_eq!(r.url, "https://example.com/a");
        assert_eq!(r.tags, vec!["x"]);
        let r = bitly_row(&json!({"id": "bit.ly/3abc", "long_url": "https://e.com"})).unwrap();
        assert_eq!(r.slug.as_deref(), Some("3abc"));
    }

    #[test]
    fn maps_rebrandly_and_shortio() {
        let r = rebrandly_row(&json!({"id": "abc", "slashtag": "promo", "destination": "https://e.com", "title": "P"})).unwrap();
        assert_eq!(
            (r.slug.as_deref(), r.url.as_str()),
            (Some("promo"), "https://e.com")
        );
        let r = shortio_row(&json!({"path": "go", "originalURL": "https://e.com", "tags": ["a"], "hasPassword": true, "archived": true, "expiresAt": 1893456000000i64})).unwrap();
        assert_eq!(r.slug.as_deref(), Some("go"));
        assert!(r.disabled);
        assert!(r.warning.is_some());
        assert_eq!(r.expires_at, Some(1893456000000));
    }

    #[test]
    fn dates_in_seconds_become_ms() {
        assert_eq!(parse_date(&json!(1700000000)), Some(1700000000000));
        assert_eq!(parse_date(&json!(1700000000000i64)), Some(1700000000000));
    }
}
