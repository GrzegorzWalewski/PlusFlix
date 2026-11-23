# AGENTS.md

PlusFlix: Stremio-backed desktop player (Rust, Slint UI + external player). Two-crate workspace; `CLAUDE.md` has the pipeline diagram and module overview — read it for architecture.

## Commands

- Run: `cargo run --release -p plusflix-app` (needs mpv >= 0.38 on PATH; VLC is also a selectable player, see `crates/plusflix-core/src/vlc.rs`)
- Test all: `cargo test --workspace`; one test: `cargo test -p plusflix-core <name>`
- No CI, lint, or formatter config in the repo; no codegen step beyond the Slint build script.

## Structure

- `crates/plusflix-core`: all logic, headless-testable (mock `Player`, fake mpv socket). Keep UI concerns out.
- `crates/plusflix-app`: `src/main.rs` (all wiring) and Slint UI in `ui/{main,components,theme}.slint`. `ui/main.slint` is the compile entrypoint and is large (~1700+ lines).
- Slint runs on the main thread; network/failover/player run on tokio. Slint images are not `Send`, so `main.rs` passes plain-data twins (e.g. `ItemData`) and decodes posters on background tasks.

## Gotchas

- `crates/plusflix-app/build.rs` compiles Slint with debug info enabled — required by the `i-slint-backend-testing` layout tests (`tests/layout.rs`, `tests/cwprobe.rs`) to find elements by id. They use the `TestWindow` component exported at the bottom of `ui/main.slint` (ids like `TestWindow::search-test`). New layout tests need a matching element there. `cwprobe.rs` tests are probes that mostly `eprintln!`; use `-- --nocapture` to see output.
- On Windows the build embeds `crates/plusflix-app/icon.ico` via `winresource`; build fails if it's missing.
- `tmp/`, `tmp_*` (screenshots, scripts, logs at repo root) are local scratch; `tmp_*` and `*.log` are gitignored, `tmp/` is not — don't commit it.
- Stremio login / `addonCollectionGet` request shapes are unverified against a real account; don't assume they're correct.
- Only direct HTTP(S) streams are supported; torrent info-hash, YouTube, and external-URL streams are skipped intentionally.
- Persistent data is one JSON file — `%APPDATA%\PlusFlix\config\store.json` on Windows, `~/.config/plusflix/store.json` on Linux, `~/Library/Application Support/PlusFlix/store.json` on macOS (session key only, never the password). Max 5 profiles.
- Failover timings (defaults: 25 s no-start, 10 s stall, EOF >30 s before end, resume at last position − 3 s) live in `session.rs` (`DEFAULT_START_TIMEOUT` / `DEFAULT_STALL_TIMEOUT`); the no-start and stall timeouts are per-profile Settings overrides (`Settings::start_timeout_secs` / `stall_timeout_secs`, 5–300 s or empty = default). Keep the README/CLAUDE.md numbers in sync if the defaults change.
