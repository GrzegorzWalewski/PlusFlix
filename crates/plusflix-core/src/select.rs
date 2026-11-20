//! Per-profile filtering and ranking of candidate streams.

use crate::lang::Lang;
use crate::model::Subtitle;
use crate::parse::StreamInfo;
use crate::settings::{LanguageMode, Quality, Settings, SubtitleRequirement};

/// External subtitles (from subtitle addons) known for the title being played.
#[derive(Debug, Clone, Default)]
pub struct ExternalSubs {
    pub list: Vec<(Lang, Subtitle)>,
}

impl ExternalSubs {
    pub fn has(&self, lang: Lang) -> bool {
        self.list.iter().any(|(l, _)| *l == lang)
    }
    pub fn for_lang(&self, lang: Lang) -> Vec<Subtitle> {
        self.list
            .iter()
            .filter(|(l, _)| *l == lang)
            .map(|(_, s)| s.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reject {
    BelowMinQuality,
    TooLarge,
    WrongAudioLanguage,
    MissingSubtitles,
}

impl Reject {
    /// Numeric reason code for the UI (the `reject_*` locale keys):
    /// 1 = below min quality, 2 = too large, 3 = wrong audio, 4 = missing subs.
    pub fn code(&self) -> i32 {
        match self {
            Reject::BelowMinQuality => 1,
            Reject::TooLarge => 2,
            Reject::WrongAudioLanguage => 3,
            Reject::MissingSubtitles => 4,
        }
    }

    /// Why the profile's settings disabled this source (for the sources list).
    pub fn label(&self) -> &'static str {
        match self {
            Reject::BelowMinQuality => "below minimum quality",
            Reject::TooLarge => "larger than max file size",
            Reject::WrongAudioLanguage => "audio language is not the must-have one",
            Reject::MissingSubtitles => "required subtitles missing",
        }
    }
}

/// How well the audio matches the profile's language. Higher is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AudioMatch {
    /// A different language is explicitly tagged.
    Other,
    /// Nothing tagged.
    Unknown,
    /// "MULTi" release - very likely contains the language, not guaranteed.
    Multi,
    Exact,
}

/// Ranking key; derive(Ord) compares fields in order: language first, then quality, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    pub audio: AudioMatch,
    pub quality: Quality,
    /// 2 = cached, 1 = unknown, 0 = needs a debrid download first (slow start)
    pub cached: u8,
    /// 1 if the subtitle language is available (embedded or external)
    pub subs: u8,
}

pub fn audio_match(info: &StreamInfo, s: &Settings) -> AudioMatch {
    let Some(want) = s.audio_language else {
        return AudioMatch::Exact;
    };
    if info.langs.audio.contains(&want) {
        AudioMatch::Exact
    } else if info.langs.multi {
        AudioMatch::Multi
    } else if info.langs.audio.is_empty() {
        AudioMatch::Unknown
    } else {
        AudioMatch::Other
    }
}

