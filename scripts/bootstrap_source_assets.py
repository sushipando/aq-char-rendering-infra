#!/usr/bin/env python3
"""Upload an immutable, checksummed AQW SWF dataset to the source bucket."""

from __future__ import annotations

import argparse
import json
import random
import re
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import boto3
from aqw_char_renderer.hashing import canonical_json, file_sha256
from aqw_char_renderer.legacy.preview_aqw_tryon import has_valid_swf_header
from botocore.exceptions import ClientError

_VERSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument(
        "--bucket", required=True, help="CDK SourceAssetBucketName output"
    )
    result.add_argument("--dataset-version", required=True)
    result.add_argument("--asset-root", type=Path, required=True)
    result.add_argument("--database", type=Path, required=True)
    result.add_argument("--character-renderer", type=Path, required=True)
    result.add_argument("--workers", type=int, default=12)
    result.add_argument("--verify-samples", type=int, default=25)
    result.add_argument("--dry-run", action="store_true")
    return result


def immutable_upload(
    client: Any,
    source: Path,
    *,
    bucket: str,
    key: str,
    sha256: str,
    content_type: str,
) -> str:
    try:
        existing = client.head_object(Bucket=bucket, Key=key)
    except ClientError as error:
        if str(error.response.get("Error", {}).get("Code")) not in {
            "404",
            "NoSuchKey",
            "NotFound",
        }:
            raise
    else:
        metadata = existing.get("Metadata") or {}
        if (
            int(existing.get("ContentLength", -1)) == source.stat().st_size
            and metadata.get("sha256") == sha256
        ):
            return "existing"
        raise RuntimeError(
            f"Immutable object already exists with different content: {key}"
        )
    client.upload_file(
        str(source),
        bucket,
        key,
        ExtraArgs={
            "ContentType": content_type,
            "Metadata": {"sha256": sha256},
        },
    )
    return "uploaded"


def main() -> int:
    args = parser().parse_args()
    if _VERSION_RE.fullmatch(args.dataset_version) is None:
        raise SystemExit(
            "--dataset-version must contain only letters, digits, dot, dash, underscore"
        )
    if args.workers < 1 or args.verify_samples < 0:
        raise SystemExit("--workers must be positive and --verify-samples nonnegative")
    asset_root = args.asset_root.expanduser().resolve()
    database = args.database.expanduser().resolve()
    character_renderer = args.character_renderer.expanduser().resolve()
    if not asset_root.is_dir():
        raise SystemExit(f"Asset root does not exist: {asset_root}")
    if not database.is_file():
        raise SystemExit(f"Item database does not exist: {database}")
    if not has_valid_swf_header(character_renderer):
        raise SystemExit(f"characterB.swf is missing or invalid: {character_renderer}")

    candidates = sorted(asset_root.rglob("*.swf"))
    assets: dict[str, dict[str, Any]] = {}
    invalid: list[str] = []
    duplicate_case: list[str] = []
    folded: set[str] = set()
    total_bytes = 0
    for path in candidates:
        relative = path.relative_to(asset_root).as_posix()
        if not has_valid_swf_header(path):
            invalid.append(relative)
            continue
        if relative.casefold() in folded:
            duplicate_case.append(relative)
            continue
        folded.add(relative.casefold())
        digest = file_sha256(path)
        size = path.stat().st_size
        total_bytes += size
        assets[relative] = {
            "key": f"datasets/{args.dataset_version}/swf/{relative}",
            "sha256": digest,
            "size": size,
        }
    if invalid or duplicate_case:
        print(
            json.dumps(
                {
                    "invalid_count": len(invalid),
                    "invalid_examples": invalid[:20],
                    "duplicate_case_count": len(duplicate_case),
                    "duplicate_case_examples": duplicate_case[:20],
                },
                indent=2,
            ),
            file=sys.stderr,
        )
        raise SystemExit("Source corpus validation failed")

    database_record = {
        "key": f"datasets/{args.dataset_version}/item_db.json",
        "sha256": file_sha256(database),
        "size": database.stat().st_size,
    }
    character_record = {
        "key": f"character-renderer/{args.dataset_version}/characterB.swf",
        "sha256": file_sha256(character_renderer),
        "size": character_renderer.stat().st_size,
    }
    manifest = {
        "schema_version": 1,
        "dataset_version": args.dataset_version,
        "created_at": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "item_database": database_record,
        "character_renderer": character_record,
        "assets": assets,
    }
    manifest_key = f"datasets/{args.dataset_version}/manifest.json"
    print(
        f"Validated {len(assets)} SWFs ({total_bytes / (1024**3):.2f} GiB); "
        f"manifest={manifest_key}",
        flush=True,
    )
    if args.dry_run:
        return 0

    client = boto3.client("s3")
    counts = {"uploaded": 0, "existing": 0}

    def upload_asset(item: tuple[str, dict[str, Any]]) -> str:
        relative, record = item
        return immutable_upload(
            client,
            asset_root / relative,
            bucket=args.bucket,
            key=record["key"],
            sha256=record["sha256"],
            content_type="application/x-shockwave-flash",
        )

    with ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = {
            executor.submit(upload_asset, item): item[0] for item in assets.items()
        }
        for completed, future in enumerate(as_completed(futures), start=1):
            counts[future.result()] += 1
            if completed % 250 == 0 or completed == len(futures):
                print(
                    f"Assets: {completed}/{len(futures)} uploaded={counts['uploaded']} "
                    f"existing={counts['existing']}",
                    flush=True,
                )
    for source, record, content_type in (
        (database, database_record, "application/json"),
        (character_renderer, character_record, "application/x-shockwave-flash"),
    ):
        counts[
            immutable_upload(
                client,
                source,
                bucket=args.bucket,
                key=record["key"],
                sha256=record["sha256"],
                content_type=content_type,
            )
        ] += 1

    try:
        existing_manifest = client.get_object(Bucket=args.bucket, Key=manifest_key)[
            "Body"
        ].read()
    except ClientError as error:
        if str(error.response.get("Error", {}).get("Code")) not in {
            "404",
            "NoSuchKey",
            "NotFound",
        }:
            raise
    else:
        existing = json.loads(existing_manifest)
        # created_at is informational and should not make a rerun fail.
        existing["created_at"] = manifest["created_at"]
        if canonical_json(existing) != canonical_json(manifest):
            raise RuntimeError(
                f"Immutable manifest differs from existing {manifest_key}"
            )
        print(f"Manifest already exists: s3://{args.bucket}/{manifest_key}")
        return 0
    client.put_object(
        Bucket=args.bucket,
        Key=manifest_key,
        Body=canonical_json(manifest),
        ContentType="application/json",
    )

    sample_count = min(args.verify_samples, len(assets))
    rng = random.Random(args.dataset_version)
    for relative in rng.sample(sorted(assets), sample_count):
        record = assets[relative]
        body = client.get_object(Bucket=args.bucket, Key=record["key"])["Body"]
        temporary = Path("/tmp") / f"aqw-char-verify-{record['sha256']}.swf"
        try:
            with temporary.open("wb") as output:
                while chunk := body.read(1024 * 1024):
                    output.write(chunk)
            if file_sha256(temporary) != record["sha256"]:
                raise RuntimeError(f"Downloaded verification hash failed: {relative}")
        finally:
            temporary.unlink(missing_ok=True)
    print(
        f"Complete: uploaded={counts['uploaded']} existing={counts['existing']} "
        f"verified={sample_count} dataset={args.dataset_version}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
