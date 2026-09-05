# ExportSourceFrames timeout: Hype Neon DragonGate ground item

Incident investigation, 2026-09-05. Job `2e49489b-414e-4288-8053-bb7753d98d3f`
failed with `RENDER_FAILED` when the `ExportSourceFrames` Map iteration for the
"Hype Neon DragonGate" ground cosmetic (`LaeDWearNeonDragonr1.swf`) hit the
300-second `aqw-char-dev-prepare` Lambda timeout.

## Summary

| | |
| --- | --- |
| Job | `2e49489b-414e-4288-8053-bb7753d98d3f` |
| Status | `FAILED` / `RENDER_FAILED` |
| Execution | `arn:aws:states:us-west-2:538522204887:execution:aqw-char-render-dev:2e49489b-414e-4288-8053-bb7753d98d3f` |
| Started / stopped | `2026-09-05T06:50:46Z` → `06:55:52Z` (~5 min) |
| Failing phase | `ExportSourceFrames` Map → `ExportSource` iteration for source `idx 3` |
| Failing item | `items/grounds/LaeDWearNeonDragonr1.swf` (class `LaeDWearNeonDragon`, char id 121) |
| Error | `Sandbox.Timedout` — `Task timed out after 300.00 seconds` |
| Root cause | First-time vector-cache build alpha-probed 104 unique ~486 KB SVG states serially with librsvg `rsvg-convert` (~1 s/probe), blowing the 300 s wall |
| Not | an infinite loop (see "Not an infinite loop" below) |

## Evidence chain

### Step Functions history (65 events)

```
Event 52  LambdaFunctionFailed  error=Sandbox.Timedout
          cause="Task timed out after 300.00 seconds"
          RequestId 6ffa8b27-1c48-4beb-b69f-3554256d62d3
Event 53  MapIterationFailed
Event 54  TaskStateAborted
Event 55  MapStateFailed
Event 56  MapStateAborted
Event 57  ParallelStateFailed
Event 59  CompleteFailedJob (terminal failure handler)
Event 64  FailStateEntered / Event 65 ExecutionFailed (CharacterRenderFailed)
```

`Sandbox.Timedout` is not in the `lambdaRetry` error list (only `Lambda.*` errors
are), so the Map iteration failed on its first attempt — no retry, straight to
the failure path.

### CloudWatch — five of six export invocations completed, the sixth hung

All six `ExportSource` invocations started at ~06:50:50Z. Five logged
`prepare_export_complete` in 5.7–9.6 s:

| source idx | SWF | duration_ms |
| --- | --- | --- |
| 0 | `classes/F/LoveKitty.swf` | 9560.6 |
| 1 | `items/Capes/GuardLlamaBack.swf` | 8021.4 |
| 2 | `items/Helms/GuardLlamaMorph.swf` | 5764.0 |
| 4 | `items/pets/AlpacacornPink.swf` | 7858.5 |
| 5 | `items/swords/GiganticCrystallisDangor1.swf` | 6042.4 |

Source `idx 3` — the ground `LaeDWearNeonDragonr1.swf` (downloaded from
`dynamic-assets/dev-v1/13/13e0e65f...63e.swf`) — **never logged completion**
and is the invocation that timed out:

```
START   RequestId: 6ffa8b27-1c48-4beb-b69f-3554256d62d3   (06:50:50.723Z)
END     RequestId: 6ffa8b27-1c48-4beb-b69f-3554256d62d3   (06:55:50.745Z)
REPORT  Duration: 300000.00 ms   Billed: 300734 ms
        Memory Size: 3008 MB   Max Memory Used: 377 MB   Status: timeout
```

377 MB max memory — a CPU-bound serial workload, not a memory failure.

### Why the expensive path ran at all

- This is the **first render of the `r1` recolor**. The only prior success
  (2026-08-30, job `6b56fbed...`) used `LaeDWearNeonDragon.swf` (non-`r1`) and
  predates the vector-state cache code.
