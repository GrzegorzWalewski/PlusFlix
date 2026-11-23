//! Playback with automatic failover.
//!
//! The session starts the best source from the pool. If the player errors,
//! never starts, stalls for too long, or the stream ends suspiciously early,
//! the next best untried source is loaded at the position where playback
//! stopped. Sources keep arriving in the pool the whole time, so there is
//! always a backup as long as any addon has something left to offer.

use crate::player::{LoadRequest, Player, PlayerEvent};
use crate::pool::{Pick, SourcePool};
use crate::settings::Settings;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{sleep_until, timeout, Instant};

/// Default max time for a source to open and start playing
/// ([`SessionConfig::start_timeout`]); user-configurable per profile.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(25);
/// Default max time playback may stay stalled before switching
/// ([`SessionConfig::stall_timeout`]); user-configurable per profile.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-profile failover timeouts ([`Settings::start_timeout_secs`] /
/// [`Settings::stall_timeout_secs`], both `None` = built-in default) scaled
/// for slow-network pre-buffering ([`Settings::prebuffer_secs`]): a start
/// that deliberately spends time filling the player's cache before
/// playback must not be mistaken for a dead or stalled source by the
/// failover loop. An explicit timeout always wins unchanged; otherwise
/// the defaults are only ever raised (never lowered) — the start window
/// gets twice the pre-buffer plus 10 s, the stall tolerance twice the
/// pre-buffer.
pub fn scaled_timeouts(
    start_secs: Option<u64>,
    stall_secs: Option<u64>,
    prebuffer_secs: Option<u64>,
) -> (Duration, Duration) {
    let prebuffer = Duration::from_secs(prebuffer_secs.unwrap_or(0));
    let start = match start_secs {
        Some(s) => Duration::from_secs(s),
        None => DEFAULT_START_TIMEOUT.max(prebuffer * 2 + Duration::from_secs(10)),
    };
    let stall = match stall_secs {
        Some(s) => Duration::from_secs(s),
        None => DEFAULT_STALL_TIMEOUT.max(prebuffer * 2),
    };
    (start, stall)
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Max time for a source to open and start playing.
    pub start_timeout: Duration,
    /// Max time playback may stay stalled (buffering) before switching.
    pub stall_timeout: Duration,
    /// How long to wait for *any* usable source to show up.
    pub no_source_timeout: Duration,
    /// Seconds to step back when resuming on another source.
    pub rewind_secs: f64,
    /// An EOF this far before the known duration is treated as a dropped connection.
    pub premature_eof_secs: f64,
    pub max_attempts: usize,
    /// Manual pick: try this candidate key first (user chose it from the
    /// sources list), then fall back to the normal best-untried flow.
    /// Only honored for the very first pick, so a failing preferred source
    /// cannot be re-picked forever.
    pub first_key: Option<String>,
    /// Addons (by name) with the profile's "Wait for this addon" toggle:
    /// their sources are always prioritized — every automatic pick (first
    /// and failover) prefers them over better-ranked sources from other
    /// addons, and they sort first in the sources list. Additionally, the
    /// FIRST pick waits for these addons to finish loading before falling
    /// back to other addons' sources.
    pub wait_addons: Vec<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            start_timeout: DEFAULT_START_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            no_source_timeout: Duration::from_secs(90),
            rewind_secs: 3.0,
            premature_eof_secs: 30.0,
            max_attempts: 40,
            first_key: None,
            wait_addons: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Trying {
        label: String,
        attempt: usize,
    },
    /// A source actually started playing; `key` identifies it for callers
    /// that want to remember the last-played source.
    Playing {
        label: String,
        key: String,
    },
    /// A source failed (load error, stall, early EOF, player error).
    Failed {
        label: String,
        reason: String,
    },
    /// The user skipped a source: not a failure, it just moves to the back
    /// of the failover queue.
    Skipped {
        label: String,
    },
    WaitingForSources,
    Position {
        secs: f64,
        duration: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Played to the end.
    Finished { position: f64 },
    /// The user closed the player.
    Closed { position: f64 },
    /// No source passed the profile's filters.
    NoSources,
    /// Every usable source failed (or the attempt budget ran out).
    Exhausted { position: f64 },
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
}

pub async fn run<P: Player>(
    player: &mut P,
    pool: &SourcePool,
    settings: &Settings,
    cfg: &SessionConfig,
    start_secs: f64,
    events: Option<UnboundedSender<SessionEvent>>,
) -> Outcome {
    let emit = |e: SessionEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(e);
        }
    };

    let mut position = start_secs;
    let mut duration: Option<f64> = None;
    let mut attempts = 0usize;
    let mut rx = pool.subscribe();
    // Consumed on the first pick only: a failing preferred source must not
    // be re-picked on every failover iteration.
    let mut first_key = cfg.first_key.clone();

    'sources: loop {
        // ---- pick the next source (waiting for the loaders if necessary) ----
        let pick: Pick = {
            // A manually chosen source (from the sources list) goes first,
            // even if the settings would have hidden it.
            if let Some(p) = first_key
                .take()
                .and_then(|k| pool.pick_by_key(&k, settings))
            {
                p
            } else {
                let give_up_at = Instant::now() + cfg.no_source_timeout;
                // Wait-gate: for the FIRST pick only, sources from the
                // toggled addons count until each of them is done loading;
                // failover picks go straight to the pool-wide pick (which
                // still ranks the toggled addons' sources first).
                let mut gated = attempts == 0 && !cfg.wait_addons.is_empty();
                loop {
                    let picked = if gated {
                        // Best across all gated addons (not the first in the
                        // list that happens to have something).
                        pool.next_untried_from(&cfg.wait_addons, settings)
                    } else {
                        pool.next_untried_preferred(&cfg.wait_addons, settings)
                    };
                    if let Some(p) = picked {
                        break p;
                    }
                    if gated && !cfg.wait_addons.iter().any(|a| pool.addon_pending(a)) {
                        // Every gated addon finished with nothing usable:
                        // lift the gate and fall back to all addons.
                        gated = false;
                        continue;
                    }
                    if !pool.is_loading() {
                        // Nothing left and nobody is going to deliver more.
                        return if attempts == 0 {
                            Outcome::NoSources
                        } else {
                            Outcome::Exhausted { position }
                        };
                    }
                    emit(SessionEvent::WaitingForSources);
                    // The player window hides the app's status text; show it on the player too.
                    player.notify("Waiting for sources…").await;
                    let remaining = give_up_at.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return if attempts == 0 {
                            Outcome::NoSources
                        } else {
                            Outcome::Exhausted { position }
                        };
                    }
                    // While waiting, keep an eye on the player itself: if it is
                    // closed or dies before any source is available, end the
                    // session now instead of burning the timeout (queued events
                    // would make every later load fail anyway).
                    let ev = tokio::select! {
                        changed = timeout(remaining, rx.changed()) => {
                            if changed.is_err() {
                                return if attempts == 0 {
                                    Outcome::NoSources
                                } else {
                                    Outcome::Exhausted { position }
                                };
                            }
                            continue;
                        }
                        ev = player.next_event() => ev,
                    };
                    if matches!(
                        ev,
                        None | Some(PlayerEvent::Closed) | Some(PlayerEvent::Error(_))
                    ) {
                        player.stop().await;
                        return Outcome::Closed { position };
                    }
                }
            }
        };
        attempts += 1;
        if attempts > cfg.max_attempts {
            return Outcome::Exhausted { position };
        }
        let label = pick.candidate.label.clone();
        let key = pick.candidate.key.clone();
        emit(SessionEvent::Trying {
            label: label.clone(),
            attempt: attempts,
        });
        if attempts > 1 {
            player.notify("Source failed – switching…").await;
        }

        let resume = if attempts > 1 && position > cfg.rewind_secs + 2.0 {
            position - cfg.rewind_secs
        } else {
            position
        };
        let req = LoadRequest {
            url: pick.candidate.url.clone(),
            headers: pick
                .candidate
                .stream
                .behavior_hints
                .proxy_headers
                .as_ref()
                .map(|h| h.request.clone())
                .unwrap_or_default(),
            start_secs: resume,
            audio_lang: settings.audio_language,
            subtitle_lang: settings.subtitle_language,
            subtitles: pick.subtitles.clone(),
        };

        if let Err(e) = player.load(req).await {
            pool.mark_tried(&key);
            emit(SessionEvent::Failed {
                label,
                reason: e.to_string(),
            });
            continue 'sources;
        }

        // ---- watch this source until it ends or fails ----
        let mut started = false;
        let mut buffering_since: Option<Instant> = None;
        let mut deadline = Instant::now() + cfg.start_timeout;

        loop {
            let event = tokio::select! {
                ev = player.next_event() => ev,
                _ = sleep_until(deadline) => {
                    let reason = if started { "stalled" } else { "did not start in time" };
                    pool.mark_tried(&key);
                    emit(SessionEvent::Failed { label: label.clone(), reason: reason.into() });
                    continue 'sources;
                }
            };

            match event {
                None | Some(PlayerEvent::Closed) => {
                    player.stop().await;
                    return Outcome::Closed { position };
                }
                Some(PlayerEvent::Started) => {
                    started = true;
                    deadline = far_future();
                    emit(SessionEvent::Playing {
                        label: label.clone(),
                        key: key.clone(),
                    });
                }
                Some(PlayerEvent::Position(p)) => {
                    position = p;
                    emit(SessionEvent::Position { secs: p, duration });
                }
                Some(PlayerEvent::Duration(d)) => duration = Some(d),
                Some(PlayerEvent::Buffering(true)) => {
                    let since = *buffering_since.get_or_insert_with(Instant::now);
                    if started {
                        deadline = since + cfg.stall_timeout;
                    }
                }
                Some(PlayerEvent::Buffering(false)) => {
                    buffering_since = None;
                    deadline = if started { far_future() } else { deadline };
                }
                Some(PlayerEvent::Ended) => {
                    let premature = duration.is_some_and(|d| d - position > cfg.premature_eof_secs);
                    if premature {
                        pool.mark_tried(&key);
                        emit(SessionEvent::Failed {
                            label: label.clone(),
                            reason: "stream ended early".into(),
                        });
                        continue 'sources;
                    }
                    player.stop().await;
                    return Outcome::Finished { position };
                }
                Some(PlayerEvent::Skipped) => {
                    // Manual skip: not a failure — the source keeps its place
                    // in the queue (at the very back) instead of being marked
                    // tried, so a later play of the item can start with it.
                    pool.mark_skipped(&key);
                    emit(SessionEvent::Skipped {
                        label: label.clone(),
                    });
                    continue 'sources;
                }
                Some(PlayerEvent::Error(reason)) => {
                    pool.mark_tried(&key);
                    emit(SessionEvent::Failed {
                        label: label.clone(),
                        reason,
                    });
                    continue 'sources;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::model::Stream;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone, Debug)]
    enum Script {
        /// Fails right after load.
        ErrorImmediately,
        /// Never reports Started.
        Hang,
        /// Starts, plays to `until`, then errors.
        PlayThenError { until: f64 },
        /// Starts, plays to `until`, then the user skips the source.
        PlayThenSkipped { until: f64 },
        /// Starts, plays 100 s of a 100 s file, ends.
        PlayToEnd,
        /// Starts, then drops the connection: EOF at 40 of 100 s.
        PrematureEof,
        /// Starts, stalls (buffering) for `secs` seconds — longer than the
        /// stall timeout on purpose — then resumes and plays to the end.
        StallThenPlay { secs: f64 },
        /// Reports nothing for `after` seconds — a slow-opening stream —
        /// then starts and plays to the end.
        LateStart { after: f64 },
    }

    struct MockPlayer {
        scripts: HashMap<String, Script>,
        loads: Arc<Mutex<Vec<(String, f64)>>>,
        tx: mpsc::UnboundedSender<PlayerEvent>,
        rx: mpsc::UnboundedReceiver<PlayerEvent>,
    }

    type Loads = Arc<Mutex<Vec<(String, f64)>>>;

    impl MockPlayer {
        #[allow(clippy::type_complexity)]
        fn new(scripts: &[(&str, Script)]) -> (Self, Loads) {
            let (tx, rx) = mpsc::unbounded_channel();
            let loads = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    scripts: scripts
                        .iter()
                        .map(|(k, v)| (format!("https://x/{k}"), v.clone()))
                        .collect(),
                    loads: loads.clone(),
                    tx,
                    rx,
                },
                loads,
            )
        }
    }

    impl Player for MockPlayer {
        async fn load(&mut self, req: LoadRequest) -> Result<()> {
            self.loads.lock().unwrap().push((
                req.url.trim_start_matches("https://x/").to_owned(),
                req.start_secs,
            ));
            // A real player drops events of the replaced file; do the same.
            while self.rx.try_recv().is_ok() {}
            let tx = self.tx.clone();
            let start = req.start_secs;
            match self.scripts.get(&req.url).cloned().unwrap_or(Script::Hang) {
                Script::ErrorImmediately => {
                    tx.send(PlayerEvent::Error("404".into())).unwrap();
                }
                Script::Hang => {}
                Script::PlayThenError { until } => {
                    tx.send(PlayerEvent::Started).unwrap();
                    tx.send(PlayerEvent::Position(start)).unwrap();
                    tx.send(PlayerEvent::Position(until)).unwrap();
                    tx.send(PlayerEvent::Error("connection reset".into()))
                        .unwrap();
                }
                Script::PlayThenSkipped { until } => {
                    tx.send(PlayerEvent::Started).unwrap();
                    tx.send(PlayerEvent::Position(start)).unwrap();
                    tx.send(PlayerEvent::Position(until)).unwrap();
                    tx.send(PlayerEvent::Skipped).unwrap();
                }
                Script::PlayToEnd => {
                    tx.send(PlayerEvent::Started).unwrap();
                    tx.send(PlayerEvent::Duration(100.0)).unwrap();
                    tx.send(PlayerEvent::Position(100.0)).unwrap();
                    tx.send(PlayerEvent::Ended).unwrap();
                }
                Script::PrematureEof => {
                    tx.send(PlayerEvent::Started).unwrap();
                    tx.send(PlayerEvent::Duration(100.0)).unwrap();
                    tx.send(PlayerEvent::Position(40.0)).unwrap();
                    tx.send(PlayerEvent::Ended).unwrap();
                }
                Script::StallThenPlay { secs } => {
                    tx.send(PlayerEvent::Started).unwrap();
                    tx.send(PlayerEvent::Duration(100.0)).unwrap();
                    tx.send(PlayerEvent::Position(start)).unwrap();
                    tx.send(PlayerEvent::Buffering(true)).unwrap();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                        tx.send(PlayerEvent::Buffering(false)).unwrap();
                        tx.send(PlayerEvent::Position(100.0)).unwrap();
                        tx.send(PlayerEvent::Ended).unwrap();
                    });
                }
                Script::LateStart { after } => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f64(after)).await;
                        tx.send(PlayerEvent::Started).unwrap();
                        tx.send(PlayerEvent::Duration(100.0)).unwrap();
                        tx.send(PlayerEvent::Position(100.0)).unwrap();
                        tx.send(PlayerEvent::Ended).unwrap();
                    });
                }
            }
            Ok(())
        }
        async fn next_event(&mut self) -> Option<PlayerEvent> {
            self.rx.recv().await
        }
        async fn stop(&mut self) {}
    }

    fn stream(url: &str, name: &str) -> Stream {
        Stream {
            url: Some(format!("https://x/{url}")),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    fn pool_with(streams: Vec<Stream>) -> SourcePool {
        let pool = SourcePool::new();
        pool.add_streams("A", streams);
        pool
    }

    #[tokio::test(start_paused = true)]
    async fn fails_over_through_error_and_hang_to_a_working_source() {
        // Ranked best-first by quality: 2160 (errors) > 1080 (hangs) > 720 (works).
        let pool = pool_with(vec![
            stream("u2160", "2160p"),
            stream("u1080", "1080p"),
            stream("u720", "720p"),
        ]);
        let (mut player, loads) = MockPlayer::new(&[
            ("u2160", Script::ErrorImmediately),
            ("u1080", Script::Hang),
            ("u720", Script::PlayToEnd),
        ]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["u2160", "u1080", "u720"]);
    }

    #[tokio::test(start_paused = true)]
    async fn resumes_at_the_failure_position_on_the_next_source() {
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a", Script::PlayThenError { until: 600.0 }),
            ("b", Script::PlayToEnd),
        ]);
        let cfg = SessionConfig::default();
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert!(matches!(out, Outcome::Finished { .. }));
        let loads = loads.lock().unwrap();
        assert_eq!(loads[0], ("a".to_string(), 0.0));
        assert_eq!(loads[1], ("b".to_string(), 600.0 - cfg.rewind_secs));
    }

    #[tokio::test(start_paused = true)]
    async fn premature_eof_counts_as_failure() {
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) =
            MockPlayer::new(&[("a", Script::PrematureEof), ("b", Script::PlayToEnd)]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert!(matches!(out, Outcome::Finished { .. }));
        assert_eq!(loads.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn late_arriving_sources_serve_as_backup() {
        // Only one (bad) source at first; a good one arrives later from a slow addon.
        let pool = pool_with(vec![stream("bad", "1080p")]);
        pool.begin_task("Slow");
        let late = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            late.add_streams("Slow", vec![stream("good", "720p")]);
            late.task_done("Slow");
        });
        let (mut player, loads) = MockPlayer::new(&[
            ("bad", Script::ErrorImmediately),
            ("good", Script::PlayToEnd),
        ]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert!(matches!(out, Outcome::Finished { .. }));
        assert_eq!(loads.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_sources_when_play_starts_before_any_loaded() {
        // Play clicked immediately: the pool is empty and every addon is still loading.
        let pool = SourcePool::new();
        pool.begin_task("Slow");
        let late = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            late.add_streams("Slow", vec![stream("good", "720p")]);
            late.task_done("Slow");
        });
        let (mut player, loads) = MockPlayer::new(&[("good", Script::PlayToEnd)]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn player_closed_while_waiting_for_sources_ends_the_session() {
        // The player dies while nothing has been loaded yet: the session must
        // end as Closed instead of timing out or failing every late source.
        let pool = SourcePool::new();
        pool.begin_task("Slow"); // loading forever, so the old wait loop would hang
        let (mut player, loads) = MockPlayer::new(&[("good", Script::PlayToEnd)]);
        let tx = player.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tx.send(PlayerEvent::Closed).unwrap();
        });
        let late = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            late.add_streams("Slow", vec![stream("good", "720p")]);
        });
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Closed { position: 0.0 });
        // The late source must never have been attempted on the dead player.
        assert!(loads.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_when_everything_fails() {
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, _) = MockPlayer::new(&[
            ("a", Script::ErrorImmediately),
            ("b", Script::ErrorImmediately),
        ]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert!(matches!(out, Outcome::Exhausted { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn no_sources_when_filters_exclude_everything() {
        let pool = pool_with(vec![stream("a", "480p")]);
        let settings = Settings {
            min_quality: crate::settings::Quality::P1080,
            ..Default::default()
        };
        let (mut player, loads) = MockPlayer::new(&[]);
        let out = run(
            &mut player,
            &pool,
            &settings,
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::NoSources);
        assert!(loads.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_addon_sources_stay_preferred_through_failover() {
        // A is "wait for this addon": both its (worse) sources must play
        // before B's better one, and the gate does not re-engage on
        // failover picks (A already finished loading by then).
        let pool = SourcePool::new();
        pool.add_streams("A", vec![stream("a480", "480p"), stream("a720", "720p")]);
        pool.add_streams("B", vec![stream("b1080", "1080p")]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a480", Script::ErrorImmediately),
            ("a720", Script::ErrorImmediately),
            ("b1080", Script::PlayToEnd),
        ]);
        let cfg = SessionConfig {
            wait_addons: vec!["A".into()],
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        // Both of A's (worse) sources before B's 1080p; best-first within A.
        assert_eq!(urls, ["a720", "a480", "b1080"]);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_addon_prefers_lower_quality_source_on_first_pick() {
        // Both addons already delivered (nothing pending): the toggled A's
        // 480p still beats B's 1080p.
        let pool = SourcePool::new();
        pool.add_streams("A", vec![stream("fromA", "480p")]);
        pool.add_streams("B", vec![stream("fromB", "1080p")]);
        let (mut player, loads) =
            MockPlayer::new(&[("fromA", Script::PlayToEnd), ("fromB", Script::PlayToEnd)]);
        let cfg = SessionConfig {
            wait_addons: vec!["A".into()],
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap()[0].0, "fromA");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_gate_prefers_slow_toggled_addons_first_source() {
        // Addon "B" delivers a good source immediately; the toggled addon "A"
        // delivers a *worse* source 5 s later. The gate must wait for A and
        // start with its source despite the worse ranking.
        let pool = SourcePool::new();
        // Both addons are loading; "B" keeps loading forever, but the gate
        // only cares about "A".
        pool.begin_task("B");
        pool.begin_task("A");
        let late = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            late.add_streams("A", vec![stream("fromA", "720p")]);
            late.task_done("A");
        });
        let (mut player, loads) =
            MockPlayer::new(&[("fromB", Script::PlayToEnd), ("fromA", Script::PlayToEnd)]);
        // Deliver B's (better) source right away.
        pool.add_streams("B", vec![stream("fromB", "1080p")]);

        let cfg = SessionConfig {
            wait_addons: vec!["A".into()],
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap()[0].0, "fromA");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_gate_falls_back_when_toggled_addon_has_nothing() {
        // Toggled addon "A" finishes without usable streams while "B" already
        // has sources: the session must start on B without waiting out the clock.
        let pool = SourcePool::new();
        pool.begin_task("A");
        let late = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            late.task_done("A"); // no streams delivered
        });
        pool.add_streams("B", vec![stream("fromB", "1080p")]);
        let (mut player, loads) = MockPlayer::new(&[("fromB", Script::PlayToEnd)]);
        let cfg = SessionConfig {
            wait_addons: vec!["A".into()],
            ..Default::default()
        };
        let started = Instant::now();
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap()[0].0, "fromB");
        // Well before the 90 s no-source timeout.
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn manual_pick_bypasses_wait_gate() {
        // A toggled addon is still loading, but a manual pick starts right away.
        let pool = pool_with(vec![stream("manual", "480p")]);
        pool.begin_task("A");
        let (mut player, loads) = MockPlayer::new(&[("manual", Script::PlayToEnd)]);
        let cfg = SessionConfig {
            first_key: Some("https://x/manual".into()),
            wait_addons: vec!["A".into()],
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap()[0].0, "manual");
    }

    #[tokio::test(start_paused = true)]
    async fn manual_skip_does_not_mark_the_source_tried() {
        // The user skips the best source: it must not count as failed/tried.
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a", Script::PlayThenSkipped { until: 42.0 }),
            ("b", Script::PlayToEnd),
        ]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["a", "b"]);
        // The skipped source is still eligible for the next pick.
        let s = Settings::default();
        let a = pool
            .rows(&s, &[])
            .into_iter()
            .find(|r| r.key == "https://x/a")
            .unwrap();
        assert!(!a.tried);
    }

    #[tokio::test(start_paused = true)]
    async fn skipped_source_goes_to_the_back_of_the_failover_queue() {
        // a (best) is skipped, b fails, c works: the queue order is a, b, c —
        // the skipped a is only picked again once everything else is gone.
        let pool = pool_with(vec![
            stream("a", "2160p"),
            stream("b", "1080p"),
            stream("c", "720p"),
        ]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a", Script::PlayThenSkipped { until: 10.0 }),
            ("b", Script::ErrorImmediately),
            ("c", Script::PlayToEnd),
        ]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["a", "b", "c"]);
    }

    #[tokio::test(start_paused = true)]
    async fn first_key_is_only_honored_for_the_first_pick() {
        // The preferred source fails: it must not be re-picked forever.
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) =
            MockPlayer::new(&[("b", Script::ErrorImmediately), ("a", Script::PlayToEnd)]);
        let cfg = SessionConfig {
            first_key: Some("https://x/b".into()),
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["b", "a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn first_key_picks_its_source_over_the_ranked_best() {
        // The preferred (worse-ranked) source is picked despite a better one
        // being available.
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) =
            MockPlayer::new(&[("a", Script::PlayToEnd), ("b", Script::PlayToEnd)]);
        let cfg = SessionConfig {
            first_key: Some("https://x/b".into()),
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap()[0].0, "b");
    }

    #[tokio::test(start_paused = true)]
    async fn stall_within_stall_timeout_keeps_the_source() {
        // Slow connection: the stream stalls for 3 s mid-play, well inside
        // the default 10 s stall tolerance, so it must not be switched away.
        let pool = pool_with(vec![stream("a", "1080p")]);
        let (mut player, loads) = MockPlayer::new(&[("a", Script::StallThenPlay { secs: 3.0 })]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["a"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stall_beyond_stall_timeout_fails_over() {
        // Same 3 s stall, but only 1 s tolerated: switch to the next source.
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a", Script::StallThenPlay { secs: 3.0 }),
            ("b", Script::PlayToEnd),
        ]);
        let cfg = SessionConfig {
            stall_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["a", "b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_start_within_start_timeout_keeps_the_source() {
        // The stream needs 20 s to open — inside the default 25 s start
        // window, so it must not be switched away.
        let pool = pool_with(vec![stream("a", "1080p")]);
        let (mut player, loads) = MockPlayer::new(&[("a", Script::LateStart { after: 20.0 })]);
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            0.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        assert_eq!(loads.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_start_beyond_start_timeout_fails_over() {
        // Same 20 s to open, but only 5 s allowed: switch to the next source.
        let pool = pool_with(vec![stream("a", "1080p"), stream("b", "720p")]);
        let (mut player, loads) = MockPlayer::new(&[
            ("a", Script::LateStart { after: 20.0 }),
            ("b", Script::PlayToEnd),
        ]);
        let cfg = SessionConfig {
            start_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let out = run(&mut player, &pool, &Settings::default(), &cfg, 0.0, None).await;
        assert_eq!(out, Outcome::Finished { position: 100.0 });
        let urls: Vec<String> = loads
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        assert_eq!(urls, ["a", "b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn user_closing_the_player_ends_the_session() {
        let pool = pool_with(vec![stream("a", "1080p")]);
        let (mut player, _) = MockPlayer::new(&[("a", Script::Hang)]);
        let tx = player.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tx.send(PlayerEvent::Closed).unwrap();
        });
        let out = run(
            &mut player,
            &pool,
            &Settings::default(),
            &SessionConfig::default(),
            12.0,
            None,
        )
        .await;
        assert_eq!(out, Outcome::Closed { position: 12.0 });
    }

    #[test]
    fn scaled_timeouts_defaults_without_overrides() {
        // No pre-buffer and no explicit timeouts: the built-in defaults.
        assert_eq!(
            scaled_timeouts(None, None, None),
            (Duration::from_secs(25), Duration::from_secs(10))
        );
        // A small pre-buffer must not raise the defaults either.
        assert_eq!(
            scaled_timeouts(None, None, Some(5)),
            (Duration::from_secs(25), Duration::from_secs(10))
        );
    }

    #[test]
    fn scaled_timeouts_raise_defaults_by_prebuffer() {
        // 30 s pre-buffer: start 25 -> 2*30+10 = 70 s, stall 10 -> 60 s.
        assert_eq!(
            scaled_timeouts(None, None, Some(30)),
            (Duration::from_secs(70), Duration::from_secs(60))
        );
        // Explicit start wins, the unset stall still scales.
        assert_eq!(
            scaled_timeouts(Some(60), None, Some(30)),
            (Duration::from_secs(60), Duration::from_secs(60))
        );
    }

    #[test]
    fn scaled_timeouts_explicit_overrides_always_win() {
        // Explicit timeouts win even when smaller than the scaled defaults.
        assert_eq!(
            scaled_timeouts(Some(20), Some(5), Some(30)),
            (Duration::from_secs(20), Duration::from_secs(5))
        );
    }
}
