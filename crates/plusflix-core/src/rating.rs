//! Age ratings and the per-profile policy that filters catalog items by them.
//!
//! The Stremio protocol has no dedicated rating field on `meta`/`meta_preview`
//! objects, so this module parses rating tokens ("PG", "TV-MA", "16", ...) out
//! of `MetaPreview::genres` AND out of the `app_extras` block some addons
//! attach to their items (`certification` plus the per-country
//! `certifications` map), and turns them into a minimum age, plus the
//! `RatingPolicy` a profile passes in to filter items. Source-verified:
//! AIOMetadata (the community kid-safe addon) emits real genres only and
//! publishes its ratings via `app_extras`, so with it installed child
//! filtering works; Stremio's default Cinemeta publishes neither (no ratings
//! at all), so its items arrive unrated and are hidden from every child
//! profile. Core never knows which profile is active: the policy is always an
//! explicit parameter.

use crate::model::{AppExtras, MetaPreview};
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

/// The strictest rating among an item's genre tokens AND its `app_extras`
/// ratings (`certification` + every `certifications` value): the candidates
/// from both sources are merged and the HIGHEST age wins, `None` when
/// nothing parses (the item is unrated). AIOMetadata-style items carry real
/// genres only - their ratings come from extras alone - while Cinemeta-style
/// items are rated via tokens in `genres` and have no extras.
pub fn rating_of_item(genres: &[String], extras: Option<&AppExtras>) -> Option<AgeRating> {
    let extras_tokens = extras.into_iter().flat_map(|x| {
        let certification = x.certification.iter().map(String::as_str);
        let countries = x
            .certifications
            .iter()
            .flat_map(|map| map.values().map(String::as_str));
        certification.chain(countries)
    });
    genres
        .iter()
        .map(String::as_str)
        .chain(extras_tokens)
        .filter_map(parse_token)
        .max()
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
/// Ratings come from the items' genre tokens and `app_extras` alike.
pub fn filter_previews(policy: &RatingPolicy, items: Vec<MetaPreview>) -> Vec<MetaPreview> {
    items
        .into_iter()
        .filter(|item| policy.allows(rating_of_item(&item.genres, item.app_extras.as_ref())))
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
            app_extras: None,
        }
    }

    /// AIOMetadata-style extras: one `certification` + country->rating pairs.
    fn extras(certification: Option<&str>, countries: &[(&str, &str)]) -> AppExtras {
        AppExtras {
            certification: certification.map(str::to_string),
            certifications: (!countries.is_empty()).then(|| {
                countries
                    .iter()
                    .map(|(c, r)| (c.to_string(), r.to_string()))
                    .collect()
            }),
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

    #[test]
    fn extras_alone_rate_the_item() {
        // AIOMetadata-style: `genres` stay real genres, the rating comes
        // from `app_extras` alone.
        let mut m = preview("tt1", &["Drama"]);
        m.app_extras = Some(extras(Some("PG"), &[]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(8))
        );
        m.app_extras = Some(extras(Some("18"), &[]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(18))
        );
        // Neither source carries a rating: unrated.
        assert_eq!(rating_of_item(&["Drama".to_string()], None), None);
    }

    #[test]
    fn strictest_wins_across_genres_and_extras() {
        // Genres say 8, extras say 17.
        let mut m = preview("tt1", &["TV-PG"]);
        m.app_extras = Some(extras(Some("R"), &[]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(17))
        );
        // The other way around: genres 18, extras 13.
        let mut m = preview("tt2", &["TV-MA"]);
        m.app_extras = Some(extras(Some("PG-13"), &[]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(18))
        );
    }

    #[test]
    fn certifications_map_values_count() {
        // Per-country ratings: every value is a candidate, strictest wins.
        let mut m = preview("tt1", &[]);
        m.app_extras = Some(extras(None, &[("US", "R"), ("PL", "16")]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(17))
        );
        // A bare digit in the map means that age.
        m.app_extras = Some(extras(None, &[("DE", "12")]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(12))
        );
        // Certification and map disagree: the stricter one wins.
        m.app_extras = Some(extras(Some("G"), &[("GB", "TV-14")]));
        assert_eq!(
            rating_of_item(&m.genres, m.app_extras.as_ref()),
            Some(AgeRating(14))
        );
    }

    #[test]
    fn unparseable_extras_leave_the_item_unrated() {
        // Nothing parses (garbage tokens only): the item is unrated, and
        // unrated stays hidden from children (counts as age 18).
        let mut m = preview("tt1", &["Comedy"]);
        m.app_extras = Some(extras(Some("n/a"), &[("XX", "fluffy")]));
        assert_eq!(rating_of_item(&m.genres, m.app_extras.as_ref()), None);
        assert!(
            !RatingPolicy::child(12).allows(rating_of_item(&m.genres, m.app_extras.as_ref()))
        );
    }

    #[test]
    fn filter_previews_reads_extras_too() {
        let mut soft = preview("tt1", &["Drama"]);
        soft.app_extras = Some(extras(Some("G"), &[]));
        let mut hard = preview("tt2", &["Drama"]);
        hard.app_extras = Some(extras(Some("R"), &[("US", "R"), ("PL", "16")]));
        let unrated = preview("tt3", &["Drama"]);

        // Child (12): only the G item passes (R = 17 hidden, unrated hidden).
        let items = vec![soft.clone(), hard.clone(), unrated.clone()];
        let kept: Vec<String> = filter_previews(&RatingPolicy::child(12), items)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(kept, ["tt1"]);

        // Adult: everything passes, extras or not, in order.
        let items = vec![soft, hard, unrated];
        let kept: Vec<String> = filter_previews(&RatingPolicy::default(), items)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(kept, ["tt1", "tt2", "tt3"]);
    }
}