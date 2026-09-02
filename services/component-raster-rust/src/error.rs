//! Shared error type for the component-raster worker.

#[derive(Debug, thiserror::Error)]
pub enum RasterError {
    #[error("{0}")]
    Invalid(String),
    #[error("s3 {operation} failed for {key}: {message}")]
    S3 {
        operation: &'static str,
        key: String,
        message: String,
    },
    #[error("bundle {key} missing member {member}: {message}")]
    Bundle {
        key: String,
        member: String,
        message: String,
    },
    #[error("svg import failed: {0}")]
    Svg(String),
    #[error("rasterization failed: {0}")]
    Raster(String),
    #[error("png encode failed: {0}")]
    Png(String),
    #[error("io error: {0}")]
    Io(std::io::Error),
    #[error("serialization error: {0}")]
    Json(String),
}

impl RasterError {
    pub fn invalid(message: impl Into<String>) -> Self {
        RasterError::Invalid(message.into())
    }
}

impl From<std::io::Error> for RasterError {
    fn from(error: std::io::Error) -> Self {
        RasterError::Io(error)
    }
}

impl From<serde_json::Error> for RasterError {
    fn from(error: serde_json::Error) -> Self {
        RasterError::Json(error.to_string())
    }
}

/// Boxed error for the Lambda runtime.
pub type LambdaError = Box<dyn std::error::Error + Send + Sync>;
