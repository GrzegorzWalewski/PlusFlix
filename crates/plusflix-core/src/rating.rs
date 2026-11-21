//! Age ratings and the per-profile policy that filters catalog items by them.
//!
//! The Stremio protocol has no dedicated rating field on `meta`/`meta_preview`
//! objects, so this module parses rating tokens ("PG", "TV-MA", "16", ...) out
//! of `MetaPreview::genres` and turns them into a minimum age, plus the
//! `RatingPolicy` a profile passes in to filter items. Some metadata/catalog
//! addons publish certification tokens there (TMDB/US-certification-based,
//! e.g. AIOMetadata), but Stremio's default Cinemeta does not: child filtering
//! depends on an addon that supplies them. Core never knows which profile is
//! active: the policy is always an explicit parameter.

use crate::model::MetaPreview;
use serde::{Deserialize, Serialize};

/// Minimum age (in years) required to see one item.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct AgeRating(pub u8);

/// Normalized form of a rating token: trimmed, uppercased, with every
/// non-alphanumeric character removed. `" tv-y 7 "` -> `"TVY7"`, `"pg-13"` ->
/// `"PG13"`.
fn normalize(token: &str) -> String {
    token
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Parses one rating token into a minimum age. `None` when the token is not a
/// known rating (a plain genre like "Action", or garbage).
///
/// * A pure number is the age itself: `"6"` -> 6, `"16"` -> 16, `"18"` -> 18.
/// * The US TV/movie systems map to a comparable age:
///
///   | token(s)             | age |
///   |---------------------|-----|
///   | G, TV-G, TV-Y, U    |   0 |
///   | TV-Y7               |   7 |
///   | PG, TV-PG           |   8 |
///   | 12A                 |  12 |
///   | PG-13               |  13 |
///   | TV-14               |  14 |
///   | R                   |  17 |
///   | NC-17, X, TV-MA     |  18 |
pub fn parse_token(token: &str) -> Option<AgeRating> {
    let token = normalize(token);
    if token.is_empty() {
        return None;
    }
    // A pure number is the age itself; numbers too large to be an age are ignored.
    if token.bytes().all(|b| b.is_ascii_digit()) {
        return token.parse::<u8>().ok().map(AgeRating);
    }
    match token.as_str() {
        "G" | "TVG" | "TVY" | "U" => Some(AgeRating(0)),
        "TVY7" => Some(AgeRating(7)),
        "PG" | "TVPG" => Some(AgeRating(8)),
        "12A" => Some(AgeRating(12)),
        "PG13" => Some(AgeRating(13)),
        "TV14" => Some(AgeRating(14)),
        "R" => Some(AgeRating(17)),
        "NC17" | "X" | "TVMA" => Some(AgeRating(18)),
        _ => None,
    }
}

/// The strictest rating among an item's genre tokens: the HIGHEST age wins.
/// `None` when no token is a rating (the item is unrated).
pub fn rating_of(genres: &[String]) -> Option<AgeRating> {
    genres.iter().filter_map(|g| parse_token(g)).max()
}

/// What a profile is allowed to see. `max_age: None` is the adult profile
/// (everything allowed); `Some(age)` is a child profile capped at `age`.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct RatingPolicy {
    /// `None` = adult / allow-all; `Some(age)` = child profile capped at `age`.
    pub max_age: Option<u8>,
}

impl RatingPolicy {
    /// A child profile: items rated above `age` are hidden.
    pub fn child(age: u8) -> Self {
        Self { max_age: Some(age) }
    }

    /// May this profile see an item with this rating?
    ///
    /// * adult (`max_age == None`): always.
    /// * child: rated items pass iff `rating <= max_age`; unrated items count
    ///   as age 18, so they are hidden from every child capped below 18.
    pub fn allows(&self, rating: Option<AgeRating>) -> bool {
        let Some(max) = self.max_age else {
            return true;
        };
        rating.unwrap_or(AgeRating(18)).0 <= max
    }
}

