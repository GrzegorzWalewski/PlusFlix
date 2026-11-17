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
    ext_subs: ExternalSubs,
    pending: usize,
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

    /// Registers background tasks (addon requests) that will report back.
    pub fn begin_tasks(&self, n: usize) {
        self.lock().pending += n;
        self.bump();
    }

    pub fn task_done(&self) {
        {
            let mut st = self.lock();
            st.pending = st.pending.saturating_sub(1);
        }
        self.bump();
    }

    /// Called when loading is abandoned (loader dropped) so waiters don't hang.
    pub fn cancel_pending(&self) {
        self.lock().pending = 0;
        self.bump();
    }

    pub fn is_loading(&self) -> bool {
        self.lock().pending > 0
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

    /// Best candidate that passes the profile's filters and has not been tried yet.
    pub fn next_untried(&self, settings: &Settings) -> Option<Pick> {
        let st = self.lock();
        let mut best: Option<(crate::select::Rank, &Candidate)> = None;
        for c in st.candidates.iter().filter(|c| !st.tried.contains(&c.key)) {
            let Ok(rank) = evaluate(&c.info, settings, &st.ext_subs) else {
                continue;
            };
            let better = match &best {
                None => true,
                // Higher rank wins; on ties the earlier arrival (earlier addon) wins.
                Some((r, b)) => rank > *r || (rank == *r && c.seq < b.seq),
            };
            if better {
                best = Some((rank, c));
            }
        }
        best.map(|(_, c)| Pick {
            subtitles: settings
                .subtitle_language
                .map(|l| st.ext_subs.for_lang(l))
                .unwrap_or_default(),
            candidate: c.clone(),
        })
    }

    /// Every known candidate with how the profile's settings treat it, for the sources list.
    pub fn rows(&self, settings: &Settings) -> Vec<SourceRow> {
        let st = self.lock();
        let mut rows: Vec<SourceRow> = st
            .candidates
            .iter()
            .map(|c| {
                let tried = st.tried.contains(&c.key);
                match evaluate(&c.info, settings, &st.ext_subs) {
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
                }
            })
            .collect();
        // Usable (best-ranked first) before hidden.
        rows.sort_by_key(|r| (r.rejected.is_some(), std::cmp::Reverse(r.rank)));
        rows
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
            pending_tasks: st.pending,
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
        let rows = pool.rows(&s);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].rejected.is_none() && !rows[0].tried);
        assert_eq!(
            rows[1].rejected,
            Some(crate::select::Reject::BelowMinQuality)
        );
        pool.mark_tried("https://x/1080");
        assert!(pool.rows(&s)[0].tried);
    }

    #[test]
    fn loading_state() {
        let pool = SourcePool::new();
        assert!(!pool.is_loading());
        pool.begin_tasks(2);
        pool.task_done();
        assert!(pool.is_loading());
        pool.cancel_pending();
        assert!(!pool.is_loading());
    }
}
