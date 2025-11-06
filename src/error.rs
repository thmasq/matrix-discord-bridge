use thiserror::Error;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("HTTP error: {0}")]
    Http(#[from] hyper::Error),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] hyper_util::client::legacy::Error),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Discord error: {0}")]
    Discord(#[from] serenity::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Matrix error: {0}")]
    Matrix(String),

    #[error("Not found")]
    NotFound,

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, BridgeError>;
