use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Stremio API error: {0}")]
    Api(String),
    #[error("invalid addon URL: {0}")]
    BadUrl(String),
    #[error("player error: {0}")]
    Player(String),
    #[error("a maximum of {0} profiles is allowed")]
    ProfileLimit(usize),
    #[error("profile not found: {0}")]
    NoSuchProfile(String),
    #[error("child profile age must be between 3 and 17, got {0}")]
    InvalidAge(u8),
    #[error("PIN must not be empty")]
    EmptyPin,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
