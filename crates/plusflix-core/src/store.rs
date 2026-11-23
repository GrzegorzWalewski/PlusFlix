//! Local persistence: Stremio login, up to 5 adult/child profiles with their
//! settings, favorites and watch progress, plus the salted hash of a global
//! parental PIN. One JSON file, written atomically.

use crate::error::{Error, Result};
use crate::settings::Settings;
use crate::stremio::AuthState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const MAX_PROFILES: usize = 5;

/// Built-in avatar icons (SVG file names under the UI's icon directory) and
/// tile colours, indexed together. `default_look` assigns them round-robin.
pub const PROFILE_ICONS: &[&str] = &[
    "cat", "dog", "rabbit", "bird", "fish", "turtle", "smile", "ghost", "bot", "star", "rocket",
    "music", "gamepad", "coffee", "plane", "car", "bike", "camera", "flower", "flame",
];
pub const PROFILE_COLORS: &[&str] = &[
    "#e5484d", "#3e63dd", "#30a46c", "#f5a623", "#8e4ec6", "#0ea5e9", "#e11d74", "#84cc16",
    "#f97316", "#14b8a6",
];

/// Icon + colour for the n-th profile, used for new profiles and migration.
pub fn default_look(n: usize) -> (&'static str, &'static str) {
    (
        PROFILE_ICONS[n % PROFILE_ICONS.len()],
        PROFILE_COLORS[n % PROFILE_COLORS.len()],
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub poster: Option<String>,
    /// Minimum-age rating stamped when the favorite was added; `None` = unrated.
    /// The app re-filters favorites with it, without refetching.
    #[serde(default)]
    pub rating_age: Option<u8>,
}

/// Everything needed to render a Continue Watching card for a progress entry.
/// Older stores have none; the entry then just doesn't show in the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressCard {
    /// "movie" or "series" (the kind of the title the popup opens).
    pub kind: String,
    /// Series or movie id the detail popup opens.
    pub title_id: String,
    /// Show / movie name.
    pub title: String,
    pub poster: Option<String>,
    /// Landscape art (backdrop or episode thumbnail) for the 16:9 card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// For series: the episode's video id (the progress map key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    /// For series: "S02E05  Episode title".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub updated_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<ProgressCard>,
    /// Minimum-age rating stamped when the entry was added; `None` = unrated.
    /// The app re-filters Continue Watching with it, without refetching.
    #[serde(default)]
    pub rating_age: Option<u8>,
}

/// Adult or child profile kind. Children are gated behind the parental PIN.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    #[default]
    Adult,
    Child,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// Adult or child; the app asks for the PIN before switching to a child.
    #[serde(default)]
    pub kind: ProfileKind,
    /// Age of a child profile; `None` for adult profiles.
    #[serde(default)]
    pub max_age: Option<u8>,
    /// Legacy avatar palette index; migrated to `icon`/`color` on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<u8>,
    /// Built-in icon name (a file in the UI icon directory).
    #[serde(default)]
    pub icon: String,
    /// Tile background colour as hex, e.g. "#e5484d".
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub favorites: Vec<Favorite>,
    /// Keyed by video id (`tt123` for movies, `tt123:1:2` for episodes).
    #[serde(default)]
    pub progress: HashMap<String, Progress>,
    /// Video ids watched to the end (or past ~92 %).
    #[serde(default)]
    pub watched: HashSet<String>,
}

/// Salted SHA-256 hash of the parental PIN. The plaintext PIN is never
/// persisted: `salt` is 16 fresh random bytes from the OS and `digest`
/// is SHA-256(salt || pin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHash {
    pub salt: [u8; 16],
    pub digest: [u8; 32],
}

impl PinHash {
    fn new(pin: &str) -> Result<Self> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt)
            .map_err(|e| Error::Other(format!("OS randomness unavailable: {e}")))?;
        Ok(Self {
            salt,
            digest: Self::digest(&salt, pin),
        })
    }

    /// The stored form of a PIN: SHA-256(salt || pin).
    fn digest(salt: &[u8; 16], pin: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(salt);
        hasher.update(pin.as_bytes());
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&hasher.finalize());
        digest
    }

    fn matches(&self, pin: &str) -> bool {
        Self::digest(&self.salt, pin) == self.digest
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub auth: Option<AuthState>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub last_profile: Option<String>,
    #[serde(default = "default_locale")]
    pub locale: String,
    /// Salted hash of the global parental PIN; `None` until one is set.
    #[serde(default)]
    pub pin: Option<PinHash>,
}

