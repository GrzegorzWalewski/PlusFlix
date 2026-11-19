# Plan: Add Translation / i18n to PlusFlix

## Overview

Extract all hardcoded UI strings into an `en.json` locale file, build a minimal translation
loader in `plusflix-core`, wire it into `plusflix-app` via the existing Slint `app_language` property,
and add a language-switcher control to the Settings page.

---

## Steps

### 1. [tier:fast] Audit all hardcoded strings in Slint files

Grep `crates/plusflix-app/ui/` for every quoted string literal that is user-visible (labels,
placeholders, error messages, section headings, tooltips). Produce a deduplicated inventory
with the source file + line for each string.

---

### 2. [tier:medium] Create `locales/en.json` with all extracted strings

Using the inventory from step 1, create `crates/plusflix-app/locales/en.json` with a flat
key→value map (snake_case keys, English values). Keys should be descriptive
(`sign_in`, `search_placeholder`, `nothing_to_show_hint`, etc.).

[acceptance]
check: fileExists path=crates/plusflix-app/locales/en.json
criteria: Every user-visible string found in the audit is present as a key; no key is duplicated; valid JSON.
deliverable: crates/plusflix-app/locales/en.json
[/acceptance]

---

### 3. [tier:fast] Document `en.json` for translators

Add a short `locales/README.md` explaining the file format, how to create a new locale
(copy `en.json` → `pl.json`, translate values, leave keys unchanged), and how to select
it in the app. No code changes.

[acceptance]
check: fileExists path=crates/plusflix-app/locales/README.md
criteria: README covers file format, creation workflow, and in-app selection.
deliverable: crates/plusflix-app/locales/README.md
[/acceptance]

---

### 4. [tier:medium] Add translation loader to `plusflix-core`

Create `crates/plusflix-core/src/i18n.rs`:

- `struct Translations { map: HashMap<String, String> }`
- `fn load(locale_code: &str, locales_dir: &Path) -> Result<Translations>`  
  — reads `<locales_dir>/<locale_code>.json`, falls back to `en.json` for missing keys.
- `fn t(&self, key: &str) -> &str` — returns translated string or the key itself as fallback.
- Expose `Translations` and `load` from `lib.rs`.

[acceptance]
check: testsPass
check: buildPasses
criteria: Unit tests cover: load en.json, load a stub pl.json, fallback to English for a missing key, and key-not-found fallback returning the key itself.
deliverable: crates/plusflix-core/src/i18n.rs
[/acceptance]

---

### 5. [tier:medium] Wire translations into `plusflix-app` / `main.rs`

- At startup, load the active locale (`en` by default) from `store.json` (add `locale` field).
- Pass a `Arc<Translations>` into wherever Slint properties are set.
- Replace all hardcoded string assignments to Slint `in-out property<string>` fields with
  calls to `t("key")`, so every label/placeholder/message flows through the translation map.
- On language-switch callback (see step 6), reload translations and push updated strings to
  all Slint properties.

[acceptance]
check: buildPasses
criteria: App builds without errors; all previously hardcoded strings are now sourced from the loaded locale file.
deliverable: crates/plusflix-app/src/main.rs (modified)
[/acceptance]

---

### 6. [tier:medium] Add language-switcher to the Settings page in `main.slint`

- Add `callback language-changed(string)` to the root component (locale code, e.g. `"pl"`).
- In the Settings screen, add a "Language" row with a `ComboBox` (or equivalent Slint
  control) listing available locales discovered at runtime (locale codes → display names
  supplied from Rust).
- Use the existing `app_language` / `app_language_name` properties for the current
  selection; bind the ComboBox to them.
- Wire `language-changed` in `main.rs` to reload translations and update all bound
  properties.

[acceptance]
check: buildPasses
criteria: Settings page shows the language selector; selecting a locale fires the callback; UI strings update without restart; selection persists across app restarts via store.json.
deliverable: crates/plusflix-app/ui/main.slint (modified), crates/plusflix-app/src/main.rs (modified)
[/acceptance]

---

### 7. [tier:medium] Persist selected locale in `store.json`

- Add `locale: String` field to the store struct in `plusflix-core` (default `"en"`).
- Save it when the user changes language; load it on startup before building the UI.

[acceptance]
check: testsPass
check: buildPasses
criteria: Store serialises/deserialises the new field; existing stores without the field default to "en" gracefully.
deliverable: crates/plusflix-core/src/store.rs (or equivalent store module)
[/acceptance]

---

### 8. [tier:medium] Ship a starter `pl.json`

Create `crates/plusflix-app/locales/pl.json` as a fully translated Polish locale (all keys from
`en.json`, Polish values). This doubles as a concrete example for future translators.

[acceptance]
check: fileExists path=crates/plusflix-app/locales/pl.json
criteria: pl.json contains every key present in en.json; all values are Polish translations; valid JSON.
deliverable: crates/plusflix-app/locales/pl.json
[/acceptance]

---

### 9. [tier:medium] End-to-end verification — build & run

Build the full workspace and run all tests. Fix any compilation errors or test failures
introduced by the changes above.

[acceptance]
check: buildPasses
check: testsPass
criteria: `cargo build --workspace` succeeds; `cargo test --workspace` passes with no new failures.
deliverable: Clean workspace build
[/acceptance]

---

### 10. [tier:heavy] Architecture & completeness review

Review the full implementation across all changed files:

- Translation key naming consistency and completeness (no string left hardcoded).
- Fallback chain correctness (missing key → English → raw key).
- Thread-safety of `Translations` shared between Tokio tasks and the Slint main thread.
- Store migration safety for existing users without a `locale` field.
- Locale discovery at runtime (what happens when no locale files are found).
- Future-proofing: pluralisation, RTL, format strings with arguments.

[acceptance]
criteria: Review produces a written list of any issues found, with severity (blocker / warning / suggestion). All blockers are resolved or tracked.
deliverable: Review notes (inline comments or appended section in this PLAN.md)
[/acceptance]
