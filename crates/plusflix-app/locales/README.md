# PlusFlix I18n Locales

Interface translations for the PlusFlix app. One JSON file per language:
`en.json` (English, the source of truth and fallback) and `pl.json` (Polish).

## File format

Locale files are flat, UTF-8 encoded JSON objects mapping translation keys
to localized strings. No nesting.

- **Keys** — `snake_case`, kept in alphabetical order, never renamed after
  release.
- **Values** — plain strings in the target language.

```json
{
  "app_name": "PlusFlix",
  "close": "Close",
  "interface_language_label": "Interface language"
}
```

Locale files are compiled into the binary with `include_str!` (no runtime
scanning), and every language is listed in the `LANGUAGES` table plus the
match arm in `locale_json()` in `crates/plusflix-app/src/main.rs`.

## Adding a language

1. Copy `en.json` to `<code>.json` (BCP-47 language tag, e.g. `de.json`
   for German).
2. Translate the **values** — never the keys (left-hand side).
3. Add a row to `const LANGUAGES` in `src/main.rs`:
   `("de", "Deutsch")` (native display name).
4. Add a match arm in `locale_json()`: `"de" => Some(include_str!("../locales/de.json"))`.

Partial translations are safe: any key missing from the selected locale
falls back to the English value from `en.json`.

## Plural convention

Some keys exist in plural variants suffixed `_one` / `_few` / `_many`
(e.g. `source_noun_one` / `source_noun_few` / `source_noun_many`). The
suffix is picked in Rust by `plural_suffix(n)` (`src/main.rs`) and in Slint
by `PluralForm.form`, both implementing the Polish rule — the most
demanding rule in use, so one rule serves every locale:

- `n == 1` → `_one`
- `n % 10` in `2..=4` and `n % 100` not in `12..=14` → `_few`
- otherwise → `_many`

English only distinguishes `n == 1`, so its `_few` keys hold the same text
as `_many`. When adding a plural, add all three keys to every locale file.

## Embedded data and composition

Most user-visible values come from the API (titles, episode names) and are
pushed as raw data. Locale files only carry the fixed UI chrome. Where a
sentence has to embed a number or a name, the locale file stores a
**prefix** and/or **suffix** key and the value is composed at render time
(`play_starting_prefix` + source name, `sources_none_prefix` + count +
`sources_none_suffix`, `season_prefix` + number, etc.). Keep new keys
composable the same way — do not bake data into translated strings.

## Key stability rules

- **Never rename or remove** a key after release — it breaks all existing
  translations.
- Add new keys to `en.json` first; it is the source of truth and the final
  fallback.
- Keep `en.json` and `pl.json` key sets identical — `tests/i18n_scan.rs`
  enforces this.