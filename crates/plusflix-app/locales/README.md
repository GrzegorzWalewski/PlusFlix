# PlusFlix I18n Locales

This directory contains the localisation files for the PlusFlix app. The application discovers and loads these files at runtime.

## File format

Locale files are flat, UTF-8 encoded JSON objects mapping translation keys to localised strings.

- **Keys** – `snake_case`, never changed once released.
- **Values** – plain strings in the target language.

```json
{
  "app_name": "PlusFlix",
  "close": "Close",
  "done": "Done",
  "interface_language_label": "Interface language"
}
```

## Creating a new locale

1. Copy `en.json` and rename it using its **BCP-47 language tag** (e.g. `pl.json` for Polish, `de.json` for German, `fr.json` for French).
2. Translate the **values** into the target language.
3. **Do not** rename, remove, or translate any **keys** (left-hand side).

Partial translations are safe — any key missing from the selected locale falls back to the English value from `en.json`.

## Selecting a locale in the app

1. Open **Settings** (top-right icon or profile menu).
2. Scroll to the **Language** section.
3. Pick the desired language from the **Interface language** dropdown.

The app scans this directory at startup and lists every `.json` file it finds.

## Key stability rules

- **Never rename or remove** a key after it has been released — doing so breaks all existing translations.
- Always add new keys to `en.json` first; it is the source of truth and the final fallback.
- When adding a key, add a matching entry (even if untranslated) to any other locale files you maintain.
