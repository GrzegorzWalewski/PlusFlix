//! Catalog rows for the Home / Movies / Series pages.

use crate::addon::{Addon, AddonClient};
use crate::model::{CatalogDecl, MetaPreview};
use futures::future::join_all;
use std::time::Duration;
use tokio::time::timeout;

/// One browsable catalog to fetch. Jobs are independent: the caller can run
/// them concurrently and show each row as soon as its job finishes.
#[derive(Debug, Clone)]
pub struct CatalogJob {
    /// Display name of the catalog (e.g. "Popular").
    pub title: String,
    /// Display name of the addon serving it (shown as a caption next to the row title).
    pub addon_name: String,
    pub kind: String,
    addon: Addon,
    cat: CatalogDecl,
}

/// The browsable catalogs to fetch, in the order the account lists its addons.
/// `kind` = `Some("movie")` / `Some("series")` restricts the type; `None` mixes everything (Home).
pub fn catalog_jobs(addons: &[Addon], kind: Option<&str>, max_rows: usize) -> Vec<CatalogJob> {
    let mut wanted = Vec::new();
    for addon in addons {
        for cat in &addon.manifest.catalogs {
            if !cat.browsable() || kind.is_some_and(|k| k != cat.kind) {
                continue;
            }
            let name = cat.name.clone().unwrap_or_else(|| cat.id.clone());
            wanted.push(CatalogJob {
                title: name,
                addon_name: addon.name().to_owned(),
                kind: cat.kind.clone(),
                addon: addon.clone(),
                cat: cat.clone(),
            });
        }
    }
    wanted.truncate(max_rows);
    wanted
}

/// Fetches one catalog job, with a timeout. `None` = empty, failed or timed out.
pub async fn fetch_job(client: &AddonClient, job: &CatalogJob) -> Option<Vec<MetaPreview>> {
    let items = timeout(
        Duration::from_secs(15),
        client.catalog(&job.addon, &job.cat.kind, &job.cat.id, None),
    )
    .await;
    match items {
        Ok(Ok(items)) if !items.is_empty() => Some(items),
        Ok(Err(e)) => {
            tracing::warn!("catalog {} / {} failed: {e}", job.addon.name(), job.cat.id);
            None
        }
        _ => None,
    }
}

/// Searches every addon catalog that supports the `search` extra, mixed kinds, deduped.
pub async fn search(client: &AddonClient, addons: &[Addon], query: &str) -> Vec<MetaPreview> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let wanted: Vec<(&Addon, &crate::model::CatalogDecl)> = addons
        .iter()
        .flat_map(|a| a.manifest.catalogs.iter().map(move |cat| (a, cat)))
        .filter(|(_, cat)| cat.extra.iter().any(|e| e.name == "search"))
        .collect();

    let fetches = wanted.into_iter().map(|(addon, cat)| async move {
        let extra = format!("search={q}");
        let items = timeout(
            Duration::from_secs(15),
            client.catalog(addon, &cat.kind, &cat.id, Some(&extra)),
        )
        .await;
        match items {
            Ok(Ok(items)) => Some(items),
            Ok(Err(e)) => {
                tracing::warn!("search {} / {} failed: {e}", addon.name(), cat.id);
                None
            }
            Err(_) => {
                tracing::warn!("search {} / {} timed out", addon.name(), cat.id);
                None
            }
        }
    });
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for items in join_all(fetches).await.into_iter().flatten() {
        for m in items {
            if seen.insert((m.kind.clone(), m.id.clone())) {
                out.push(m);
            }
        }
    }
    out
}
