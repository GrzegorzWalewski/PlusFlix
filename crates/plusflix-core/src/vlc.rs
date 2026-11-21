//! `Player` implementation that drives VLC through its HTTP interface.
//!
//! VLC has no JSON IPC like mpv, so PlusFlix starts it with its web interface
//! (`--extraintf=http` on a random port with a random password) and polls
//! `status.json` to map VLC state onto `PlayerEvent`s. The failover session
//! works unchanged on top of that.
//!
//! Limitations: VLC cannot set arbitrary HTTP request headers, so streams
//! that need proxy headers are refused (the session fails over to the next
//! source). Position events are poll-based (~0.4 s granularity).

use crate::error::{Error, Result};
use crate::lang::Lang;
use crate::player::{LoadRequest, Player, PlayerEvent};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

/// How often the HTTP interface is polled for state.
const POLL: Duration = Duration::from_millis(400);
/// Without a position advance for this long we report `Buffering(true)`.
const STALL_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct VlcConfig {
    pub binary: PathBuf,
    pub fullscreen: bool,
    pub extra_args: Vec<String>,
}

impl Default for VlcConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("vlc"),
            fullscreen: true,
            extra_args: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Shared {
    /// Bumped on every `load`; the poll loop resets its per-file state.
    generation: AtomicU64,
    /// External subtitles to add once the current file plays.
    pending_subs: Mutex<Vec<String>>,
}

pub struct VlcPlayer {
    child: Option<Arc<AsyncMutex<Child>>>,
    http: reqwest::Client,
    /// e.g. `http://127.0.0.1:53211/requests`
    base: String,
    password: String,
    shared: Arc<Shared>,
    events: UnboundedReceiver<PlayerEvent>,
}

/// Locates the VLC executable: PATH first, then the usual Windows
/// install locations.
pub fn find_vlc() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "vlc.exe" } else { "vlc" };
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let cand = dir.join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Ok(base) = std::env::var(var) {
                let cand = PathBuf::from(base).join(r"VideoLAN\VLC").join(exe);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

fn free_port() -> Result<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .ok_or_else(|| Error::Player("could not find a free port for VLC".into()))
}

fn random_password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("reel-{}-{nanos}", std::process::id())
}

/// Percent-encodes a query parameter value.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn get_json(http: &reqwest::Client, url: &str, password: &str) -> Result<Value> {
    let r = http
        .get(url)
        .basic_auth("", Some(password))
        .send()
        .await
        .map_err(|e| Error::Player(format!("VLC HTTP request failed: {e}")))?;
    let r = r
        .error_for_status()
        .map_err(|e| Error::Player(format!("VLC HTTP error: {e}")))?;
    r.json()
        .await
        .map_err(|e| Error::Player(format!("VLC sent invalid JSON: {e}")))
}

impl VlcPlayer {
    /// Starts VLC with its HTTP interface and connects to it.
    pub async fn spawn(cfg: &VlcConfig) -> Result<Self> {
        let binary = if cfg.binary == Path::new("vlc") {
            find_vlc().ok_or_else(|| {
                Error::Player(
                    "VLC not found. Install VLC from videolan.org or put vlc.exe on PATH."
                        .to_owned(),
                )
            })?
        } else {
            cfg.binary.clone()
        };
        let port = free_port()?;
        let password = random_password();
        let mut cmd = Command::new(&binary);
        cmd.args([
            "--extraintf=http",
            "--http-host=127.0.0.1",
            "--no-qt-privacy-ask",
            "--no-video-title-show",
            "--play-and-exit",
            // Never hand playback to an already-running VLC instance: that
            // would exit our child instantly and lose HTTP control of the
            // stream (the session would see the player as closed).
            "--no-one-instance",
            "--no-one-instance-when-started-from-file",
        ])
        .arg(format!("--http-port={port}"))
        .arg(format!("--http-password={password}"));
        if cfg.fullscreen {
            cmd.arg("--fullscreen");
        }
        cmd.args(&cfg.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| Error::Player(format!("cannot start VLC ({}): {e}", binary.display())))?;
        let child = Arc::new(AsyncMutex::new(child));
        let base = format!("http://127.0.0.1:{port}/requests");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| Error::Player(format!("HTTP client: {e}")))?;

