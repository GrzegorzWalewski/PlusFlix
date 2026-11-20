//! The shared pool of candidate sources for one title.
//!
//! Addons deliver streams into the pool in the background (even while the
//! player is already running); the failover session pulls the best untried
//! candidate out of it whenever it needs one.

use crate::lang::Lang;
use crate::model::{Stream, Subtitle};
use crate::parse::{analyze, StreamInfo};
use crate::select::{evaluate, ExternalSubs, Reject};
use crate::settings::Settings;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Candidate {
    /// Unique key (the stream URL).
    pub key: String,
    pub url: String,
    pub addon: String,
    pub label: String,
    pub stream: Stream,
    pub info: StreamInfo,
    seq: u64,
}

/// What the session needs to start playing a candidate.
#[derive(Debug, Clone)]
pub struct Pick {
    pub candidate: Candidate,
    /// External subtitles in the profile's subtitle language (may be empty).
    pub subtitles: Vec<Subtitle>,
}

#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    pub total: usize,
    pub usable: usize,
    pub untried_usable: usize,
    pub not_playable: usize,
    pub rejected: HashMap<Reject, usize>,
    pub pending_tasks: usize,
}

/// One row of the sources list: a candidate and whether the settings allow it.
#[derive(Debug, Clone)]
pub struct SourceRow {
    /// Unique candidate key (the stream URL) for manual selection.
    pub key: String,
    pub label: String,
    /// Already attempted by the session.
    pub tried: bool,
    /// `None` if the profile's settings rejected the source.
    pub rejected: Option<crate::select::Reject>,
    rank: Option<crate::select::Rank>,
}

#[derive(Default)]
struct State {
    candidates: Vec<Candidate>,
    keys: HashSet<String>,
    tried: HashSet<String>,
    /// Manually skipped by the user: still eligible, just last in line.
    skipped: HashSet<String>,
    ext_subs: ExternalSubs,
    /// In-flight loader tasks per addon name.
    pending: HashMap<String, usize>,
    not_playable: usize,
    seq: u64,
}

#[derive(Clone)]
pub struct SourcePool {
    state: Arc<Mutex<State>>,
    tx: Arc<watch::Sender<u64>>,
}

impl Default for SourcePool {
    fn default() -> Self {
        Self::new()
    }
}

impl SourcePool {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(0u64);
        Self {
            state: Arc::new(Mutex::new(State::default())),
            tx: Arc::new(tx),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Wakes up whenever sources, subtitles or loading state change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }

    /// Registers one background task (addon request) that will report back.
    pub fn begin_task(&self, addon: &str) {
        *self.lock().pending.entry(addon.to_owned()).or_insert(0) += 1;
        self.bump();
    }

