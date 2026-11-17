//! Language table plus heuristics that pull audio / subtitle languages out of
//! the free-form text addons put into stream names and descriptions.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lang {
    Pl,
    En,
    De,
    Fr,
    Es,
    It,
    Pt,
    Ru,
    Uk,
    Cs,
    Sk,
    Nl,
    Hu,
    Tr,
    Ja,
    Ko,
    Zh,
}

struct Entry {
    lang: Lang,
    code2: &'static str,
    code3: &'static str,
    name: &'static str,
    flags: &'static [&'static str],
    /// Case-insensitive whole-word tokens.
    words: &'static [&'static str],
    /// Two-letter tokens that are only trusted when written in capitals ("PL", not "pl").
    caps2: &'static [&'static str],
}

const TABLE: &[Entry] = &[
    Entry {
        lang: Lang::Pl,
        code2: "pl",
        code3: "pol",
        name: "Polish",
        flags: &["🇵🇱"],
        words: &["pol", "polish", "polski", "polskie", "lektor", "polsub"],
        caps2: &["PL"],
    },
    Entry {
        lang: Lang::En,
        code2: "en",
        code3: "eng",
        name: "English",
        flags: &["🇬🇧", "🇺🇸"],
        words: &["eng", "english"],
        caps2: &["EN"],
    },
    Entry {
        lang: Lang::De,
        code2: "de",
        code3: "ger",
        name: "German",
        flags: &["🇩🇪"],
        words: &["ger", "deu", "german", "deutsch"],
        caps2: &["DE"],
    },
    Entry {
        lang: Lang::Fr,
        code2: "fr",
        code3: "fre",
        name: "French",
        flags: &["🇫🇷"],
        words: &[
            "fre",
            "fra",
            "french",
            "vf",
            "vff",
            "vfq",
            "vfi",
            "truefrench",
        ],
        caps2: &["FR"],
    },
    Entry {
        lang: Lang::Es,
        code2: "es",
        code3: "spa",
        name: "Spanish",
        flags: &["🇪🇸", "🇲🇽"],
        words: &["spa", "spanish", "castellano", "latino"],
        caps2: &["ES"],
    },
    // "IT" is deliberately not trusted: it collides with the movie title.
    Entry {
        lang: Lang::It,
        code2: "it",
        code3: "ita",
        name: "Italian",
        flags: &["🇮🇹"],
        words: &["ita", "italian"],
        caps2: &[],
    },
    Entry {
        lang: Lang::Pt,
        code2: "pt",
        code3: "por",
        name: "Portuguese",
        flags: &["🇵🇹", "🇧🇷"],
        words: &["por", "portuguese", "dublado"],
        caps2: &["PT"],
    },
    Entry {
        lang: Lang::Ru,
        code2: "ru",
        code3: "rus",
        name: "Russian",
        flags: &["🇷🇺"],
        words: &["rus", "russian"],
        caps2: &["RU"],
    },
    Entry {
        lang: Lang::Uk,
        code2: "uk",
        code3: "ukr",
        name: "Ukrainian",
        flags: &["🇺🇦"],
        words: &["ukr", "ukrainian"],
        caps2: &["UA"],
    },
    Entry {
        lang: Lang::Cs,
        code2: "cs",
        code3: "cze",
        name: "Czech",
        flags: &["🇨🇿"],
        words: &["cze", "ces", "czech"],
        caps2: &["CZ"],
    },
    Entry {
        lang: Lang::Sk,
        code2: "sk",
        code3: "slo",
        name: "Slovak",
        flags: &["🇸🇰"],
        words: &["slo", "slk", "slovak"],
        caps2: &["SK"],
    },
    Entry {
        lang: Lang::Nl,
        code2: "nl",
        code3: "dut",
        name: "Dutch",
        flags: &["🇳🇱"],
        words: &["dut", "nld", "dutch"],
        caps2: &["NL"],
    },
    Entry {
        lang: Lang::Hu,
        code2: "hu",
        code3: "hun",
        name: "Hungarian",
        flags: &["🇭🇺"],
        words: &["hun", "hungarian"],
        caps2: &["HU"],
    },
    Entry {
        lang: Lang::Tr,
        code2: "tr",
        code3: "tur",
        name: "Turkish",
        flags: &["🇹🇷"],
        words: &["tur", "turkish"],
        caps2: &["TR"],
    },
    Entry {
        lang: Lang::Ja,
        code2: "ja",
        code3: "jpn",
        name: "Japanese",
        flags: &["🇯🇵"],
        words: &["jpn", "jap", "japanese"],
        caps2: &["JP"],
    },
    Entry {
        lang: Lang::Ko,
        code2: "ko",
        code3: "kor",
        name: "Korean",
        flags: &["🇰🇷"],
        words: &["kor", "korean"],
        caps2: &["KR"],
    },
    Entry {
        lang: Lang::Zh,
        code2: "zh",
        code3: "chi",
        name: "Chinese",
        flags: &["🇨🇳"],
        words: &["chi", "zho", "chinese", "mandarin"],
        caps2: &["CN"],
    },
];

