# AVM1 playback support: Shadowfall and other assets

Follow-up: [River construction metadata and additive effects](render-fixes-river-and-additive-auras-2026-09-09.md)
extends the supported inert placement metadata and supersedes the cache policy below.

Job `eefaa5cb-3c61-4162-990e-9766289e3347` failed during background SVG export for
Bloome's charpage. Fetching succeeded. The asset was `etc/chardetail/bgs/cp-shadowfall.swf`
(background index 6), and the exception ended at sprite 145 with
`AVM1 timeline actions are unsupported`.

The exact cached, wrapped SWF has SHA-256
`782caf7d4431c6534960eae224bb68056cb7c1c1372935e4163fb5bfdfe8e93e`.
FFDec found three action blocks:

| Sprite | Frame | Instructions |
| --- | --- | --- |
| 145 | 1 | `stop();` |
| 145 | 37 | `gotoAndStop("Move"); play();` |
| 235 | 1 | `stop();` |

Sprite 145's `Move` label is on frame 9. Its default `Idle` frame stops at frame 1.
Accepting the file by simply ignoring these commands would animate stopped clips
incorrectly. Its existing background-setup approvals were empty because these
are real playback controls, not the metadata/color setup supported previously.

## Implementation

[avm1.rs](../services/pipeline-rust/src/avm1.rs) decodes bounded DoAction bytecode
directly. Playback support does not depend on filenames, sprite IDs, decompiled
class names, or exact ActionScript source formatting. It applies to requested
AVM1 item trees as well as wrapped backgrounds.

Supported instructions include:

- Stop, Play, NextFrame, PreviousFrame.
- GotoFrame (zero-based encoded frame), GotoLabel, and GotoFrame2 with a literal
  frame/label and optional scene offset.
- String, integer, float and double pushes, local constant pools, Pop,
  PushDuplicate, and StackSwap used to supply literal playback targets.

The translator creates per-sprite timeline metadata and removes translated
action tags from the export copy. The original S3 asset is unchanged. Independent
placements receive separate state through the existing instance expansion.
[scene_timeline.rs](../services/pipeline-rust/src/scene_timeline.rs) advances the
instance tree and bakes display frames for FFDec. Finite introductions and final
held poses are retained, and children of a stopped clip continue animating.

AVM1 destination callbacks enter a queue: a block's remaining instructions finish
before the queued callback runs. For example, `gotoAndStop(2); play();` followed
by frame 2's `stop()` ends stopped. Intermediate goto callbacks are retained too.
Relative next/previous controls use the live playhead when their block executes.
The AVM1 queue is separate from existing AS3 callback handling.

Label lookup follows AVM1's ASCII-insensitive behavior. A missing label records a
warning and leaves the playhead/play state unchanged. GotoLabel treats a numeric
label as a label; GotoFrame2 can interpret a numeric string as a frame. Semantics
were checked against Ruffle's [AVM1 opcode handlers](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/core/src/avm1/activation.rs),
[MovieClip target conversion](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/core/src/avm1/globals/movie_clip.rs),
and [display timeline/action queue](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/core/src/display_object/movie_clip.rs).
The SWF operand layouts are documented in its [AVM1 reader](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/swf/src/avm1/read.rs).

The export cache policy is now `rust-effective-svg-v13-avm1-timeline-controls`.
This invalidates derived vector/script metadata made under the prior policy.
The cached original SWFs remain reusable. Resolution and encoding options do not
change. More complex AVM1 assets use bounded timeline simulation, so local timings
should not be interpreted as AWS performance measurements.

## Limits

This adds literal playback support, not a complete AVM1 virtual machine. Dynamic
expressions, branches, function calls, cross-clip target paths, variable/display
mutations, clip-event handlers, and DoInitAction class initialization still need
their own semantics. Unsupported bytecode fails with sprite, frame, opcode and
offset context where available. AS3 files do not use this AVM1 translator.

The existing background-only, byte-hash-bound approvals still handle previously
reviewed inert setup. They do not authorize arbitrary extra code. The decoder
checks entire action blocks, including operand lengths and End markers. Limits
include 1 MiB per block, 256 stack operands, 4,096 constants, a shared two-million
instruction/tree budget, and 10,000 queued callbacks per tick. Invalid target
encodings and overflowing/nonintegral numeric targets remain explicit errors.

AVM1 mouse-event/button dispatch is not added here. The prior AS3 click feature
continues to work; this fix handles the autonomous AVM1 frame controls above.

## Reusable validation

The ordinary native suite includes generated SWF regressions for label/numeric
gotos, stop/play order, finite playback, child clocks, independent instances,
multiple action tags, queued callbacks, invalid bytecode, and bounded cycles.
The final run passed **134 unit tests and six pipeline integration tests**;
14 unit and six integration tests were skipped as opt-in/ignored.

```sh
cargo test --manifest-path services/pipeline-rust/Cargo.toml --lib --test pipeline
```

The real local regression is included in
[tests/avm1_background.rs](../services/pipeline-rust/tests/avm1_background.rs).
It uses the exact cached background, exports 120 frames through the production
path, rasterizes every unique state, verifies both authored stops, and checks a
warm vector-cache hit. It does not submit an AWS render.

```sh
aws s3 cp \
  s3://aqw-char-rendering-dev-sourceassetbucket61a8aef8-ma96leqehezz/dynamic-assets/dev-v1/presentation/background-timeline-v1/782caf7d4431c6534960eae224bb68056cb7c1c1372935e4163fb5bfdfe8e93e.swf \
  /tmp/aqw-shadowfall-wrapped.swf --profile aqw-char-dev --region us-west-2

AQW_TEST_SHADOWFALL_WRAPPED_SWF=/tmp/aqw-shadowfall-wrapped.swf \
AQW_TEST_FFDEC=/path/to/ffdec.jar \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --test avm1_background -- --ignored --nocapture
```

The local test passed: **120 exported frames, five SVG states, five distinct
raster states, both stops preserved, and a warm cache hit**. The preview shows
the Shadowfall hall and NPC artwork. This is not a pixel-perfect comparison with
the live Flash/Ruffle charpage. Preview PNGs and object-store artifacts are written
to the temporary directory printed by the test; the regression code is in the repo.

The previous escaped-scythe/background-18 export regression also passed. Background
18 retains its original normalized SWF bytes and its 50-frame/49-state animation.
No whole-corpus result is claimed for this patch.

After your ARM build and deployment, use `/retry-render` with the original job ID
`eefaa5cb-3c61-4162-990e-9766289e3347` and no input overrides. The CLI equivalent is:

```sh
AWS_PROFILE=aqw-char-dev AWS_DEFAULT_REGION=us-west-2 \
  scripts/render-character --restart eefaa5cb-3c61-4162-990e-9766289e3347
```

This creates a new job. The failing background stage has been tested locally;
the complete AWS character render still needs that deployment check.
