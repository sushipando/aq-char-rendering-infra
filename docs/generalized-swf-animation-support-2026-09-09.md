# Generalized SWF animation support — September 9, 2026

This extends the two conditional-corpus batches with instance-aware timeline evaluation, generated-layer support, and optional animation clicks. No deployment or ARM build was performed. The full local corpus should still be rerun by the operator before deployment.

## Click behavior and inputs

`render.click_assets` selects asset slots. It defaults to `[]`. Supported values are `armor`, `weapon`, `cape`, `helm`, `pet`, `ground`, `hair`, and `background`. Values are validated, deduplicated, sorted, and included in cache identities.

Each selected rendered asset instance receives one animation-producing click after its initialization and idle-state selection. Frame-one setup runs even when the selected Idle label is later. A click targets one registered display object, using mouse-down, mouse-up, and click handlers for that object. It does not rerun constructors or frame callbacks to discover the handler. Armor parts and front/back weapon instances retain independent state.

Shop, quest, bank, sound, and notification services are absent from the render host. UI-only handlers do not select an animation or change the result. The implementation selects the first registered target that produces a timeline command; it does not support choosing among multiple buttons or clicking again during playback.

Command line:

```sh
scripts/render-character ___cj --click-assets pet
scripts/render-character ___cj --click-assets armor weapon
```

In the test-guild `/retry-render` command, use its existing JSON overrides:

```json
{"render":{"click_assets":["pet"]}}
```

To clear an inherited click setting:

```json
{"render":{"click_assets":[]}}
```

The existing slash commands already use Discord's 25-option allowance. Click settings therefore use the existing retry overrides and the Python client input rather than adding another slash option.

## What changed

| Previous problem | New behavior |
| --- | --- |
| Parameterized helpers, local state, conditionals and component setters | A bounded ActionScript evaluator follows reachable expressions, helper calls, defaults, setters, dictionaries, arrays and early returns. Unknown reachable operations still produce an explicit error. |
| Automatic frame callbacks | Registered frame-constructed, enter-frame and exit-frame handlers run through the evaluator. Instance creation/removal callbacks are handled by the synchronized fallback. Numeric non-object frame callbacks clear the registration. |
| Shared symbols with different placement context | Placements receive independent definitions and script state. Front/back shoulders, hands, thighs, shins, daggers and gauntlets carry their actual holder names. Hair and helms include the head container in their ancestry. |
| Parents controlling child playheads | A synchronized instance tree advances on a shared clock and bakes display-list changes for FFDec. Parent and child controls no longer have to be reduced to independent loops. |
| Non-contiguous loops and finite click transitions | Selected frame sequences preserve jumps, introductions and final held poses. Surviving children keep their identities across frame changes. |
| Adobe Animate advanced layers | Verified generated methods are translated into layer bindings and baked transforms/effects. This includes multiple pairs, Graphic components, generated frame-range setters, custom callbacks beside generated scaffolding, and the older runtime template. Default Graphic looping wraps shorter artwork on its controlling clock. |
| Generated masks | Existing authored clip-depth masks are retained; verified flat-layer mask transforms and mask clocks are normalized. Non-neutral mask color composition and camera/depth behavior remain explicit unsupported cases. |
| No exported item class | Stage-only item artwork gets a synthetic export root, preserving registration coordinates. The derived source has its own cache policy. |
| Zero-frame clips containing artwork | Complete, unbound, display-only definitions get an implicit first display frame. Scripted/malformed zero-frame clips are not indiscriminately repaired. |
| Missing children and invalid labels in authored callbacks | Known runtime faults preserve work before the fault and appear as warnings, rather than being confused with unsupported interpreter operations. Older SWFs retain their version-specific missing-label behavior. |
| Hair/head/weapon visibility changes | Supported host visibility changes reach composition. A changing or conflicting host-layer visibility schedule is still rejected explicitly. |
| Long sanity runs during local development | Every run copies the native helper into its own output directory and records its hash, FFDec hash and Git provenance. Rebuilding the workspace helper cannot mix executable versions within a report. |

The prior fixes for deterministic BasicFire initialization, idle combat registration, bank component setup, escaped identifiers, missing End tags and approved AVM1 background initialization remain part of this worktree.

Vector export policy is `rust-effective-svg-v12-instance-script-state`. The policy change prevents reuse of vector results normalized under the older behavior; initial exports can therefore miss the vector cache.

## Implementation map

- [script_eval.rs](../services/pipeline-rust/src/script_eval.rs): bounded expression/statement evaluation, callbacks and click dispatch.
- [instances.rs](../services/pipeline-rust/src/instances.rs): per-placement definitions, class aliases and ancestor names.
- [timeline.rs](../services/pipeline-rust/src/timeline.rs): fast timeline resolution and fallback selection.
- [scene_timeline.rs](../services/pipeline-rust/src/scene_timeline.rs): synchronized playback, instance lifetimes and capture.
- [display.rs](../services/pipeline-rust/src/display.rs): lossless placement snapshots and deltas, including clearing stale effects on backward jumps.
- [animate.rs](../services/pipeline-rust/src/animate.rs): generated-runtime verification, property bindings, Graphic clocks and masks.
- [stage_asset.rs](../services/pipeline-rust/src/stage_asset.rs), [swf.rs](../services/pipeline-rust/src/swf.rs): stage roots and structural repairs.
- [resolve.rs](../services/pipeline-rust/src/resolve.rs), [export.rs](../services/pipeline-rust/src/export.rs), [finish.rs](../services/pipeline-rust/src/finish.rs): input context, export/cache integration, warnings and host visibility.
- [sanity_check_swfs.py](../scripts/sanity_check_swfs.py): local corpus checks and `--retry-report`.
- `../aq-image-search/bot/char_render_client.py` and `char_render_retry.py`: click input and retry inheritance/overrides.

