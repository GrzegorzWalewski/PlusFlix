//! PlusFlix core: Stremio account + addons, source selection and failover playback.
//! No UI code lives here, so the whole playback logic is testable headlessly.

pub mod addon;
pub mod error;
pub mod genres;
pub mod home;
pub mod i18n;
pub mod lang;
pub mod loader;
pub mod model;
pub mod mpv;
pub mod parse;
pub mod player;
pub mod pool;
pub mod rating;
pub mod select;
pub mod session;
pub mod settings;
pub mod store;
pub mod stremio;
#[cfg(test)]
pub(crate) mod testhttp;
pub mod vlc;

pub use error::{Error, Result};
pub use i18n::Translations;