- The content-addressed vector cache for sha
  `ce557052b3effab8fda152c7b4aee7769207da45e90c868c44d5b0e6950ae10c` does not
  exist in the source bucket
  (`vector-states/5/26.2.1/z1/start-1/frames-128/ce557052.../` is empty), so the
  full first-time cache build — including all alpha probes — ran inline inside
  the 300 s export Lambda.
- No animation-manifest entry either (`precomputed_loop` is `null` in
  `prepare/input.json`), so `complete_loop` exported the full 128 frames
  (120 `max_frames` + 8 `LOOP_VALIDATION_FRAMES`) instead of a manifest-derived
  loop length.

## Reproduction and phase timings

Replayed `prepare_export_source` phases locally against the exact SWF
(85,155 bytes, sha256-verified) using the pinned FFDec 26.2.1 jar
(sha256 `0333b569...` — matches the Dockerfile) and librsvg `rsvg-convert`.

| Phase (128 frames) | Local (Apple Silicon) |
| --- | --- |
| FFDec sprite export (`-selectid 121 -select 121:1 -sublength 128`) | ~4.5 s |
| FFDec AS3 script export (color rules / terminal stops / random pose) | ~2.1 s |
| `detect_mirror_flip_frame` (→ 49) / `settled_timeline` (none) / color transforms | ~0.8 s |
| **`_build_vector_cache` — rsvg alpha probes** | **100.6 s** |
| bundle tar.gz (60 MB → 24 MB) | ~1.2 s |

Key facts about the source:

- **128 frames → 104 unique states** (dedup via `file_sha256`). The item is
  authored as a *random dragon* display — `LaeDWearNeonDragonr1_fla.RandomDragon_3`
  appears in the exported `<defs>` — so the timeline cycles varied sprites and
  dedup does not collapse the probe count.
- Each frame SVG is **~486 KB** with hundreds of nested `<use>` sprites. 128
  frames ≈ 60 MB of SVG.
- A single 512 px librsvg probe takes **~1.0 s of CPU** (measured 0.93–1.10 s
  per frame). 104 serial probes ≈ 100 s locally.

The dev Lambda (~1.7 vCPU) is per-core slower than this Mac, so the probe pass
alone is an estimated **~250–350 s** there; plus export/AS3/bundle/misc
(~30 s) the invocation crossed the 300 s wall.

## Where the probe code lives

The probe is callers in `stages/prepare.py`, implementation in the legacy
item-renderer module:

```
_build_vector_cache (stages/prepare.py:654)        hot loop — 104× for this item
  └─ _probe_state_bounds (stages/prepare.py:452)    once per unique state
       └─ probe_svg_alpha (legacy/render_swf_items.py:1140)
            └─ render_svg_to_maximum (legacy/render_swf_items.py:1079)
                 └─ subprocess.run(rsvg-convert …)  # timeout=120, line 1127
```

| Location | Function | Role |
| --- | --- | --- |
| `stages/prepare.py:452` | `_probe_state_bounds` | Strip `<defs>`, validate wrapper matrix, probe at 512 then 1024 px if empty; `_INVISIBLE_STATE` detection |
| `stages/prepare.py:654` | `_build_vector_cache` | Dedup frames, call the probe serially per unique state, package cache archive |
| `legacy/render_swf_items.py:1140` | `probe_svg_alpha` | Render to temp PNG, read alpha channel only |
| `legacy/render_swf_items.py:1079` | `render_svg_to_maximum` | Build the `rsvg-convert --format png --width/--height N` command; `timeout=120` |
| `legacy/render_swf_items.py:1042` | `_render_svg_with_resvg` | resvg fast path (not reached here — see below) |
| `config.py:102` | `rsvg_convert` | Default `/usr/bin/rsvg-convert`; no `CHAR_RENDER_RSVG_CONVERT` override in the stack |

The probe is **discarded**, not converted-for-output: `probe_svg_alpha` writes a
temp `.bounds.png`, extracts only the alpha bbox, and `unlink`s it. The probe's
sole purpose is the `bounds` tuple stored in the cache manifest (tight visible
bounds for viewbox sizing; null bounds mark invisible blink states).

