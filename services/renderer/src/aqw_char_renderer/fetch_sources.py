"""First workflow step: acquire all AQW inputs in AWS and persist them to S3."""
from __future__ import annotations

from dataclasses import replace
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextvars import copy_context
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
from urllib.error import HTTPError
from uuid import uuid4

import boto3

from aqw_char_renderer.contracts import JobRequest
from aqw_char_renderer.character_svg import appearance_assets
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon
from aqw_char_renderer.source_assets import SourceAssetCatalog, SourceAssetError
from aqw_char_renderer.source_http import SourceConnectionError, source_session, source_worker_session
from aqw_char_renderer.storage import S3ObjectStore


def _background(fields, render):
    enabled = (render.presentation or {}).get("background", render.view == "charpage")
    value = str(fields.get("bgindex", ""))
    index = int(value, 36) if value and value.isascii() and value.isalnum() else 0
    if not enabled or index == 0:
        return None
    records = json.loads(Path(os.environ.get("AQW_BACKGROUND_CATALOG",
        str(Path(__file__).with_name("background_sources.json")))).read_text())
    return next((record for record in records if record["index"] == index), None)


def _planned_paths(fields, request, catalog, store, bucket, root):
    fields = dict(fields)
    render = request.render
    if render.show_hidden:
        fields["ia1"] = str(int(fields.get("ia1", "0") or 0) & ~7)
    if render.override:
        record = catalog.item_database
        path = store.download(bucket, record.key, root / "items.json", expected_sha256=record.sha256)
        item = next((v for v in json.loads(path.read_text()) if v["id"] == render.override.item_id), None)
        if item is None:
            raise SourceAssetError("Override item is absent from dataset")
        slot = str(item["slot"]).lower()
        if slot in {"gauntlet", "handgun", "rifle", "whip"}:
            slot = "weapon"
        if slot not in {"armor", "weapon", "helm", "cape", "ground"} or (
                render.override.slot is not None and render.override.slot != slot):
            raise SourceAssetError("Invalid override item slot")
        remote = item["file"]
        if slot == "armor" and "/" not in remote:
            remote = f"classes/{fields.get('strGender', 'M').upper()}/{remote}"
        title = {"armor": "Armor", "ground": "Misc"}.get(slot, slot.title())
        prefix = ("strMisc" if slot == "ground" else f"strCust{title}" if not render.base_items
                  else "strClass" if slot == "armor" else f"str{title}")
        fields[prefix + "File"] = tryon.normalize_asset_path(remote)
        fields[prefix + "Name"] = item.get("name") or "Override"
        if slot in {"helm", "cape"}:
            fields["ia1"] = str(int(fields.get("ia1", "0") or 0) & ~(2 if slot == "helm" else 1))
    assets = appearance_assets(fields, use_cosmetics=not render.base_items)
    if "armor" not in assets:
        raise SourceAssetError("Character has no visible armor SWF")
    paths = {v.remote_path: None for v in assets.values()}
    if render.override:
        # Rust reads the override SWF first to infer its exported class name.
        paths[tryon.normalize_asset_path(remote)] = None
    background = _background(fields, render)
    if background:
        paths[f"etc/chardetail/bgs/{background['file']}"] = background["sha256"]
    return paths


