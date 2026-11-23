//! `Player` implementation that drives an external mpv through its JSON IPC.
//!
//! mpv runs in its own (fullscreen) window, so decoding, hardware acceleration,
//! subtitles and OSD are all mpv's. PlusFlix only sends commands and reads events.
//! Needs mpv >= 0.38 (`loadfile` with an index argument).
//!
//! Windows: named pipe `\\.\pipe\reel-mpv-*`; Unix: a socket in the temp dir.

use crate::error::{Error, Result};
use crate::lang::Lang;
use crate::player::{LoadRequest, Player, PlayerEvent};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[cfg(unix)]
type Ipc = tokio::net::UnixStream;
#[cfg(windows)]
type Ipc = tokio::net::windows::named_pipe::NamedPipeClient;

type Writer = Arc<tokio::sync::Mutex<WriteHalf<Ipc>>>;

#[derive(Debug, Clone)]
pub struct MpvConfig {
    pub binary: PathBuf,
    pub fullscreen: bool,
    pub extra_args: Vec<String>,
    /// Demuxer read-ahead cache cap in MiB; `None` uses mpv's default
    /// (150 MiB).
    pub buffer_ahead_mib: Option<u64>,
    /// Seconds to buffer before playback starts; `None` starts immediately.
    pub prebuffer_secs: Option<u64>,
}

impl Default for MpvConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("mpv"),
            fullscreen: true,
            extra_args: Vec::new(),
            buffer_ahead_mib: None,
            prebuffer_secs: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    /// A file is loading or playing (between `start-file` and `end-file`).
    active: AtomicBool,
    /// `end-file(stop)` events caused by our own `loadfile … replace` that must not look like "closed".
    pending_stops: AtomicUsize,
    /// External subtitles to attach once the current file has loaded.
    pending_subs: Mutex<Vec<(String, Lang)>>,
}

pub struct MpvPlayer {
    child: Option<Child>,
    writer: Writer,
    events: UnboundedReceiver<PlayerEvent>,
    shared: Arc<Shared>,
    next_id: u64,
}