pub fn migrate_store_path(
    old_path: &std::path::Path,
    new_path: &std::path::Path,
) -> std::io::Result<()> {
    if old_path.exists() {
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Ensure the new file doesn't exist before renaming to avoid errors on some platforms
        if new_path.exists() {
            std::fs::remove_file(new_path)?;
        }
        std::fs::rename(old_path, new_path)?;
    }
    Ok(())
}

pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "PlusFlix")
        .map(|d| d.config_dir().join("store.json"))
        .unwrap_or_else(|| PathBuf::from("plusflix-store.json"))
}

fn default_locale() -> String {
    "en".to_owned()
}

impl Default for Store {
    fn default() -> Self {
        Self {
            auth: None,
            profiles: Vec::new(),
            last_profile: None,
            locale: "en".to_owned(),
            pin: None,
        }
    }
}

impl Store {
    pub fn load(path: &Path) -> Result<Store> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut store: Store = serde_json::from_slice(&bytes)?;
                store.migrate();
                Ok(store)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Fills in fields added after the first release: maps the legacy
    /// `avatar` index to an icon + colour pair.
    pub fn migrate(&mut self) {
        for (n, p) in self.profiles.iter_mut().enumerate() {
            if p.icon.is_empty() || p.color.is_empty() {
                let idx = p.avatar.map(|a| a as usize).unwrap_or(n);
                let (icon, color) = default_look(idx);
                if p.icon.is_empty() {
                    p.icon = icon.to_owned();
                }
                if p.color.is_empty() {
                    p.color = color.to_owned();
                }
            }
            p.avatar = None;
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Next free `pN` id: one past the highest existing N.
    fn next_profile_id(&self) -> String {
        let next = self
            .profiles
            .iter()
            .filter_map(|p| p.id.strip_prefix('p')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        format!("p{next}")
    }

    pub fn add_profile(&mut self, name: &str) -> Result<&Profile> {
        if self.profiles.len() >= MAX_PROFILES {
            return Err(Error::ProfileLimit(MAX_PROFILES));
        }
        let id = self.next_profile_id();
        let (icon, color) = default_look(self.profiles.len());
        self.profiles.push(Profile {
            id,
            name: name.trim().to_owned(),
            kind: ProfileKind::Adult,
            max_age: None,
            avatar: None,
            icon: icon.to_owned(),
            color: color.to_owned(),
            settings: Settings::default(),
            favorites: Vec::new(),
            progress: HashMap::new(),
            watched: HashSet::new(),
        });
        Ok(self.profiles.last().expect("just pushed"))
    }

    /// Creates a child profile (gated behind the parental PIN) for a child
    /// between 3 and 17 years old. Counts toward the same profile limit.
    pub fn add_child_profile(&mut self, name: &str, max_age: u8) -> Result<&Profile> {
        if !(3..=17).contains(&max_age) {
            return Err(Error::InvalidAge(max_age));
        }
        if self.profiles.len() >= MAX_PROFILES {
            return Err(Error::ProfileLimit(MAX_PROFILES));
        }
        let id = self.next_profile_id();
        let (icon, color) = default_look(self.profiles.len());
        self.profiles.push(Profile {
            id,
            name: name.trim().to_owned(),
            kind: ProfileKind::Child,
            max_age: Some(max_age),
            avatar: None,
            icon: icon.to_owned(),
            color: color.to_owned(),
            settings: Settings::default(),
            favorites: Vec::new(),
            progress: HashMap::new(),
            watched: HashSet::new(),
        });
        Ok(self.profiles.last().expect("just pushed"))
    }

    pub fn remove_profile(&mut self, id: &str) {
        self.profiles.retain(|p| p.id != id);
        if self.last_profile.as_deref() == Some(id) {
            self.last_profile = None;
        }
        // The parental PIN only guards child profiles; with the last one
        // gone it has nothing left to protect.
        if !self.profiles.iter().any(|p| p.kind == ProfileKind::Child) {
            self.pin = None;
        }
    }

    /// Sets (or replaces) the global parental PIN, stored as a salted
    /// SHA-256 hash. The plaintext never reaches the store file.
    pub fn set_pin(&mut self, pin: &str) -> Result<()> {
        if pin.is_empty() {
            return Err(Error::EmptyPin);
        }
        self.pin = Some(PinHash::new(pin)?);
        Ok(())
    }

    /// Checks a PIN against the stored hash: `false` when no PIN is set,
    /// on empty input, or on mismatch.
    pub fn verify_pin(&self, pin: &str) -> bool {
        !pin.is_empty() && self.pin.as_ref().is_some_and(|hash| hash.matches(pin))
    }

    /// Removes the parental PIN.
    pub fn clear_pin(&mut self) {
        self.pin = None;
    }

    pub fn profile(&self, id: &str) -> Result<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::NoSuchProfile(id.to_owned()))
    }

    pub fn profile_mut(&mut self, id: &str) -> Result<&mut Profile> {
        self.profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::NoSuchProfile(id.to_owned()))
    }
}

