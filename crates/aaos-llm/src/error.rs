use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("API returned error: {status} — {message}")]
    ApiError { status: u16, message: String },

    #[error("failed to parse API response: {0}")]
    ParseError(String),

    #[error("authentication failed — check API key")]
    AuthError,

    #[error("rate limited — retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    #[error("model not supported: {model}")]
    UnsupportedModel { model: String },

    #[error("{0}")]
    Other(String),
}

pub type LlmResult<T> = std::result::Result<T, LlmError>;
