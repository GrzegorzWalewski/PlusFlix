//! Talking to Stremio addons over their HTTP protocol.

use crate::error::{Error, Result};
use crate::model::*;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// An installed addon (an entry of the account's addon collection).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Addon {
    pub transport_url: String,
    pub manifest: Manifest,
}

impl Addon {
    pub fn name(&self) -> &str {
        if self.manifest.name.is_empty() {
            &self.manifest.id
        } else {
            &self.manifest.name
        }
    }

    /// Base URL = transport URL without the trailing `/manifest.json`.
    pub fn base_url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.transport_url)
            .map_err(|e| Error::BadUrl(format!("{}: {e}", self.transport_url)))?;
        if url.path().ends_with("/manifest.json") {
            url.path_segments_mut()
                .map_err(|_| Error::BadUrl(self.transport_url.clone()))?
                .pop();
        }
        Ok(url)
    }

    /// Does this addon provide `resource` for the given content type and id?
    pub fn supports(&self, resource: &str, kind: &str, id: &str) -> bool {
        let m = &self.manifest;
        m.resources.iter().any(|r| {
            let (name, types, prefixes) = match r {
                ResourceDecl::Short(n) => (n.as_str(), &m.types, &m.id_prefixes),
                ResourceDecl::Full {
                    name,
                    types,
                    id_prefixes,
                } => (
                    name.as_str(),
                    if types.is_empty() { &m.types } else { types },
                    if id_prefixes.is_empty() {
                        &m.id_prefixes
                    } else {
                        id_prefixes
                    },
                ),
            };
            name == resource
                && types.iter().any(|t| t == kind)
                && (prefixes.is_empty() || prefixes.iter().any(|p| id.starts_with(p.as_str())))
        })
    }
}

#[derive(Clone)]
pub struct AddonClient {
    http: reqwest::Client,
}

impl AddonClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Reel/", env!("CARGO_PKG_VERSION")))
            // Self-hosted addons can be slow; Stremio itself waits patiently too.
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { http })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    fn resource_url(
        addon: &Addon,
        resource: &str,
        kind: &str,
        id: &str,
        extra: Option<&str>,
    ) -> Result<Url> {
        let mut url = addon.base_url()?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| Error::BadUrl(addon.transport_url.clone()))?;
            segs.push(resource).push(kind);
            match extra {
                Some(extra) => {
                    segs.push(id).push(&format!("{extra}.json"));
                }
                None => {
                    segs.push(&format!("{id}.json"));
                }
            }
        }
        Ok(url)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json::<T>()
            .await?)
    }

    pub async fn catalog(
        &self,
        addon: &Addon,
        kind: &str,
        id: &str,
        extra: Option<&str>,
    ) -> Result<Vec<MetaPreview>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct R {
            metas: Vec<MetaPreview>,
        }
        let url = Self::resource_url(addon, "catalog", kind, id, extra)?;
        Ok(self.get::<R>(url).await?.metas)
    }

    pub async fn meta(&self, addon: &Addon, kind: &str, id: &str) -> Result<Meta> {
        #[derive(Deserialize)]
        struct R {
            meta: Meta,
        }
        let url = Self::resource_url(addon, "meta", kind, id, None)?;
        Ok(self.get::<R>(url).await?.meta)
    }

    pub async fn streams(&self, addon: &Addon, kind: &str, video_id: &str) -> Result<Vec<Stream>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct R {
            streams: Vec<Stream>,
        }
        let url = Self::resource_url(addon, "stream", kind, video_id, None)?;
        Ok(self.get::<R>(url).await?.streams)
    }

    pub async fn subtitles(
        &self,
        addon: &Addon,
        kind: &str,
        video_id: &str,
    ) -> Result<Vec<Subtitle>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct R {
            subtitles: Vec<Subtitle>,
        }
        let url = Self::resource_url(addon, "subtitles", kind, video_id, None)?;
        Ok(self.get::<R>(url).await?.subtitles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addon(url: &str, manifest: serde_json::Value) -> Addon {
        serde_json::from_value(serde_json::json!({ "transportUrl": url, "manifest": manifest }))
            .unwrap()
    }

    #[test]
    fn base_url_keeps_config_segment() {
        let a = addon(
            "https://torrentio.example/realdebrid=KEY/manifest.json",
            serde_json::json!({}),
        );
        assert_eq!(
            a.base_url().unwrap().as_str(),
            "https://torrentio.example/realdebrid=KEY"
        );
    }

    #[test]
    fn resource_urls() {
        let a = addon("https://a.example/manifest.json", serde_json::json!({}));
        let u = AddonClient::resource_url(&a, "stream", "series", "tt1:1:2", None).unwrap();
        assert_eq!(u.as_str(), "https://a.example/stream/series/tt1:1:2.json");
        let u = AddonClient::resource_url(&a, "catalog", "movie", "top", Some("skip=20")).unwrap();
        assert_eq!(
            u.as_str(),
            "https://a.example/catalog/movie/top/skip=20.json"
        );
        let u = AddonClient::resource_url(&a, "catalog", "movie", "top", Some("search=star wars"))
            .unwrap();
        assert_eq!(
            u.as_str(),
            "https://a.example/catalog/movie/top/search=star%20wars.json"
        );
    }

    #[test]
    fn supports_short_and_full_resources() {
        let a = addon(
            "https://a.example/manifest.json",
            serde_json::json!({
                "id": "x", "types": ["movie", "series"], "idPrefixes": ["tt"],
                "resources": ["stream", {"name": "subtitles", "types": ["movie"], "idPrefixes": ["tt"]}]
            }),
        );
        assert!(a.supports("stream", "series", "tt123:1:1"));
        assert!(!a.supports("stream", "series", "kitsu:1"));
        assert!(a.supports("subtitles", "movie", "tt1"));
        assert!(!a.supports("subtitles", "series", "tt1:1:1"));
        assert!(!a.supports("meta", "movie", "tt1"));
    }
}