impl Profile {
    pub fn is_favorite(&self, id: &str) -> bool {
        self.favorites.iter().any(|f| f.id == id)
    }

    /// Returns the new state (`true` = now a favorite).
    pub fn toggle_favorite(&mut self, fav: Favorite) -> bool {
        if let Some(i) = self.favorites.iter().position(|f| f.id == fav.id) {
            self.favorites.remove(i);
            false
        } else {
            self.favorites.insert(0, fav);
            true
        }
    }

    pub fn set_progress(
        &mut self,
        video_id: &str,
        position_secs: f64,
        duration_secs: Option<f64>,
        card: Option<ProgressCard>,
    ) {
        let updated_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // An update without card info keeps whatever card the entry already had.
        let card = card.or_else(|| self.progress.get(video_id).and_then(|p| p.card.clone()));
        // Likewise the rating age stamped when the entry was added.
        let rating_age = self.progress.get(video_id).and_then(|p| p.rating_age);
        self.progress.insert(
            video_id.to_owned(),
            Progress {
                position_secs,
                duration_secs,
                updated_unix,
                card,
                rating_age,
            },
        );
    }

    /// Resume point, ignoring the first seconds and the last 2 minutes (basically finished).
    pub fn resume_position(&self, video_id: &str) -> f64 {
        match self.progress.get(video_id) {
            Some(p)
                if p.position_secs > 10.0
                    && p.duration_secs.is_none_or(|d| d - p.position_secs > 120.0) =>
            {
                p.position_secs
            }
            _ => 0.0,
        }
    }

    pub fn is_watched(&self, video_id: &str) -> bool {
        self.watched.contains(video_id)
    }

    pub fn mark_watched(&mut self, video_id: &str) {
        self.watched.insert(video_id.to_owned());
        // Watched (or nearly finished) entries leave the Continue Watching row.
        if let Some(p) = self.progress.get_mut(video_id) {
            if Self::passed_watch_threshold(p.position_secs, p.duration_secs) {
                self.progress.remove(video_id);
            }
        }
    }

    pub fn unmark_watched(&mut self, video_id: &str) {
        self.watched.remove(video_id);
    }

    /// Watched counts as done at ~92 % of the duration. Without a known
    /// duration only an explicit "played to the end" marks it watched.
    pub fn passed_watch_threshold(position_secs: f64, duration_secs: Option<f64>) -> bool {
        match duration_secs {
            Some(d) if d > 0.0 => position_secs / d >= 0.92,
            _ => false,
        }
    }

    /// Continue Watching entries for this profile, newest first. Only entries
    /// with render info and a valid resume point are listed.
    pub fn continue_watching(&self) -> Vec<&Progress> {
        let mut items: Vec<&Progress> = self
            .progress
            .values()
            .filter(|p| {
                p.card.is_some()
                    && p.position_secs > 10.0
                    && p.duration_secs.is_none_or(|d| d - p.position_secs > 120.0)
            })
            .collect();
        items.sort_by_key(|a| std::cmp::Reverse(a.updated_unix));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_five_profiles() {
        let mut s = Store::default();
        for i in 0..5 {
            s.add_profile(&format!("P{i}")).unwrap();
        }
        assert!(matches!(s.add_profile("six"), Err(Error::ProfileLimit(5))));
        let id = s.profiles[2].id.clone();
        s.remove_profile(&id);
        let p = s.add_profile("again").unwrap();
        assert_ne!(p.id, id.as_str().to_owned() + "x");
        assert_eq!(s.profiles.len(), 5);
        let ids: std::collections::HashSet<_> = s.profiles.iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids.len(), 5, "ids stay unique after remove + add");
    }

