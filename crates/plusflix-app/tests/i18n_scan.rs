//! I18n lint tests: no untranslated string literals in the .slint sources,
//! and `en.json`/`pl.json` key parity.
//!
//! Every user-visible string in `crates/plusflix-app/ui` must come from a
//! `t-*` property (pushed by `apply_language` in src/main.rs), from a
//! model/struct-field binding, or be an allowed non-translatable literal
//! (brand name, pure symbol, TestWindow harness string).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const UI_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ui");
const LOCALES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/locales");

/// Properties that render user-visible text (tooltip-related included).
const WATCHED: [&str; 7] = [
    "text",
    "placeholder-text",
    "accessible-label",
    "tooltip",
    "title",
    "caption",
    "label",
];

#[test]
fn no_untranslated_string_literals_in_ui() {
    let mut files = Vec::new();
    collect_slint_files(Path::new(UI_DIR), &mut files);
    assert!(!files.is_empty(), "no .slint files found under {UI_DIR}");
    files.sort();
    let mut failures = Vec::new();
    for file in &files {
        scan_file(file, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "untranslated string literals (use a t-* property or a model/struct-field \
         binding instead; see locales/README.md):\n{}",
        failures.join("\n"),
    );
}

#[test]
fn locale_files_have_identical_key_sets() {
    let en = json_keys(&read(&Path::new(LOCALES_DIR).join("en.json")));
    let pl = json_keys(&read(&Path::new(LOCALES_DIR).join("pl.json")));
    assert!(!en.is_empty(), "en.json has no keys");
    let (en, pl) = (BTreeSet::from_iter(en), BTreeSet::from_iter(pl));
    let missing_in_pl: Vec<_> = en.difference(&pl).collect();
    let missing_in_en: Vec<_> = pl.difference(&en).collect();
    assert!(
        missing_in_pl.is_empty() && missing_in_en.is_empty(),
        "locale key mismatch: missing in pl.json: {missing_in_pl:?}; \
         missing in en.json: {missing_in_en:?}",
    );
}

// ---------------------------------------------------------------- scanner

fn collect_slint_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_slint_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "slint") {
            out.push(path);
        }
    }
}

/// Scans one .slint file for watched properties assigned a bare string
/// literal that is not on the allow-list.
fn scan_file(path: &Path, failures: &mut Vec<String>) {
    let stripped = strip_comments(&read(path));
    let chars: Vec<char> = stripped.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Only start a match at an identifier boundary (not inside or right
        // after another word: `title-t` / `play-label` are different names).
        let boundary = i == 0 || !(is_word_char(chars[i - 1]) || chars[i - 1] == '-');
        if boundary {
            for name in WATCHED {
                let name_chars: Vec<char> = name.chars().collect();
                let end = i + name_chars.len();
                if end > chars.len() || chars[i..end] != name_chars[..] {
                    continue;
                }
                if matches!(chars.get(end), Some(&c) if is_word_char(c) || c == '-') {
                    continue;
                }
                // Skip whitespace, require `:` then whitespace then a `"`.
                let mut j = end;
                while matches!(chars.get(j), Some(&c2) if c2.is_whitespace()) {
                    j += 1;
                }
                if chars.get(j) != Some(&':') {
                    continue;
                }
                j += 1;
                while matches!(chars.get(j), Some(&c2) if c2.is_whitespace()) {
                    j += 1;
                }
                // Not a string literal: a property reference, an expression or
                // a two-way binding — nothing to translate.
                if chars.get(j) != Some(&'"') {
                    continue;
                }
                let (literal, _) = read_string_literal(&chars, j);
                if !allowed_literal(&literal) {
                    failures.push(format!(
                        "{}: `{}` is a bare string literal ({literal:?})",
                        path.display(),
                        name,
                    ));
                }
            }
        }
        i += 1;
    }
}

/// Reads the string literal starting at `chars[start] == '"'`, returning
/// (unescaped content, index after the closing quote).
fn read_string_literal(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut escaped = false;
    let mut i = start + 1;
    while let Some(&c) = chars.get(i) {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return (out, i + 1);
        } else {
            out.push(c);
        }
        i += 1;
    }
    (out, i)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Brand names (never translated), TestWindow harness strings and pure
/// symbols/digits are exempt; everything else must go through a t-* key.
fn allowed_literal(literal: &str) -> bool {
    let t = literal.trim();
    if t.is_empty() {
        return true;
    }
    const BRANDS: [&str; 6] = ["PlusFlix", "mpv", "MPV", "VLC", "Stremio", "Cinemeta"];
    if BRANDS.contains(&t) {
        return true;
    }
    // Fixture strings in the TestWindow harness at the bottom of main.slint.
    const HARNESS: [&str; 5] = ["Gear", "Close", "Fav", "Tip", "Search…"];
    if HARNESS.contains(&t) {
        return true;
    }
    // Pure symbols and digits (episode markers "S"/"E", counts, ×, %, …).
    t.chars()
        .all(|c| matches!(c, '×' | '%' | '…' | '—' | 'S' | 'E' | ' ') || c.is_ascii_digit())
}

/// Comment-free copy of `src` (string literals preserved, newlines kept so
/// error positions stay meaningful).
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else {
                match c {
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c2 in chars.by_ref() {
                    if c2 == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c2 in chars.by_ref() {
                    if prev == '*' && c2 == '/' {
                        break;
                    }
                    if c2 == '\n' {
                        out.push('\n');
                    }
                    prev = c2;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------- json keys

/// Top-level keys of a flat JSON object: every string literal (outside any
/// other string) whose next non-whitespace character is `:`.
fn json_keys(src: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if in_string {
            if escaped {
                current.push(c);
                escaped = false;
            } else {
                match c {
                    '\\' => escaped = true,
                    '"' => {
                        in_string = false;
                        let mut j = i + 1;
                        while matches!(chars.get(j), Some(&c2) if c2.is_whitespace()) {
                            j += 1;
                        }
                        if chars.get(j) == Some(&':') {
                            keys.push(std::mem::take(&mut current));
                        } else {
                            current.clear();
                        }
                    }
                    _ => current.push(c),
                }
            }
        } else if c == '"' {
            in_string = true;
        }
        i += 1;
    }
    keys
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
