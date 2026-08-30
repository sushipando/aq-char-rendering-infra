"""Offline AQW SWF animation analyzer.

Runs FFDec once per immutable SWF in the source corpus, derives each
exported symbol timeline's state schedule, period, and blink classification,
and uploads a content-addressed animation manifest. At render time
prepare_resolve looks up these manifests so the output frame count, blink
alignment, and batch partition are known before any vector-state export.

Output layout (per immutable SWF, one object):

  animation-metadata/<schema>/<ffdec-version>/<swf-sha256>.json

  {
    "schema_version": 2,
    "swf_sha256": "...",
    "ffdec_version": "26.2.1",
    "scan_frames": 2008,
    "symbols": {
      "<class-or-key>": {
        "root_frame": N,
        "frame_signatures": ["<sha256>", ...],
        "unique_states": K,
        "period": P | null,
        "mirror_flip_frame": F | null,
        "random_pose_as3": true|false,
        "settled_stop_frame": S | null,
        "states": ["<sha256-of-unique-state>", ...]
      }
    }
  }
"""

from __future__ import annotations

import argparse
import tempfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

import boto3
from aqw_char_renderer import character_svg
from aqw_char_renderer.hashing import canonical_json, file_sha256
from aqw_char_renderer.legacy import preview_aqw_tryon as tryon

FFDEC_VERSION = "26.2.1"
ANIMATION_METADATA_SCHEMA = 2
MAX_OUTPUT_FRAMES = 2000
SCAN_FRAMES = MAX_OUTPUT_FRAMES + character_svg.LOOP_VALIDATION_FRAMES


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--bucket", required=True, help="SourceAssetBucketName output")
    result.add_argument("--dataset-version", required=True)
    result.add_argument("--asset-root", type=Path, required=True)
    result.add_argument("--ffdec", type=Path, default=Path("/opt/ffdec/ffdec-cli.jar"))
    result.add_argument("--workers", type=int, default=4)
    result.add_argument("--dry-run", action="store_true")
    return result


def symbol_class_entries(source: Path) -> list[tuple[int, str]]:
    try:
        return tryon.symbol_class_entries(source)
    except Exception:  # noqa: BLE001 - analyzer is best-effort per asset
        return []


