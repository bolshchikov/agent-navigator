use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("too many redirects (cap {cap}) fetching {url}")]
    TooManyRedirects { url: String, cap: usize },

    #[error("redirect from {from} missing Location header")]
    MissingRedirectLocation { from: String },

    #[error("refusing to fetch {url}: {reason}")]
    ForbiddenUrl { url: String, reason: String },

    #[error("robots.txt disallows {url}")]
    RobotsDisallowed { url: String },

    #[error("response body from {url} is {size} bytes; limit is {limit}")]
    BodyTooLarge { url: String, size: u64, limit: u64 },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("session error: {0}")]
    Session(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ForbiddenUrl { .. } => "forbidden_url",
            Self::RobotsDisallowed { .. } => "robots_disallowed",
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::Session(_) => "session_error",
            Self::TooManyRedirects { .. } => "too_many_redirects",
            Self::InvalidUrl(_) => "invalid_url",
            _ => "request_failed",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
