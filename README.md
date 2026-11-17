# Reel

A Stremio-backed desktop player written in Rust (Slint UI + mpv). It signs in with your
Stremio account, uses the addons installed there, and plays with automatic source failover.

## Run

```
# needs mpv (>= 0.38) on PATH; on Windows e.g. `winget install mpv` or put mpv.exe next to reel-app.exe
cargo run --release -p reel-app
cargo test --workspace
```

Data (login key, profiles, favorites, progress) is stored in one JSON file under the OS config
dir (`Reel/store.json`). Your Stremio password is never stored, only the session key.

## Features

- Stremio account login; addons are read from the account's addon collection
- Pages: Home, Movies, Series, Favorites (+ Settings), details with episode list for series
- Up to 5 local profiles, each with its own favorites, watch progress and settings
- Continue Watching row on Home: local progress plus entries synced from the account's
  Stremio library (`datastoreGet` on `libraryItem`, same rule Stremio uses)
- Per-profile settings: player (mpv / VLC), audio language (prefer / must), subtitles language, subtitles requirement
  (required / required only if audio isn't the preferred language / not required), minimum
  quality, maximum file size
- Sources load in the background as soon as a title (or episode) is opened

## How failover works

```
open title ──► SourceLoader (one task per addon) ──► SourcePool  ◄── next_untried(settings)
                                                        ▲                    │
                  keeps adding sources while playing ───┘                    ▼
Play ──► session::run ──► best untried source ──► mpv (loadfile ... start=<position>)
              ▲                                         │
              └── error / no start in 25 s / stall 15 s / EOF >30 s before end
```

When a source fails, the session loads the next best untried source into the same mpv window
at the last known position (minus 3 s). Sources that arrive late are used as backups.

Ranking: audio language match, then quality, then debrid-cached, then subtitles available.

## Layout

- `crates/reel-core` – all logic, no UI: Stremio API, addon client, stream parsing, filtering,
  `SourcePool`, failover `session`, mpv/VLC players, profile store
- `crates/reel-app` – Slint UI and wiring

## Icons

The SVG icons in `crates/reel-app/ui/icons/` are from [Lucide](https://lucide.dev),
licensed under the ISC license. Most are from the current Lucide release; `smile.svg`,
`music.svg`, `trash-2.svg` and `gamepad.svg` come from Lucide v0.263.1 (they were
renamed upstream since).

## Fonts

The bundled [Inter](https://rsms.me/inter/) font files in `crates/reel-app/ui/fonts/`
are licensed under the SIL Open Font License 1.1; see `LICENSE.txt` in that directory.

## Known limitations / not yet verified

- **Not run against a real Stremio account or real addons.** The login / `addonCollectionGet`
  request shapes follow community docs and should be checked on first run.
- **UI was compiled but not visually tested**, and the Windows named-pipe mpv code path was not
  compiled here (Linux sandbox). Failover logic and the mpv IPC layer are tested on Linux against a
  mock player and a fake mpv socket.
- Language / quality / size / subtitle info is parsed from addon text (names, titles, flags,
  filenames), so it is heuristic. Streams that don't state a size are not excluded by max size;
  streams with unknown resolution are excluded when a minimum quality is set.
- Only direct HTTP(S) streams (debrid + HTTP addons). Torrent info-hash, YouTube and external-URL
  streams are skipped.
- mpv runs in its own window (fullscreen). Reel's window stays behind it with a "Playing" banner.
- Next steps: gamepad/remote navigation, search page, Continue Watching row, bundled mpv.