def analyze_one(swf: Path, ffdec: Path, scan_frames: int) -> dict[str, Any] | None:
    """Export every SymbolClass-tagged sprite timeline once and derive metadata.

    Some SymbolClass entries (e.g. MainTimeline wrappers) are not FFDec-
    exportable sprites; those are probed with a tiny frame count first and
    skipped, so one bad class never fails the whole SWF's manifest. Legacy
    assets with no SymbolClass table fall back to the unnamed compiled root
    (highest DefineSprite id), matching symbol_id(source, "").
    """
    entries = symbol_class_entries(swf)
    requests: list[character_svg.SymbolRequest] = []
    if not entries:
        # Legacy unnamed-sprite asset: use the compiled root like the renderer.
        from aqw_char_renderer.legacy import render_swf_items as item_renderer

        try:
            metadata = item_renderer.parse_swf_sprite_metadata(
                swf.read_bytes(), swf.stem
            )
        except OSError:
            metadata = {}
        root_id = metadata.get("root")
        if isinstance(root_id, int) and root_id > 0:
            requests.append(
                character_svg.SymbolRequest(
                    key="root",
                    source=swf,
                    class_name=swf.stem,
                    character_id=root_id,
                    frame=1,
                    root_timeline_frames=character_svg.unlabeled_root_timeline_frame_count(
                        swf, swf.stem
                    ),
                )
            )
    else:
        for character_id, class_name in entries:
            frame = _root_frame(swf, class_name, character_id)
            requests.append(
                character_svg.SymbolRequest(
                    key=f"symbol_{character_id}",
                    source=swf,
                    class_name=class_name or f"symbol_{character_id}",
                    character_id=character_id,
                    frame=frame,
                    root_timeline_frames=character_svg.unlabeled_root_timeline_frame_count(
                        swf, class_name
                    ),
                )
            )
    if not requests:
        return None
    with tempfile.TemporaryDirectory(prefix="aqw-analyze-") as temporary:
        root = Path(temporary)
        # Probe each class with a small export to find exportable symbols.
        exportable: list[character_svg.SymbolRequest] = []
        for request in requests:
            try:
                character_svg.export_requested_symbol_frames(
                    [request],
                    ffdec=ffdec,
                    zoom=1.0,
                    destination=root / f"probe-{request.character_id}",
                    frame_count=8,
                )
            except Exception:  # noqa: BLE001, S112 - class is not sprite-exportable
                continue
            exportable.append(request)
        if not exportable:
            return None
        try:
            exported = character_svg.export_requested_symbol_frames(
                exportable,
                ffdec=ffdec,
                zoom=1.0,
                destination=root / "exports",
                frame_count=scan_frames,
            )
        except Exception:  # noqa: BLE001 - analyzer is best-effort per asset
            return None
        terminal_stops = character_svg.parse_terminal_stop_frames(
            swf,
            ffdec=ffdec,
            destination=root / "scripts",
        )
        settled_timelines: dict[str, character_svg.StoppedChildTimeline] = {}
        for request in exportable:
            paths = exported.get(request.key) or []
            if not paths:
                continue
            settled = character_svg.stopped_direct_child_timeline(
                request, paths[0], terminal_stops
            )
            if settled is not None:
                settled_timelines[request.key] = settled
        if settled_timelines:
            settled_exports = character_svg.export_requested_symbol_frames(
                [settled.request for settled in settled_timelines.values()],
                ffdec=ffdec,
                zoom=1.0,
                destination=root / "settled-exports",
                frame_count=scan_frames,
            )
            for key, settled in settled_timelines.items():
                exported[key] = [
                    character_svg.transform_ffdec_registration(
                        path,
                        root / "settled-transformed" / key / f"{index:06d}.svg",
                        placement=settled.placement,
                        zoom=1.0,
                    )
                    for index, path in enumerate(settled_exports[key], start=1)
                ]
        symbols: dict[str, Any] = {}
        for request in exportable:
            paths = exported.get(request.key)
            if not paths:
                continue
            signatures = [file_sha256(path) for path in paths]
            pattern = character_svg.signature_state_pattern(signatures)
            first_signature_by_state: dict[int, str] = {}
            for frame_index, state_id in enumerate(pattern):
                first_signature_by_state.setdefault(state_id, signatures[frame_index])
            states = [
                first_signature_by_state[state_id]
                for state_id in range(len(first_signature_by_state))
            ]
            period = character_svg.detect_loop_from_signatures(
                {request.key: signatures},
                max_frames=max(1, scan_frames - character_svg.LOOP_VALIDATION_FRAMES),
                validation_frames=character_svg.LOOP_VALIDATION_FRAMES,
            )
            # Random-pose cosmetics mirror the same display list mid-timeline
            # (the ground gate reads as "swapping direction" when looped).
            # These items are authored to freeze on a random pose via AS3.
            # Scan only the leading window; the runtime export path repeats
            # this detection when the source is dynamic and has no manifest.
            mirror_flip_frame = (
                character_svg.detect_mirror_flip_frame(paths[:120]) or 0
            )
            symbols[request.class_name.casefold()] = {
                "root_frame": request.frame,
                "root_timeline_frames": request.root_timeline_frames,
                "frame_signatures": signatures,
                "unique_states": len(states),
                "period": period,
                "states": states,
                "mirror_flip_frame": mirror_flip_frame,
                "random_pose_as3": request.class_name and "random" in request.class_name.casefold(),
                "settled_stop_frame": (
                    settled_timelines[request.key].stop_frame
                    if request.key in settled_timelines
                    else None
                ),
            }
    return {"schema_version": ANIMATION_METADATA_SCHEMA, "symbols": symbols}


def _root_frame(source: Path, class_name: str, character_id: int) -> int:
    try:
        return character_svg._symbol_frame(source, class_name)
    except Exception:  # noqa: BLE001 - fall back to frame 1
        return 1


def main() -> int:
    args = parser().parse_args()
    asset_root = args.asset_root.expanduser().resolve()
    ffdec = args.ffdec.expanduser().resolve()
    if not asset_root.is_dir():
        raise SystemExit(f"Asset root does not exist: {asset_root}")
    if not ffdec.is_file():
        raise SystemExit(f"FFDec does not exist: {ffdec}")
    candidates = sorted(asset_root.rglob("*.swf"))
    if args.dry_run:
        print(f"Would analyze {len(candidates)} SWFs")
        return 0
    client = boto3.client("s3")
    results = {"analyzed": 0, "skipped": 0, "failed": 0, "existing": 0}

    def run_one(path: Path) -> str:
        digest = file_sha256(path)
        key = (
            f"animation-metadata/{ANIMATION_METADATA_SCHEMA}/"
            f"{FFDEC_VERSION}/{digest}.json"
        )
        try:
            client.head_object(Bucket=args.bucket, Key=key)
        except Exception:  # noqa: BLE001,S110 - missing means compute
            pass
        else:
            return "existing"
        payload = analyze_one(path, ffdec, SCAN_FRAMES)
        if payload is None:
            return "skipped"
        payload["swf_sha256"] = digest
        payload["ffdec_version"] = FFDEC_VERSION
        payload["scan_frames"] = SCAN_FRAMES
        body = canonical_json(payload)
        try:
            client.put_object(
                Bucket=args.bucket,
                Key=key,
                Body=body,
                ContentType="application/json",
                Metadata={"swf-sha256": digest},
            )
        except Exception:  # noqa: BLE001
            return "failed"
        return "analyzed"

    with ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = {executor.submit(run_one, path): path for path in candidates}
        for done, future in enumerate(as_completed(futures), start=1):
            results[future.result()] += 1
            if done % 50 == 0 or done == len(futures):
                print(f"{done}/{len(futures)} {results}", flush=True)
    print(f"Complete {results}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
