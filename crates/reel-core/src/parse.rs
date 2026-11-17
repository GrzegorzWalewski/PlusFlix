//! Heuristics that turn the free-text of an addon stream into structured facts.

use crate::lang::{self, DetectedLangs};
use crate::model::Stream;
use crate::settings::Quality;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub quality: Quality,
    pub size_bytes: Option<u64>,
    pub langs: DetectedLangs,
    /// `Some(true)` = debrid says it is cached/instant, `Some(false)` = needs a download first.
    pub cached: Option<bool>,
}

static QUALITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(2160p|4k|uhd|1440p|1080[pi]|720p|576p|480p|360p)\b").unwrap()
});
static SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\d+(?:[.,]\d+)?)\s*(tib|tb|gib|gb|mib|mb)\b").unwrap());
static CACHED_YES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[A-Za-z]{2,3}\+\]|⚡").unwrap());
static CACHED_NO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[[A-Za-z]{2,3}\s*download\]|\bnot cached\b").unwrap());

pub fn parse_quality(text: &str) -> Quality {
    QUALITY
        .find_iter(text)
        .map(|m| match m.as_str().to_ascii_lowercase().as_str() {
            "2160p" | "4k" | "uhd" => Quality::P2160,
            "1440p" => Quality::P1440,
            "1080p" | "1080i" => Quality::P1080,
            "720p" => Quality::P720,
            "576p" | "480p" => Quality::P480,
            _ => Quality::P360,
        })
        .max()
        // No quality marker at all is treated as 720p: most unlabeled addon
        // streams are HD-ish, and this keeps them eligible and reasonably ranked.
        .unwrap_or(Quality::P720)
}

/// Sizes are treated as binary (GB == GiB), which is what debrid addons print.
pub fn parse_size(text: &str) -> Option<u64> {
    let c = SIZE.captures(text)?;
    let n: f64 = c[1].replace(',', ".").parse().ok()?;
    let mult = match c[2].to_ascii_lowercase().as_str() {
        "tb" | "tib" => 1024f64.powi(4),
        "gb" | "gib" => 1024f64.powi(3),
        _ => 1024f64.powi(2),
    };
    Some((n * mult) as u64)
}

pub fn analyze(stream: &Stream) -> StreamInfo {
    let text = stream.text();
    let cached = if CACHED_NO.is_match(&text) {
        Some(false)
    } else if CACHED_YES.is_match(&text) {
        Some(true)
    } else {
        None
    };
    StreamInfo {
        quality: parse_quality(&text),
        size_bytes: stream
            .behavior_hints
            .video_size
            .or_else(|| parse_size(&text)),
        langs: lang::detect(&text),
        cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_variants() {
        assert_eq!(parse_quality("Movie.2160p.HDR"), Quality::P2160);
        assert_eq!(parse_quality("Movie 4K"), Quality::P2160);
        assert_eq!(parse_quality("x 1080p y 720p"), Quality::P1080);
        assert_eq!(parse_quality("DVDRip"), Quality::P720);
        assert_eq!(parse_quality("no quality marker"), Quality::P720);
    }

    #[test]
    fn sizes() {
        assert_eq!(
            parse_size("💾 2.5 GB ⚙️ src"),
            Some((2.5 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(parse_size("700 MB"), Some(700 * 1024 * 1024));
        assert_eq!(parse_size("no size here 1080p"), None);
    }

    #[test]
    fn debrid_cache_markers() {
        let s = Stream {
            name: Some("[RD+] Torrentio\n1080p".into()),
            ..Default::default()
        };
        assert_eq!(analyze(&s).cached, Some(true));
        let s = Stream {
            name: Some("[RD download] Torrentio\n1080p".into()),
            ..Default::default()
        };
        assert_eq!(analyze(&s).cached, Some(false));
    }

    #[test]
    fn video_size_hint_wins() {
        let mut s = Stream {
            title: Some("5 GB".into()),
            ..Default::default()
        };
        s.behavior_hints.video_size = Some(123);
        assert_eq!(analyze(&s).size_bytes, Some(123));
    }
}
