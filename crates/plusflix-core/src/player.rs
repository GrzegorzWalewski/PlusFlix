//! The interface between the failover session and an actual video player.

use crate::error::Result;
use crate::lang::Lang;
use crate::model::Subtitle;
use std::collections::HashMap;
use std::future::Future;

#[derive(Debug, Clone)]
pub struct LoadRequest {
    pub url: String,
    /// Extra HTTP request headers some addons require.
    pub headers: HashMap<String, String>,
    /// Where to start, in seconds.
    pub start_secs: f64,
    /// Preferred audio / subtitle languages for track auto-selection.
    pub audio_lang: Option<Lang>,
    pub subtitle_lang: Option<Lang>,
    /// External subtitle files in the subtitle language.
    pub subtitles: Vec<Subtitle>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// The file opened and playback is about to run.
    Started,
    Position(f64),
    Duration(f64),
    /// `true` while the player is stalled waiting for data.
    Buffering(bool),
    /// Reached the end of the file.
    Ended,
    /// The user closed the player / it exited.
    Closed,
    Error(String),
    /// The user manually skipped this source (e.g. a "next source" button).
    /// Not a failure: the session should move the source to the back of the
    /// failover queue instead of marking it tried.
    Skipped,
}

pub trait Player: Send {
    /// Replace whatever is playing with `req`. Must not report the replaced file as `Closed`.
    fn load(&mut self, req: LoadRequest) -> impl Future<Output = Result<()>> + Send;
    fn next_event(&mut self) -> impl Future<Output = Option<PlayerEvent>> + Send;
    /// Short on-screen message (e.g. "Switching source…"). Optional.
    fn notify(&mut self, _text: &str) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn stop(&mut self) -> impl Future<Output = ()> + Send;
}
