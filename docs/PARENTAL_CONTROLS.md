# Parental controls

PlusFlix filters content per profile. A child profile is one with an age between 3 and 17 (children
count toward the 5-profile cap). While a child profile is active, catalogs, search and genre pages
show only titles rated at or below the child's age; favorites and continue-watching are filtered the
same way, and opening an over-age title is blocked at playback. Unrated titles are treated as 18+
and hidden from every child profile. The short version lives in the [README](../README.md).

## Where the ratings come from

Stremio meta/catalog responses have no dedicated age-rating field, so PlusFlix reads age-rating
tokens out of the `genres` arrays of addon catalog and meta responses, mapping each accepted token
(matched case- and spacing/separator-insensitively) to a minimum age. Some addons publish their
ratings in the `app_extras` block instead (`app_extras.certification` plus the per-country
`app_extras.certifications` map, e.g. `{"US":"R","PL":"16"}`) and keep `genres` to real genres.
PlusFlix merges all of these; the strictest rating wins.

## Accepted rating tokens

- `G`, `TV-G`, `TV-Y`, `U` → 0
- `TV-Y7` → 7
- `PG`, `TV-PG` → 8
- `12A` → 12
- `PG-13` → 13
- `TV-14` → 14
- `R` → 17
- `NC-17`, `X`, `TV-MA` → 18
- a bare number like `16` means that age

## What is filtered

While a child profile is active:

- catalogs and genre pages
- search
- favorites
- continue-watching
- opening an over-age title is blocked at playback

## The parent PIN

- Created together with the first child profile, entered twice to confirm. Digits only, non-empty,
  no fixed length.
- Required to enter an adult profile, to create, edit or delete profiles, and to turn manage mode on.
  Entering a child profile stays PIN-free.
- Stored as a salted SHA-256 hash in the same `store.json`, never plaintext.
- Auto-cleared when the last child profile is removed.
- Bypassable by editing `store.json` (delete the PIN hash, flip a profile's kind, or bump its age) —
  parental-control grade, not security.
- Forgotten PIN: delete `store.json` (loses the profiles and settings).

## Addon compatibility

### Works

- AIOMetadata (public instance https://aiometadata.elfhosted.com): keeps `genres` to real genres and
  publishes its age ratings via `app_extras`, so child filtering works.
- Any addon that puts TMDB/US certification tokens into `genres`.

### Doesn't work

- Stremio's default Cinemeta: real genres only (e.g. `["Drama","Fantasy"]`), no ratings anywhere —
  every item arrives unrated, so with only Cinemeta installed a child profile's catalog is empty.
- A self-hosted `stremio-catalog-providers` instance in kids-catalog mode: filters addon-side and
  publishes no ratings either.

### The AIOMetadata caveat

AIOMetadata's own kid-safety is server-side Content Rating filtering configured on its /configure
page. It applies to everyone using that addon config — account-wide, for all PlusFlix profiles,
adults included, not per profile. PlusFlix's per-profile filtering is its own layer on top.