/// Keeps only the previews the policy allows (adult: everything, in order).
pub fn filter_previews(policy: &RatingPolicy, items: Vec<MetaPreview>) -> Vec<MetaPreview> {
    items
        .into_iter()
        .filter(|item| policy.allows(rating_of(&item.genres)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(id: &str, genres: &[&str]) -> MetaPreview {
        MetaPreview {
            id: id.to_string(),
            kind: "movie".to_string(),
            name: format!("Title {id}"),
            poster: None,
            release_info: None,
            description: None,
            genres: genres.iter().map(|g| g.to_string()).collect(),
        }
    }

    #[test]
    fn token_table() {
        for (token, age) in [
            ("6", 6),
            ("16", 16),
            ("18", 18),
            ("G", 0),
            ("TV-G", 0),
            ("TVY", 0),
            ("TV-Y", 0),
            ("U", 0),
            ("TV-Y7", 7),
            ("PG", 8),
            ("TV-PG", 8),
            ("12A", 12),
            ("PG-13", 13),
            ("TV-14", 14),
            ("R", 17),
            ("NC-17", 18),
            ("X", 18),
            ("TV-MA", 18),
        ] {
            assert_eq!(parse_token(token), Some(AgeRating(age)), "token {token}");
        }
    }

    #[test]
    fn tokens_are_normalized_before_matching() {
        assert_eq!(parse_token(" tv-y 7 "), Some(AgeRating(7)));
        assert_eq!(parse_token("pg-13"), Some(AgeRating(13)));
        assert_eq!(parse_token("  tv-ma "), Some(AgeRating(18)));
        assert_eq!(parse_token("tv_14"), Some(AgeRating(14)));
        assert_eq!(parse_token("nc-17"), Some(AgeRating(18)));
        assert_eq!(parse_token("pg"), Some(AgeRating(8)));
        assert_eq!(parse_token("12a"), Some(AgeRating(12)));
    }

    #[test]
    fn non_ratings_are_ignored() {
        assert_eq!(parse_token("Action"), None);
        assert_eq!(parse_token("Sci-Fi"), None);
        assert_eq!(parse_token("Comedy"), None);
        assert_eq!(parse_token(""), None);
        assert_eq!(parse_token("  -  "), None);
        // "16+" normalizes down to a plain age.
        assert_eq!(parse_token("16+"), Some(AgeRating(16)));
        // A number too large to be an age is ignored, not clamped.
        assert_eq!(parse_token("300"), None);
    }

    #[test]
    fn strictest_rating_wins() {
        assert_eq!(rating_of(&[]), None);
        assert_eq!(rating_of(&["Comedy".to_string()]), None);
        // The highest parsed age wins, whatever the order.
        assert_eq!(
            rating_of(&["G".to_string(), "PG".to_string()]),
            Some(AgeRating(8))
        );
        assert_eq!(
            rating_of(&["TV-MA".to_string(), "G".to_string()]),
            Some(AgeRating(18))
        );
        assert_eq!(
            rating_of(&["G".to_string(), "TV-MA".to_string()]),
            Some(AgeRating(18))
        );
        // Plain genres never contribute a rating.
        assert_eq!(
            rating_of(&["Comedy".to_string(), "R".to_string(), "Drama".to_string()]),
            Some(AgeRating(17))
        );
    }

    #[test]
    fn adult_policy_allows_everything() {
        let adult = RatingPolicy::default();
        assert_eq!(adult, RatingPolicy { max_age: None });
        assert!(adult.allows(None)); // unrated
        assert!(adult.allows(Some(AgeRating(0))));
        assert!(adult.allows(Some(AgeRating(13))));
        assert!(adult.allows(Some(AgeRating(18))));
        assert!(adult.allows(Some(AgeRating(21))));
    }

    #[test]
    fn child_policy_caps_the_age() {
        let child = RatingPolicy::child(12);
        assert_eq!(child.max_age, Some(12));
        assert!(child.allows(Some(AgeRating(0))));
        assert!(child.allows(Some(AgeRating(12))));
        assert!(!child.allows(Some(AgeRating(13))));
        assert!(!child.allows(Some(AgeRating(17))));
        assert!(!child.allows(Some(AgeRating(18))));
    }

    #[test]
    fn child_policy_hides_unrated() {
        // Unrated counts as age 18: hidden from every child capped below 18.
        assert!(!RatingPolicy::child(3).allows(None));
        assert!(!RatingPolicy::child(12).allows(None));
        assert!(!RatingPolicy::child(17).allows(None));
    }

    #[test]
    fn filter_previews_keeps_allowed_in_order() {
        let items = vec![
            preview("tt1", &["G"]),
            preview("tt2", &["TV-MA"]),
            preview("tt3", &["Comedy"]),      // unrated
            preview("tt4", &["PG", "R"]),     // strictest: 17
        ];
        let adult = RatingPolicy::default();
        let kept: Vec<String> =
            filter_previews(&adult, items.clone()).into_iter().map(|m| m.id).collect();
        assert_eq!(kept, ["tt1", "tt2", "tt3", "tt4"]);

        let child = RatingPolicy::child(12);
        let kept: Vec<String> =
            filter_previews(&child, items).into_iter().map(|m| m.id).collect();
        assert_eq!(kept, ["tt1"]); // 18+, unrated and 17+ all hidden
    }
}