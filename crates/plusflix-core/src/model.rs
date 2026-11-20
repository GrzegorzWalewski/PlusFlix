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
    /// Long manifest format: `extra: [{ name, isRequired, options, optionsLimit }]`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub extra: Vec<ExtraDecl>,
    /// Short manifest format: `extraSupported: ["skip", "genre", ...]` (no options).
    #[serde(default, deserialize_with = "null_as_default")]
    pub extra_supported: Vec<String>,
    /// Short manifest format: `extraRequired: [...]` (no options).
    #[serde(default, deserialize_with = "null_as_default")]
    pub extra_required: Vec<String>,
}

impl CatalogDecl {
    /// All extras this catalog accepts: the long `extra` list plus the short
    /// `extraSupported` / `extraRequired` names, normalized into one list.
    pub fn extras(&self) -> std::borrow::Cow<'_, [ExtraDecl]> {
        use std::borrow::Cow;
        if self.extra_supported.is_empty() && self.extra_required.is_empty() {
            return Cow::Borrowed(&self.extra);
        }
        let mut merged = self.extra.clone();
        for (names, required) in [
            (self.extra_required.as_slice(), true),
            (self.extra_supported.as_slice(), false),
        ] {
            for name in names {
                match merged.iter_mut().find(|e| e.name == *name) {
                    Some(e) => {
                        if required {
                            e.is_required = true;
                        }
                    }
                    None => merged.push(ExtraDecl {
                        name: name.clone(),
                        is_required: required,
                        ..Default::default()
                    }),
                }
            }
        }
        Cow::Owned(merged)
    }

    /// Does the catalog accept this extra (in either manifest format)?
    pub fn supports_extra(&self, name: &str) -> bool {
        self.extras().iter().any(|e| e.name == name)
    }

    /// Does the catalog require an extra other than the given ones?
    /// (A catalog with other required extras can't be browsed by genre.)
    pub fn requires_other_than(&self, names: &[&str]) -> bool {
        self.extras()
            .iter()
            .any(|e| e.is_required && !names.contains(&e.name.as_str()))
    }

    /// The declared options of the `genre` extra (empty when the catalog takes no genre).
    /// Only the long `extra` format declares options; short-format catalogs return empty.
    pub fn genre_options(&self) -> &[String] {
        self.extra
            .iter()
            .find(|e| e.name == "genre")
            .map_or(&[], |e| &e.options)
    }

    /// Catalogs that need a mandatory argument (search, genre, ...) can't be shown as plain rows.
    pub fn browsable(&self) -> bool {
        !self.extras().iter().any(|e| e.is_required)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ExtraDecl {
    pub name: String,
    pub is_required: bool,
    /// The values this extra accepts, as declared by the addon (may be empty).
    #[serde(default, deserialize_with = "null_as_default")]
    pub options: Vec<String>,
    /// How many values may be selected at once; the protocol default is 1.
    pub options_limit: Option<u32>,
}

impl ExtraDecl {
    /// The effective options limit (protocol default 1).
    pub fn effective_options_limit(&self) -> u32 {
        self.options_limit.unwrap_or(1)
    }

    /// Options that are all 4-digit numbers are YEARS (e.g. Cinemeta's "New"
    /// catalog hides a year filter behind a required `genre` extra), not genre
    /// names: such catalogs are excluded from genre lists. Extension point for
    /// a future Year filter - not built yet.
    pub fn is_year_options(&self) -> bool {
        !self.options.is_empty()
            && self
                .options
                .iter()
                .all(|o| o.len() == 4 && o.bytes().all(|b| b.is_ascii_digit()))
    }
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

    #[test]
    fn short_extra_format_parses() {
        // Short format: extraSupported / extraRequired at the catalog level.
        let c: CatalogDecl = serde_json::from_str(
            r#"{"type":"movie","id":"top","extraSupported":["skip","genre"],"extraRequired":null}"#,
        )
        .unwrap();
        assert!(c.supports_extra("skip") && c.supports_extra("genre"));
        assert!(!c.requires_other_than(&["genre", "skip"]));
        assert!(c.browsable()); // nothing is required
        assert!(c.genre_options().is_empty()); // short format declares no options
    }

    #[test]
    fn short_extra_required_marks_catalog_unbrowsable() {
        let c: CatalogDecl = serde_json::from_str(
            r#"{"type":"movie","id":"year","extraRequired":["genre"],"extraSupported":["skip"]}"#,
        )
        .unwrap();
        assert!(!c.browsable());
        assert!(c.supports_extra("skip") && c.supports_extra("genre"));
        assert!(!c.requires_other_than(&["genre", "skip"]));
    }

    #[test]
    fn long_extra_format_parses_with_options() {
        let c: CatalogDecl = serde_json::from_str(
            r#"{"type":"movie","id":"top","extra":[
                {"name":"genre","isRequired":false,"options":["Action","Sci-Fi"],"optionsLimit":1},
                {"name":"skip","isRequired":false,"options":null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            c.genre_options(),
            &["Action".to_string(), "Sci-Fi".to_string()]
        );
        assert!(c.supports_extra("skip"));
        assert!(!c.requires_other_than(&["genre", "skip"]));
        // A required extra other than genre/skip makes genre browsing impossible.
        let c2: CatalogDecl = serde_json::from_str(
            r#"{"type":"movie","id":"kinds","extra":[{"name":"genre","options":["Action"]},{"name":"kind","isRequired":true}]}"#,
        )
        .unwrap();
        assert!(c2.requires_other_than(&["genre", "skip"]));
    }

    #[test]
    fn year_options_detected() {
        let year: ExtraDecl = serde_json::from_str(
            r#"{"name":"genre","isRequired":true,"options":["2023","2024","2019"]}"#,
        )
        .unwrap();
        assert!(year.is_year_options());
        let genre: ExtraDecl =
            serde_json::from_str(r#"{"name":"genre","options":["Action","Sci-Fi"]}"#).unwrap();
        assert!(!genre.is_year_options());
        assert!(!ExtraDecl::default().is_year_options());
        assert_eq!(genre.effective_options_limit(), 1);
        let two: ExtraDecl =
            serde_json::from_str(r#"{"name":"genre","options":["a"],"optionsLimit":5}"#).unwrap();
        assert_eq!(two.effective_options_limit(), 5);
    }
}
