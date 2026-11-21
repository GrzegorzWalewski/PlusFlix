# PlusFlix

A Stremio-backed desktop player written in Rust (Slint UI + mpv). It signs in with your
Stremio account, uses the addons installed there, and plays with automatic source failover.

## Run

```
# needs mpv (>= 0.38) on PATH; on Windows e.g. `winget install mpv` or put mpv.exe next to plusflix-app.exe
cargo run --release -p plusflix-app
cargo test --workspace
```

Data (login key, profiles, favorites, progress) is stored in one JSON file under the OS config
dir (`PlusFlix/config/store.json`). Your Stremio password is never stored, only the session key.

## Features

- Stremio account login; addons are read from the account's addon collection
- Pages: Home, Movies, Series, Favorites (+ Settings), details with episode list for series
- Up to 5 local profiles, each with its own favorites, watch progress and settings
- Continue Watching row on Home: local progress plus entries synced from the account's
  Stremio library (`datastoreGet` on `libraryItem`, same rule Stremio uses)
- Interface language switcher (Settings → Language): English and Polski, applied immediately
  without restart; the choice is persisted in the store. Locale files live in
  `crates/plusflix-app/locales/` (`en.json`, `pl.json`); see `README.md` there for adding a new
  language. Missing keys fall back to English.
- Per-profile settings: player (mpv / VLC), audio language (prefer / must), subtitles language, subtitles requirement
  (required / required only if audio isn't the preferred language / not required), minimum
  quality, maximum file size
- Sources load in the background as soon as a title (or episode) is opened

## Parental controls (child profiles & PIN)

Profiles are the existing feature (up to 5 in total; children count toward that cap); a child
profile is one with an age between 3 and 17. While it is active, catalogs, search and genre
pages show only titles whose age rating is at or below the child's age; favorites and
continue-watching are filtered the same way, and opening an over-age title is blocked at
playback. Unrated titles are treated as 18+ and hidden from every child profile.

Where the ratings come from: Stremio meta/catalog responses have no dedicated age-rating field,
so PlusFlix reads age-rating tokens out of the `genres` arrays of addon catalog and meta
responses, mapping each accepted token (case- and spacing-insensitive) to a minimum age. Some
addons publish their ratings in the `app_extras` block instead (`app_extras.certification` plus
the per-country `app_extras.certifications` map, e.g. `{"US":"R","PL":"16"}`) and keep `genres`
to real genres; PlusFlix merges both sources and the strictest rating wins. Accepted tokens:

- `G`, `TV-G`, `TV-Y`, `U` → 0
- `TV-Y7` → 7
- `PG`, `TV-PG` → 8
- `12A` → 12
- `PG-13` → 13
- `TV-14` → 14
- `R` → 17
- `NC-17`, `X`, `TV-MA` → 18
- a bare number like `16` means that age

Tested example: AIOMetadata (public instance https://aiometadata.elfhosted.com), the
community-standard kid-safe metadata addon, keeps `genres` to real genres and publishes its age
ratings via `app_extras` (`certification` plus per-country `certifications`), so with
AIOMetadata installed child filtering works. Its own kid-safety remains the addon's server-side
Content Rating filtering (configured on its /configure page), which applies to everyone using
that addon config: account-wide for all PlusFlix profiles (adults included), not per profile;
PlusFlix's per-profile filtering is its own layer on top. Stremio's default Cinemeta addon
publishes no age ratings at all (real genres only, e.g. `["Drama","Fantasy"]`, and no
`app_extras` ratings either), so with just Cinemeta installed every item arrives unrated and a
child profile's catalog stays empty. A self-hosted `stremio-catalog-providers` instance in
kids-catalog mode filters the same addon-side way and is no rating source either. Any addon
that puts TMDB/US certification tokens into `genres` works too.

The parent PIN is created together with the first child profile (entered twice to confirm; digits
only, non-empty, no fixed length). It is required to enter an adult profile, to create, edit or
delete profiles, and to turn manage mode on; entering a child profile stays PIN-free. It is
stored as a salted SHA-256 hash, never plaintext, in the same `store.json`. This is
parental-control grade, not security: a forgotten PIN means deleting the store file (losing the
profiles and settings).

## How failover works

```
open title ──► SourceLoader (one task per addon) ──► SourcePool  ◄── next_untried(settings)
                                                        ▲                    │
                  keeps adding sources while playing ───┘                    ▼
Play ──► session::run ──► best untried source ──► mpv (loadfile ... start=<position>)
              ▲                                         │
              └── error / no start in 25 s / stall 10 s / EOF >30 s before end
```

When a source fails, the session loads the next best untried source into the same mpv window
at the last known position (minus 3 s). Sources that arrive late are used as backups.

Ranking: audio language match, then quality, then debrid-cached, then subtitles available.

## Layout

- `crates/plusflix-core` – all logic, no UI: Stremio API, addon client, stream parsing, filtering,
  `SourcePool`, failover `session`, mpv/VLC players, profile store
- `crates/plusflix-app` – Slint UI and wiring

## Icons

The SVG icons in `crates/plusflix-app/ui/icons/` are from [Lucide](https://lucide.dev),
licensed under the ISC license. Most are from the current Lucide release; `smile.svg`,
`music.svg`, `trash-2.svg` and `gamepad.svg` come from Lucide v0.263.1 (they were
renamed upstream since).

## Fonts

The bundled [Inter](https://rsms.me/inter/) font files in `crates/plusflix-app/ui/fonts/`
are licensed under the SIL Open Font License 1.1; see `LICENSE.txt` in that directory.

