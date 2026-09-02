//! Object access abstractions plus the S3 implementation used by Lambda.

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;

use crate::error::ComposeError;
use crate::telemetry::sha256_hex;

/// Reads the prepare manifest and component rasters.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, ComposeError>;
    async fn fetch_bytes(
        &self,
        key: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>, ComposeError>;
}

/// Writes encoded frames and the compose-batch manifest.
#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    async fn put_webp(
        &self,
        frame_number: i64,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), ComposeError>;
    async fn put_json(&self, key: &str, value: &serde_json::Value) -> Result<(), ComposeError>;
}

pub struct S3Store {
    client: S3Client,
    bucket: String,
}

impl S3Store {
    pub fn new(client: S3Client, bucket: String) -> Self {
        S3Store { client, bucket }
    }

    fn error(&self, operation: &'static str, key: &str, message: String) -> ComposeError {
        ComposeError::S3 {
            operation,
            key: key.to_string(),
            message,
        }
    }
}

#[async_trait::async_trait]
impl Source for S3Store {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, ComposeError> {
        let bytes = self
            .fetch_bytes(key, None)
            .await
            .map_err(|error| match error {
                ComposeError::S3 { .. } => error,
                other => self.error("get", key, other.to_string()),
            })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| self.error("get", key, format!("invalid JSON: {error}")))
    }

    async fn fetch_bytes(
        &self,
        key: &str,
        expected_sha256: Option<&str>,
    ) -> Result<Vec<u8>, ComposeError> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| self.error("get", key, error.to_string()))?;
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|error| self.error("get", key, error.to_string()))?
            .into_bytes()
            .to_vec();
        if let Some(expected) = expected_sha256 {
            let actual = sha256_hex(&bytes);
            if actual != expected {
                return Err(self.error(
                    "get",
                    key,
                    format!("checksum mismatch: expected {expected}, got {actual}"),
                ));
            }
        }
        Ok(bytes)
    }
}

#[async_trait::async_trait]
impl Sink for S3Store {
    async fn put_webp(
        &self,
        _frame_number: i64,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), ComposeError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("image/webp")
            .body(ByteStream::from(bytes.to_vec()))
            .send()
            .await
            .map_err(|error| self.error("put", key, error.to_string()))?;
        Ok(())
    }

    async fn put_json(&self, key: &str, value: &serde_json::Value) -> Result<(), ComposeError> {
        let bytes =
            serde_json::to_vec(value).map_err(|error| self.error("put", key, error.to_string()))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/json")
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|error| self.error("put", key, error.to_string()))?;
        Ok(())
    }
}
