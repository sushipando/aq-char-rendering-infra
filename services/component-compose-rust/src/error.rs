//! Shared error type for the component-compose worker.
//!
//! Every failure is an explicit chunk failure: a missing component must fail
//! the whole chunk rather than silently render an incomplete character.

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("{0}")]
    Invalid(String),
    #[error("component PNG for task {task_id} was not decoded")]
    MissingPng { task_id: String },
    #[error("s3 {operation} failed for {key}: {message}")]
    S3 {
        operation: &'static str,
        key: String,
        message: String,
    },
    #[error("png decode failed: {0}")]
    Png(String),
    #[error("cwebp encode failed: {0}")]
    Encode(String),
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("serialization error: {0}")]
    Json(String),
}

impl ComposeError {
    pub fn invalid(message: impl Into<String>) -> Self {
        ComposeError::Invalid(message.into())
    }

    pub fn missing_png(task_id: impl Into<String>) -> Self {
        ComposeError::MissingPng {
            task_id: task_id.into(),
        }
    }
}

impl From<std::io::Error> for ComposeError {
    fn from(error: std::io::Error) -> Self {
        ComposeError::Io(error)
    }
}

impl From<serde_json::Error> for ComposeError {
    fn from(error: serde_json::Error) -> Self {
        ComposeError::Json(error.to_string())
    }
}

/// Convert a private worker error into the Lambda runtime's boxed error so
/// the invocation fails and Step Functions retries/terminal-handles it.
pub type LambdaError = Box<dyn std::error::Error + Send + Sync>;
