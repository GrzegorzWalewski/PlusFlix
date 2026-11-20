use crate::error::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

pub struct Translations {
    map: HashMap<String, String>,
    fallback: HashMap<String, String>, // always English
}

/// Keys already warned about by `t()` (each missing key is logged once).
static WARNED_KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

impl Translations {
    /// Build translations from two JSON strings: `locale_json` for the
    /// requested locale and `fallback_json` (normally English) for missing
    /// keys. Used with `include_str!`-embedded locale files.
    pub fn from_json(locale_json: &str, fallback_json: &str) -> Result<Translations> {
        let fallback: HashMap<String, String> = serde_json::from_str(fallback_json)?;
        let map = if locale_json == fallback_json {
            fallback.clone()
        } else {
            serde_json::from_str(locale_json)?
        };
        Ok(Translations { map, fallback })
    }

    /// Load a locale. `locale_code` is e.g. "en", "pl".
    /// `locales_dir` is the directory containing `en.json`, `pl.json`, etc.
    /// Falls back to `en.json` for any key missing from the requested locale.
    /// If `locale_code` is "en" or the file doesn't exist, only en.json is loaded.
    pub fn load(locale_code: &str, locales_dir: &Path) -> Result<Translations> {
        let en_path = locales_dir.join("en.json");
        let fallback_json = if en_path.exists() {
            fs::read_to_string(&en_path)?
        } else {
            "{}".to_owned()
        };
        if locale_code == "en" {
            return Translations::from_json(&fallback_json, &fallback_json);
        }
        let path = locales_dir.join(format!("{}.json", locale_code));
        let locale_json = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            "{}".to_owned()
        };
        Translations::from_json(&locale_json, &fallback_json)
    }

    /// Return translated string for `key`, falling back through:
    ///   1. selected locale map
    ///   2. English fallback map
    ///   3. the key itself (as a last resort)
    /// Returns the translation for `key`, falling back to English, then to the key itself.
    pub fn t(&self, key: &str) -> String {
        if let Some(value) = self.map.get(key).or_else(|| self.fallback.get(key)) {
            return value.clone();
        }
        // Total miss: warn once per key, then return the key itself.
        let warned = WARNED_KEYS.get_or_init(|| Mutex::new(HashSet::new()));
        if let Ok(mut keys) = warned.lock() {
            if keys.insert(key.to_owned()) {
                tracing::warn!("missing translation key: {key}");
            }
        }
        key.to_owned()
    }

    /// Translated string for `key`, or `None` when neither the locale nor the
    /// English fallback has it (no warning). Used for optional keys whose
    /// natural fallback is caller-supplied (e.g. addon genre names).
    pub fn t_opt(&self, key: &str) -> Option<String> {
        self.map
            .get(key)
            .or_else(|| self.fallback.get(key))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = temp_dir().join(format!("reel-i18n-{}-{}", name, std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_load_en() -> Result<()> {
        let dir = test_dir("load_en");
        fs::write(dir.join("en.json"), r#"{"app_name": "PlusFlix"}"#)?;
        let translations = Translations::load("en", &dir)?;
        assert_eq!(translations.t("app_name"), "PlusFlix");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn test_load_pl_with_translation() -> Result<()> {
        let dir = test_dir("load_pl");
        fs::write(dir.join("en.json"), r#"{"app_name": "PlusFlix"}"#)?;
        fs::write(dir.join("pl.json"), r#"{"app_name": "PlusFliksu"}"#)?;
        let translations = Translations::load("pl", &dir)?;
        assert_eq!(translations.t("app_name"), "PlusFliksu");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn test_fallback_to_english() -> Result<()> {
        let dir = test_dir("fallback");
        fs::write(dir.join("en.json"), r#"{"app_name": "PlusFlix"}"#)?;
        fs::write(dir.join("pl.json"), r#"{"unrelated_key": "value"}"#)?;
        let translations = Translations::load("pl", &dir)?;
        assert_eq!(translations.t("app_name"), "PlusFlix");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn test_key_not_found() -> Result<()> {
        let dir = test_dir("key_not_found");
        fs::write(dir.join("en.json"), r#"{"key": "value"}"#)?;
        let translations = Translations::load("en", &dir)?;
        assert_eq!(translations.t("nonexistent_key"), "nonexistent_key");
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn test_from_json() -> Result<()> {
        let translations = Translations::from_json(
            r#"{"app_name": "PlusFliksu"}"#,
            r#"{"app_name": "PlusFlix", "other": "x"}"#,
        )?;
        assert_eq!(translations.t("app_name"), "PlusFliksu");
        // Missing from the locale, present in the fallback.
        assert_eq!(translations.t("other"), "x");
        // Missing everywhere: returns the key (and warns once per key).
        assert_eq!(translations.t("nonexistent_key"), "nonexistent_key");
        assert_eq!(translations.t("nonexistent_key"), "nonexistent_key");
        Ok(())
    }

    #[test]
    fn test_t_opt() -> Result<()> {
        let translations = Translations::from_json(
            r#"{"genre_action": "Akcja"}"#,
            r#"{"genre_action": "Action", "genre_horror": "Horror"}"#,
        )?;
        // Locale hit.
        assert_eq!(translations.t_opt("genre_action"), Some("Akcja".to_owned()));
        // Locale miss, English fallback hit.
        assert_eq!(
            translations.t_opt("genre_horror"),
            Some("Horror".to_owned())
        );
        // Missing everywhere: None (no warn, caller supplies the fallback).
        assert_eq!(translations.t_opt("genre_bogus"), None);
        Ok(())
    }
}
