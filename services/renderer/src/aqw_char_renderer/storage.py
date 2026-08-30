"""S3 object helpers with checksum verification and atomic local downloads."""

from __future__ import annotations

import json
import re
import shutil
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import boto3
from botocore.config import Config as BotoConfig
from botocore.exceptions import ClientError

from aqw_char_renderer.hashing import file_sha256

_SAFE_KEY_RE = re.compile(r"^[A-Za-z0-9!_.*'()/-]+$")


class StorageError(RuntimeError):
    pass


def validate_key(key: str) -> str:
    if (
        not key
        or key.startswith("/")
        or "//" in key
        or any(part in {"", ".", ".."} for part in key.split("/"))
        or _SAFE_KEY_RE.fullmatch(key) is None
    ):
        raise StorageError(f"Unsafe S3 object key: {key!r}")
    return key


class S3ObjectStore:
    def __init__(self, client: Any | None = None, *, max_pool_connections: int = 10) -> None:
        if max_pool_connections < 1:
            raise ValueError("max_pool_connections must be positive")
        self.client = client or boto3.client(
            "s3",
            config=BotoConfig(max_pool_connections=max_pool_connections),
        )

    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None:
        validate_key(key)
        try:
            return self.client.head_object(Bucket=bucket, Key=key)
        except ClientError as error:
            code = str(error.response.get("Error", {}).get("Code", ""))
            if code in {"404", "NoSuchKey", "NotFound"}:
                return None
            raise

    def download(
        self,
        bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path:
        validate_key(key)
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.is_file() and (
            expected_sha256 is None or file_sha256(destination) == expected_sha256
        ):
            return destination
        temporary = destination.with_name(f".{destination.name}.partial")
        try:
            self.client.download_file(bucket, key, str(temporary))
            if expected_sha256 is not None:
                actual = file_sha256(temporary)
                if actual != expected_sha256:
                    raise StorageError(
                        f"Checksum mismatch for s3://{bucket}/{key}: "
                        f"expected {expected_sha256}, got {actual}"
                    )
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)
        return destination

    def upload_file(
        self,
        source: Path,
        bucket: str,
        key: str,
        *,
        content_type: str | None = None,
        cache_control: str | None = None,
        metadata: Mapping[str, str] | None = None,
    ) -> None:
        validate_key(key)
        extra: dict[str, Any] = {}
        if content_type:
            extra["ContentType"] = content_type
        if cache_control:
            extra["CacheControl"] = cache_control
        if metadata:
            extra["Metadata"] = dict(metadata)
        self.client.upload_file(str(source), bucket, key, ExtraArgs=extra or None)

    def upload_file_if_absent(
        self,
        source: Path,
        bucket: str,
        key: str,
        *,
        content_type: str | None = None,
        metadata: Mapping[str, str] | None = None,
    ) -> bool:
        """Atomically create a small immutable object; return false if it exists."""
        validate_key(key)
        arguments: dict[str, Any] = {
            "Bucket": bucket,
            "Key": key,
            "IfNoneMatch": "*",
        }
        if content_type:
            arguments["ContentType"] = content_type
        if metadata:
            arguments["Metadata"] = dict(metadata)
        try:
            with source.open("rb") as body:
                self.client.put_object(Body=body, **arguments)
        except ClientError as error:
            code = str(error.response.get("Error", {}).get("Code", ""))
            status = int(error.response.get("ResponseMetadata", {}).get("HTTPStatusCode", 0))
            if code in {"PreconditionFailed", "ConditionalRequestConflict"} or status in {
                409,
                412,
            }:
                return False
            raise
        return True

    def copy(
        self,
        bucket: str,
        source_key: str,
        destination_key: str,
        *,
        content_type: str,
        cache_control: str,
        metadata: Mapping[str, str],
    ) -> None:
        validate_key(source_key)
        validate_key(destination_key)
        self.client.copy_object(
            Bucket=bucket,
            Key=destination_key,
            CopySource={"Bucket": bucket, "Key": source_key},
            ContentType=content_type,
            CacheControl=cache_control,
            ContentDisposition="inline",
            Metadata=dict(metadata),
            MetadataDirective="REPLACE",
        )

    def delete(self, bucket: str, key: str) -> None:
        validate_key(key)
        self.client.delete_object(Bucket=bucket, Key=key)

    def read_json(self, bucket: str, key: str) -> Any:
        validate_key(key)
        response = self.client.get_object(Bucket=bucket, Key=key)
        try:
            return json.loads(response["Body"].read())
        except (KeyError, json.JSONDecodeError, UnicodeDecodeError) as error:
            raise StorageError(f"Invalid JSON in s3://{bucket}/{key}") from error

    def write_json(self, bucket: str, key: str, value: Any) -> None:
        validate_key(key)
        payload = json.dumps(
            value, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        self.client.put_object(
            Bucket=bucket,
            Key=key,
            Body=payload,
            ContentType="application/json",
        )


class FilesystemObjectStore:
    """Test/local implementation with the same bucket/key interface."""

    def __init__(self, root: Path) -> None:
        self.root = root.resolve()

    def _path(self, bucket: str, key: str) -> Path:
        validate_key(key)
        if not bucket or "/" in bucket or bucket in {".", ".."}:
            raise StorageError(f"Unsafe local bucket name: {bucket!r}")
        path = (self.root / bucket / key).resolve()
        if not path.is_relative_to(self.root):
            raise StorageError(f"Object path escapes local store: {key!r}")
        return path

    def exists(self, bucket: str, key: str) -> Mapping[str, Any] | None:
        path = self._path(bucket, key)
        return {"ContentLength": path.stat().st_size} if path.is_file() else None

    def download(
        self,
        bucket: str,
        key: str,
        destination: Path,
        *,
        expected_sha256: str | None = None,
    ) -> Path:
        source = self._path(bucket, key)
        if not source.is_file():
            raise StorageError(f"Missing local object {bucket}/{key}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(source.read_bytes())
        if expected_sha256 is not None and file_sha256(destination) != expected_sha256:
            destination.unlink(missing_ok=True)
            raise StorageError(f"Checksum mismatch for {bucket}/{key}")
        return destination

    def upload_file(
        self,
        source: Path,
        bucket: str,
        key: str,
        **_kwargs: Any,
    ) -> None:
        destination = self._path(bucket, key)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(source.read_bytes())

    def upload_file_if_absent(
        self,
        source: Path,
        bucket: str,
        key: str,
        **_kwargs: Any,
    ) -> bool:
        destination = self._path(bucket, key)
        destination.parent.mkdir(parents=True, exist_ok=True)
        try:
            with source.open("rb") as input_file, destination.open("xb") as output_file:
                shutil.copyfileobj(input_file, output_file)
        except FileExistsError:
            return False
        return True

    def copy(self, bucket: str, source_key: str, destination_key: str, **_kwargs: Any) -> None:
        destination = self._path(bucket, destination_key)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(self._path(bucket, source_key).read_bytes())

    def delete(self, bucket: str, key: str) -> None:
        self._path(bucket, key).unlink(missing_ok=True)

    def read_json(self, bucket: str, key: str) -> Any:
        return json.loads(self._path(bucket, key).read_text(encoding="utf-8"))

    def write_json(self, bucket: str, key: str, value: Any) -> None:
        destination = self._path(bucket, key)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(
            json.dumps(value, ensure_ascii=False, allow_nan=False, sort_keys=True) + "\n",
            encoding="utf-8",
        )
