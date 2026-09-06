# Nested idle timeline resolution

## Root cause

The old export correction inspected the first SVG, found one stop frame, and
optionally replaced the root with one child. It did not recursively resolve
controllers inside that child. It also treated a regex match for `stop()` as a
terminal stop even when the statement was conditional or unreachable.

The M4tr1x Visionairy Quibble pet (`items/pets/QuibbleBFCM2024Pet.swf`) demonstrates
the problem:

```text
QuibbleBFCM2024Pet, sprite 114: Idle at 8; stop(); CCPet.gotoAndPlay("Idle")
  CCPet_2, sprite 113: Idle at 1; stop at 16; Walk starts at 17
    _quible_idle, sprite 94: ordinary 37-frame animated artwork
```

Holding only sprite 114 left sprite 113 free to advance into Walk. The correct
settled export holds 114 at 8 and 113 at 16, while 94 keeps animating. None of
these names or frame numbers is special-cased in production code.

## Implementation

`script.rs` tokenizes FFDec's decompiled ActionScript, excluding comments and
string contents from code matching. It reads literal `addFrameScript` pairs in
unconditional constructor calls and compiles their callbacks into a small typed
control representation. Supported controls include self `stop`/`play`, literal
frame-number or frame-label `gotoAndStop`/`gotoAndPlay`, direct named-child gotos,
and simple local no-argument helpers. Callback order and destination callbacks
are accounted for. Unreachable Walk callbacks do not invalidate a proven Idle
path. Initial root selection prefers the actual Idle label, not Idle minus one.

`timeline.rs` traverses reachable display-list instances recursively. It follows
supported controls to a hold frame or contiguous loop, retaining descendants
that have independent animation. It does not infer every nested clip is idle or
freeze all descendants when a parent stops. Root sampling without self-controls
retains the existing static-root selection contract.

The resolver rewrites only affected DefineSprite timelines in a temporary,
export-only SWF. It retains original character IDs, symbol classes, hierarchy,
placement/update/removal order, transforms, color transforms, masks, and filters.
For a hold, display-list updates through the selected frame precede a single
ShowFrame; for a loop, the selected contiguous interval is retained. Unaffected
sprite payloads are unchanged. Both FWS and CWS inputs are supported. The source
SWF in S3 is never modified. The normalized SWF is **not** intended for execution
in a Flash runtime: its original bytecode frame registrations are not rewritten.

FFDec generates SVGs only after normalization. There is no provisional export or
SVG child replacement. The existing inotify -> S3 -> SQS prefetch continues to
publish each unique completed SVG immediately, now using corrected timelines.
There is no additional Step Functions stage or raster pass.

Each source manifest stores `timeline_decisions`. The
`export_timeline_resolution` log records the same character IDs, class names,
and selected frame/range for diagnosis.

## Deliberate limits

This is a bounded static export resolver, **not a Flash/ActionScript VM** or an
exact simulation of elapsed startup time. It resolves a settled state and an
independent animation phase. It does not emulate game event handlers, external
helper behavior, runtime property mutation, or arbitrary ActionScript inheritance.

Recognized uniform random-pose expressions retain a deterministic first-variation
policy, matching the renderer's existing still-pose behavior. Only explicit
allowlisted idioms are accepted; other dynamic targets are not evaluated.

Detected unsupported reachable controls fail before SVG publication: conditional
timeline code, dynamic callbacks/targets, indirect child paths, child stop/play
without a known target, noncontiguous scripted loops, repeating parents that
control child playheads, AVM1 actions, and shared symbols requested in conflicting
per-instance states. These last cases need per-instance cloning or synchronized
playhead simulation; silently rewriting a shared definition would be incorrect.

The resolver also bounds file size, tokens, nesting, tag count, display-list work,
helper recursion, and hierarchy depth, and detects cycles and invalid targets.
Failure messages include the sprite/class and reachable frame where applicable.
Extending the supported subset requires a focused fixture, not an asset-name
exception or a fallback to the first regex-matched stop.

## Caches and rollout

The export policy is `rust-effective-svg-v2-timelines`.

- Source-script metadata and vector manifests are policy-keyed.
- The final render hash now includes the export policy, preventing an old final
  WebP from hiding corrected export behavior after deployment.
- Legacy animation-period metadata is ignored unless it explicitly declares the
  current export policy; a period measured from the broken timeline must not
  truncate corrected exports. The legacy metadata builder is not relabeled as
  compatible. Until a compatible entry exists, normal SVG schedule analysis runs.
- Content-addressed bounds and component caches remain usable when their actual
  inputs are identical. Existing cache objects are not deleted.

The first render after deployment can therefore miss old export/final caches.
Deployment remains manual; this change does not pause admissions or change
Lambda architecture, renderer threading, or dependencies.

## Regression checks

```sh
cargo test --manifest-path services/pipeline-rust/Cargo.toml

AQW_TEST_FFDEC=/path/to/ffdec-cli.jar \
AQW_TEST_MOGLIN_SWF=/path/to/QuibbleBFCM2024Pet.swf \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --lib real_moglin_export -- --ignored --nocapture

AQW_TEST_FFDEC=/path/to/ffdec-cli.jar \
AQW_TEST_SOURCE_CORPUS=/path/to/downloaded-job-directory \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --lib real_source_corpus -- --ignored --nocapture
```

The moglin fixture checksum is
`777ad52938825594c49c4b46af688314e4e44ad5eae9dc44ae2c5450601b33f6`.
It is supplied locally, not committed. Its regression exports 76 frames, asserts
that walking artwork IDs never appear, checks visible animation with resvg,
checks the intended controller decisions, verifies unchanged idle artwork, and
checks both the exact prefetched SVG set and a subsequent vector-cache hit.

The optional corpus test expects `input.json` with prepared sources/requests and
downloaded source files under `source/<source key>`. It decompiles and normalizes
locally without AWS calls. The saved corpus exercised armor (11 roots), cape,
helm, a random-pose dragon ground asset, another pet, and a weapon.

Synthetic tests cover multi-level controllers, unchanged animated descendants,
siblings, removal and MOVE updates, matrix/CXFORM/mask/filter bytes, literal
loops, destination callbacks, ordering, invalid/conditional controls, conflicting
instances, compressed SWFs, cycles, and incompatible animation-cache metadata.

## Format references

The hierarchy and display-list rewrite follow the
[Adobe SWF specification](https://open-flash.github.io/mirrors/swf-spec-19.pdf).
MovieClip playhead semantics, including independently playing child clips and
literal goto targets, are documented in the
[AIR MovieClip API reference](https://airsdk.dev/reference/actionscript/3.0/flash/display/MovieClip.html).