    pub fn task_done(&self, addon: &str) {
        {
            let mut st = self.lock();
            if let Some(n) = st.pending.get_mut(addon) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    st.pending.remove(addon);
                }
            }
        }
        self.bump();
    }

    /// True while the addon still has in-flight loader tasks.
    pub fn addon_pending(&self, addon: &str) -> bool {
        self.lock().pending.contains_key(addon)
    }

    /// Called when loading is abandoned (loader dropped) so waiters don't hang.
    pub fn cancel_pending(&self) {
        self.lock().pending.clear();
        self.bump();
    }

    pub fn is_loading(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    pub fn add_streams(&self, addon: &str, streams: Vec<Stream>) {
        {
            let mut st = self.lock();
            for stream in streams {
                let Some(url) = stream.http_url().map(str::to_owned) else {
                    st.not_playable += 1;
                    continue;
                };
                if !st.keys.insert(url.clone()) {
                    continue;
                }
                let info = analyze(&stream);
                st.seq += 1;
                let seq = st.seq;
                let label = describe(addon, &stream, &info);
                st.candidates.push(Candidate {
                    key: url.clone(),
                    url,
                    addon: addon.to_owned(),
                    label,
                    stream,
                    info,
                    seq,
                });
            }
        }
        self.bump();
    }

    pub fn add_subtitles(&self, subs: Vec<Subtitle>) {
        {
            let mut st = self.lock();
            for sub in subs {
                if let Some(lang) = Lang::parse(&sub.lang) {
                    if !st.ext_subs.list.iter().any(|(_, s)| s.url == sub.url) {
                        st.ext_subs.list.push((lang, sub));
                    }
                }
            }
        }
        self.bump();
    }

    pub fn mark_tried(&self, key: &str) {
        self.lock().tried.insert(key.to_owned());
    }

    /// Manual skip: the candidate is *not* failed, but it only gets picked
    /// once every unskipped candidate has been exhausted.
    pub fn mark_skipped(&self, key: &str) {
        self.lock().skipped.insert(key.to_owned());
    }

    /// Best candidate that passes the profile's filters and has not been tried yet.
    pub fn next_untried(&self, settings: &Settings) -> Option<Pick> {
        self.best_untried(settings, &[], &[])
    }

    /// Best untried candidate, with `preferred` addons' sources ranked ahead
    /// of everything else ("Wait for this addon"): a preferred source always
    /// beats a better-ranked one from a non-preferred addon.
    pub fn next_untried_preferred(
        &self,
        preferred: &[String],
        settings: &Settings,
    ) -> Option<Pick> {
        self.best_untried(settings, &[], preferred)
    }

    /// Best untried candidate coming only from `addons` (the wait-gate pick).
    /// Picks the best *across* all of them, not the first in list order.
    pub fn next_untried_from(&self, addons: &[String], settings: &Settings) -> Option<Pick> {
        self.best_untried(settings, addons, addons)
    }

    /// `addons`: when non-empty, only candidates from these addons count
    /// (the wait-gate). `preferred`: candidates from these addons rank ahead
    /// of all others, regardless of quality or arrival order.
    fn best_untried(
        &self,
        settings: &Settings,
        addons: &[String],
        preferred: &[String],
    ) -> Option<Pick> {
        let st = self.lock();
        let mut best: Option<(bool, crate::select::Rank, &Candidate)> = None;
        // Manually skipped sources stay in the failover queue, but only
        // after every unskipped one is exhausted.
        let mut best_skipped: Option<(bool, crate::select::Rank, &Candidate)> = None;
        for c in st
            .candidates
            .iter()
            .filter(|c| {
                (addons.is_empty() || addons.iter().any(|a| *a == c.addon))
                    && !st.tried.contains(&c.key)
            })
        {
            let Ok(rank) = evaluate(&c.info, settings, &st.ext_subs) else {
                continue;
            };
            // Preferred ("wait for this addon") sources always rank first.
            let is_preferred = preferred.iter().any(|a| *a == c.addon);
            let slot = if st.skipped.contains(&c.key) {
                &mut best_skipped
            } else {
                &mut best
            };
            let better = match slot {
                None => true,
                // Preferred beats non-preferred; then higher rank wins; on
                // ties the earlier arrival (earlier addon) wins.
                Some((p, r, b)) => {
                    (is_preferred, rank) > (*p, *r)
                        || ((is_preferred, rank) == (*p, *r) && c.seq < b.seq)
                }
            };
            if better {
                *slot = Some((is_preferred, rank, c));
            }
        }
        best.or(best_skipped).map(|(_, _, c)| Pick {
            subtitles: settings
                .subtitle_language
                .map(|l| st.ext_subs.for_lang(l))
                .unwrap_or_default(),
            candidate: c.clone(),
        })
    }

    /// Every known candidate with how the profile's settings treat it, for the sources list.
    ///
    /// `preferred` addons' rows come first ("Wait for this addon"), then by
    /// rank as usual; rejected rows stay last.
    pub fn rows(&self, settings: &Settings, preferred: &[String]) -> Vec<SourceRow> {
        let st = self.lock();
        let mut rows: Vec<(bool, SourceRow)> = st
            .candidates
            .iter()
            .map(|c| {
                let preferred_row = preferred.iter().any(|a| *a == c.addon);
                let tried = st.tried.contains(&c.key);
                let row = match evaluate(&c.info, settings, &st.ext_subs) {
                    Ok(rank) => SourceRow {
                        key: c.key.clone(),
                        label: c.label.clone(),
                        tried,
                        rejected: None,
                        rank: Some(rank),
                    },
                    Err(r) => SourceRow {
                        key: c.key.clone(),
                        label: c.label.clone(),
                        tried,
                        rejected: Some(r),
                        rank: None,
                    },
                };
                (preferred_row, row)
            })
            .collect();
        // Usable (preferred first, then best-ranked) before hidden.
        rows.sort_by_key(|(pref, r)| (r.rejected.is_some(), !*pref, std::cmp::Reverse(r.rank)));
        rows.into_iter().map(|(_, r)| r).collect()
    }

    /// A specific candidate by key (manual pick from the sources list),
    /// regardless of tried state or the profile's filters.
    pub fn pick_by_key(&self, key: &str, settings: &Settings) -> Option<Pick> {
        let st = self.lock();
        let c = st.candidates.iter().find(|c| c.key == key)?;
        Some(Pick {
            subtitles: settings
                .subtitle_language
                .map(|l| st.ext_subs.for_lang(l))
                .unwrap_or_default(),
            candidate: c.clone(),
        })
    }

    pub fn stats(&self, settings: &Settings) -> PoolStats {
        let st = self.lock();
        let mut stats = PoolStats {
            total: st.candidates.len(),
            not_playable: st.not_playable,
            pending_tasks: st.pending.values().sum(),
            ..Default::default()
        };
        for c in &st.candidates {
            match evaluate(&c.info, settings, &st.ext_subs) {
                Ok(_) => {
                    stats.usable += 1;
                    if !st.tried.contains(&c.key) {
                        stats.untried_usable += 1;
                    }
                }
                Err(r) => *stats.rejected.entry(r).or_default() += 1,
            }
        }
        stats
    }
}

