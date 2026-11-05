//! Link model, validation and the compact KV record used by the redirect path.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Utm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl Utm {
    pub fn is_empty(&self) -> bool {
        self.pairs().is_empty()
    }

    pub fn pairs(&self) -> Vec<(&'static str, &str)> {
        [
            ("utm_source", &self.source),
            ("utm_medium", &self.medium),
            ("utm_campaign", &self.campaign),
            ("utm_term", &self.term),
            ("utm_content", &self.content),
        ]
        .into_iter()
        .filter_map(|(k, v)| {
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| (k, v))
        })
        .collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Device {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
}

impl Device {
    pub fn is_empty(&self) -> bool {
        self.ios.is_none() && self.android.is_none() && self.desktop.is_none()
    }
}

/// A link as returned by the admin API.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Link {
    pub id: i64,
    pub slug: String,
    pub url: String,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub tags: Vec<String>,
    pub redirect_type: u16,
    pub disabled: bool,
    pub expires_at: Option<i64>,
    pub max_clicks: Option<i64>,
    pub fallback_url: Option<String>,
    pub has_password: bool,
    #[serde(skip)]
    pub password_hash: Option<String>,
    pub passthrough: bool,
    pub utm: Utm,
    pub geo: BTreeMap<String, String>,
    pub device: Device,
    pub source: String,
    pub clicks: i64,
    pub last_click_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Raw D1 row.
#[derive(Deserialize, Debug)]
pub struct LinkRow {
    pub id: i64,
    pub slug: String,
    pub url: String,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub tags: Option<String>,
    pub redirect_type: i64,
    pub disabled: i64,
    pub expires_at: Option<i64>,
    pub max_clicks: Option<i64>,
    pub fallback_url: Option<String>,
    pub password_hash: Option<String>,
    pub passthrough: i64,
    pub utm: Option<String>,
    pub geo: Option<String>,
    pub device: Option<String>,
    pub source: Option<String>,
    pub clicks: i64,
    pub last_click_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn parse_json<T: for<'a> Deserialize<'a> + Default>(s: &Option<String>) -> T {
    s.as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

impl From<LinkRow> for Link {
    fn from(r: LinkRow) -> Self {
        Link {
            id: r.id,
            slug: r.slug,
            url: r.url,
            title: r.title,
            notes: r.notes,
            tags: parse_json(&r.tags),
            redirect_type: r.redirect_type as u16,
            disabled: r.disabled != 0,
            expires_at: r.expires_at,
            max_clicks: r.max_clicks,
            fallback_url: r.fallback_url,
            has_password: r.password_hash.is_some(),
            password_hash: r.password_hash,
            passthrough: r.passthrough != 0,
            utm: parse_json(&r.utm),
            geo: parse_json(&r.geo),
            device: parse_json(&r.device),
            source: r.source.unwrap_or_else(|| "manual".into()),
            clicks: r.clicks,
            last_click_at: r.last_click_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
            aliases: vec![],
        }
    }
}

/// Deserialises a field that distinguishes "absent" (None) from "null" (Some(None)).
fn double<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Create/update payload. Every field is optional; `null` clears nullable fields.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct LinkInput {
    pub slug: Option<String>,
    pub slug_style: Option<String>,
    pub url: Option<String>,
    #[serde(default, deserialize_with = "double")]
    pub title: Option<Option<String>>,
    #[serde(default, deserialize_with = "double")]
    pub notes: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
    pub redirect_type: Option<u16>,
    pub disabled: Option<bool>,
    #[serde(default, deserialize_with = "double")]
    pub expires_at: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double")]
    pub max_clicks: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double")]
    pub fallback_url: Option<Option<String>>,
    /// Plain-text password; empty string or null removes it.
    #[serde(default, deserialize_with = "double")]
    pub password: Option<Option<String>>,
    pub passthrough: Option<bool>,
    pub utm: Option<Utm>,
    pub geo: Option<BTreeMap<String, String>>,
    pub device: Option<Device>,
    pub source: Option<String>,
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn validate_url(u: &str) -> Result<(), String> {
    if u.len() > 4096 {
        return Err("URL is longer than 4096 characters".into());
    }
    match url::Url::parse(u) {
        Ok(p) if (p.scheme() == "http" || p.scheme() == "https") && p.host_str().is_some() => {
            Ok(())
        }
        Ok(_) => Err(format!("only http(s) URLs are allowed: {u}")),
        Err(e) => Err(format!("invalid URL {u}: {e}")),
    }
}

pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty() && t.len() <= 64)
        .collect();
    out.sort();
    out.dedup();
    out.truncate(32);
    out
}

impl LinkInput {
    /// Applies this input on top of `link` (all fields except slug and password,
    /// which need async/database work and are handled by the store).
    pub fn apply(&self, link: &mut Link) -> Result<(), String> {
        if let Some(u) = &self.url {
            link.url = u.trim().to_string();
        }
        if let Some(t) = &self.title {
            link.title = clean(t.clone());
        }
        if let Some(n) = &self.notes {
            link.notes = clean(n.clone());
        }
        if let Some(t) = &self.tags {
            link.tags = normalize_tags(t.clone());
        }
        if let Some(r) = self.redirect_type {
            link.redirect_type = r;
        }
        if let Some(d) = self.disabled {
            link.disabled = d;
        }
        if let Some(e) = self.expires_at {
            link.expires_at = e;
        }
        if let Some(m) = self.max_clicks {
            link.max_clicks = m.filter(|m| *m > 0);
        }
        if let Some(f) = &self.fallback_url {
            link.fallback_url = clean(f.clone());
        }
        if let Some(p) = self.passthrough {
            link.passthrough = p;
        }
        if let Some(u) = &self.utm {
            link.utm = Utm {
                source: clean(u.source.clone()),
                medium: clean(u.medium.clone()),
                campaign: clean(u.campaign.clone()),
                term: clean(u.term.clone()),
                content: clean(u.content.clone()),
            };
        }
        if let Some(g) = &self.geo {
            link.geo = g
                .iter()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| (k.trim().to_uppercase(), v.trim().to_string()))
                .collect();
        }
        if let Some(d) = &self.device {
            link.device = Device {
                ios: clean(d.ios.clone()),
                android: clean(d.android.clone()),
                desktop: clean(d.desktop.clone()),
            };
        }
        if let Some(s) = &self.source {
            link.source = s.clone();
        }
        validate_link(link)
    }
}

