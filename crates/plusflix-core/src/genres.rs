//! Genre browsing (Netflix-style): the union of genre options the installed
//! addons really support, plus a paged multi-catalog browser for one genre.
//!
//! Never assume an addon supports genres: everything here is driven by what
//! each addon's manifest declares at runtime. Catalogs whose `genre` options
//! are all 4-digit numbers are year filters, not genres, and are excluded
//! (see `ExtraDecl::is_year_options` for the future Year-filter extension point).

use crate::addon::{Addon, AddonClient};
use crate::model::{CatalogDecl, MetaPreview};
use crate::rating::{rating_of_item, RatingPolicy};
use futures::future::join_all;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::timeout;

/// Page size convention of the Stremio catalog protocol. A response with
/// fewer items than a full page (or an empty one) means the end of the catalog.
pub const PAGE_SIZE: usize = 100;

/// Per-catalog request timeout: one failing addon must not break the others.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// One catalog of one addon, addressable for genre browsing.
#[derive(Debug, Clone)]
pub struct CatalogRef {
    /// Addon display name (e.g. "Cinemeta").
    pub addon_name: String,
    /// Catalog display name (e.g. "Popular").
    pub catalog_name: String,
    pub kind: String,
    /// The addon's transport URL; identifies the addon in the chips filter.
    pub addon_url: String,
    pub(crate) addon: Addon,
    pub(crate) cat: CatalogDecl,
}

impl CatalogRef {
    /// The genre options this catalog declares (empty when the catalog
    /// supports `genre` but doesn't list its values).
    pub fn genre_options(&self) -> &[String] {
        self.cat.genre_options()
    }

    /// Does the catalog accept this specific genre? Catalogs that declare the
    /// `genre` extra without options are assumed to accept any genre.
    /// Comparison is normalized (case-, whitespace- and punctuation-insensitive),
    /// so a genre chosen from a merged `GenreOption` still routes correctly.
    pub fn supports_genre(&self, genre: &str) -> bool {
        let options = self.cat.genre_options();
        let genre_key = normalize_genre_key(genre);
        options.is_empty() || options.iter().any(|o| normalize_genre_key(o) == genre_key)
    }

    /// Identity of this catalog across transports: (addon manifest id,
    /// catalog id). The same addon re-installed under a different transport
    /// URL (e.g. after a language change) keeps this identity.
    pub fn identity(&self) -> (&str, &str) {
        (&self.addon.manifest.id, &self.cat.id)
    }
}

/// Dedupe/matching key for a genre name: lowercased, trimmed, with internal
/// whitespace collapsed and runs of non-alphanumeric characters folded into a
/// single space. "Talk show", "Talk  show ", "Talk-show" and "TALK SHOW" all
/// share the key `talk show`.
pub fn normalize_genre_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !key.is_empty() {
                key.push(' ');
            }
            pending_space = false;
            for lower in ch.to_lowercase() {
                key.push(lower);
            }
        } else {
            // Whitespace and punctuation both act as separators.
            pending_space = true;
        }
    }
    key
}

/// Every catalog of `kind` that can be browsed by genre: it declares a `genre`
/// extra (in either manifest format), requires no other mandatory extras, and
/// doesn't hide a year filter behind it.
pub fn genre_catalogs(addons: &[Addon], kind: &str) -> Vec<CatalogRef> {
    // The same addon can appear twice in the collection (e.g. the old and the
    // new install after a language change) under different transport URLs.
    // Identity for genre browsing is the addon MANIFEST id + catalog id, not
    // the transport URL. The LAST occurrence wins: the newest install is the
    // current one, so each catalog contributes exactly one `CatalogRef`,
    // carrying the newest entry's options.
    let mut out: Vec<CatalogRef> = Vec::new();
    for addon in addons {
        for cat in &addon.manifest.catalogs {
            if cat.kind != kind || !cat.supports_extra("genre") {
                continue;
            }
            let extras = cat.extras();
            let genre_extra = extras
                .iter()
                .find(|e| e.name == "genre")
                .expect("supports_extra just checked for this");
            if genre_extra.is_year_options() {
                // Year filter, not genres ("New" in Cinemeta). Excluded here;
                // extension point for a future Year filter.
                continue;
            }
            if cat.requires_other_than(&["genre", "skip"]) {
                continue;
            }
            let catalog = CatalogRef {
                addon_name: addon.name().to_owned(),
                catalog_name: cat.name.clone().unwrap_or_else(|| cat.id.clone()),
                kind: cat.kind.clone(),
                addon_url: addon.transport_url.clone(),
                addon: addon.clone(),
                cat: cat.clone(),
            };
            match out
                .iter()
                .position(|c| c.addon.manifest.id == addon.manifest.id && c.cat.id == cat.id)
            {
                // A re-install of the same addon under a different transport
                // URL: the later (newest) entry overrides the earlier one.
                Some(i) => out[i] = catalog,
                None => out.push(catalog),
            }
        }
    }
    out
}

