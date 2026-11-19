//! Data types of the Stremio addon protocol (only the fields PlusFlix uses).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Treats an explicit JSON `null` like a missing field. Real addon data
/// (and the account's addon collection) contains nulls where the protocol
/// documents lists; `#[serde(default)]` alone only covers absent fields.
pub(crate) fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MetaPreview {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub poster: Option<String>,
    pub release_info: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Meta {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub description: Option<String>,
    pub release_info: Option<String>,
    pub runtime: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub genres: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub videos: Vec<Video>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Video {
    /// For series this is "tt1234567:season:episode" and is what stream addons expect.
    pub id: String,
    #[serde(alias = "name")]
    pub title: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub released: Option<String>,
    pub thumbnail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Stream {
    pub url: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub yt_id: Option<String>,
    pub info_hash: Option<String>,
    pub file_idx: Option<u32>,
    pub external_url: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub behavior_hints: BehaviorHints,
    #[serde(default, deserialize_with = "null_as_default")]
    pub subtitles: Vec<Subtitle>,
}

impl Stream {
    /// All free text an addon gave us about this stream, for heuristics.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for part in [
            &self.name,
            &self.title,
            &self.description,
            &self.behavior_hints.filename,
        ]
        .into_iter()
        .flatten()
        {
            s.push_str(part);
            s.push('\n');
        }
        s
    }

    /// PlusFlix plays direct HTTP(S) URLs only (debrid / direct addons).
    pub fn http_url(&self) -> Option<&str> {
        self.url
            .as_deref()
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct BehaviorHints {
    pub not_web_ready: bool,
    pub binge_group: Option<String>,
    pub video_size: Option<u64>,
    pub filename: Option<String>,
    pub proxy_headers: Option<ProxyHeaders>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProxyHeaders {
    pub request: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Subtitle {
    pub id: Option<String>,
    pub url: String,
    pub lang: String,
}

// ---- manifest ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub types: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub id_prefixes: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub resources: Vec<ResourceDecl>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub catalogs: Vec<CatalogDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResourceDecl {
    Short(String),
    Full {
        name: String,
        #[serde(default, deserialize_with = "null_as_default")]
        types: Vec<String>,
        #[serde(default, rename = "idPrefixes", deserialize_with = "null_as_default")]
        id_prefixes: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CatalogDecl {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub extra: Vec<ExtraDecl>,
}

impl CatalogDecl {
    /// Catalogs that need a mandatory argument (search, genre, ...) can't be shown as plain rows.
    pub fn browsable(&self) -> bool {
        !self.extra.iter().any(|e| e.is_required)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtraDecl {
    pub name: String,
    pub is_required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_lists_parse_as_empty() {
        let m: Manifest = serde_json::from_str(
            r#"{"id":"a","name":"A","types":null,"idPrefixes":null,"resources":null,"catalogs":null}"#,
        )
        .unwrap();
        assert!(m.types.is_empty() && m.resources.is_empty() && m.catalogs.is_empty());

        let s: Stream = serde_json::from_str(
            r#"{"url":"https://x/v","name":"n","behaviorHints":null,"subtitles":null}"#,
        )
        .unwrap();
        assert!(s.behavior_hints.filename.is_none() && s.subtitles.is_empty());

        let c: CatalogDecl =
            serde_json::from_str(r#"{"type":"movie","id":"top","extra":null}"#).unwrap();
        assert!(c.browsable());
    }
}