pub fn validate_link(link: &Link) -> Result<(), String> {
    validate_url(&link.url)?;
    if ![301, 302, 307, 308].contains(&link.redirect_type) {
        return Err("redirect_type must be 301, 302, 307 or 308".into());
    }
    if let Some(f) = &link.fallback_url {
        validate_url(f)?;
    }
    for (cc, u) in &link.geo {
        if cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_uppercase()) {
            return Err(format!("geo key must be a 2-letter country code: {cc}"));
        }
        validate_url(u)?;
    }
    for u in [&link.device.ios, &link.device.android, &link.device.desktop]
        .into_iter()
        .flatten()
    {
        validate_url(u)?;
    }
    if link.title.as_ref().is_some_and(|t| t.len() > 512) {
        return Err("title is longer than 512 characters".into());
    }
    if link.notes.as_ref().is_some_and(|t| t.len() > 4096) {
        return Err("notes are longer than 4096 characters".into());
    }
    Ok(())
}

/// Merges UTM parameters into a URL (overriding existing utm_* keys).
pub fn apply_utm(u: &str, utm: &Utm) -> String {
    if utm.is_empty() {
        return u.to_string();
    }
    let Ok(mut parsed) = url::Url::parse(u) else {
        return u.to_string();
    };
    let keys: Vec<&str> = utm.pairs().iter().map(|(k, _)| *k).collect();
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !keys.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut q = parsed.query_pairs_mut();
        q.clear();
        for (k, v) in &kept {
            q.append_pair(k, v);
        }
        for (k, v) in utm.pairs() {
            q.append_pair(k, v);
        }
    }
    parsed.to_string()
}

/// Appends the visitor's query string to a destination, skipping our own `qr` marker.
pub fn passthrough_query(dest: &str, visitor_query: Option<&str>) -> String {
    let Some(q) = visitor_query.filter(|q| !q.is_empty()) else {
        return dest.to_string();
    };
    let extra: Vec<&str> = q
        .split('&')
        .filter(|p| !p.is_empty() && *p != "qr" && !p.starts_with("qr="))
        .collect();
    if extra.is_empty() {
        return dest.to_string();
    }
    let (base, frag) = match dest.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (dest, None),
    };
    let sep = if base.contains('?') {
        if base.ends_with('?') || base.ends_with('&') {
            ""
        } else {
            "&"
        }
    } else {
        "?"
    };
    let mut out = format!("{base}{sep}{}", extra.join("&"));
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// The compact record stored in KV under `s:<slug>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct KvRecord {
    pub id: i64,
    pub url: String,
    #[serde(rename = "type")]
    pub redirect_type: u16,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_clicks: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub geo: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Device::is_empty")]
    pub device: Device,
    #[serde(default)]
    pub passthrough: bool,
}