pub fn evaluate(info: &StreamInfo, s: &Settings, ext: &ExternalSubs) -> Result<Rank, Reject> {
    // Quality: unknown resolution can't prove it meets a minimum, so it is rejected.
    if s.min_quality > Quality::Unknown && info.quality < s.min_quality {
        return Err(Reject::BelowMinQuality);
    }
    // Size: if the addon doesn't tell us, we can't hold it against the stream.
    if let (Some(max), Some(size)) = (s.max_filesize_bytes(), info.size_bytes) {
        if size > max {
            return Err(Reject::TooLarge);
        }
    }

    let audio = audio_match(info, s);
    if s.language_mode == LanguageMode::Must
        && s.audio_language.is_some()
        && !matches!(audio, AudioMatch::Exact | AudioMatch::Multi)
    {
        return Err(Reject::WrongAudioLanguage);
    }

    let sub_available = s
        .subtitle_language
        .map(|l| info.langs.subs.contains(&l) || ext.has(l))
        .unwrap_or(false);
    let audio_is_preferred =
        s.audio_language.is_none() || matches!(audio, AudioMatch::Exact | AudioMatch::Multi);
    let need_subs = match s.effective_subtitle_requirement() {
        SubtitleRequirement::Required => true,
        // With no preferred audio language this is vacuous: nothing is "not the preferred one".
        SubtitleRequirement::RequiredIfAudioNotPreferred => !audio_is_preferred,
        SubtitleRequirement::NotRequired => false,
    };
    if need_subs && !sub_available {
        return Err(Reject::MissingSubtitles);
    }

    Ok(Rank {
        audio,
        quality: info.quality,
        cached: match info.cached {
            Some(true) => 2,
            None => 1,
            Some(false) => 0,
        },
        subs: sub_available as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::DetectedLangs;

    fn info(q: Quality, audio: &[Lang], subs: &[Lang], size_gb: Option<f64>) -> StreamInfo {
        StreamInfo {
            quality: q,
            size_bytes: size_gb.map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64),
            langs: DetectedLangs {
                audio: audio.to_vec(),
                subs: subs.to_vec(),
                multi: false,
            },
            cached: None,
        }
    }

    fn pl_settings() -> Settings {
        Settings {
            audio_language: Some(Lang::Pl),
            subtitle_language: Some(Lang::Pl),
            ..Settings::default()
        }
    }

    #[test]
    fn min_quality_and_size() {
        let mut s = pl_settings();
        s.min_quality = Quality::P1080;
        s.max_filesize_gb = Some(10.0);
        let ext = ExternalSubs::default();
        assert_eq!(
            evaluate(&info(Quality::P720, &[], &[], None), &s, &ext),
            Err(Reject::BelowMinQuality)
        );
        assert_eq!(
            evaluate(&info(Quality::Unknown, &[], &[], None), &s, &ext),
            Err(Reject::BelowMinQuality)
        );
        assert_eq!(
            evaluate(&info(Quality::P1080, &[], &[], Some(11.0)), &s, &ext),
            Err(Reject::TooLarge)
        );
        assert!(evaluate(&info(Quality::P1080, &[], &[], Some(9.0)), &s, &ext).is_ok());
        assert!(evaluate(&info(Quality::P2160, &[], &[], None), &s, &ext).is_ok());
    }

    #[test]
    fn must_language_filters_others() {
        let mut s = pl_settings();
        s.language_mode = LanguageMode::Must;
        let ext = ExternalSubs::default();
        assert!(evaluate(&info(Quality::P1080, &[Lang::Pl], &[], None), &s, &ext).is_ok());
        assert_eq!(
            evaluate(&info(Quality::P1080, &[Lang::En], &[], None), &s, &ext),
            Err(Reject::WrongAudioLanguage)
        );
        assert_eq!(
            evaluate(&info(Quality::P1080, &[], &[], None), &s, &ext),
            Err(Reject::WrongAudioLanguage)
        );
    }

    #[test]
    fn prefer_language_ranks_before_quality() {
        let s = pl_settings();
        let ext = ExternalSubs::default();
        let pl720 = evaluate(&info(Quality::P720, &[Lang::Pl], &[], None), &s, &ext).unwrap();
        let en2160 = evaluate(&info(Quality::P2160, &[Lang::En], &[], None), &s, &ext).unwrap();
        assert!(pl720 > en2160);
    }

    #[test]
    fn subtitle_modes() {
        let ext_pl = ExternalSubs {
            list: vec![(
                Lang::Pl,
                Subtitle {
                    id: None,
                    url: "http://s/pl.srt".into(),
                    lang: "pol".into(),
                },
            )],
        };
        let none = ExternalSubs::default();
        let mut s = pl_settings();

        // Required: needs subs even if the audio is Polish.
        s.subtitle_requirement = SubtitleRequirement::Required;
        assert_eq!(
            evaluate(&info(Quality::P1080, &[Lang::Pl], &[], None), &s, &none),
            Err(Reject::MissingSubtitles)
        );
        assert!(evaluate(&info(Quality::P1080, &[Lang::Pl], &[], None), &s, &ext_pl).is_ok());
        assert!(evaluate(
            &info(Quality::P1080, &[Lang::En], &[Lang::Pl], None),
            &s,
            &none
        )
        .is_ok());

        // Only if audio isn't the preferred language.
        s.subtitle_requirement = SubtitleRequirement::RequiredIfAudioNotPreferred;
        assert!(evaluate(&info(Quality::P1080, &[Lang::Pl], &[], None), &s, &none).is_ok());
        assert_eq!(
            evaluate(&info(Quality::P1080, &[Lang::En], &[], None), &s, &none),
            Err(Reject::MissingSubtitles)
        );
        assert!(evaluate(&info(Quality::P1080, &[Lang::En], &[], None), &s, &ext_pl).is_ok());

        // Not required.
        s.subtitle_requirement = SubtitleRequirement::NotRequired;
        assert!(evaluate(&info(Quality::P1080, &[Lang::En], &[], None), &s, &none).is_ok());

        // No subtitle language chosen -> requirement is ignored.
        s.subtitle_requirement = SubtitleRequirement::Required;
        s.subtitle_language = None;
        assert!(evaluate(&info(Quality::P1080, &[Lang::En], &[], None), &s, &none).is_ok());
    }
}