## Validation and limits

The regression suite covers independent placement context, one-time click initialization, later Idle selection, UI-only clicks, generated/custom callback combinations, Graphic wrapping, retained child identity, effect clearing, request validation and retry inheritance. Real local SWFs are exported with FFDec and rasterized with resvg in a separate ignored regression test. These checks validate code paths and nonempty/changing pixels; they are not a full Flash/Ruffle pixel-conformance suite.

The broad recheck completed with **1,193 `ok` and six `timeline_review`**, out of 1,199 previously flagged unique contents/policies. It is saved at:

`/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-0lkms5sj/REPORT.md`

All six remaining diagnostics are isolated internal skull clips whose parent is missing in that standalone probe:

- `NecronautHeadPet-9Jan16.swf`
- `NecronautHeadPet2-4Jan16.swf`
- `Silentchucklespet.swf`
- `SkullKingPet.swf`
- `chucklespet-28Dec15.swf`
- `chucklespetr3.swf`

Their full pet roots pass. The checker now also writes `timeline_roots_passed` and `script_warnings`, retaining the unsuccessful standalone probes. It still exits nonzero when review entries exist.

The broad report uses an immutable helper snapshot. Subsequent fixes to click initialization, Graphic wrapping, callback error handling, and report detail were checked with focused regressions and a further recheck of the 46 hardest files; this is not a claim that every report was produced from an identical binary.

The expanded visual regression passed **10 export/raster cases across nine local SWFs**, exporting 36 frames per case and rasterizing SVG states at 384 pixels. Cases cover the clicked/unclicked DarkBloodEviscerater hand, Lanceofcthulhu, MutantShadowDragonPet, ArchFMageSword, ScarlettaNCMirrorC2, dBSGauntletsr2, Kaharakasandknightslasherdaggers, AuraMaxingPetr1, and SoulDevourerBlade. Artifacts:

`/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-generalized-visual-PeT5KA/`

Python validation passed **50 renderer/checker tests**, plus **32 bot tests and 51 subtests**. The final native suite passed **118 Rust unit tests and six pipeline integration tests**. The default suite skips 14 unit tests and six integration tests marked opt-in/ignored; the ten-case real export/raster test was run explicitly as described above.

The final focused recheck produced **40 `ok` and the same six context-only `timeline_review` entries**, out of the 46 hardest previously flagged files:

`/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-n42rpppw/REPORT.md`

Its `results.jsonl` explicitly lists successful top-level roots for all six: `NecronautHeadPet`, `NecronautHeadPet2`, `Silentchucklespet`, `SkullKingPet`, and `chucklespet` in each of the two corresponding files.

The checker probes every exported sprite independently. An internal skull animation that calls `parent.gotoAndPlay("Idle")` cannot run as an isolated top-level asset. When its actual pet root passes, that internal-root diagnostic is not evidence that a real pet render will fail. Do not erase such diagnostics or call every successful parser check a visual pass.

This is a bounded render-oriented evaluator, not a complete Flash VM. Game services, network calls, timers, arbitrary object construction, arbitrary display-tree mutations and general 3D/camera behavior are not implemented. The render host has no equipped avatar-copy service or quest progression; callbacks depending on absent game objects can emit warnings and leave authored artwork visible. No shops or quests are opened.

Native timings on this M1 Mac are not AWS performance measurements. Extra instance definitions and synchronized simulation can add preparation work for complex assets. Identical SVG/component states still use the existing deduplication/cache paths. Encoding settings, image resolution and lossy/lossless choices are unchanged; correctly restored animation can naturally contain more distinct frames than an incorrectly frozen render.

Primary-source checks for frame registration, label lookup and initial timeline behavior used Ruffle's [MovieClip API implementation](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/core/src/avm2/globals/flash/display/movie_clip.rs) and [MovieClip display implementation](https://raw.githubusercontent.com/ruffle-rs/ruffle/master/core/src/display_object/movie_clip.rs). Host hierarchy was checked against the locally decompiled `characterB.swf` (`AvatarMC.onLoadSkinComplete`, hair/helm and weapon loading).

## Operator checks

Run the complete local corpus with the final native helper:

```sh
python3 scripts/sanity_check_swfs.py \
  ../aq-image-search/bot/assets/swf_item_index/swf_assets \
  --ffdec /tmp/aqw-background-animation/ffdec/ffdec.jar
```

Or recheck only entries previously flagged:

```sh
python3 scripts/sanity_check_swfs.py \
  --retry-report /var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-6793ar3x/results.jsonl \
  --ffdec /tmp/aqw-background-animation/ffdec/ffdec.jar
```

Each command creates a new temporary report directory. It never overwrites another run's `REPORT.md`.

The real export/raster regression is local only:

```sh
AQW_TEST_LOCAL_CORPUS="$PWD/../aq-image-search/bot/assets/swf_item_index/swf_assets" \
AQW_TEST_FFDEC=/tmp/aqw-background-animation/ffdec/ffdec.jar \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  real_generalized_timeline_exports_preserve_pixels_and_click_animation \
  --lib -- --ignored --nocapture
```

After the operator's ARM build and deployment, compare the same character with an empty click list and the relevant clicked slot. Include a transformation item, a bank pet with ordinary idle animation, a generated-layer weapon, and the masked cape. Check frame continuity, front/back placement, loop metadata, file size and AWS preparation/raster durations independently.
