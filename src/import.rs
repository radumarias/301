//! Importers for Bitly, Rebrandly, Short.io, TinyURL (API) and CSV (parsed in the browser).
//!
//! Provider fetches are page-at-a-time so the browser drives pagination and no single
//! Worker request runs into the subrequest limit. Tokens are never stored.

use crate::model::{normalize_tags, validate_url, Link};
use crate::slug::{self, Style};
use crate::store::{self, n, s, SResult, StoreError};
use crate::util::now_ms;
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
        Value::String(s) if !s.is_empty() => date_str(s),
        _ => None,
    }
}

#[cfg(target_arch = "wasm32")]
fn date_str(s: &str) -> Option<i64> {
    // JS accepts "+0000" offsets poorly; normalise to "+00:00".
    let fixed =
        if s.len() > 5 && s.as_bytes()[s.len() - 5] == b'+' && !s[s.len() - 4..].contains(':') {
            format!("{}:{}", &s[..s.len() - 2], &s[s.len() - 2..])
        } else {
            s.to_string()
        };
    let t = js_sys::Date::parse(&fixed);
    if t.is_nan() {
        None
    } else {
        Some(t as i64)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn date_str(_s: &str) -> Option<i64> {
    None
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
    let mut url =
        "https://api.rebrandly.com/v1/links?limit=25&orderBy=createdAt&orderDir=desc".to_string();
    if let Some(c) = cursor {
        if !c.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            return Err("bad cursor".into());
        }
        url.push_str(&format!("&last={c}"));
    }
    if let Some(d) = domain.map(str::trim).filter(|d| !d.is_empty()) {
        url.push_str(&format!(
            "&domain.fullName={}",
            percent_encoding::utf8_percent_encode(d, percent_encoding::NON_ALPHANUMERIC)
        ));
    }
    let body = get_json(&url, &[("apikey", token.to_string())]).await?;
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
    let mut url = format!("https://api.short.io/api/links?domain_id={domain_id}&limit=150");
    if let Some(t) = &page_token {
        url.push_str(&format!(
            "&pageToken={}",
            percent_encoding::utf8_percent_encode(t, percent_encoding::NON_ALPHANUMERIC)
        ));
    }
    let body = get_json(&url, &auth).await?;
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
    let mut rows = vec![];
    for item in items.iter().skip(start).take(PER_PAGE) {
        let (Some(domain), Some(alias)) = (str_of(item, "domain"), str_of(item, "alias")) else {
            continue;
        };
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        let detail = get_json(
            &format!(
                "https://api.tinyurl.com/alias/{}/{}",
                enc(&domain),
                enc(&alias)
            ),
            &auth,
        )
        .await;
        let data = detail
            .as_ref()
            .ok()
            .and_then(|d| d.get("data"))
            .cloned()
            .unwrap_or(Value::Null);
        match str_of(&data, "url") {
            Some(url) => rows.push(ImportRow {
                slug: Some(alias),
                url,
                tags: tags_of(item, "tags"),
                expires_at: item.get("expires_at").and_then(parse_date),
                created_at: item.get("created_at").and_then(parse_date),
                source: Some("tinyurl".into()),
                ..Default::default()
            }),
            None => rows.push(ImportRow {
                slug: Some(alias.clone()),
                url: String::new(),
                source: Some("tinyurl".into()),
                warning: Some(format!("could not read the long URL for {domain}/{alias}")),
                ..Default::default()
            }),
        }
    }
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

async fn taken(env: &Env, slugs: &[String]) -> SResult<HashMap<String, i64>> {
    #[derive(Deserialize)]
    struct Row {
        slug: String,
        id: i64,
    }
    let d = store::db(env)?;
    let mut out = HashMap::new();
    for chunk in slugs.chunks(40) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut binds: Vec<_> = chunk.iter().map(|c| s(c)).collect();
        binds.extend(chunk.iter().map(|c| s(c)));
        let rows = d
            .prepare(format!(
                "SELECT slug, id FROM links WHERE slug IN ({marks}) UNION ALL SELECT slug, link_id AS id FROM aliases WHERE slug IN ({marks})"
            ))
            .bind(&binds)?
            .all()
            .await?
            .results::<Row>()?;
        out.extend(rows.into_iter().map(|r| (r.slug, r.id)));
    }
    Ok(out)
}

pub async fn commit(env: &Env, req: CommitRequest) -> SResult<CommitResult> {
    if req.rows.len() > 500 {
        return Err(StoreError::Bad(
            "at most 500 rows per commit; send them in chunks".into(),
        ));
    }
    if !["skip", "overwrite", "rename"].contains(&req.on_conflict.as_str()) {
        return Err(StoreError::Bad(
            "on_conflict must be skip, overwrite or rename".into(),
        ));
    }
    let st = store::settings(env).await.unwrap_or_default();
    let mut result = CommitResult::default();
    let given: Vec<String> = req
        .rows
        .iter()
        .filter_map(|r| r.slug.clone())
        .map(|s| s.trim().trim_matches('/').to_string())
        .collect();
    let existing = taken(env, &given).await?;
    let mut seen: HashSet<String> = HashSet::new();
    let d = store::db(env)?;
    let mut stmts = vec![];
    let mut kv_deletes = vec![];
    let now = now_ms();

    for (i, row) in req.rows.into_iter().enumerate() {
        let url = row.url.trim().to_string();
        if let Err(e) = validate_url(&url) {
            result
                .errors
                .push(serde_json::json!({ "row": i, "slug": row.slug, "error": e }));
            continue;
        }
        let mut slug_v = match row
            .slug
            .as_deref()
            .map(|s| s.trim().trim_matches('/'))
            .filter(|s| !s.is_empty())
        {
            Some(sl) => {
                if let Err(e) = slug::validate(sl) {
                    result
                        .errors
                        .push(serde_json::json!({ "row": i, "slug": sl, "error": e }));
                    continue;
                }
                sl.to_string()
            }
            None => {
                store::unique_slug(
                    env,
                    Style::parse(&st.default_slug_style),
                    st.default_slug_length,
                )
                .await?
            }
        };
        let mut tags = row.tags.clone();
        tags.extend(req.tags.iter().cloned());
        let tags = normalize_tags(tags);
        let conflict = existing.get(&slug_v).copied();
        let dup = seen.contains(&slug_v);

        if conflict.is_some() || dup {
            match req.on_conflict.as_str() {
                "skip" => {
                    result.skipped += 1;
                    continue;
                }
                "overwrite" if !dup => {
                    let id = conflict.unwrap_or_default();
                    stmts.push(
                        d.prepare("UPDATE links SET url = ?2, title = COALESCE(?3, title), tags = ?4, notes = COALESCE(?5, notes), expires_at = ?6, disabled = ?7, updated_at = ?8 WHERE id = ?1")
                            .bind(&[
                                n(id),
                                s(&url),
                                store::os(&row.title),
                                s(&serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into())),
                                store::os(&row.notes),
                                store::on(row.expires_at),
                                n(row.disabled as i64),
                                n(now),
                            ])?,
                    );
                    kv_deletes.push(slug_v.clone());
                    seen.insert(slug_v);
                    result.updated += 1;
                    continue;
                }
                _ => {
                    // rename (or a duplicate inside this import under overwrite)
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
                        result.errors.push(serde_json::json!({ "row": i, "slug": base, "error": "no free renamed slug" }));
                        continue;
                    };
                    result
                        .renamed
                        .push(serde_json::json!({ "from": base, "to": newslug }));
                    slug_v = newslug;
                }
            }
        }

        let link = Link {
            slug: slug_v.clone(),
            url,
            title: row.title.clone().filter(|t| !t.trim().is_empty()),
            notes: row.notes.clone().filter(|t| !t.trim().is_empty()),
            tags,
            redirect_type: st.default_redirect,
            disabled: row.disabled,
            expires_at: row.expires_at,
            passthrough: true,
            source: row.source.clone().unwrap_or_else(|| "csv".into()),
            created_at: row.created_at.unwrap_or(now),
            updated_at: now,
            ..Default::default()
        };
        if let Err(e) = crate::model::validate_link(&link) {
            result
                .errors
                .push(serde_json::json!({ "row": i, "slug": slug_v, "error": e }));
            continue;
        }
        stmts.push(store::insert_statement(&d, &link)?);
        seen.insert(slug_v);
        result.created += 1;
    }

    for chunk in stmts.chunks(50) {
        d.batch(chunk.to_vec()).await?;
    }
    // Overwritten links: drop their KV record so the next click refills it from D1.
    let kvs = store::kv(env)?;
    for sl in kv_deletes {
        let _ = kvs.delete(&store::kv_key(&sl)).await;
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