def _seed(remote, expected_hash, catalog, store, bucket, root, timeout):
    try:
        record = catalog.get(remote)
    except SourceAssetError:
        digest = hashlib.sha256(remote.casefold().encode()).hexdigest()
        key = f"dynamic-assets/{catalog.dataset_version}/{digest[:2]}/{digest}.swf"
        head = store.exists(bucket, key)
        if head:
            metadata = head.get("Metadata", {})
            checksum = metadata.get("sha256", "")
            if (len(checksum) != 64 or int(head.get("ContentLength", 0)) < 8
                    or metadata.get("path-sha256", digest) != digest):
                raise SourceAssetError("Invalid cached source metadata")
            if expected_hash and expected_hash != checksum:
                raise SourceAssetError("Background source checksum differs from pinned catalog")
            return key, True
        if os.environ.get("CHAR_RENDER_ALLOW_OFFICIAL_ASSET_FALLBACK", "true").lower() not in {"true", "1", "yes"}:
            raise SourceAssetError("Official source downloads are disabled")
        def download(url, destination):
            tryon.download_swf(url, destination, timeout=timeout)
            if expected_hash and hashlib.sha256(destination.read_bytes()).hexdigest() != expected_hash:
                raise SourceAssetError("Background source checksum differs from pinned catalog")
        _, record = catalog.resolve_and_download(remote, store=store, bucket=bucket, root=root,
                                                 allow_official_fallback=True, timeout=timeout, downloader=download)
        if expected_hash and record.sha256 != expected_hash:
            raise SourceAssetError("Background source checksum differs from pinned catalog")
        return record.key, False
    head = store.exists(bucket, record.key)
    if not head or int(head.get("ContentLength", 0)) != record.size:
        raise SourceAssetError("Manifest source is absent or has an invalid size")
    if expected_hash and record.sha256 != expected_hash:
        raise SourceAssetError("Background source checksum differs from pinned catalog")
    return record.key, True


def _saved_appearance(store, bucket, job):
    for suffix in ["fetch/request.json", "fetch/appearance.json", "prepare/input.json", "prepare/manifest.json"]:
        key = f"jobs/{job}/{suffix}"
        if store.exists(bucket, key):
            saved = store.read_json(bucket, key)
            if saved.get("job_id") != job:
                raise SourceAssetError("Saved source belongs to another job")
            fields = saved.get("appearance") or saved.get("fields")
            if fields:
                return fields
    return None


