# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

PlusFlix is a Stremio-backed desktop media player in Rust: Slint UI + mpv (driven via JSON IPC). It logs in with a Stremio account, uses the account's installed addons, and plays with automatic source failover. Requires mpv (>= 0.38) on PATH at runtime.

## Commands

```
cargo run --release -p plusflix-app    # run the app
cargo test --workspace             # run all tests
cargo test -p plusflix-core <name>     # run a single test
cargo build --workspace
```

## Architecture

Two-crate workspace:

- **`crates/plusflix-core`** – all logic, no UI. Everything here is testable headlessly; playback logic is tested against a mock `Player` and a fake mpv socket.
- **`crates/plusflix-app`** – Slint UI (`ui/main.slint`) and wiring. Slint runs on the main thread; networking, failover sessions, and mpv run on a tokio runtime. Slint images are not `Send`, so `main.rs` uses plain-data twins (e.g. `ItemData`) and decodes posters on background tasks.

### Playback pipeline (the core design)

```
open title ──► SourceLoader (one task per addon) ──► SourcePool ◄── next_untried(settings)
                                                     ▲                    │
               keeps adding sources while playing ───┘                    ▼
Play ──► session::run ──► best untried source ──► mpv (loadfile ... start=<position>)
             ▲                                        │
             └── error / no start in 25s / stall 15s / EOF >30s before end
```

- `addon` / `stremio` – Stremio API client and addon client (`AddonClient`).
- `loader` – `SourceLoader`: one background task per addon, streams flow into the pool. Dropping it cancels pending requests.
- `pool` – `SourcePool`: shared candidate pool; addons keep feeding it even while playing, so failover always has backups. Candidates are ranked: audio language match, then quality, then debrid-cached, then subtitles.
- `select` / `parse` / `lang` – filtering/evaluation and heuristic parsing of language/quality/size/subtitle info from addon text (names, titles, flags, filenames).
- `rating` – age-rating token parsing into `AgeRating`; when sources disagree the strictest wins; `RatingPolicy` (profile kind + max age) filters previews, search, and genre pages.
- `session` – failover loop (`session::run`): starts best source; on error, no start in 25 s, stall > 15 s, or premature EOF, loads the next best untried source into the same mpv instance at last position minus 3 s.
- `player` – `Player` trait (session ↔ player interface); `mpv` implements it via mpv JSON IPC (Windows named pipe, Unix socket). mpv owns decoding/subtitles/OSD in its own fullscreen window.
- `store` / `settings` – one JSON store under the OS config dir (`PlusFlix/config/store.json`): up to 5 local profiles with favorites, progress, and per-profile settings (audio/subtitle language requirements, min quality, max size). Session key stored, never the password.
- `i18n` – interface translations (`Translations::load(locale, dir)` from disk / `Translations::from_json` from JSON embedded in plusflix-app via `include_str!`). English is always loaded as fallback; missing keys fall back to it, and a total miss logs a warning and returns the key. Locale JSONs live in `crates/plusflix-app/locales/`; supported languages are the `LANGUAGES` table in `crates/plusflix-app/src/main.rs` (adding a language = new JSON file + one table row + one `include_str!`).

### Constraints

- Only direct HTTP(S) streams are supported; torrent info-hash, YouTube and external-URL streams are skipped.
- Login / `addonCollectionGet` request shapes follow community docs and have not been verified against a real Stremio account — treat Stremio API assumptions as unverified.
