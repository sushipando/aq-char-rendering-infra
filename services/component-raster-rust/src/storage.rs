//! Object access: S3 (Lambda) and a filesystem store mirroring the Python
//! `FilesystemObjectStore` layout (`<root>/<bucket>/<key>`) for local parity.

use std::path::PathBuf;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;

use crate::error::RasterError;

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, RasterError>;
    async fn fetch(&self, key: &str) -> Result<Vec<u8>, RasterError>;
    /// `true` if the object exists. Best-effort: any error is reported as a
    /// miss, which is safe for an optional content-addressed cache.
    async fn exists(&self, key: &str) -> Result<bool, RasterError>;
}

#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    async fn put(&self, key: &str, content_type: &str, bytes: &[u8]) -> Result<(), RasterError>;
}

pub struct S3Store {
    client: S3Client,
    bucket: String,
}

impl S3Store {
    pub fn new(client: S3Client, bucket: String) -> Self {
        S3Store { client, bucket }
    }

    fn error(&self, operation: &'static str, key: &str, message: String) -> RasterError {
        RasterError::S3 {
            operation,
            key: key.to_string(),
            message,
        }
    }
}

#[async_trait::async_trait]
impl Source for S3Store {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, RasterError> {
        let bytes = self.fetch(key).await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| self.error("get", key, format!("invalid JSON: {error}")))
    }

    async fn fetch(&self, key: &str) -> Result<Vec<u8>, RasterError> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| self.error("get", key, error.to_string()))?;
        response
            .body
            .collect()
            .await
            .map_err(|error| self.error("get", key, error.to_string()))
            .map(|aggregate| aggregate.into_bytes().to_vec())
    }

    async fn exists(&self, key: &str) -> Result<bool, RasterError> {
        // Any S3 error (most commonly a 404) is treated as a cache miss;
        // recomputing the raster is always safe.
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

#[async_trait::async_trait]
impl Sink for S3Store {
    async fn put(&self, key: &str, content_type: &str, bytes: &[u8]) -> Result<(), RasterError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(bytes.to_vec()))
            .send()
            .await
            .map_err(|error| self.error("put", key, error.to_string()))?;
        Ok(())
    }
}

/// Local store over `<root>/work/<key>`.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: PathBuf) -> Self {
        FsStore { root }
    }

    fn path(&self, key: &str) -> Result<PathBuf, RasterError> {
        if key.is_empty()
            || key.starts_with('/')
            || key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(RasterError::invalid(format!("unsafe object key {key}")));
        }
        Ok(self.root.join("work").join(key))
    }
}

#[async_trait::async_trait]
impl Source for FsStore {
    async fn read_json(&self, key: &str) -> Result<serde_json::Value, RasterError> {
        let path = self.path(key)?;
        let bytes = tokio::fs::read(&path).await.map_err(|error| {
            RasterError::invalid(format!("cannot read {}: {error}", path.display()))
        })?;
        serde_json::from_slice(&bytes).map_err(|error| {
            RasterError::invalid(format!("invalid JSON {}: {error}", path.display()))
        })
    }

    async fn fetch(&self, key: &str) -> Result<Vec<u8>, RasterError> {
        let path = self.path(key)?;
        tokio::fs::read(&path).await.map_err(|error| {
            RasterError::invalid(format!("cannot read {}: {error}", path.display()))
        })
    }

    async fn exists(&self, key: &str) -> Result<bool, RasterError> {
        let path = self.path(key)?;
        Ok(tokio::fs::try_exists(&path).await.unwrap_or(false))
    }
}

#[async_trait::async_trait]
impl Sink for FsStore {
    async fn put(&self, key: &str, _content_type: &str, bytes: &[u8]) -> Result<(), RasterError> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }
}

/// Convert a manifest's placement_colors map into (parent, child) keys.
pub fn parse_placement_key_string(
    map: &std::collections::HashMap<String, crate::contract::ColorTransformValues>,
) -> std::collections::HashMap<(i64, i64), crate::contract::ColorTransformValues> {
    map.iter()
        .filter_map(|(pair, value)| {
            let mut parts = pair.split(',');
            let parent = parts.next()?.trim().parse().ok()?;
            let child = parts.next()?.trim().parse().ok()?;
            Some(((parent, child), value.clone()))
        })
        .collect()
}
