//! Genre browsing (Netflix-style): the union of genre options the installed
//! addons really support, plus a paged multi-catalog browser for one genre.
//!
//! Never assume an addon supports genres: everything here is driven by what
//! each addon's manifest declares at runtime. Catalogs whose `genre` options
//! are all 4-digit numbers are year filters, not genres, and are excluded
//! (see `ExtraDecl::is_year_options` for the future Year-filter extension point).

use crate::addon::{Addon, AddonClient};
use crate::model::{CatalogDecl, MetaPreview};
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
    pub fn supports_genre(&self, genre: &str) -> bool {
        let options = self.cat.genre_options();
        options.is_empty() || options.iter().any(|o| o.eq_ignore_ascii_case(genre))
    }
}

/// Every catalog of `kind` that can be browsed by genre: it declares a `genre`
/// extra (in either manifest format), requires no other mandatory extras, and
/// doesn't hide a year filter behind it.
pub fn genre_catalogs(addons: &[Addon], kind: &str) -> Vec<CatalogRef> {
    let mut out = Vec::new();
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
            out.push(CatalogRef {
                addon_name: addon.name().to_owned(),
                catalog_name: cat.name.clone().unwrap_or_else(|| cat.id.clone()),
                kind: cat.kind.clone(),
                addon_url: addon.transport_url.clone(),
                addon: addon.clone(),
                cat: cat.clone(),
            });
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

/// The genres browseable for `kind`: the case-insensitive union of the genre
/// options of all catalogs that accept a genre, keeping the first-seen
/// spelling, sorted alphabetically. Never assumed: only what some addon's
/// manifest really declares.
pub fn genre_options(addons: &[Addon], kind: &str) -> Vec<GenreOption> {
    let catalogs = genre_catalogs(addons, kind);
    let mut genres: Vec<(String, GenreOption)> = Vec::new();
    for catalog in &catalogs {
        for name in catalog.genre_options() {
            let key = name.to_lowercase();
            match genres.iter_mut().find(|(k, _)| *k == key) {
                Some((_, option)) => {
                    if !option
                        .catalogs
                        .iter()
                        .any(|c| c.addon_url == catalog.addon_url && c.cat.id == catalog.cat.id)
                    {
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
}

impl GenreBrowser {
    pub fn new(kind: &str, genre: &str, catalogs: Vec<CatalogRef>) -> Self {
        let count = catalogs.len();
        Self {
            kind: kind.to_owned(),
            genre: genre.to_owned(),
            catalogs,
            skip: vec![0; count],
            exhausted: vec![false; count],
            seen: HashSet::new(),
            generation: 0,
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

    /// Manifest with one movie catalog `id` whose `genre` extra takes `options`.
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
        serde_json::json!({ "id": "addon", "name": "Test Addon", "types": ["movie"], "catalogs": catalogs })
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
        let mut browser = GenreBrowser::new("movie", "Action", catalogs);
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
        );
        let plan = browser.plan();
        assert_eq!(plan.requests[0].skip, 0);
        let responses = fetch_page(&client, &plan).await;
        let _ = browser.absorb(&plan, &responses).unwrap();
        // skip advanced by the number of returned items (short page = exhausted anyway).
        assert_eq!(browser.skip[0], 2);
    }
}