impl Lang {
    pub const ALL: &'static [Lang] = &[
        Lang::Pl,
        Lang::En,
        Lang::De,
        Lang::Fr,
        Lang::Es,
        Lang::It,
        Lang::Pt,
        Lang::Ru,
        Lang::Uk,
        Lang::Cs,
        Lang::Sk,
        Lang::Nl,
        Lang::Hu,
        Lang::Tr,
        Lang::Ja,
        Lang::Ko,
        Lang::Zh,
    ];

    fn entry(self) -> &'static Entry {
        TABLE
            .iter()
            .find(|e| e.lang == self)
            .expect("every Lang has a table entry")
    }

    pub fn code2(self) -> &'static str {
        self.entry().code2
    }
    pub fn code3(self) -> &'static str {
        self.entry().code3
    }
    pub fn name(self) -> &'static str {
        self.entry().name
    }

    /// Parses codes/names as used by Stremio subtitle addons ("pol", "pl", "Polish", "pt-BR").
    pub fn parse(s: &str) -> Option<Lang> {
        let s = s.trim().to_ascii_lowercase();
        let head = s.split(['-', '_']).next().unwrap_or(&s);
        TABLE.iter().find_map(|e| {
            let hit = head == e.code2
                || head == e.code3
                || head == e.name.to_ascii_lowercase()
                || e.words.contains(&head);
            hit.then_some(e.lang)
        })
    }
}

/// Languages found in the text of a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectedLangs {
    pub audio: Vec<Lang>,
    /// Subtitles the addon claims are embedded / bundled.
    pub subs: Vec<Lang>,
    /// "MULTi" release: several audio tracks, exact set unknown.
    pub multi: bool,
}

static SUB_SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(subs?|subtitles?|napisy|sub)\b[\s:=\-\[(]*([^\n|]{0,60})").unwrap()
});
static MULTI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bmulti\b").unwrap());
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());

fn detect_in(text: &str) -> Vec<Lang> {
    let mut found: Vec<Lang> = Vec::new();
    let mut push = |l: Lang| {
        if !found.contains(&l) {
            found.push(l)
        }
    };
    for e in TABLE {
        if e.flags.iter().any(|f| text.contains(f)) {
            push(e.lang);
        }
    }
    for m in WORD.find_iter(text) {
        let w = m.as_str();
        let lw = w.to_ascii_lowercase();
        for e in TABLE {
            if e.words.iter().any(|x| *x == lw) || e.caps2.contains(&w) {
                push(e.lang);
            }
        }
    }
    found
}

pub fn detect(text: &str) -> DetectedLangs {
    let mut subs = Vec::new();
    for cap in SUB_SEGMENT.captures_iter(text) {
        let keyword = cap
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        let mut langs = detect_in(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        if langs.is_empty() && keyword == "napisy" {
            langs.push(Lang::Pl);
        }
        for l in langs {
            if !subs.contains(&l) {
                subs.push(l);
            }
        }
    }
    // Remove the subtitle segments before looking for audio languages so that
    // "ENG audio, subs: PL" doesn't claim Polish audio.
    let audio_text = SUB_SEGMENT.replace_all(text, " ");
    let mut audio = detect_in(&audio_text);
    // Lektor/dubbing in Polish releases is audio by definition.
    if audio_text.to_lowercase().contains("lektor") && !audio.contains(&Lang::Pl) {
        audio.push(Lang::Pl);
    }
    DetectedLangs {
        audio,
        subs,
        multi: MULTI.is_match(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_words() {
        let d = detect("Movie 2023 1080p\n🇬🇧 / 🇵🇱");
        assert!(d.audio.contains(&Lang::En) && d.audio.contains(&Lang::Pl));
        let d = detect("Movie.2023.1080p.WEB-DL.Lektor.PL.x264");
        assert_eq!(d.audio, vec![Lang::Pl]);
    }

    #[test]
    fn subs_are_not_audio() {
        let d = detect("Movie 1080p ENG audio | Subs: PL, DE");
        assert_eq!(d.audio, vec![Lang::En]);
        assert!(d.subs.contains(&Lang::Pl) && d.subs.contains(&Lang::De));
        let d = detect("Film.2020.1080p.Napisy");
        assert_eq!(d.subs, vec![Lang::Pl]);
        assert!(d.audio.is_empty());
    }

    #[test]
    fn the_movie_it_is_not_italian() {
        let d = detect("IT (2017) 1080p BluRay");
        assert!(d.audio.is_empty());
        assert!(detect("Film 1080p ITA").audio.contains(&Lang::It));
    }

    #[test]
    fn multi_flag_and_parse() {
        assert!(detect("Movie MULTi 1080p").multi);
        assert_eq!(Lang::parse("pol"), Some(Lang::Pl));
        assert_eq!(Lang::parse("pt-BR"), Some(Lang::Pt));
        assert_eq!(Lang::parse("Polish"), Some(Lang::Pl));
        assert_eq!(Lang::parse("xx"), None);
    }
}