### "Convert again later" — the other rasterization is a different engine

The same SVG is rasterized twice in the pipeline, but by different engines for
different purposes:

| Pass | Engine | Size | Kept? | Purpose |
| --- | --- | --- | --- | --- |
| prepare probe (per unique state, cache-miss only) | `/usr/bin/rsvg-convert` (librsvg) | 512 px | deleted | bounds metadata (4 floats) |
| component-raster (per unique placed state) | resvg or thorvg in-process, Rust | 4096 px | yes | the actual output pixels |

The full-size conversion runs later in `services/component-raster-rust`
(`raster.rs::render_svg`/`component_svg.rs`) and is not a second pass over the
same probe. Note the fast resvg rasterizer (`/opt/resvg/resvg`, built in
`services/renderer/Dockerfile`) is only reachable if `rsvg_convert`'s basename
starts with `resvg`; the default configuration never hits that path.

## Not an infinite loop

Checked explicitly — the failure is a finite, oversized serial workload that hit
a hard wall, not an unbounded loop:

- **No unbounded loops in the code.** All `while` loops in
  `services/renderer/src` are bounded byte-walks, paginated scans capped at
  `maximum`, or `while chunk := read(...)` EOF loops. No `while True`.
- **Subprocesses guard themselves.** `render_svg_to_maximum` has `timeout=120`;
  the FFDec AS3 script export has `timeout=240`.
- **The workflow cannot retry-loop on this error.** `Sandbox.Timedout` is not in
  `lambdaRetry`, so the iteration failed once.
- The "hang" look is a logging gap: `prepare_export_source` logs exactly once,
  at the very end (`prepare_export_complete`). 300 s of CPU work under a
  recorded `START`→`END` with no intermediate logs is indistinguishable from a
  hang.

## One latent unbounded-hang risk

`export_requested_symbol_frames` (`character_svg.py:984`) runs the FFDec sprite
export with `subprocess.run(command, capture_output=True, text=True)` and **no
`timeout=`** — the only unguarded subprocess in the prepare pipeline. A JVM
deadlock on some future SWF would hang there until the Lambda kill. It did not
happen here (the same jar+SWF exported in ~4 s locally), but it should be given
the same `timeout=` treatment as its sibling at `_decompiled_as3_paths`.

## Candidate fixes (not implemented)

1. **Cheapest, likely biggest win: point `CHAR_RENDER_RSVG_CONVERT` at the
   resvg binary.** `/opt/resvg/resvg` is already baked into the shared renderer
   image by the Dockerfile, and `render_svg_to_maximum` dispatches to the resvg
   path whenever the configured name starts with `resvg`. resvg is benchmarked
   several × faster than librsvg on complex AQW SVGs.
2. **Parallelize the probe loop.** `_build_vector_cache` probes 104 states
   serially; a `ThreadPoolExecutor` over subprocess-bound probes would use the
   Lambda's spare vCPU (gain bounded by vCPU, needs testing).
3. **Precompute probe metadata offline.** Extend `bootstrap_source_assets.py` /
   `build_animation_manifest.py` to also publish per-state bounds into the
   animation metadata, so export-time probing is skipped (cache hit ⇒ zero
   probes by construction on repeat renders).
4. **Make timeouts diagnosable.** Add phase-level structured logs inside
   `prepare_export_source` and a `timeout=` on the FFDec sprite export
   subprocess.

## References

- Workflow definition: `lib/aqw-char-rendering-infra-stack.ts:447`
  (`ExportSourceFrames` Map, `ExportSource` worker)
- Probe loop: `services/renderer/src/aqw_char_renderer/stages/prepare.py`
  (`_build_vector_cache` :654, `_probe_state_bounds` :452)
- Probe implementation: `services/renderer/src/aqw_char_renderer/legacy/render_swf_items.py`
- Unguarded FFDec subprocess: `services/renderer/src/aqw_char_renderer/character_svg.py:984`
- Config default: `services/renderer/src/aqw_char_renderer/config.py:102`
- Container image (librsvg + resvg): `services/renderer/Dockerfile`