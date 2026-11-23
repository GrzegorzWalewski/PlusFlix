//! Data types of the Stremio addon protocol (only the fields PlusFlix uses).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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

/// A field that should be a single string: any other JSON shape (number,
/// object, ...) degrades to `None` instead of failing the item.
pub(crate) fn string_or_none<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(serde_json::Value::deserialize(d)?.as_str().map(str::to_string))
}

/// A field that should be a country -> rating map: keeps the string values,
/// drops the rest, and degrades to `None` for any non-object JSON shape.
pub(crate) fn string_map_or_none<'de, D>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(map) => {
            let kept = map
                .into_iter()
                .filter_map(|(country, rating)| {
                    rating.as_str().map(|r| (country, r.to_string()))
                })
                .collect::<BTreeMap<_, _>>();
            (!kept.is_empty()).then_some(kept)
        }
        _ => None,
    })
}

/// `app_extras` carries arbitrary addon data (AIOMetadata adds cast lists,
/// release dates, ...), so the block is kept only when it is an object:
/// any other shape (null, string, array, ...) degrades to `None` and can
/// never fail the surrounding meta item. (An array must NOT fall through to
/// serde's positional struct-from-seq path.)
pub(crate) fn app_extras_or_none<'de, D>(d: D) -> Result<Option<AppExtras>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(d)? {
        v @ serde_json::Value::Object(_) => serde_json::from_value(v).ok(),
        _ => None,
    })
}

/// The `app_extras` block some addons attach to `meta_preview`/`meta` items.
/// Only the two age-rating keys are modeled (see `crate::rating`): unknown
/// keys are dropped by serde, and every odd value shape degrades to
/// `None`/empty rather than failing the item. AIOMetadata-style items carry
/// their ratings here and keep `genres` to real genres.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AppExtras {
    /// One age rating (AIOMetadata: the US value; may be null).
    #[serde(default, deserialize_with = "string_or_none")]
    pub certification: Option<String>,
    /// Country -> rating map (AIOMetadata: may be null / missing entirely).
    #[serde(default, deserialize_with = "string_map_or_none")]
    pub certifications: Option<BTreeMap<String, String>>,
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
    /// Genre list, carrying age-rating tokens ("PG", "TV-MA", "16", ...) on
    /// Cinemeta-style catalogs; see `crate::rating`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub genres: Vec<String>,
    /// `app_extras` block: addon-specific data. AIOMetadata-style addons
    /// publish their age ratings here (`certification` +
    /// `certifications`) and keep `genres` to real genres; both sources
    /// are merged in `crate::rating`. The block arrives under two wire
    /// spellings: AIOMetadata sends it as snake_case `app_extras` (upstream
    /// issue #704) while other addons use camelCase `appExtras`; the
    /// field-level `rename` below overrides the struct's `rename_all` for
    /// this one field, and `alias` keeps the other spelling deserializing.
    #[serde(
        rename = "app_extras",
        alias = "appExtras",
        default,
        deserialize_with = "app_extras_or_none"
    )]
    pub app_extras: Option<AppExtras>,
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
    /// `app_extras` block: addon-specific data. AIOMetadata-style addons
    /// publish their age ratings here (`certification` +
    /// `certifications`) and keep `genres` to real genres; both sources
    /// are merged in `crate::rating`. The block arrives under two wire
    /// spellings: AIOMetadata sends it as snake_case `app_extras` (upstream
    /// issue #704) while other addons use camelCase `appExtras`; the
    /// field-level `rename` below overrides the struct's `rename_all` for
    /// this one field, and `alias` keeps the other spelling deserializing.
    #[serde(
        rename = "app_extras",
        alias = "appExtras",
        default,
        deserialize_with = "app_extras_or_none"
    )]
    pub app_extras: Option<AppExtras>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub videos: Vec<Video>,
    /// Trailer list of the detail response (`trailerStreams[].ytId`): the
    /// preferred shape. Optional — many metas carry no trailer at all.
    #[serde(default, deserialize_with = "null_as_default")]
    pub trailer_streams: Vec<TrailerStream>,
    /// Legacy trailer list: `trailers[].source` holds the YouTube id.
    #[serde(default, deserialize_with = "null_as_default")]
    pub trailers: Vec<Trailer>,
}

