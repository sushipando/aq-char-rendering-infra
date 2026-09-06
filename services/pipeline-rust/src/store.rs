use anyhow::{ensure, Context, Result};
use aws_sdk_s3::{error::ProvideErrorMetadata, primitives::ByteStream};
use serde::{de::DeserializeOwned, Serialize};
use std::path::PathBuf;

pub fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && !key.starts_with('/')
            && key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!_.*'()/-".contains(&c))
            && key.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "unsafe object key"
    );
    Ok(())
}

#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>>;
    async fn put(
        &self,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
        immutable: bool,
    ) -> Result<()>;
    async fn exists(&self, bucket: &str, key: &str) -> Result<bool> {
        Ok(self.get(bucket, key).await?.is_some())
    }
}

pub async fn read<T: DeserializeOwned>(store: &dyn Store, bucket: &str, key: &str) -> Result<T> {
    serde_json::from_slice(
        &store
            .get(bucket, key)
            .await?
            .with_context(|| format!("missing {bucket}/{key}"))?,
    )
    .context("invalid stored JSON")
}

pub async fn cached<T: DeserializeOwned>(
    store: &dyn Store,
    bucket: &str,
    key: &str,
) -> Result<Option<T>> {
    store
        .get(bucket, key)
        .await?
        .map(|bytes| serde_json::from_slice(&bytes).context("invalid cached JSON"))
        .transpose()
}

pub async fn write<T: Serialize + Sync>(
    store: &dyn Store,
    bucket: &str,
    key: &str,
    value: &T,
    immutable: bool,
) -> Result<()> {
    store
        .put(
            bucket,
            key,
            serde_json::to_vec(value)?,
            "application/json",
            immutable,
        )
        .await
}

pub struct S3Store(pub aws_sdk_s3::Client);

#[async_trait::async_trait]
impl Store for S3Store {
    async fn exists(&self, bucket: &str, key: &str) -> Result<bool> {
        validate_key(key)?;
        match self.0.head_object().bucket(bucket).key(key).send().await {
            Ok(_) => Ok(true),
            Err(error)
                if error.as_service_error().is_some_and(|e| {
                    matches!(e.code(), Some("NotFound" | "NoSuchKey" | "404"))
                }) =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
    async fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        validate_key(key)?;
        match self.0.get_object().bucket(bucket).key(key).send().await {
            Ok(response) => Ok(Some(response.body.collect().await?.into_bytes().to_vec())),
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|e| e.code() == Some("NoSuchKey")) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn put(
        &self,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
        immutable: bool,
    ) -> Result<()> {
        validate_key(key)?;
        let checksum = crate::sha256(&bytes);
        let mut call = self
            .0
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type(content_type)
            .metadata("sha256", checksum)
            .body(ByteStream::from(bytes));
        if matches!(content_type, "image/webp" | "image/avif") && key.starts_with("renders/") {
            call = call
                .cache_control("public, max-age=86400")
                .content_disposition("inline");
        }
        if immutable {
            call = call.if_none_match("*");
        }
        match call.send().await {
            Ok(_) => Ok(()),
            // 409 is a concurrent conflict, not proof of an existing object.
            // Let SDK/workflow retries resolve it; only 412 is a cache winner.
            Err(error)
                if immutable
                    && error
                        .as_service_error()
                        .is_some_and(|e| e.code() == Some("PreconditionFailed")) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

pub struct FsStore(pub PathBuf);

impl FsStore {
    fn path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        ensure!(
            !bucket.is_empty() && !bucket.contains('/') && bucket != "." && bucket != "..",
            "unsafe bucket"
        );
        Ok(self.0.join(bucket).join(key))
    }
}

#[async_trait::async_trait]
impl Store for FsStore {
    async fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.path(bucket, key)?).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn put(
        &self,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
        _: &str,
        immutable: bool,
    ) -> Result<()> {
        use std::io::Write;
        let path = self.path(bucket, key)?;
        let parent = path.parent().context("object has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        if immutable {
            match temporary.persist_noclobber(path) {
                Ok(_) => (),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.into()),
            }
        } else {
            temporary.persist(path)?;
        }
        Ok(())
    }
}