    #[test]
    fn roundtrip_and_independent_profiles() {
        let dir = std::env::temp_dir().join(format!("reel-store-test-{}", std::process::id()));
        let path = dir.join("store.json");
        let mut s = Store::default();
        let a = s.add_profile("A").unwrap().id.clone();
        let b = s.add_profile("B").unwrap().id.clone();
        s.profile_mut(&a).unwrap().settings.max_filesize_gb = Some(8.0);
        assert!(s.profile_mut(&a).unwrap().toggle_favorite(Favorite {
            id: "tt1".into(),
            kind: "movie".into(),
            name: "X".into(),
            poster: None,
            rating_age: None
        }));
        s.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(
            loaded.profile(&a).unwrap().settings.max_filesize_gb,
            Some(8.0)
        );
        assert!(loaded.profile(&a).unwrap().is_favorite("tt1"));
        assert!(!loaded.profile(&b).unwrap().is_favorite("tt1"));
        assert_eq!(loaded.profile(&b).unwrap().settings.max_filesize_gb, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The per-profile failover timeouts round-trip through save/load, and
    /// stores written before they existed keep loading (default `None`,
    /// i.e. the built-in timeouts).
    #[test]
    fn settings_timeouts_roundtrip_and_default() {
        let dir = std::env::temp_dir().join(format!("reel-store-timeouts-{}", std::process::id()));
        let path = dir.join("store.json");

        // Old store.json without the timeout keys: both default to None.
        let old = r#"{
            "profiles": [
                {"id": "p1", "name": "Old", "settings": {"min_quality": "P1080"}}
            ]
        }"#;
        let mut store: Store = serde_json::from_str(old).unwrap();
        store.migrate();
        assert_eq!(store.profile("p1").unwrap().settings.start_timeout_secs, None);
        assert_eq!(store.profile("p1").unwrap().settings.stall_timeout_secs, None);

        // Custom values survive a save/load round-trip.
        {
            let p = store.profile_mut("p1").unwrap();
            p.settings.start_timeout_secs = Some(60);
            p.settings.stall_timeout_secs = Some(30);
        }
        store.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        let p = loaded.profile("p1").unwrap();
        assert_eq!(p.settings.start_timeout_secs, Some(60));
        assert_eq!(p.settings.stall_timeout_secs, Some(30));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The per-profile buffering settings round-trip through save/load, and
    /// stores written before they existed keep loading (default `None`,
    /// i.e. no pre-buffering).
    #[test]
    fn settings_buffering_roundtrip_and_default() {
        let dir =
            std::env::temp_dir().join(format!("reel-store-buffering-{}", std::process::id()));
        let path = dir.join("store.json");

        // Old store.json without the buffering keys: both default to None.
        let old = r#"{
            "profiles": [
                {"id": "p1", "name": "Old", "settings": {"min_quality": "P1080"}}
            ]
        }"#;
        let mut store: Store = serde_json::from_str(old).unwrap();
        store.migrate();
        assert_eq!(store.profile("p1").unwrap().settings.buffer_ahead_mib, None);
        assert_eq!(store.profile("p1").unwrap().settings.prebuffer_secs, None);

        // Custom values survive a save/load round-trip.
        {
            let p = store.profile_mut("p1").unwrap();
            p.settings.buffer_ahead_mib = Some(768);
            p.settings.prebuffer_secs = Some(30);
        }
        store.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        let p = loaded.profile("p1").unwrap();
        assert_eq!(p.settings.buffer_ahead_mib, Some(768));
        assert_eq!(p.settings.prebuffer_secs, Some(30));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resume_ignores_start_and_finished() {
        let mut s = Store::default();
        let id = s.add_profile("A").unwrap().id.clone();
        let p = s.profile_mut(&id).unwrap();
        p.set_progress("tt1", 5.0, Some(7000.0), None);
        assert_eq!(p.resume_position("tt1"), 0.0);
        p.set_progress("tt1", 3000.0, Some(7000.0), None);
        assert_eq!(p.resume_position("tt1"), 3000.0);
        p.set_progress("tt1", 6950.0, Some(7000.0), None);
        assert_eq!(p.resume_position("tt1"), 0.0);
    }

    /// A store.json written by an older version (avatar index, no icon/color/
    /// watched, progress without card info) must keep loading.
    #[test]
    fn old_format_store_migrates() {
        let old = r#"{
            "profiles": [
                {
                    "id": "p1",
                    "name": "Old",
                    "avatar": 2,
                    "settings": {"min_quality": "P1080"},
                    "favorites": [],
                    "progress": {
                        "tt1": {"position_secs": 300.0, "duration_secs": 7000.0, "updated_unix": 100}
                    }
                }
            ],
            "last_profile": "p1"
        }"#;
        let mut store: Store = serde_json::from_str(old).unwrap();
        store.migrate();
        let p = store.profile("p1").unwrap();
        assert_eq!(p.icon, "rabbit");
        assert_eq!(p.color, "#30a46c");
        assert_eq!(p.settings.min_quality, crate::settings::Quality::P1080);
        assert!(p.watched.is_empty());
        assert_eq!(p.resume_position("tt1"), 300.0);
        assert!(
            p.continue_watching().is_empty(),
            "no card info in old store"
        );
        // Migrated profiles save without the legacy field and reload fine.
        let dir = std::env::temp_dir().join(format!("reel-store-old-{}", std::process::id()));
        let path = dir.join("store.json");
        store.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.profile("p1").unwrap().icon, "rabbit");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn continue_watching_is_newest_first_and_filtered() {
        let mut s = Store::default();
        let id = s.add_profile("A").unwrap().id.clone();
        let p = s.profile_mut(&id).unwrap();
        let card = |title: &str| ProgressCard {
            kind: "movie".into(),
            title_id: title.into(),
            title: title.into(),
            poster: None,
            background: None,
            episode_id: None,
            episode_label: None,
        };
        p.set_progress("a", 300.0, Some(7000.0), Some(card("A")));
        p.progress.get_mut("a").unwrap().updated_unix = 100;
        p.set_progress("b", 50.0, Some(7000.0), Some(card("B")));
        p.progress.get_mut("b").unwrap().updated_unix = 200;
        p.set_progress("c", 5.0, Some(7000.0), Some(card("C"))); // too early
        p.progress.get_mut("c").unwrap().updated_unix = 300;
        p.set_progress("d", 6990.0, Some(7000.0), Some(card("D"))); // finished
        p.progress.get_mut("d").unwrap().updated_unix = 400;
        p.set_progress("e", 300.0, Some(7000.0), None); // no card
        p.progress.get_mut("e").unwrap().updated_unix = 500;

        let cw: Vec<String> = p
            .continue_watching()
            .iter()
            .map(|pr| pr.card.as_ref().unwrap().title.clone())
            .collect();
        assert_eq!(cw, vec!["B".to_owned(), "A".to_owned()]);
    }

    #[test]
    fn watched_marking_and_threshold() {
        let mut s = Store::default();
        let id = s.add_profile("A").unwrap().id.clone();
        let p = s.profile_mut(&id).unwrap();
        assert!(!Profile::passed_watch_threshold(5000.0, Some(7000.0)));
        assert!(Profile::passed_watch_threshold(6500.0, Some(7000.0)));
        assert!(!Profile::passed_watch_threshold(6500.0, None));

        p.set_progress("tt1", 6800.0, Some(7000.0), None);
        p.mark_watched("tt1");
        assert!(p.is_watched("tt1"));
        assert!(!p.progress.contains_key("tt1"), "finished entry removed");

        p.set_progress("tt2", 3000.0, Some(7000.0), None);
        p.mark_watched("tt2");
        assert!(p.is_watched("tt2"));
        assert!(p.progress.contains_key("tt2"), "mid-way entry stays");
        p.unmark_watched("tt2");
        assert!(!p.is_watched("tt2"));
    }

    #[test]
    fn store_locale_defaults_to_en() {
        // Old store.json without a locale field must deserialise to "en".
        let old = r#"{"profiles": [], "last_profile": null}"#;
        let store: Store = serde_json::from_str(old).unwrap();
        assert_eq!(store.locale, "en");

        // Default construction also yields "en".
        assert_eq!(Store::default().locale, "en");

        // Round-trip: save and reload preserves the locale field.
        let dir = std::env::temp_dir().join(format!("reel-store-locale-{}", std::process::id()));
        let path = dir.join("store.json");
        let s = Store {
            locale: "pl".to_owned(),
            ..Store::default()
        };
        s.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.locale, "pl");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// New fields — profile kind/age, item rating ages and the parental
    /// PIN — round-trip through save/load.
    #[test]
    fn roundtrip_child_profiles_and_pin() {
        let dir = std::env::temp_dir().join(format!("reel-store-pin-{}", std::process::id()));
        let path = dir.join("store.json");
        let mut s = Store::default();
        let adult = s.add_profile("Adult").unwrap().id.clone();
        let kid = s.add_child_profile("Kid", 8).unwrap().id.clone();
        assert!(s.profile_mut(&adult).unwrap().toggle_favorite(Favorite {
            id: "tt1".into(),
            kind: "movie".into(),
            name: "X".into(),
            poster: None,
            rating_age: Some(16)
        }));
        s.profile_mut(&kid).unwrap().set_progress("tt2", 300.0, Some(7000.0), None);
        s.profile_mut(&kid)
            .unwrap()
            .progress
            .get_mut("tt2")
            .unwrap()
            .rating_age = Some(12);
        // Keep the timestamp small so the plaintext scan below is stable.
        s.profile_mut(&kid)
            .unwrap()
            .progress
            .get_mut("tt2")
            .unwrap()
            .updated_unix = 100;
        s.set_pin("1357").unwrap();
        s.save(&path).unwrap();

        // The PIN is stored only as salt + SHA-256; no plaintext in the file.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("1357"), "PIN plaintext leaked into store.json");

        let loaded = Store::load(&path).unwrap();
        let adult = loaded.profile(&adult).unwrap();
        let kid = loaded.profile(&kid).unwrap();
        assert_eq!(adult.kind, ProfileKind::Adult);
        assert_eq!(adult.max_age, None);
        assert_eq!(kid.kind, ProfileKind::Child);
        assert_eq!(kid.max_age, Some(8));
        assert_eq!(kid.id, "p2");
        assert!(kid.id.strip_prefix('p').unwrap().parse::<u32>().is_ok());
        assert_eq!(adult.favorites[0].rating_age, Some(16));
        assert_eq!(kid.progress.get("tt2").unwrap().rating_age, Some(12));
        assert!(loaded.verify_pin("1357"));
        assert!(!loaded.verify_pin("0000"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A store.json written before child profiles and the PIN existed (no
    /// `kind`, `max_age`, `pin` or `rating_age` keys) must keep loading.
    #[test]
    fn old_format_store_defaults_kind_age_and_pin() {
        let old = r#"{
            "profiles": [
                {
                    "id": "p1",
                    "name": "Old",
                    "avatar": 2,
                    "settings": {"min_quality": "P1080"},
                    "favorites": [
                        {"id": "tt1", "kind": "movie", "name": "F", "poster": null}
                    ],
                    "progress": {
                        "tt1": {"position_secs": 300.0, "duration_secs": 7000.0, "updated_unix": 100}
                    }
                }
            ],
            "last_profile": "p1"
        }"#;
        let mut store: Store = serde_json::from_str(old).unwrap();
        store.migrate();
        let p = store.profile("p1").unwrap();
        assert_eq!(p.kind, ProfileKind::Adult);
        assert_eq!(p.max_age, None);
        assert_eq!(p.favorites[0].rating_age, None);
        assert_eq!(p.progress.get("tt1").unwrap().rating_age, None);
        assert!(store.pin.is_none());
        assert!(!store.verify_pin("1234"), "no PIN set");
    }

    #[test]
    fn set_verify_and_clear_pin() {
        let mut s = Store::default();
        assert!(!s.verify_pin("1234"), "no PIN set yet");
        assert!(matches!(s.set_pin(""), Err(Error::EmptyPin)));
        assert!(s.pin.is_none(), "empty PIN was not stored");
        s.set_pin("1234").unwrap();
        assert!(s.verify_pin("1234"));
        assert!(!s.verify_pin("1234 "), "whitespace is not the PIN");
        assert!(!s.verify_pin("4321"), "wrong PIN");
        assert!(!s.verify_pin(""), "empty input");
        s.clear_pin();
        assert!(!s.verify_pin("1234"), "cleared PIN no longer verifies");
        assert!(s.pin.is_none());
    }

    /// Two stores holding the same PIN text must salt independently, so
    /// one store's hash is useless against the other's PIN.
    #[test]
    fn same_pin_text_salts_differently_per_store() {
        let mut a = Store::default();
        let mut b = Store::default();
        a.set_pin("1234").unwrap();
        b.set_pin("1234").unwrap();
        let (ha, hb) = (a.pin.as_ref().unwrap(), b.pin.as_ref().unwrap());
        assert_ne!(ha.salt, hb.salt);
        assert_ne!(ha.digest, hb.digest);
        assert!(a.verify_pin("1234"));
        assert!(b.verify_pin("1234"));

        // Re-setting the same PIN rotates the salt (and the digest).
        let old = ha.clone();
        a.set_pin("1234").unwrap();
        assert_ne!(a.pin.as_ref().unwrap().salt, old.salt);
        assert!(a.verify_pin("1234"));
    }

    #[test]
    fn child_profile_age_validation() {
        let mut s = Store::default();
        for bad in [0u8, 2, 18, 200] {
            assert!(matches!(
                s.add_child_profile("X", bad),
                Err(Error::InvalidAge(age)) if age == bad
            ));
        }
        assert!(s.profiles.is_empty(), "rejected children were not created");
        // Boundary ages are fine.
        assert_eq!(s.add_child_profile("Three", 3).unwrap().max_age, Some(3));
        assert_eq!(
            s.add_child_profile("Seventeen", 17).unwrap().max_age,
            Some(17)
        );
    }

    #[test]
    fn child_profiles_count_toward_profile_limit() {
        let mut s = Store::default();
        let kid = s.add_child_profile("Kid", 7).unwrap();
        assert_eq!(kid.kind, ProfileKind::Child);
        assert_eq!(kid.max_age, Some(7));
        assert!(kid.id.strip_prefix('p').unwrap().parse::<u32>().is_ok());
        for i in 0..4 {
            s.add_profile(&format!("A{i}")).unwrap();
        }
        assert_eq!(s.profiles.len(), MAX_PROFILES);
        assert!(matches!(
            s.add_child_profile("Six", 9),
            Err(Error::ProfileLimit(5))
        ));
        assert!(matches!(s.add_profile("six"), Err(Error::ProfileLimit(5))));
    }

    /// Deleting the last child profile clears the parental PIN with it;
    /// while any child remains, the PIN stays.
    #[test]
    fn removing_last_child_profile_clears_pin() {
        let mut s = Store::default();
        let first = s.add_child_profile("First", 6).unwrap().id.clone();
        let second = s.add_child_profile("Second", 10).unwrap().id.clone();
        s.set_pin("1234").unwrap();
        s.remove_profile(&first);
        assert!(s.pin.is_some(), "a child remains, PIN stays");
        s.remove_profile(&second);
        assert!(s.pin.is_none(), "last child gone, PIN auto-cleared");
        assert!(!s.verify_pin("1234"));

        // No child profile at all → nothing for the PIN to guard.
        s.set_pin("5678").unwrap();
        let adult = s.add_profile("Adult").unwrap().id.clone();
        s.remove_profile(&adult);
        assert!(s.pin.is_none());
    }

    /// Progress updates (which carry no rating info) keep the rating age
    /// stamped when the entry was added.
    #[test]
    fn progress_updates_keep_stamped_rating_age() {
        let mut s = Store::default();
        let id = s.add_profile("A").unwrap().id.clone();
        let p = s.profile_mut(&id).unwrap();
        p.set_progress("tt1", 300.0, Some(7000.0), None);
        assert_eq!(p.progress.get("tt1").unwrap().rating_age, None);
        p.progress.get_mut("tt1").unwrap().rating_age = Some(15);
        p.set_progress("tt1", 600.0, Some(7000.0), None);
        assert_eq!(
            p.progress.get("tt1").unwrap().rating_age,
            Some(15),
            "progress updates keep the stamped rating age"
        );
    }
}
