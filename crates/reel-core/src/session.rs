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
    pub first_key: Option<String>,
    /// Addons (by name) whose sources the first pick waits for: the session
    /// only starts from one of these addons unless they deliver nothing
    /// usable. Applies to the first pick only; failover stays ungated.
    pub wait_addons: Vec<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(25),
            stall_timeout: Duration::from_secs(15),
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
    Trying { label: String, attempt: usize },
    Playing { label: String },
    Failed { label: String, reason: String },
    WaitingForSources,
    Position { secs: f64, duration: Option<f64> },
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

    'sources: loop {
        // ---- pick the next source (waiting for the loaders if necessary) ----
        let pick: Pick = {
            // A manually chosen source (from the sources list) goes first,
            // even if the settings would have hidden it.
            if let Some(p) = cfg
                .first_key
                .as_deref()
                .and_then(|k| pool.pick_by_key(k, settings))
            {
                p
            } else {
                let give_up_at = Instant::now() + cfg.no_source_timeout;
                // Wait-gate: for the first pick, only sources from the
                // toggled addons count until each of them is done loading.
                let mut gated = !cfg.wait_addons.is_empty();
                loop {
                    let picked = if gated {
                        cfg.wait_addons
                            .iter()
                            .find_map(|a| pool.next_untried_from(a, settings))
                    } else {
                        pool.next_untried(settings)
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
        pool.mark_tried(&pick.candidate.key);
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
                        emit(SessionEvent::Failed {
                            label: label.clone(),
                            reason: "stream ended early".into(),
                        });
                        continue 'sources;
                    }
                    player.stop().await;
                    return Outcome::Finished { position };
                }
                Some(PlayerEvent::Error(reason)) => {
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
        /// Starts, plays 100 s of a 100 s file, ends.
        PlayToEnd,
        /// Starts, then drops the connection: EOF at 40 of 100 s.
        PrematureEof,
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
}
