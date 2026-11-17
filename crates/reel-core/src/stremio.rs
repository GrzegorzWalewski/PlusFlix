//! Stremio account API (https://api.strem.io): login + addon collection.
//!
//! Endpoint names and bodies follow the community-documented Stremio API
//! (`login`, `addonCollectionGet`, `logout`). They have not been exercised
//! against a live account in this repo's tests; see README.

use crate::addon::Addon;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_ENDPOINT: &str = "https://api.strem.io";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthState {
    pub auth_key: String,
    pub email: String,
}

#[derive(Clone)]
pub struct StremioApi {
    http: reqwest::Client,
    endpoint: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<T>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    message: Option<String>,
}

impl StremioApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            endpoint: DEFAULT_ENDPOINT.into(),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}/api/{method}", self.endpoint.trim_end_matches('/'));
        let env: Envelope<T> = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(err) = env.error {
            return Err(Error::Api(
                err.message.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        env.result
            .ok_or_else(|| Error::Api(format!("{method}: empty result")))
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<AuthState> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            auth_key: String,
        }
        let r: R = self
            .call(
                "login",
                json!({ "type": "Login", "email": email, "password": password, "facebook": false }),
            )
            .await?;
        Ok(AuthState {
            auth_key: r.auth_key,
            email: email.to_owned(),
        })
    }

    pub async fn addon_collection(&self, auth_key: &str) -> Result<Vec<Addon>> {
        #[derive(Deserialize)]
        struct R {
            addons: Vec<serde_json::Value>,
        }
        let r: R = self
            .call(
                "addonCollectionGet",
                json!({ "type": "AddonCollectionGet", "authKey": auth_key, "update": true }),
            )
            .await?;
        // One broken addon entry must not hide all the others.
        Ok(r.addons
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<Addon>(v) {
                Ok(a) => Some(a),
                Err(e) => {
                    tracing::warn!("skipping unreadable addon entry: {e}");
                    None
                }
            })
            .collect())
    }

    pub async fn logout(&self, auth_key: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("logout", json!({ "type": "Logout", "authKey": auth_key }))
            .await?;
        Ok(())
    }

    /// The account's library (watch progress + watchlist), via `datastoreGet`
    /// on the `libraryItem` collection. Broken entries are skipped.
    pub async fn library(&self, auth_key: &str) -> Result<Vec<LibraryItem>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            items: Vec<serde_json::Value>,
        }
        // The datastore returns the raw collection, not an `items` wrapper;
        // accept both shapes.
        let raw: serde_json::Value = self
            .call_value(
                "datastoreGet",
                json!({
                    "authKey": auth_key,
                    "collection": "libraryItem",
                    "ids": [],
                    "all": true
                }),
            )
            .await?;
        let entries: Vec<serde_json::Value> = if raw.is_array() {
            serde_json::from_value(raw).unwrap_or_default()
        } else {
            match serde_json::from_value::<R>(raw.clone()) {
                Ok(r) => r.items,
                Err(_) => {
                    tracing::warn!("unexpected datastoreGet result shape: {raw}");
                    Vec::new()
                }
            }
        };
        Ok(entries
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<LibraryItem>(v) {
                Ok(i) => Some(i),
                Err(e) => {
                    tracing::warn!("skipping unreadable library entry: {e}");
                    None
                }
            })
            .collect())
    }

    async fn call_value(&self, method: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/api/{method}", self.endpoint.trim_end_matches('/'));
        let env: Envelope<serde_json::Value> = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(err) = env.error {
            return Err(Error::Api(
                err.message.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        env.result
            .ok_or_else(|| Error::Api(format!("{method}: empty result")))
    }
}

/// One entry of the account library (`datastoreGet` on `libraryItem`).
/// Mirrors the fields Stremio itself stores; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryItem {
    /// Meta id (`tt…`).
    #[serde(rename = "_id")]
    pub id: String,
    pub name: String,
    /// "movie", "series", "channel", "other", …
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub temp: bool,
    #[serde(default)]
    pub state: LibraryItemState,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItemState {
    /// RFC 3339 timestamp of the last watch (kept as a string; ISO timestamps
    /// compare correctly as plain strings for ordering).
    #[serde(default)]
    pub last_watched: Option<String>,
    /// Milliseconds into the video where playback stopped.
    #[serde(default)]
    pub time_offset: u64,
    /// Total duration in milliseconds (0 when unknown).
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub times_watched: u32,
    /// Last played video id (`tt…` for movies, `tt…:s:e` for episodes).
    #[serde(default, rename = "video_id")]
    pub video_id: Option<String>,
}

impl LibraryItem {
    /// Same rule Stremio uses for its Continue Watching row.
    pub fn in_continue_watching(&self) -> bool {
        self.kind != "other" && (!self.removed || self.temp) && self.state.time_offset > 0
    }

    /// 0.0..1.0, or 0.0 when the duration is unknown.
    pub fn progress_fraction(&self) -> f64 {
        if self.state.time_offset > 0 && self.state.duration > 0 {
            self.state.time_offset as f64 / self.state.duration as f64
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_items_and_cw_filter() {
        let raw = serde_json::json!([
            {
                "_id": "tt0118571",
                "name": "Heat",
                "type": "movie",
                "poster": "https://x/poster.jpg",
                "removed": false,
                "temp": false,
                "_mtime": "2026-11-17T08:00:00.000Z",
                "state": {
                    "lastWatched": "2026-11-17T08:00:00.000Z",
                    "timeWatched": 600000,
                    "timeOffset": 3600000,
                    "overallTimeWatched": 600000,
                    "timesWatched": 1,
                    "flaggedWatched": 0,
                    "duration": 10800000,
                    "video_id": "tt0118571",
                    "watched": null,
                    "noNotif": false
                }
            },
            {
                "_id": "tt33043892",
                "name": "Some Series",
                "type": "series",
                "poster": null,
                "removed": true,
                "temp": true,
                "state": {
                    "timeOffset": 300000,
                    "duration": 2400000,
                    "video_id": "tt33043892:1:2"
                }
            },
            {
                "_id": "tt1234567",
                "name": "Finished movie",
                "type": "movie",
                "removed": false,
                "temp": false,
                "state": { "timeOffset": 5000000, "duration": 5000000 }
            },
            {
                "_id": "tt7654321",
                "name": "Not started",
                "type": "movie",
                "removed": false,
                "temp": false,
                "state": { "timeOffset": 0 }
            }
        ]);
        let items: Vec<LibraryItem> = serde_json::from_value(raw).expect("library items parse");
        assert_eq!(items.len(), 4);

        // Stremio's rule is just `timeOffset > 0`; the app additionally drops
        // finished entries when merging.
        let cw: Vec<&LibraryItem> = items.iter().filter(|i| i.in_continue_watching()).collect();
        assert_eq!(cw.len(), 3);

        let heat = cw.iter().find(|i| i.id == "tt0118571").unwrap();
        assert!((heat.progress_fraction() - 3600000.0 / 10800000.0).abs() < 1e-9);
        assert_eq!(heat.state.video_id.as_deref(), Some("tt0118571"));

        let series = cw.iter().find(|i| i.id == "tt33043892").unwrap();
        assert_eq!(series.state.video_id.as_deref(), Some("tt33043892:1:2"));
        assert!((series.progress_fraction() - 0.125).abs() < 1e-9);

        let finished = cw.iter().find(|i| i.id == "tt1234567").unwrap();
        assert!((finished.progress_fraction() - 1.0).abs() < 1e-9);
    }
}