/// The catalogs to browse for one selected genre, in account order.
pub fn catalogs_for_genre(addons: &[Addon], kind: &str, genre: &str) -> Vec<CatalogRef> {
    genre_catalogs(addons, kind)
        .into_iter()
        .filter(|c| c.supports_genre(genre))
        .collect()
}

/// One genre offered for `kind`.
#[derive(Debug, Clone)]
pub struct GenreOption {
    /// Original spelling as first declared by an addon.
    pub name: String,
    /// The catalogs serving this genre.
    pub catalogs: Vec<CatalogRef>,
}

/// The genres browseable for `kind`: the normalized union (case-, whitespace-
/// and punctuation-insensitive) of the genre options of all catalogs that
/// accept a genre, keeping the first-seen spelling, sorted alphabetically.
/// Never assumed: only what some addon's manifest really declares.
pub fn genre_options(addons: &[Addon], kind: &str) -> Vec<GenreOption> {
    let catalogs = genre_catalogs(addons, kind);
    let mut genres: Vec<(String, GenreOption)> = Vec::new();
    for catalog in &catalogs {
        for name in catalog.genre_options() {
            let key = normalize_genre_key(name);
            match genres.iter_mut().find(|(k, _)| *k == key) {
                Some((_, option)) => {
                    // `genre_catalogs` already dedupes by manifest id, but a
                    // different addon may declare the same genre: catalog
                    // identity here is (manifest id, catalog id).
                    if !option.catalogs.iter().any(|c| {
                        c.addon.manifest.id == catalog.addon.manifest.id
                            && c.cat.id == catalog.cat.id
                    }) {
                        option.catalogs.push(catalog.clone());
                    }
                }
                None => genres.push((
                    key,
                    GenreOption {
                        name: name.clone(),
                        catalogs: vec![catalog.clone()],
                    },
                )),
            }
        }
    }
    genres.sort_by(|a, b| a.0.cmp(&b.0));
    genres.into_iter().map(|(_, option)| option).collect()
}

/// One planned page: everything the parallel fetch needs, tagged with the
/// generation it was planned at so stale pages can be dropped.
#[derive(Debug, Clone)]
pub struct PagePlan {
    generation: u64,
    genre: String,
    requests: Vec<PageRequest>,
}

#[derive(Debug, Clone)]
struct PageRequest {
    addon: Addon,
    kind: String,
    id: String,
    skip: u32,
}

/// One page of merged results.
#[derive(Debug, Clone, Default)]
pub struct GenrePage {
    /// New items, round-robin interleaved across the catalogs and deduped by
    /// id (also against everything shown on earlier pages).
    pub items: Vec<MetaPreview>,
    /// False once every catalog is exhausted.
    pub has_more: bool,
    /// How many of the queried catalogs failed or timed out on this page.
    pub failed: usize,
    /// How many catalogs were queried on this page.
    pub queried: usize,
}

/// Headless, testable pager over a genre selection. The app plans a page
/// (`plan`), fetches it outside any state lock (`fetch_page`), and merges it
/// (`absorb`); changing the selection bumps the generation so anything still
/// in flight for the old selection is dropped.
pub struct GenreBrowser {
    kind: String,
    genre: String,
    catalogs: Vec<CatalogRef>,
    /// Next `skip` offset per catalog.
    skip: Vec<u32>,
    exhausted: Vec<bool>,
    /// (kind, id) already shown on any page of this selection.
    seen: HashSet<(String, String)>,
    generation: u64,
    /// Age-rating policy: absorbed items above the profile's age are dropped.
    policy: RatingPolicy,
}

