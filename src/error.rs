use thiserror::Error;

/// Error types for the AWS AppSync Events client
#[derive(Error, Debug)]
pub enum Error {
    /// WebSocket connection error
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_websockets::Error),
    
    /// JSON serialization or deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    
    /// HTTP client error
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),
    
    /// URI parsing error
    #[error("URI error: {0}")]
    Uri(#[from] http::uri::InvalidUri),
    
    /// AWS SigV4 signing error
    #[error("AWS signing error: {0}")]
    AwsSigning(String),
    
    /// Connection timeout error
    #[error("Connection timeout")]
    ConnectionTimeout,
    
    /// Authentication error
    #[error("Authentication error: {0}")]
    Authentication(String),
    
    /// Subscription error from the server
    #[error("Subscription error: {0}")]
    SubscriptionError(String),
    
    /// Connection was closed unexpectedly
    #[error("Connection closed: {0}")]
    ConnectionClosed(String),
    
    /// Handshake error
    #[error("Handshake error: {0}")]
    HandshakeError(String),
    
    /// Non-retryable error
    #[error("Non-retryable error: {0}")]
    NonRetryable(String),

    /// Unauthorized error
    #[error("Unauthorized error")]
    Unauthorized,
    
    /// Generic error
    #[error("{0}")]
    Other(String),
}

/// Result type used throughout the crate
pub type Result<T> = std::result::Result<T, Error>; 