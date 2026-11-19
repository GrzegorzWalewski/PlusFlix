use crate::lang::Lang;
use serde::{Deserialize, Serialize};

/// Ordered so that `a < b` means "b is better quality".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum Quality {
    #[default]
    Unknown,
    P360,
    P480,
    P720,
    P1080,
    P1440,
    P2160,
}

impl Quality {
    pub const CHOICES: &'static [Quality] = &[
        Quality::Unknown,
        Quality::P480,
        Quality::P720,
        Quality::P1080,
        Quality::P1440,
        Quality::P2160,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Unknown => "Any",
            Quality::P360 => "360p",
            Quality::P480 => "480p",
            Quality::P720 => "720p",
            Quality::P1080 => "1080p",
            Quality::P1440 => "1440p",
            Quality::P2160 => "4K",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LanguageMode {
    /// Streams in the language are ranked first, others still allowed.
    #[default]
    Prefer,
    /// Streams that are not (or may not be) in the language are never used.
    Must,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SubtitleRequirement {
    /// Only sources that come with subtitles in the subtitle language are used.
    Required,
    /// Subtitles are required only when the audio is not in the preferred language.
    RequiredIfAudioNotPreferred,
    #[default]
    NotRequired,
}

/// Which external player plays the streams. mpv is the default (it supports
/// custom HTTP headers and precise start times); VLC is the alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PlayerKind {
    #[default]
    Mpv,
    Vlc,
}

impl PlayerKind {
    pub fn label(self) -> &'static str {
        match self {
            PlayerKind::Mpv => "MPV",
            PlayerKind::Vlc => "VLC",
        }
    }
}

/// Per-profile playback settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub audio_language: Option<Lang>,
    pub language_mode: LanguageMode,
    pub subtitle_language: Option<Lang>,
    pub subtitle_requirement: SubtitleRequirement,
    pub min_quality: Quality,
    /// Largest acceptable file in GiB; `None` = unlimited.
    pub max_filesize_gb: Option<f64>,
    /// Which external player to use.
    pub player: PlayerKind,
    /// Addons (by manifest id) whose sources are waited for before the first
    /// playback start, even when other addons deliver earlier.
    pub wait_for_addons: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio_language: None,
            language_mode: LanguageMode::Prefer,
            subtitle_language: None,
            subtitle_requirement: SubtitleRequirement::NotRequired,
            min_quality: Quality::Unknown,
            max_filesize_gb: None,
            player: PlayerKind::default(),
            wait_for_addons: Vec::new(),
        }
    }
}

impl Settings {
    /// Subtitles can only be "required" if a subtitle language is chosen.
    pub fn effective_subtitle_requirement(&self) -> SubtitleRequirement {
        if self.subtitle_language.is_none() {
            SubtitleRequirement::NotRequired
        } else {
            self.subtitle_requirement
        }
    }

    pub fn max_filesize_bytes(&self) -> Option<u64> {
        self.max_filesize_gb
            .filter(|g| *g > 0.0)
            .map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64)
    }
}