impl KvRecord {
    pub fn from_link(l: &Link) -> Self {
        KvRecord {
            id: l.id,
            url: apply_utm(&l.url, &l.utm),
            redirect_type: l.redirect_type,
            disabled: l.disabled,
            expires_at: l.expires_at,
            max_clicks: l.max_clicks,
            fallback: l.fallback_url.clone(),
            pw: l.password_hash.clone(),
            geo: l
                .geo
                .iter()
                .map(|(k, v)| (k.clone(), apply_utm(v, &l.utm)))
                .collect(),
            device: Device {
                ios: l.device.ios.as_deref().map(|u| apply_utm(u, &l.utm)),
                android: l.device.android.as_deref().map(|u| apply_utm(u, &l.utm)),
                desktop: l.device.desktop.as_deref().map(|u| apply_utm(u, &l.utm)),
            },
            passthrough: l.passthrough,
        }
    }

    /// Picks the destination: device rule, then geo rule, then the default.
    pub fn destination(&self, os: &str, device: &str, country: Option<&str>) -> &str {
        let by_device = match os {
            "iOS" => self.device.ios.as_deref(),
            "Android" => self.device.android.as_deref(),
            _ if device == "desktop" => self.device.desktop.as_deref(),
            _ => None,
        };
        by_device
            .or_else(|| country.and_then(|c| self.geo.get(c)).map(String::as_str))
            .unwrap_or(&self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utm_merges() {
        let utm = Utm {
            source: Some("x".into()),
            campaign: Some("launch day".into()),
            ..Default::default()
        };
        assert_eq!(
            apply_utm("https://e.com/p?a=1&utm_source=old", &utm),
            "https://e.com/p?a=1&utm_source=x&utm_campaign=launch+day"
        );
        assert_eq!(
            apply_utm("https://e.com/", &Utm::default()),
            "https://e.com/"
        );
    }

    #[test]
    fn passthrough() {
        assert_eq!(
            passthrough_query("https://e.com/p", Some("a=1&qr=1")),
            "https://e.com/p?a=1"
        );
        assert_eq!(
            passthrough_query("https://e.com/p?x=2#h", Some("a=1")),
            "https://e.com/p?x=2&a=1#h"
        );
        assert_eq!(
            passthrough_query("https://e.com/p", Some("qr")),
            "https://e.com/p"
        );
        assert_eq!(
            passthrough_query("https://e.com/p", None),
            "https://e.com/p"
        );
    }

    #[test]
    fn destination_rules() {
        let mut r = KvRecord {
            id: 1,
            url: "https://default".into(),
            redirect_type: 302,
            disabled: false,
            expires_at: None,
            max_clicks: None,
            fallback: None,
            pw: None,
            geo: BTreeMap::from([("RO".into(), "https://ro".into())]),
            device: Device {
                ios: Some("https://ios".into()),
                ..Default::default()
            },
            passthrough: false,
        };
        assert_eq!(r.destination("iOS", "mobile", Some("RO")), "https://ios");
        assert_eq!(r.destination("Android", "mobile", Some("RO")), "https://ro");
        assert_eq!(
            r.destination("Windows", "desktop", Some("US")),
            "https://default"
        );
        r.device.desktop = Some("https://desk".into());
        assert_eq!(
            r.destination("Windows", "desktop", Some("RO")),
            "https://desk"
        );
    }

    #[test]
    fn validation() {
        let mut l = Link {
            url: "https://ok.com".into(),
            redirect_type: 302,
            ..Default::default()
        };
        assert!(validate_link(&l).is_ok());
        l.url = "javascript:alert(1)".into();
        assert!(validate_link(&l).is_err());
        l.url = "https://ok.com".into();
        l.redirect_type = 200;
        assert!(validate_link(&l).is_err());
        l.redirect_type = 301;
        l.geo.insert("usa".into(), "https://x.com".into());
        assert!(validate_link(&l).is_err());
    }

    #[test]
    fn patch_null_clears() {
        let input: LinkInput =
            serde_json::from_str(r#"{"title": null, "tags": [" A ", "b", "a"]}"#).unwrap();
        let mut l = Link {
            url: "https://ok.com".into(),
            redirect_type: 302,
            title: Some("t".into()),
            ..Default::default()
        };
        input.apply(&mut l).unwrap();
        assert_eq!(l.title, None);
        assert_eq!(l.tags, vec!["a", "b"]);
        let input: LinkInput = serde_json::from_str(r#"{}"#).unwrap();
        l.title = Some("keep".into());
        input.apply(&mut l).unwrap();
        assert_eq!(l.title.as_deref(), Some("keep"));
    }

    #[test]
    fn kv_record_roundtrip_is_compact() {
        let l = Link {
            id: 7,
            url: "https://ok.com".into(),
            redirect_type: 301,
            ..Default::default()
        };
        let s = serde_json::to_string(&KvRecord::from_link(&l)).unwrap();
        assert_eq!(
            s,
            r#"{"id":7,"url":"https://ok.com","type":301,"passthrough":false}"#
        );
    }
}