impl Meta {
    /// The first trailer as a YouTube watch URL, or `None` when the meta
    /// carries no trailer data: `trailerStreams[].ytId` first, falling back
    /// to the legacy `trailers[].source`. Trailers stay on the meta side
    /// (opened in the system browser) and never enter the stream pool.
    pub fn trailer_url(&self) -> Option<String> {
        let yt_id = self
            .trailer_streams
            .iter()
            .filter_map(|t| t.yt_id.as_deref())
            .find(|id| !id.is_empty())
            .or_else(|| {
                self.trailers
                    .iter()
                    .filter_map(|t| t.source.as_deref())
                    .find(|id| !id.is_empty())
            })?;
        Some(format!("https://www.youtube.com/watch?v={yt_id}"))
    }
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

/// Trailer entry of the `trailerStreams` list of a meta detail response:
/// `ytId` is the YouTube video id. Optional — many metas carry no trailer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TrailerStream {
    pub title: Option<String>,
    pub yt_id: Option<String>,
}

/// Legacy trailer entry of the `trailers` list: `source` holds the YouTube
/// video id. The `type` field is renamed — `type` is a Rust keyword.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Trailer {
    pub source: Option<String>,
    #[serde(rename = "type")]
    pub trailer_type: Option<String>,
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
    // The end-to-end child-filtering proof borrows the policy from `crate::rating`.
    use crate::rating::{filter_previews, RatingPolicy};

    #[test]
    fn app_extras_shapes_never_fail_meta_deserialization() {
        // `app_extras` carries arbitrary addon data, so every odd shape must
        // degrade to `None`/empty instead of failing the surrounding item.
        for payload in [
            r#"{"id":"tt1","type":"movie","name":"M"}"#, // absent
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":null}"#,
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":"PG"}"#,
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":["PG"]}"#,
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":16}"#,
        ] {
            let preview: MetaPreview = serde_json::from_str(payload).unwrap();
            assert!(preview.app_extras.is_none(), "payload {payload}");
            let meta: Meta = serde_json::from_str(payload).unwrap();
            assert!(meta.app_extras.is_none(), "payload {payload}");
        }

        // Odd values inside an object-shaped block degrade field by field.
        let preview: MetaPreview = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":{
                "certification":16,"certifications":null,
                "cast":["Who"],"releaseDates":{"US":"2024"}}}"#,
        )
        .unwrap();
        let extras = preview.app_extras.expect("object extras are kept");
        assert_eq!(extras.certification, None); // number, not a string

        let preview: MetaPreview = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":{
                "certification":{"US":"R"},"certifications":["R","PG"],
                "directors":["Who"]}}"#,
        )
        .unwrap();
        let extras = preview.app_extras.expect("object extras are kept");
        assert_eq!(extras.certification, None); // object, not a string
        assert!(extras.certifications.is_none()); // array, not a map

        let meta: Meta = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":{
                "certifications":{"PL":16}}}"#,
        )
        .unwrap();
        let extras = meta.app_extras.expect("object extras are kept");
        assert!(extras.certifications.is_none()); // numeric value dropped

        // Real AIOMetadata shapes keep working.
        let meta: Meta = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M","genres":["Drama"],
                "appExtras":{"certification":"R",
                "certifications":{"US":"R","PL":"16"},"seasonPosters":{}}}"#,
        )
        .unwrap();
        let extras = meta.app_extras.unwrap();
        assert_eq!(extras.certification.as_deref(), Some("R"));
        assert_eq!(
            extras
                .certifications
                .as_ref()
                .and_then(|c| c.get("PL").map(String::as_str)),
            Some("16")
        );
    }

    #[test]
    fn app_extras_snake_case_key_parses() {
        // AIOMetadata (aiometadata.elfhosted.com) sends the block as snake_case
        // `app_extras` on both catalog previews and meta details (upstream
        // issue #704), so both structs must pick that spelling up.
        let preview: MetaPreview = serde_json::from_str(
            r#"{"type":"series","id":"tt123","name":"Test","genres":[],
                "app_extras":{"certification":"TV-MA"}}"#,
        )
        .unwrap();
        let extras = preview.app_extras.expect("preview extras are kept");
        assert_eq!(extras.certification.as_deref(), Some("TV-MA"));

        // `Meta` is deserialized by callers out of a `{"meta": {...}}` wrapper;
        // the struct's container-level `default` lets every other field be
        // omitted from the payload.
        let meta: Meta = serde_json::from_str(r#"{"app_extras":{"certification":"TV-MA"}}"#)
            .unwrap();
        assert_eq!(meta.id, ""); // omitted fields fall back to Default
        let extras = meta.app_extras.expect("meta extras are kept");
        assert_eq!(extras.certification.as_deref(), Some("TV-MA"));
    }

    #[test]
    fn app_extras_camel_case_key_still_parses() {
        // Regression guard: the legacy camelCase spelling (every pre-existing
        // fixture uses it) keeps deserializing through the `alias`.
        let preview: MetaPreview = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M","appExtras":{"certification":"PG"}}"#,
        )
        .unwrap();
        assert_eq!(
            preview.app_extras.and_then(|e| e.certification),
            Some("PG".to_string())
        );

        let meta: Meta = serde_json::from_str(
            r#"{"id":"tt2","type":"series","name":"S","appExtras":{"certification":"PG"}}"#,
        )
        .unwrap();
        assert_eq!(
            meta.app_extras.and_then(|e| e.certification),
            Some("PG".to_string())
        );
    }

    #[test]
    fn snake_case_ratings_reach_the_child_filter() {
        // End-to-end over the AIOMetadata wire shape. Before the field-level
        // rename, snake_case extras were silently dropped, every item counted
        // as unrated (= age 18) and child profiles saw nothing at all. A child
        // capped at 14 must keep the TV-14 item and drop the TV-MA one.
        fn wire_preview(id: &str, certification: &str) -> MetaPreview {
            serde_json::from_str(&format!(
                r#"{{"type":"series","id":"{id}","name":"{id}","genres":[],
                    "app_extras":{{"certification":"{certification}"}}}}"#
            ))
            .unwrap()
        }
        let items = vec![wire_preview("tt14", "TV-14"), wire_preview("ttma", "TV-MA")];
        let kept: Vec<String> = filter_previews(&RatingPolicy::child(14), items)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(kept, ["tt14"]);
    }

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

    #[test]
    fn trailer_url_prefers_trailer_streams() {
        // Real Cinemeta meta detail shape (both optional fields present).
        let m: Meta = serde_json::from_str(
            r#"{"id":"tt0111161","type":"movie","name":"The Shawshank Redemption",
                "trailers":[{"source":"PLl99DlL6b4","type":"Trailer"},{"source":"xyXX8LXiNJ4","type":"Trailer"}],
                "trailerStreams":[{"title":"The Shawshank Redemption","ytId":"PLl99DlL6b4"},{"title":"The Shawshank Redemption","ytId":"xyXX8LXiNJ4"}]}"#,
        )
        .unwrap();
        assert_eq!(
            m.trailer_url().as_deref(),
            Some("https://www.youtube.com/watch?v=PLl99DlL6b4")
        );
    }

    #[test]
    fn trailer_url_without_trailer_data_is_none() {
        // Absent, null, empty and empty-id shapes all parse and yield None.
        for payload in [
            r#"{"id":"tt1","type":"movie","name":"M"}"#,
            r#"{"id":"tt1","type":"movie","name":"M","trailers":null,"trailerStreams":null}"#,
            r#"{"id":"tt1","type":"movie","name":"M","trailers":[],"trailerStreams":[]}"#,
            r#"{"id":"tt1","type":"movie","name":"M",
                "trailers":[{"source":"","type":"Trailer"}],
                "trailerStreams":[{"title":"T","ytId":""}]}"#,
        ] {
            let m: Meta = serde_json::from_str(payload).unwrap();
            assert_eq!(m.trailer_url(), None, "payload {payload}");
        }
    }

    #[test]
    fn trailer_url_falls_back_to_legacy_trailers() {
        let m: Meta = serde_json::from_str(
            r#"{"id":"tt1","type":"movie","name":"M",
                "trailers":[{"source":"xyXX8LXiNJ4","type":"Trailer"}]}"#,
        )
        .unwrap();
        assert_eq!(
            m.trailer_url().as_deref(),
            Some("https://www.youtube.com/watch?v=xyXX8LXiNJ4")
        );
    }
}