def fetch_request(payload, *, store, source_bucket, work_bucket, dataset, proxy_config,
                  appearance_fetcher=tryon.fetch_character_flashvars, previous_request=None, deadline=None):
    request = JobRequest.from_dict(payload)
    key = f"jobs/{request.job_id}/fetch/request.json"
    # Internal task retries reuse a completed acquisition without a new origin request.
    if store.exists(work_bucket, key):
        saved = JobRequest.from_dict(store.read_json(work_bucket, key))
        if (saved.job_id != request.job_id or saved.render != request.render
                or saved.source_job_id != request.source_job_id or saved.appearance_overrides != request.appearance_overrides):
            raise SourceAssetError("Saved fetch inputs do not match this job")
        return saved.to_dict()
    catalog = SourceAssetCatalog(store.read_json(source_bucket, f"datasets/{dataset}/manifest.json"))
    if catalog.dataset_version != dataset:
        raise SourceAssetError("Source dataset mismatch")
    timeout = int(os.environ.get("CHAR_RENDER_OFFICIAL_ASSET_TIMEOUT_SECONDS", "30"))
    if not 1 <= timeout <= 60:
        raise ValueError("Invalid source timeout")
    attempts = int(os.environ.get("CHAR_RENDER_SOURCE_FETCH_ATTEMPTS", "3"))
    workers = int(os.environ.get("CHAR_RENDER_SOURCE_FETCH_WORKERS", "6"))
    if not 1 <= attempts <= 3 or not 1 <= workers <= 8:
        raise ValueError("Invalid source fetch attempts/concurrency")
    deadline = min(deadline or float("inf"), time.monotonic() + 240)
    snapshot = None
    if request.source_job_id:
        snapshot = _saved_appearance(store, work_bucket, request.source_job_id)
        if snapshot is None and previous_request is not None:
            snapshot = (previous_request(request.source_job_id) or {}).get("appearance")
    snapshot = snapshot or request.appearance

    for attempt in range(1, attempts + 1):
        try:
            # A new identifier requests a fresh Bright Data sticky session, even
            # when Lambda is invoked again for the same job.
            with source_session(str(uuid4()), proxy_config=proxy_config, deadline=deadline), tempfile.TemporaryDirectory(prefix="aqw-fetch-") as tmp:
                # Historical snapshots retain retry-render semantics. Refresh the
                # page for cookies/session bootstrap on recovery, without changing
                # the outfit that a user explicitly asked to retry.
                fresh = appearance_fetcher(request.render.username, timeout=timeout) if not snapshot or attempt > 1 else None
                fields = {**(snapshot or fresh), **(request.appearance_overrides or {})}
                enriched = JobRequest.from_dict(replace(request, appearance=fields).to_dict())
                store.write_json(work_bucket, f"jobs/{request.job_id}/fetch/appearance.json",
                                 {"job_id": request.job_id, "fields": fields})
                paths = _planned_paths(fields, enriched, catalog, store, source_bucket, Path(tmp))
                records, failures = [], []

                def seed(remote, checksum):
                    # Each task has its own curl session and filesystem location;
                    # the proxy identity and initial page cookies are shared.
                    root = Path(tmp) / hashlib.sha256(remote.encode()).hexdigest()
                    with source_worker_session():
                        return _seed(remote, checksum, catalog, store, source_bucket, root, timeout)

                with ThreadPoolExecutor(max_workers=workers) as pool:
                    futures = {pool.submit(copy_context().run, seed, remote, checksum): remote
                               for remote, checksum in paths.items()}
                    for future in as_completed(futures):
                        remote = futures[future]
                        try:
                            source_key, hit = future.result()
                            records.append({"path": remote, "key": source_key, "cache_hit": hit})
                        except Exception as error:
                            failures.append(error)
                            print(json.dumps({"event": "source_asset_fetch_failed", "job_id": request.job_id,
                                              "attempt": attempt, "path": remote,
                                              "error_type": type(error).__name__,
                                              "http_status": error.code if isinstance(error, HTTPError) else None}))
                    # Do not cancel peers on failure: their successful uploads
                    # must finish before another session retries missing objects.
                if failures:
                    raise next((e for e in failures if not _retryable(e)), failures[0])
                records.sort(key=lambda record: record["path"])
                store.write_json(work_bucket, f"jobs/{request.job_id}/fetch/sources.json", {"job_id": request.job_id, "sources": records})
                store.write_json(work_bucket, key, enriched.to_dict())
                return enriched.to_dict()
        except Exception as error:
            delay = 2 ** (attempt - 1)
            if not _retryable(error) or attempt == attempts or time.monotonic() + delay >= deadline:
                raise
            # Error messages may contain credentials from third-party clients;
            # log only controlled fields, never proxy configuration or raw errors.
            print(json.dumps({"event": "source_fetch_retry", "job_id": request.job_id,
                              "attempt": attempt, "next_attempt": attempt + 1,
                              "error_type": type(error).__name__,
                              "http_status": error.code if isinstance(error, HTTPError) else None}))
            time.sleep(delay)


def _retryable(error):
    return isinstance(error, SourceConnectionError) or (
        isinstance(error, HTTPError) and (error.code in {403, 408, 429} or 500 <= error.code <= 599))


def handler(event, context):
    parameter = os.environ["AQW_BRIGHTDATA_CONFIG_PARAMETER"]
    response = boto3.client("ssm").get_parameter(Name=parameter, WithDecryption=True)["Parameter"]
    if response.get("Type") != "SecureString":
        raise SourceAssetError("Bright Data configuration must be SecureString")
    def previous_request(job):
        table = os.environ.get("CHAR_RENDER_JOB_TABLE")
        if not table:
            return None
        from boto3.dynamodb.types import TypeDeserializer
        item = boto3.client("dynamodb").get_item(TableName=table,
            Key={"PK": {"S": f"JOB#{job}"}, "SK": {"S": "META"}}, ConsistentRead=True).get("Item")
        return {key: TypeDeserializer().deserialize(value) for key, value in (item or {}).items()}.get("request")
    payload = event.get("request", event)
    return fetch_request(payload, store=S3ObjectStore(), source_bucket=os.environ["CHAR_RENDER_SOURCE_BUCKET"],
                         work_bucket=os.environ["CHAR_RENDER_WORK_BUCKET"],
                         dataset=os.environ["CHAR_RENDER_ASSET_DATASET_VERSION"], proxy_config=response["Value"], previous_request=previous_request,
                         deadline=time.monotonic() + max(1, context.get_remaining_time_in_millis() / 1000 - 10))