fn ipc_path() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let name = format!("reel-mpv-{}-{nanos}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\{name}")
    } else {
        std::env::temp_dir()
            .join(format!("{name}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(unix)]
async fn connect(path: &str) -> std::io::Result<Ipc> {
    tokio::net::UnixStream::connect(path).await
}
#[cfg(windows)]
async fn connect(path: &str) -> std::io::Result<Ipc> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(path)
}

async fn connect_with_retry(path: &str) -> Result<Ipc> {
    let mut last = None;
    for _ in 0..80 {
        match connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::Player(format!(
        "could not connect to mpv at {path}: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

async fn write_json(writer: &Writer, v: Value) -> Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Player(format!("mpv IPC write failed: {e}")))?;
    w.flush()
        .await
        .map_err(|e| Error::Player(format!("mpv IPC flush failed: {e}")))
}

/// The mpv command line for `cfg`, without the binary: idle and headless,
/// the IPC socket, optional fullscreen, optional pre-playback buffering,
/// then the user's extra args last.
pub fn command_args(cfg: &MpvConfig, ipc_path: &str) -> Vec<String> {
    let mut args = vec![
        "--idle=yes".into(),
        "--force-window=yes".into(),
        "--no-terminal".into(),
        "--keep-open=no".into(),
        format!("--input-ipc-server={ipc_path}"),
    ];
    if cfg.fullscreen {
        args.push("--fs".into());
    }
    if let Some(mib) = cfg.buffer_ahead_mib {
        args.push(format!("--demuxer-max-bytes={mib}MiB"));
        args.push(format!("--demuxer-max-back-bytes={mib}MiB"));
    }
    if let Some(secs) = cfg.prebuffer_secs {
        args.push("--cache-pause-initial=yes".into());
        args.push(format!("--cache-pause-wait={secs}"));
    }
    args.extend(cfg.extra_args.iter().cloned());
    args
}

impl MpvPlayer {
    /// Starts mpv (idle, waiting for commands) and connects to it.
    pub async fn spawn(cfg: &MpvConfig) -> Result<Self> {
        let path = ipc_path();
        let mut cmd = Command::new(&cfg.binary);
        cmd.args(command_args(cfg, &path))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| {
            Error::Player(format!("cannot start mpv ({}): {e}", cfg.binary.display()))
        })?;
        let stream = connect_with_retry(&path).await?;
        Self::from_stream(stream, Some(child)).await
    }

    async fn from_stream(stream: Ipc, child: Option<Child>) -> Result<Self> {
        let (read, write) = tokio::io::split(stream);
        let writer: Writer = Arc::new(tokio::sync::Mutex::new(write));
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(reader_loop(read, writer.clone(), shared.clone(), tx));
        for (id, prop) in [(1, "time-pos"), (2, "duration"), (3, "paused-for-cache")] {
            write_json(
                &writer,
                json!({ "command": ["observe_property", id, prop] }),
            )
            .await?;
        }
        Ok(Self {
            child,
            writer,
            events: rx,
            shared,
            next_id: 0,
        })
    }

    async fn cmd(&mut self, args: Value) -> Result<()> {
        self.next_id += 1;
        write_json(
            &self.writer,
            json!({ "command": args, "request_id": self.next_id }),
        )
        .await
    }
}

fn lang_list(l: Option<Lang>) -> Value {
    match l {
        Some(l) => json!([l.code3(), l.code2()]),
        None => json!([]),
    }
}

impl Player for MpvPlayer {
    async fn load(&mut self, req: LoadRequest) -> Result<()> {
        if self.shared.active.swap(false, SeqCst) {
            // mpv will report `end-file(stop)` for the file we are about to replace.
            self.shared.pending_stops.fetch_add(1, SeqCst);
        }
        // Whatever the previous file still had queued is obsolete.
        while self.events.try_recv().is_ok() {}

        let headers: Vec<String> = req
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        self.cmd(json!(["set_property", "http-header-fields", headers]))
            .await?;
        self.cmd(json!(["set_property", "alang", lang_list(req.audio_lang)]))
            .await?;
        self.cmd(json!([
            "set_property",
            "slang",
            lang_list(req.subtitle_lang)
        ]))
        .await?;

        let subs: Vec<(String, Lang)> = match req.subtitle_lang {
            Some(l) => req.subtitles.iter().map(|s| (s.url.clone(), l)).collect(),
            None => Vec::new(),
        };
        *self
            .shared
            .pending_subs
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = subs;

        self.cmd(json!([
            "loadfile",
            req.url,
            "replace",
            -1,
            format!("start={:.3}", req.start_secs.max(0.0))
        ]))
        .await
    }

    async fn next_event(&mut self) -> Option<PlayerEvent> {
        self.events.recv().await
    }

    async fn notify(&mut self, text: &str) {
        let _ = self.cmd(json!(["show-text", text, 3000])).await;
    }

    async fn stop(&mut self) {
        let _ = self.cmd(json!(["quit"])).await;
        if let Some(child) = self.child.as_mut() {
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}

async fn reader_loop(
    read: ReadHalf<Ipc>,
    writer: Writer,
    shared: Arc<Shared>,
    tx: UnboundedSender<PlayerEvent>,
) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        handle_line(&line, &writer, &shared, &tx).await;
    }
    // mpv exited or the pipe broke.
    let _ = tx.send(PlayerEvent::Closed);
}

async fn handle_line(
    line: &str,
    writer: &Writer,
    shared: &Shared,
    tx: &UnboundedSender<PlayerEvent>,
) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let Some(event) = v.get("event").and_then(Value::as_str) else {
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            if err != "success" {
                tracing::debug!("mpv command reply: {line}");
            }
        }
        return;
    };
    match event {
        "start-file" => shared.active.store(true, SeqCst),
        "file-loaded" => {
            shared.active.store(true, SeqCst);
            let subs = std::mem::take(
                &mut *shared
                    .pending_subs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
            for (url, lang) in subs {
                let _ = write_json(
                    writer,
                    json!({ "command": ["sub-add", url, "select", lang.name(), lang.code3()] }),
                )
                .await;
            }
            let _ = tx.send(PlayerEvent::Started);
        }
        "end-file" => {
            let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
            if reason == "stop"
                && shared
                    .pending_stops
                    .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                    .is_ok()
            {
                return; // caused by our own loadfile(replace)
            }
            shared.active.store(false, SeqCst);
            let ev = match reason {
                "eof" => PlayerEvent::Ended,
                "error" => PlayerEvent::Error(
                    v.get("file_error")
                        .and_then(Value::as_str)
                        .unwrap_or("playback error")
                        .to_owned(),
                ),
                "redirect" => return,
                _ => PlayerEvent::Closed,
            };
            let _ = tx.send(ev);
        }
        "property-change" => {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("");
            let data = v.get("data");
            match name {
                "time-pos" => {
                    if let Some(p) = data.and_then(Value::as_f64) {
                        let _ = tx.send(PlayerEvent::Position(p));
                    }
                }
                "duration" => {
                    if let Some(d) = data.and_then(Value::as_f64) {
                        let _ = tx.send(PlayerEvent::Duration(d));
                    }
                }
                "paused-for-cache" => {
                    if let Some(b) = data.and_then(Value::as_bool) {
                        let _ = tx.send(PlayerEvent::Buffering(b));
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    /// A tiny stand-in for mpv: understands `loadfile` and emits the events mpv would.
    async fn fake_mpv(listener: UnixListener, fail_urls: Vec<String>) {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut playing = false;
        let mut chunk = [0u8; 4096];
        loop {
            let n = match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let v: Value = serde_json::from_slice(&line).unwrap();
                let cmd = &v["command"];
                if cmd[0] == "loadfile" {
                    let url = cmd[1].as_str().unwrap().to_owned();
                    let mut out = String::new();
                    if playing {
                        out += &json!({"event":"end-file","reason":"stop"}).to_string();
                        out.push('\n');
                    }
                    out += &json!({"event":"start-file"}).to_string();
                    out.push('\n');
                    if fail_urls.contains(&url) {
                        playing = false;
                        out += &json!({"event":"end-file","reason":"error","file_error":"loading failed"}).to_string();
                        out.push('\n');
                    } else {
                        playing = true;
                        for ev in [
                            json!({"event":"file-loaded"}),
                            json!({"event":"property-change","id":2,"name":"duration","data":100.0}),
                            json!({"event":"property-change","id":1,"name":"time-pos","data":5.0}),
                        ] {
                            out += &ev.to_string();
                            out.push('\n');
                        }
                    }
                    sock.write_all(out.as_bytes()).await.unwrap();
                }
            }
        }
    }

    async fn setup(fail: &[&str]) -> MpvPlayer {
        let path = std::env::temp_dir().join(format!(
            "reel-test-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(fake_mpv(
            listener,
            fail.iter().map(|s| s.to_string()).collect(),
        ));
        let stream = connect_with_retry(path.to_str().unwrap()).await.unwrap();
        MpvPlayer::from_stream(stream, None).await.unwrap()
    }

    fn req(url: &str, start: f64) -> LoadRequest {
        LoadRequest {
            url: url.into(),
            headers: Default::default(),
            start_secs: start,
            audio_lang: Some(Lang::Pl),
            subtitle_lang: None,
            subtitles: vec![],
        }
    }

    async fn next(p: &mut MpvPlayer) -> PlayerEvent {
        tokio::time::timeout(Duration::from_secs(2), p.next_event())
            .await
            .expect("event in time")
            .expect("channel open")
    }

    #[tokio::test]
    async fn load_reports_started_duration_position() {
        let mut p = setup(&[]).await;
        p.load(req("http://x/a", 0.0)).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
        assert_eq!(next(&mut p).await, PlayerEvent::Duration(100.0));
        assert_eq!(next(&mut p).await, PlayerEvent::Position(5.0));
    }

    #[tokio::test]
    async fn replacing_a_playing_file_is_not_reported_as_closed() {
        let mut p = setup(&[]).await;
        p.load(req("http://x/a", 0.0)).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
        p.load(req("http://x/b", 42.0)).await.unwrap();
        // Everything that follows belongs to the new file; no Closed / Error may appear.
        let mut saw_started = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(300), p.next_event()).await
        {
            assert!(
                !matches!(ev, PlayerEvent::Closed | PlayerEvent::Error(_)),
                "unexpected {ev:?}"
            );
            saw_started |= ev == PlayerEvent::Started;
        }
        assert!(saw_started);
    }

    #[tokio::test]
    async fn open_failure_is_an_error_event_and_the_next_load_works() {
        let mut p = setup(&["http://x/bad"]).await;
        p.load(req("http://x/bad", 0.0)).await.unwrap();
        assert_eq!(
            next(&mut p).await,
            PlayerEvent::Error("loading failed".into())
        );
        p.load(req("http://x/good", 0.0)).await.unwrap();
        assert_eq!(next(&mut p).await, PlayerEvent::Started);
    }
}

#[cfg(test)]
mod command_args_tests {
    use super::*;

    #[test]
    fn default_config_builds_the_expected_args() {
        // Default: fullscreen (the built-in default), no buffering flags,
        // no extra args.
        assert_eq!(
            command_args(&MpvConfig::default(), "/tmp/reel.sock"),
            [
                "--idle=yes",
                "--force-window=yes",
                "--no-terminal",
                "--keep-open=no",
                "--input-ipc-server=/tmp/reel.sock",
                "--fs",
            ]
        );
        // Windowed: no `--fs`.
        let cfg = MpvConfig {
            fullscreen: false,
            ..Default::default()
        };
        assert_eq!(
            command_args(&cfg, "/tmp/reel.sock"),
            [
                "--idle=yes",
                "--force-window=yes",
                "--no-terminal",
                "--keep-open=no",
                "--input-ipc-server=/tmp/reel.sock",
            ]
        );
    }

    #[test]
    fn fullscreen_buffering_and_extra_args_in_order() {
        let cfg = MpvConfig {
            fullscreen: true,
            buffer_ahead_mib: Some(768),
            prebuffer_secs: Some(30),
            extra_args: vec!["--volume=50".into()],
            ..Default::default()
        };
        assert_eq!(
            command_args(&cfg, "/tmp/reel.sock"),
            [
                "--idle=yes",
                "--force-window=yes",
                "--no-terminal",
                "--keep-open=no",
                "--input-ipc-server=/tmp/reel.sock",
                "--fs",
                "--demuxer-max-bytes=768MiB",
                "--demuxer-max-back-bytes=768MiB",
                "--cache-pause-initial=yes",
                "--cache-pause-wait=30",
                "--volume=50",
            ]
        );
    }
}