impl GenreBrowser {
    pub fn new(
        kind: &str,
        genre: &str,
        catalogs: Vec<CatalogRef>,
        policy: RatingPolicy,
    ) -> Self {
        let count = catalogs.len();
        Self {
            kind: kind.to_owned(),
            genre: genre.to_owned(),
            catalogs,
            skip: vec![0; count],
            exhausted: vec![false; count],
            seen: HashSet::new(),
            generation: 0,
            policy,
        }
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn genre(&self) -> &str {
        &self.genre
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.catalogs.is_empty()
    }

    /// Changes the selection (genre and/or the catalogs to browse) and drops
    /// all paging state. Anything still in flight for the old selection is
    /// stale: `absorb` refuses it.
    pub fn set_selection(&mut self, genre: &str, catalogs: Vec<CatalogRef>) {
        let count = catalogs.len();
        self.genre = genre.to_owned();
        self.catalogs = catalogs;
        self.skip = vec![0; count];
        self.exhausted = vec![false; count];
        self.seen.clear();
        self.generation += 1;
    }

    /// Everything one parallel page fetch needs, without holding state while
    /// the requests run. Exhausted catalogs are not queried again.
    pub fn plan(&self) -> PagePlan {
        PagePlan {
            generation: self.generation,
            genre: self.genre.clone(),
            requests: self
                .catalogs
                .iter()
                .zip(&self.skip)
                .zip(&self.exhausted)
                .filter(|((_, _), exhausted)| !**exhausted)
                .map(|((catalog, skip), _)| PageRequest {
                    addon: catalog.addon.clone(),
                    kind: catalog.kind.clone(),
                    id: catalog.cat.id.clone(),
                    skip: *skip,
                })
                .collect(),
        }
    }

    /// Merges one fetched page into the browse state. Returns `None` when the
    /// page is stale (the selection changed while it was in flight).
    pub fn absorb(
        &mut self,
        plan: &PagePlan,
        responses: &[Option<Vec<MetaPreview>>],
    ) -> Option<GenrePage> {
        if plan.generation != self.generation {
            return None;
        }
        // The planned requests align, in order, with the catalogs that were
        // not exhausted at `plan` time. Nothing mutated since (same generation).
        let planned: Vec<usize> = (0..self.catalogs.len())
            .filter(|i| !self.exhausted[*i])
            .collect();
        let mut failed = 0;
        let mut columns: Vec<Vec<MetaPreview>> = Vec::new();
        for (i, response) in responses.iter().enumerate() {
            let Some(catalog_index) = planned.get(i) else {
                break;
            };
            match response {
                Some(items) => {
                    if items.len() < PAGE_SIZE {
                        // Fewer than a full page (or empty): end of that catalog.
                        self.exhausted[*catalog_index] = true;
                    }
                    self.skip[*catalog_index] += items.len() as u32;
                    columns.push(items.clone());
                }
                // Failed or timed out: not exhausted, retried on the next page.
                None => failed += 1,
            }
        }
        // Round-robin interleave across the catalogs, dedupe by (kind, id)
        // within the page and against every earlier page.
        let mut items = Vec::new();
        let mut columns: Vec<std::vec::IntoIter<MetaPreview>> =
            columns.into_iter().map(IntoIterator::into_iter).collect();
        loop {
            let mut added = false;
            for column in &mut columns {
                let Some(item) = column.next() else {
                    continue;
                };
                added = true;
                // The rating policy drops an item before it ever reaches the
                // dedupe set (ratings come from genre tokens or from
                // `app_extras`); short pages under a child policy are expected.
                if !self.policy.allows(rating_of_item(&item.genres, item.app_extras.as_ref())) {
                    continue;
                }
                if self.seen.insert((item.kind.clone(), item.id.clone())) {
                    items.push(item);
                }
            }
            if !added {
                break;
            }
        }
        Some(GenrePage {
            items,
            has_more: self.exhausted.iter().any(|e| !e),
            failed,
            queried: planned.len(),
        })
    }
}

/// Runs one planned page: every request in parallel, each with its own
/// timeout, so one dead addon can't break the rest. `None` per failed request.
pub async fn fetch_page(client: &AddonClient, plan: &PagePlan) -> Vec<Option<Vec<MetaPreview>>> {
    join_all(plan.requests.iter().map(|request| async move {
        let skip = request.skip.to_string();
        let extra = [("genre", plan.genre.as_str()), ("skip", skip.as_str())];
        let items = timeout(
            FETCH_TIMEOUT,
            client.catalog(&request.addon, &request.kind, &request.id, &extra),
        )
        .await;
        match items {
            Ok(Ok(items)) => Some(items),
            Ok(Err(e)) => {
                tracing::warn!(
                    "genre catalog {} / {} failed: {e}",
                    request.addon.name(),
                    request.id
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    "genre catalog {} / {} timed out",
                    request.addon.name(),
                    request.id
                );
                None
            }
        }
    }))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testhttp::FakeServer;
    use std::collections::HashMap;

    /// Manifest with one movie catalog `id` whose `genre` extra takes
    /// `options`. The manifest id derives from the catalog id: two test
    /// manifests with the same catalog id model the SAME addon installed
    /// twice (e.g. old and new install after a language change).
    fn manifest(catalog_id: &str, genre_options: serde_json::Value) -> serde_json::Value {
        let catalogs = match genre_options {
            serde_json::Value::Null => serde_json::json!([{
                "type": "movie", "id": catalog_id, "name": "Test Catalog",
                "extraSupported": ["genre"]
            }]),
            options => serde_json::json!([{
                "type": "movie", "id": catalog_id, "name": "Test Catalog",
                "extra": [{"name": "genre", "options": options}, {"name": "skip"}]
            }]),
        };
        serde_json::json!({ "id": catalog_id, "name": "Test Addon", "types": ["movie"], "catalogs": catalogs })
    }

    fn addon(url: &str, manifest: serde_json::Value) -> Addon {
        let transport_url = format!("{url}/manifest.json");
        serde_json::from_value(
            serde_json::json!({ "transportUrl": transport_url, "manifest": manifest }),
        )
        .unwrap()
    }

    fn metas(kind: &str, ids: &[&str]) -> String {
        let metas: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "type": kind, "name": format!("Title {id}") }))
            .collect();
        serde_json::json!({ "metas": metas }).to_string()
    }

    /// `metas` with a `genres` list per item, so age-rating filtering has
    /// something to bite on. An empty list leaves the item unrated.
    fn metas_rated(kind: &str, rated: &[(&str, &[&str])]) -> String {
        let metas: Vec<serde_json::Value> = rated
            .iter()
            .map(|(id, genres)| {
                serde_json::json!({
                    "id": id,
                    "type": kind,
                    "name": format!("Title {id}"),
                    "genres": genres.to_vec(),
                })
            })
            .collect();
        serde_json::json!({ "metas": metas }).to_string()
    }

    fn route(catalog_id: &str, genre: &str, skip: u32, body: &str) -> (String, String) {
        (
            format!("/catalog/movie/{catalog_id}/genre={genre}&skip={skip}.json"),
            body.to_string(),
        )
    }

    /// Route for an addon mounted under a base path prefix (config segment).
    fn prefixed_route(
        prefix: &str,
        catalog_id: &str,
        genre: &str,
        skip: u32,
        body: &str,
    ) -> (String, String) {
        (
            format!("{prefix}/catalog/movie/{catalog_id}/genre={genre}&skip={skip}.json"),
            body.to_string(),
        )
    }

    #[test]
    fn normalize_genre_key_merges_spelling_variants() {
        let key = normalize_genre_key("Talk show");
        assert_eq!(normalize_genre_key("Talk  show "), key);
        assert_eq!(normalize_genre_key("Talk-show"), key);
        assert_eq!(normalize_genre_key("TALK SHOW"), key);
        assert_eq!(key, "talk show");
        // Leading/trailing separators are trimmed, not padded.
        assert_eq!(normalize_genre_key("  Romans  "), "romans");
        // Non-ASCII letters are lowercased too.
        assert_eq!(normalize_genre_key("Rímské"), "rímské");
    }

    #[test]
    fn genre_options_merge_normalized_duplicates() {
        let addons = vec![
            addon(
                "https://a.example",
                manifest("top", serde_json::json!(["Action", "Romans"])),
            ),
            addon(
                "https://b.example",
                manifest(
                    "imdbRating",
                    serde_json::json!(["action", "  Romans ", "Talk-show"]),
                ),
            ),
            addon(
                "https://c.example",
                manifest("short", serde_json::json!(["Talk  show"])),
            ),
        ];
        let options = genre_options(&addons, "movie");
        let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
        // First-seen spelling wins; case/whitespace/punctuation variants merge.
        assert_eq!(names, ["Action", "Romans", "Talk-show"]);
        // a and b serve Action (c declares only its Talk-show variant).
        assert_eq!(options[0].catalogs.len(), 2);
        // b ("Talk-show") and c ("Talk  show") merged into one option.
        assert_eq!(options[2].catalogs.len(), 2);
    }

    #[test]
    fn duplicate_addon_install_contributes_one_catalog() {
        // The same addon manifest id, installed twice under different
        // transport URLs (e.g. before/after a language change).
        let addons = vec![
            addon(
                "https://old.example",
                manifest("top", serde_json::json!(["Action"])),
            ),
            addon(
                "https://new.example",
                manifest("top", serde_json::json!(["Action", "Comedy"])),
            ),
        ];
        let catalogs = catalogs_for_genre(&addons, "movie", "Action");
        // One CatalogRef per (manifest id, catalog id): no duplicate chips.
        assert_eq!(catalogs.len(), 1);
        assert_eq!(catalogs[0].cat.id, "top");

        let options = genre_options(&addons, "movie");
        let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
        // Last install wins: the newest entry overrides the older one, so its
        // Comedy option surfaces and no option carries two catalogs.
        assert_eq!(names, ["Action", "Comedy"]);
        assert!(options.iter().all(|o| o.catalogs.len() == 1));
    }

    #[test]
    fn supports_genre_matches_through_normalization() {
        let addons = vec![addon(
            "https://a.example",
            manifest("top", serde_json::json!(["Talk-show"])),
        )];
        let catalogs = catalogs_for_genre(&addons, "movie", "Talk show ");
        assert_eq!(catalogs.len(), 1);
        assert!(catalogs[0].supports_genre("TALK SHOW"));
        assert!(!catalogs[0].supports_genre("talk shows"));
    }

    #[test]
    fn genre_options_union_sorted_deduped() {
        let mut addons = vec![
            addon(
                "https://a.example",
                manifest("top", serde_json::json!(["Action", "Sci-Fi"])),
            ),
            addon(
                "https://b.example",
                manifest("imdbRating", serde_json::json!(["action", "Reality-TV"])),
            ),
            // Short format: supports genre but declares no options.
            addon(
                "https://c.example",
                manifest("short", serde_json::Value::Null),
            ),
        ];
        // A year catalog: required genre extra whose options are all 4-digit years.
        let year_manifest = serde_json::json!({
            "id": "year", "name": "Year Addon", "types": ["movie"],
            "catalogs": [{
                "type": "movie", "id": "year", "name": "New",
                "extra": [{"name": "genre", "isRequired": true, "options": ["2023", "2024"]}]
            }]
        });
        addons.push(addon("https://d.example", year_manifest));
        // A catalog with another required extra: not browsable.
        let fussy_manifest = serde_json::json!({
            "id": "fussy", "name": "Fussy Addon", "types": ["movie"],
            "catalogs": [{
                "type": "movie", "id": "fussy", "name": "Fussy",
                "extra": [{"name": "genre", "options": ["Documentary"]}, {"name": "kind", "isRequired": true}]
            }]
        });
        addons.push(addon("https://e.example", fussy_manifest));

        let options = genre_options(&addons, "movie");
        let names: Vec<&str> = options.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Action", "Reality-TV", "Sci-Fi"]);
        // "action" (b) dedupes into "Action" (a); each option carries both catalogs.
        assert_eq!(options[0].catalogs.len(), 2);
        assert_eq!(options[2].catalogs.len(), 1);
        // Series kind has nothing.
        assert!(genre_options(&addons, "series").is_empty());
    }

    #[tokio::test]
    async fn merge_dedupe_and_interleave() {
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &metas("movie", &["tt1", "tt2"]))]);
        routes.extend([prefixed_route(
            "/two",
            "second",
            "Action",
            0,
            &metas("movie", &["tt2", "tt3"]),
        )]);
        let server = FakeServer::start(routes).await;
        let addons = vec![
            addon(
                server.url.as_str(),
                manifest("top", serde_json::json!(["Action"])),
            ),
            addon(
                format!("{}/two", server.url).as_str(),
                manifest("second", serde_json::json!(["Action"])),
            ),
        ];
        let catalogs = catalogs_for_genre(&addons, "movie", "Action");
        assert_eq!(catalogs.len(), 2);
        let client = AddonClient::new().unwrap();
        let mut browser =
            GenreBrowser::new("movie", "Action", catalogs, RatingPolicy::default());
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
        // Round-robin across the two catalogs, tt2 deduped (second occurrence dropped).
        assert_eq!(ids, ["tt1", "tt2", "tt3"]);
        assert_eq!(page.failed, 0);
        assert!(!page.has_more); // both returned < PAGE_SIZE
    }

    #[tokio::test]
    async fn rating_policy_filters_pages() {
        // One short page mixing rating tokens with plain genres.
        let body = metas_rated(
            "movie",
            &[
                ("tt1", &["G"]),            // age 0
                ("tt2", &["PG", "Comedy"]), // strictest: age 8
                ("tt3", &["R", "Comedy"]),  // strictest: age 17
                ("tt4", &["TV-MA"]),        // age 18
                ("tt5", &[]),               // unrated
                ("tt6", &["Comedy"]),       // genre only: unrated
            ],
        );
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &body)]);
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action"])),
        )];
        let client = AddonClient::new().unwrap();

        // Child (12): tt1 (0) and tt2 (8) pass; the 17+/18+ items and both
        // unrated ones are dropped before they ever reach the dedupe set.
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::child(12),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["tt1", "tt2"]);
        assert_eq!(page.queried, 1);
        assert_eq!(page.failed, 0);
        // The short page exhausted the catalog; filtering never extends paging.
        assert!(!page.has_more);

        // Adult (allow-all): the same page passes through untouched.
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["tt1", "tt2", "tt3", "tt4", "tt5", "tt6"]);
    }

    #[tokio::test]
    async fn rating_policy_reads_app_extras() {
        // AIOMetadata-style catalog: `genres` stay real genres, the ratings
        // live only in `appExtras` (certification + per-country map).
        let body = serde_json::json!({
            "metas": [
                {"id": "tt1", "type": "movie", "name": "Title tt1", "genres": ["Animation"],
                 "appExtras": {"certification": "PG"}},
                {"id": "tt2", "type": "movie", "name": "Title tt2", "genres": ["Drama"],
                 "appExtras": {"certifications": {"US": "PG-13", "PL": "16"}}},
                {"id": "tt3", "type": "movie", "name": "Title tt3", "genres": ["Crime"],
                 "appExtras": {"certification": "R", "certifications": {"US": "R"}}},
                {"id": "tt4", "type": "movie", "name": "Title tt4", "genres": ["Drama"],
                 "appExtras": "not-an-object"},
                {"id": "tt5", "type": "movie", "name": "Title tt5", "genres": ["Comedy"]}
            ]
        })
        .to_string();
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &body)]);
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action"])),
        )];
        let client = AddonClient::new().unwrap();

        // Child (12): only tt1 (PG = 8) passes; tt2 (PG-13/16 = 16),
        // tt3 (R = 17), tt4 (odd extras = unrated) and tt5 (unrated)
        // drop before they ever reach the dedupe set.
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::child(12),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["tt1"]);
        assert_eq!(page.queried, 1);
        assert_eq!(page.failed, 0);
        // The short page exhausted the catalog; filtering never extends paging.
        assert!(!page.has_more);

        // Adult (allow-all): the same page passes through untouched.
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["tt1", "tt2", "tt3", "tt4", "tt5"]);
    }

    #[tokio::test]
    async fn pagination_to_the_end() {
        let mut routes = HashMap::new();
        let pages: Vec<(u32, usize)> = vec![(0, PAGE_SIZE), (1, PAGE_SIZE), (2, 50)];
        for (page, count) in &pages {
            let start = *page as usize * PAGE_SIZE;
            let ids: Vec<String> = (start..start + count).map(|i| format!("tt{i}")).collect();
            let ids: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            routes.extend([route("top", "Action", page * 100, &metas("movie", &ids))]);
        }
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action"])),
        )];
        let client = AddonClient::new().unwrap();
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );

        let mut total = 0;
        for expected_new in [PAGE_SIZE, PAGE_SIZE, 50, 0] {
            let plan = browser.plan();
            let responses = fetch_page(&client, &plan).await;
            let page = browser.absorb(&plan, &responses).unwrap();
            assert_eq!(page.items.len(), expected_new);
            total += page.items.len();
        }
        assert_eq!(total, 250);
        // All catalogs exhausted: nothing more to query.
        let plan = browser.plan();
        assert!(plan.requests.is_empty());
    }

    #[tokio::test]
    async fn failing_addon_does_not_break_the_rest() {
        // Addon A serves one item; addon B's routes are missing (404 = failure).
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &metas("movie", &["tt1"]))]);
        let server = FakeServer::start(routes).await;
        let addons = vec![
            addon(
                server.url.as_str(),
                manifest("top", serde_json::json!(["Action"])),
            ),
            addon(
                format!("{}/broken", server.url).as_str(),
                manifest("second", serde_json::json!(["Action"])),
            ),
        ];
        let client = AddonClient::new().unwrap();
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        assert_eq!(plan.requests.len(), 2);
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        assert_eq!(page.queried, 2);
        assert_eq!(page.failed, 1); // addon B failed
        assert_eq!(page.items.len(), 1); // addon A's item still merged
        assert!(page.has_more); // the failed one is not exhausted: retried next page
    }

    #[tokio::test]
    async fn stale_page_discarded_after_selection_change() {
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &metas("movie", &["tt1"]))]);
        routes.extend([route("top", "Comedy", 0, &metas("movie", &["tt9"]))]);
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action", "Comedy"])),
        )];
        let client = AddonClient::new().unwrap();
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;

        // The user switches genre while the page was in flight.
        let old_generation = browser.generation();
        browser.set_selection("Comedy", catalogs_for_genre(&addons, "movie", "Comedy"));
        assert_eq!(browser.generation(), old_generation + 1);

        // The stale page is dropped wholesale.
        assert!(browser.absorb(&plan, &responses).is_none());
        // The new selection still starts from scratch.
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "tt9");
    }

    #[tokio::test]
    async fn exhausted_catalogs_not_requeried() {
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &metas("movie", &["tt1"]))]);
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action"])),
        )];
        let client = AddonClient::new().unwrap();
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        let responses = fetch_page(&client, &plan).await;
        let page = browser.absorb(&plan, &responses).unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(!page.has_more);
        // Exhausted: the next plan queries nothing.
        assert!(browser.plan().requests.is_empty());
    }

    #[tokio::test]
    async fn skip_advances_by_returned_count() {
        // A catalog that ignores skip and always returns the same page would
        // otherwise loop forever: we mark it exhausted after one short page.
        let mut routes = HashMap::new();
        routes.extend([route("top", "Action", 0, &metas("movie", &["tt1", "tt2"]))]);
        routes.extend([route("top", "Action", 2, &metas("movie", &["tt1", "tt2"]))]);
        let server = FakeServer::start(routes).await;
        let addons = vec![addon(
            server.url.as_str(),
            manifest("top", serde_json::json!(["Action"])),
        )];
        let client = AddonClient::new().unwrap();
        let mut browser = GenreBrowser::new(
            "movie",
            "Action",
            catalogs_for_genre(&addons, "movie", "Action"),
            RatingPolicy::default(),
        );
        let plan = browser.plan();
        assert_eq!(plan.requests[0].skip, 0);
        let responses = fetch_page(&client, &plan).await;
        let _ = browser.absorb(&plan, &responses).unwrap();
        // skip advanced by the number of returned items (short page = exhausted anyway).
        assert_eq!(browser.skip[0], 2);
    }
}