fn describe(addon: &str, stream: &Stream, info: &StreamInfo) -> String {
    let mut parts = vec![addon.to_owned()];
    if info.quality != crate::settings::Quality::Unknown {
        parts.push(info.quality.label().to_owned());
    }
    if let Some(sz) = info.size_bytes {
        parts.push(format!("{:.1} GB", sz as f64 / (1024.0 * 1024.0 * 1024.0)));
    }
    if let Some(l) = info.langs.audio.first() {
        parts.push(l.code2().to_uppercase());
    }
    let _ = stream;
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn http_stream(url: &str, name: &str) -> Stream {
        Stream {
            url: Some(url.into()),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    #[test]
    fn dedupes_and_skips_non_http() {
        let pool = SourcePool::new();
        let torrent = Stream {
            info_hash: Some("abc".into()),
            ..Default::default()
        };
        pool.add_streams(
            "A",
            vec![
                http_stream("https://x/1", "1080p"),
                torrent,
                http_stream("https://x/1", "1080p"),
            ],
        );
        let st = pool.stats(&Settings::default());
        assert_eq!((st.total, st.not_playable), (1, 1));
    }

    #[test]
    fn picks_best_then_next_after_mark_tried() {
        let pool = SourcePool::new();
        pool.add_streams(
            "A",
            vec![
                http_stream("https://x/720", "720p"),
                http_stream("https://x/1080", "1080p"),
            ],
        );
        let s = Settings::default();
        assert_eq!(
            pool.next_untried(&s).unwrap().candidate.url,
            "https://x/1080"
        );
        pool.mark_tried("https://x/1080");
        assert_eq!(
            pool.next_untried(&s).unwrap().candidate.url,
            "https://x/720"
        );
        pool.mark_tried("https://x/720");
        assert!(pool.next_untried(&s).is_none());
    }

    #[test]
    fn late_subtitles_make_a_source_usable() {
        use crate::settings::SubtitleRequirement;
        let pool = SourcePool::new();
        pool.add_streams("A", vec![http_stream("https://x/1", "1080p ENG")]);
        let s = Settings {
            audio_language: Some(Lang::Pl),
            subtitle_language: Some(Lang::Pl),
            subtitle_requirement: SubtitleRequirement::RequiredIfAudioNotPreferred,
            ..Default::default()
        };
        assert!(pool.next_untried(&s).is_none());
        pool.add_subtitles(vec![Subtitle {
            id: None,
            url: "https://s/pl.srt".into(),
            lang: "pol".into(),
        }]);
        let pick = pool.next_untried(&s).unwrap();
        assert_eq!(pick.subtitles.len(), 1);
    }

    #[test]
    fn rows_show_rejections_and_ranking() {
        let pool = SourcePool::new();
        pool.add_streams(
            "A",
            vec![
                http_stream("https://x/720", "720p"),
                http_stream("https://x/1080", "1080p"),
            ],
        );
        let s = Settings {
            min_quality: crate::settings::Quality::P1080,
            ..Default::default()
        };
        let rows = pool.rows(&s, &[]);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].rejected.is_none() && !rows[0].tried);
        assert_eq!(
            rows[1].rejected,
            Some(crate::select::Reject::BelowMinQuality)
        );
        pool.mark_tried("https://x/1080");
        assert!(pool.rows(&s, &[])[0].tried);
    }

    #[test]
    fn skipped_candidates_rank_last_but_stay_eligible() {
        let pool = SourcePool::new();
        pool.add_streams(
            "A",
            vec![
                http_stream("https://x/1080", "1080p"),
                http_stream("https://x/720", "720p"),
            ],
        );
        let s = Settings::default();
        pool.mark_skipped("https://x/1080");
        // The skipped 1080 loses to the unskipped 720…
        assert_eq!(
            pool.next_untried(&s).unwrap().candidate.url,
            "https://x/720"
        );
        // …but stays in the queue once nothing else is left.
        pool.mark_tried("https://x/720");
        assert_eq!(
            pool.next_untried(&s).unwrap().candidate.url,
            "https://x/1080"
        );
    }

    #[test]
    fn loading_state() {
        let pool = SourcePool::new();
        assert!(!pool.is_loading());
        pool.begin_task("A");
        pool.begin_task("B");
        assert!(pool.is_loading());
        pool.task_done("A");
        assert!(!pool.addon_pending("A"));
        assert!(pool.addon_pending("B"));
        pool.task_done("B");
        assert!(!pool.is_loading());
        pool.begin_task("C");
        pool.cancel_pending();
        assert!(!pool.addon_pending("C"));
        assert!(!pool.is_loading());
    }

    #[test]
    fn addon_pending_tracks_addons_independently() {
        let pool = SourcePool::new();
        pool.begin_task("A");
        pool.begin_task("A");
        assert_eq!(pool.stats(&Settings::default()).pending_tasks, 2);
        pool.task_done("A");
        assert!(pool.addon_pending("A"));
        pool.task_done("A");
        assert!(!pool.addon_pending("A"));
    }

    #[test]
    fn next_untried_from_ignores_other_addons_candidates() {
        let pool = SourcePool::new();
        pool.add_streams("A", vec![http_stream("https://x/a", "1080p")]);
        pool.add_streams("B", vec![http_stream("https://x/b", "2160p")]);
        let s = Settings::default();
        // Pool-wide pick prefers B's better source.
        assert_eq!(pool.next_untried(&s).unwrap().candidate.url, "https://x/b");
        // Restricted pick only considers A's candidates.
        assert_eq!(
            pool.next_untried_from(&["A".to_owned()], &s)
                .unwrap()
                .candidate
                .url,
            "https://x/a"
        );
        assert!(pool.next_untried_from(&["Missing".to_owned()], &s).is_none());
    }

    #[test]
    fn preferred_addon_beats_better_ranked_other_addons() {
        let pool = SourcePool::new();
        // B's source is much better, but A is "wait for this addon"-toggled.
        pool.add_streams("A", vec![http_stream("https://x/a480", "480p")]);
        pool.add_streams("B", vec![http_stream("https://x/b1080", "1080p")]);
        let s = Settings::default();
        assert_eq!(
            pool.next_untried(&s).unwrap().candidate.url,
            "https://x/b1080"
        );
        let preferred = vec!["A".to_owned()];
        assert_eq!(
            pool.next_untried_preferred(&preferred, &s)
                .unwrap()
                .candidate
                .url,
            "https://x/a480"
        );
        // …also when A's source arrived last (seq does not matter).
        let pool = SourcePool::new();
        pool.add_streams("B", vec![http_stream("https://x/b1080", "1080p")]);
        pool.add_streams("A", vec![http_stream("https://x/a480", "480p")]);
        assert_eq!(
            pool.next_untried_preferred(&preferred, &s)
                .unwrap()
                .candidate
                .url,
            "https://x/a480"
        );
    }

    #[test]
    fn preferred_addon_rows_sort_first() {
        let pool = SourcePool::new();
        pool.add_streams("A", vec![http_stream("https://x/a480", "480p")]);
        pool.add_streams("B", vec![http_stream("https://x/b1080", "1080p")]);
        let s = Settings::default();
        // Without the toggle, best rank first.
        assert_eq!(
            pool.rows(&s, &[])[0].key,
            "https://x/b1080"
        );
        // With it, A's (worse) row comes first; rejected rows stay last.
        let preferred = vec!["A".to_owned()];
        let rows = pool.rows(&s, &preferred);
        assert_eq!(rows[0].key, "https://x/a480");
        assert_eq!(rows[1].key, "https://x/b1080");
    }

    #[test]
    fn gate_pick_takes_the_best_across_all_gated_addons() {
        let pool = SourcePool::new();
        // Listed first but worse: the gate pick must not follow list order.
        pool.add_streams("A", vec![http_stream("https://x/a720", "720p")]);
        pool.add_streams("B", vec![http_stream("https://x/b1080", "1080p")]);
        let s = Settings::default();
        let gated = vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(
            pool.next_untried_from(&gated, &s).unwrap().candidate.url,
            "https://x/b1080"
        );
    }
}