        // Wait for the web interface to come up.
        let mut up = false;
        for _ in 0..80 {
            if get_json(&http, &format!("{base}/status.json"), &password)
                .await
                .is_ok()
            {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !up {
            if let Ok(Some(_)) = child.lock().await.try_wait() {
                // already gone
            } else {
                let _ = child.lock().await.kill().await;
            }
            return Err(Error::Player(
                "VLC started but its HTTP interface did not answer.".into(),
            ));
        }
        Ok(Self::connect(http, base, password, Some(child)).await)
    }

    /// Attaches to an already-running VLC HTTP interface (used by tests).
    pub async fn connect(
        http: reqwest::Client,
        base: String,
        password: String,
        child: Option<Arc<AsyncMutex<Child>>>,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let poll_child = child.clone().unwrap_or_else(|| {
            // No process to watch: the poll loop only relies on status.json.
            Arc::new(AsyncMutex::new(dummy_child()))
        });
        tokio::spawn(poll_loop(
            http.clone(),
            base.clone(),
            password.clone(),
            poll_child,
            shared.clone(),
            tx,
        ));
        Self {
            child,
            http,
            base,
            password,
            shared,
            events: rx,
        }
    }

    async fn cmd(&self, query: &str) -> Result<()> {
        let url = format!("{}/status.json?{}", self.base, query);
        let r = self
            .http
            .get(&url)
            .basic_auth("", Some(&self.password))
            .send()
            .await
            .map_err(|e| Error::Player(format!("VLC HTTP request failed: {e}")))?;
        r.error_for_status()
            .map_err(|e| Error::Player(format!("VLC HTTP error: {e}")))?;
        Ok(())
    }
}

/// A child that never runs, for the poll loop when there is no real process
/// (tests). It never reports an exit status.
#[cfg(windows)]
fn dummy_child() -> Child {
    // `waitfor` is unreliable across Windows environments, so use the
    // classic `ping` delay: one echo per second for ~10 minutes.
    Command::new("ping")
        .args(["-n", "600", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dummy child")
}

/// A child that never runs, for the poll loop when there is no real process
/// (tests). It never reports an exit status.
#[cfg(not(windows))]
fn dummy_child() -> Child {
    Command::new("sleep")
        .arg("600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dummy child")
}

/// Ended when playback got (close to) the end, Closed otherwise.
fn exit_event(pos: f64, len: f64) -> PlayerEvent {
    if len > 0.0 && pos / len >= 0.92 {
        PlayerEvent::Ended
    } else {
        PlayerEvent::Closed
    }
}

async fn poll_loop(
    http: reqwest::Client,
    base: String,
    password: String,
    child: Arc<AsyncMutex<Child>>,
    shared: Arc<Shared>,
    tx: UnboundedSender<PlayerEvent>,
) {
    let mut my_gen = shared.generation.load(SeqCst);
    let mut started = false;
    let mut stop_sent = false;
    let mut buffering_sent = false;
    let mut last_pos = -1.0f64;
    let mut last_len = 0.0f64;
    let mut last_advance = tokio::time::Instant::now();
    let mut errors = 0usize;

    loop {
        tokio::time::sleep(POLL).await;
        // The player was dropped (cancelled / replaced): kill the VLC process,
        // otherwise it would keep playing with nobody watching it.
        if tx.is_closed() {
            let _ = child.lock().await.kill().await;
            return;
        }
        let g = shared.generation.load(SeqCst);
        if g != my_gen {
            my_gen = g;
            started = false;
            stop_sent = false;
            buffering_sent = false;
            last_pos = -1.0;
            last_len = 0.0;
            last_advance = tokio::time::Instant::now();
            errors = 0;
        }

        if let Ok(Some(_)) = child.lock().await.try_wait() {
            // VLC quits when an input fails to open (play-and-exit), which
            // must not be mistaken for the user closing a healthy player.
            let _ = tx.send(if started {
                exit_event(last_pos.max(0.0), last_len)
            } else {
                PlayerEvent::Error("VLC exited without starting playback".into())
            });
            return;
        }

        match get_json(&http, &format!("{base}/status.json"), &password).await {
            Ok(v) => {
                errors = 0;
                let state = v.get("state").and_then(Value::as_str).unwrap_or("stopped");
                let length = v.get("length").and_then(Value::as_f64).unwrap_or(0.0);
                let frac = v.get("position").and_then(Value::as_f64).unwrap_or(0.0);
                let secs = v.get("time").and_then(Value::as_f64).unwrap_or(0.0);
                if length > 0.0 {
                    last_len = length;
                }
                let pos = if last_len > 0.0 {
                    frac * last_len
                } else {
                    secs
                };

                if state != "stopped" {
                    if !started {
                        started = true;
                        last_pos = pos;
                        last_advance = tokio::time::Instant::now();
                        let _ = tx.send(PlayerEvent::Started);
                        if last_len > 0.0 {
                            let _ = tx.send(PlayerEvent::Duration(last_len));
                        }
                        let _ = tx.send(PlayerEvent::Position(pos));
                        let subs = std::mem::take(
                            &mut *shared
                                .pending_subs
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()),
                        );
                        for url in subs {
                            let _ = get_json(
                                &http,
                                &format!(
                                    "{base}/status.json?command=addsubtitle&val={}",
                                    enc(&url)
                                ),
                                &password,
                            )
                            .await;
                        }
                    } else if pos != last_pos {
                        last_pos = pos;
                        last_advance = tokio::time::Instant::now();
                        if buffering_sent {
                            buffering_sent = false;
                            let _ = tx.send(PlayerEvent::Buffering(false));
                        }
                        let _ = tx.send(PlayerEvent::Position(pos));
                    } else if !buffering_sent && last_advance.elapsed() > STALL_AFTER {
                        buffering_sent = true;
                        let _ = tx.send(PlayerEvent::Buffering(true));
                    }
                } else if started && !stop_sent {
                    // VLC reached the end (state "stopped"), possibly before
                    // the process itself exits.
                    stop_sent = true;
                    let _ = tx.send(exit_event(last_pos.max(0.0), last_len));
                }
            }
            Err(_) => {
                errors += 1;
                if errors >= 8 {
                    let exited = child
                        .lock()
                        .await
                        .try_wait()
                        .map(|s| s.is_some())
                        .unwrap_or(false);
                    let _ = tx.send(if exited {
                        exit_event(last_pos.max(0.0), last_len)
                    } else {
                        PlayerEvent::Closed
                    });
                    return;
                }
            }
        }
    }
}

fn lang_pair(l: Lang) -> String {
    format!("{},{}", l.code3(), l.code2())
}

impl Player for VlcPlayer {
    async fn load(&mut self, req: LoadRequest) -> Result<()> {
        if !req.headers.is_empty() {
            return Err(Error::Player(
                "needs custom HTTP headers, which VLC cannot send – use MPV for this source".into(),
            ));
        }
        // Everything the previous file still had queued is obsolete.
        while self.events.try_recv().is_ok() {}
        self.shared.generation.fetch_add(1, SeqCst);

        let subs: Vec<String> = match req.subtitle_lang {
            Some(_) => req.subtitles.iter().map(|s| s.url.clone()).collect(),
            None => Vec::new(),
        };
        *self
            .shared
            .pending_subs
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = subs;

        let mut query = format!("command=in_play&input={}", enc(&req.url));
        query.push_str(&format!(
            "&option={}",
            enc(&format!("start-time={}", req.start_secs.max(0.0) as i64))
        ));
        if let Some(l) = req.audio_lang {
            query.push_str(&format!(
                "&option={}",
                enc(&format!("audio-language={}", lang_pair(l)))
            ));
        }
        if let Some(l) = req.subtitle_lang {
            query.push_str(&format!(
                "&option={}",
                enc(&format!("sub-language={}", lang_pair(l)))
            ));
        }
        self.cmd(&query).await
    }

    async fn next_event(&mut self) -> Option<PlayerEvent> {
        self.events.recv().await
    }

    async fn stop(&mut self) {
        let _ = self.cmd("command=pl_stop").await;
        if let Some(child) = &self.child {
            if tokio::time::timeout(Duration::from_secs(2), async {
                let mut c = child.lock().await;
                let _ = c.wait().await;
            })
            .await
            .is_err()
            {
                let _ = child.lock().await.kill().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::Path;

    /// Shared fake VLC state, mutated by the tests.
    #[derive(Clone)]
    struct FakeState {
        playing: bool,
        position: f64,
        length: f64,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                playing: false,
                position: 0.0,
                length: 0.0,
            }
        }
    }

    /// Minimal stand-in for VLC's web interface: a plain blocking TCP server
    /// on its own OS thread that serves `status.json` from the shared state
    /// and records every request path. (Deliberately not tokio-based: two
    /// tokio sockets in one process trigger a Windows readiness bug in the
    /// current mio/tokio version, which would hang the client.)
    fn serve(
        listener: TcpListener,
        state: Arc<Mutex<FakeState>>,
        requests: Arc<Mutex<Vec<String>>>,
    ) {
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let mut buf = vec![0u8; 4096];
                let mut data = Vec::new();
                loop {
                    let n = match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    data.extend_from_slice(&buf[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&data);
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(path);

                let st = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let body = if st.playing {
                    json!({
                        "state": "playing",
                        "length": st.length,
                        "time": st.position,
                        "position": if st.length > 0.0 { st.position / st.length } else { 0.0 },
                    })
                } else {
                    json!({ "state": "stopped", "length": 0, "time": 0, "position": 0.0 })
                }
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.shutdown(std::net::Shutdown::Both);
            }
        });
    }

    async fn setup() -> (VlcPlayer, Arc<Mutex<FakeState>>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        serve(listener, state.clone(), requests.clone());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let player = VlcPlayer::connect(
            http,
            format!("http://127.0.0.1:{port}/requests"),
            "pw".into(),
            None,
        )
        .await;
        (player, state, requests)
    }

    fn req(url: &str, start: f64, headers: &[(&str, &str)]) -> LoadRequest {
        LoadRequest {
            url: url.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            start_secs: start,
            audio_lang: Some(Lang::Pl),
            subtitle_lang: None,
            subtitles: vec![],
        }
    }

    async fn next(p: &mut VlcPlayer) -> PlayerEvent {
        tokio::time::timeout(Duration::from_secs(3), p.next_event())
            .await
            .expect("event in time")
            .expect("channel open")
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.01
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_plays_and_reports_started_duration_position() {
        let (mut p, state, requests) = setup().await;
        {
            let mut s = state.lock().unwrap();
            s.playing = true;
            s.length = 100.0;
            s.position = 5.0;
        }
        p.load(req("http://x/a", 42.0, &[])).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
        assert!(matches!(next(&mut p).await, PlayerEvent::Duration(d) if approx(d, 100.0)));
        assert!(matches!(next(&mut p).await, PlayerEvent::Position(x) if approx(x, 5.0)));

        let reqs = requests.lock().unwrap().clone();
        let in_play = reqs
            .iter()
            .find(|r| r.contains("command=in_play"))
            .expect("in_play request recorded");
        assert!(in_play.contains("input=http%3A%2F%2Fx%2Fa"));
        assert!(in_play.contains("start-time%3D42"));
        assert!(in_play.contains("audio-language%3Dpol%2Cpl"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_with_headers_are_refused() {
        let (mut p, _state, _requests) = setup().await;
        let err = p
            .load(req("http://x/a", 0.0, &[("Referer", "https://x")]))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_near_the_end_reports_ended() {
        let (mut p, state, _requests) = setup().await;
        {
            let mut s = state.lock().unwrap();
            s.playing = true;
            s.length = 100.0;
            s.position = 95.0;
        }
        p.load(req("http://x/a", 0.0, &[])).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
        assert!(matches!(next(&mut p).await, PlayerEvent::Duration(d) if approx(d, 100.0)));
        assert!(matches!(next(&mut p).await, PlayerEvent::Position(x) if approx(x, 95.0)));
        // VLC reaches the end: state flips to "stopped" at 95 of 100 s.
        state.lock().unwrap().playing = false;
        assert_eq!(next(&mut p).await, PlayerEvent::Ended);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_early_reports_closed() {
        let (mut p, state, _requests) = setup().await;
        {
            let mut s = state.lock().unwrap();
            s.playing = true;
            s.length = 100.0;
            s.position = 40.0;
        }
        p.load(req("http://x/a", 0.0, &[])).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
        assert!(matches!(next(&mut p).await, PlayerEvent::Duration(d) if approx(d, 100.0)));
        assert!(matches!(next(&mut p).await, PlayerEvent::Position(x) if approx(x, 40.0)));
        state.lock().unwrap().playing = false;
        assert_eq!(next(&mut p).await, PlayerEvent::Closed);
    }

    #[test]
    fn vlc_discovery_paths() {
        // Just checks it does not panic; whether VLC is installed is env-dependent.
        let _ = find_vlc();
        assert!(Path::new("vlc.exe").file_name().is_some());
    }
}
