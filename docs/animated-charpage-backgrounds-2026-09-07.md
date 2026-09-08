# Animated charpage backgrounds

The original implementation flattened background SWFs to their first frame.
This caused the reported omissions in these successful jobs:

| Job | Selection | Official source |
| --- | --- | --- |
| `680c8e3f-cbfc-4155-b7af-50f6c78e926c` | W / 32 | `cp-bg32.swf` |
| `70783b86-fb61-43dd-9944-fc4ac807ecee` | Y / 34 | `cp-bg34.swf` |

## Implementation

All selectable backgrounds now use their complete pinned SWF source, through
the existing source cache and distributed export/bounds/component raster path.
A synthetic one-frame parent preserves the original stage transforms while
FFDec advances its root and nested timelines. Background state IDs participate
in full-frame composition recipes and deduplication. This is not a whitelist
for just these two backgrounds. The two reported sources have real-asset tests;
other selectable sources use the same path but have not all been replayed.

The background covers the viewport without changing character-fit bounds or
mirroring with character facing. The beige base is below it; the fade is above
it and below the character; information text remains above the character.
Static presentation layers are decoded once per compose batch. Background
pixels use the ordinary cached component PNG path. AVIF output still uses
zstd-compressed lossless RGBA between composition and final encoding.

Authored background cycle lengths are included in duration/loop decisions.
We conservatively combine sprite lengths rather than infer a complete cycle
from a quiet prefix. W/32 has a 197-frame authored cycle (about 8.2 seconds at
24 fps). Y/34 contains multiple timelines; their conservative combined period
is much longer than the normal cap. A capped output is not labeled a complete
loop merely because its character repeats. Existing maximum-frame limits apply.

Metadata now records `aqw:background`, displayed in Discord General Info as
`Background W (32)`, `Background Y (34)`, `Default (0)`, or `None` when disabled.
Older metadata remains readable. Final render cache policies were incremented;
old static-background results are not reused.

## Validation

- Exact SHA-verified W and Y sources: 210 scheduled frames each, 31 and 47
  unique exported SVG states respectively. Imported/rasterized sample frames
  differ visibly; inspected W's sky effect and Y's running character. Repeat
  exports hit vector cache.
- Full pipeline suite: 79 unit tests and 10 integration tests passed;
  environment-dependent tests skipped. The real-background test above was run
  separately and passed.
- Composer: 45 unit and 6 integration tests passed. Layer-order coverage checks
  the background overlay is below character pixels in WebP and zstd AVIF paths.
- Discord metadata: 7 tests and 14 subtests passed, including both file formats.
- Synthetic pipeline regression verifies quiet-prefix handling, background-first
  composition, unchanged character framing, and no background mirroring.

These are local macOS correctness checks, not AWS latency measurements. Motion
adds distinct frames and can increase encoding size and raster work compared
with a still background. Output quality and compression settings were not raised.

Reproduce the exact-source regression:

```sh
AQW_TEST_BACKGROUND_DIR=/tmp/aqw-background-animation \
AQW_TEST_FFDEC=/tmp/aqw-background-animation/ffdec/ffdec.jar \
AQW_TEST_BACKGROUND_PREVIEWS=/tmp/aqw-background-animation/previews \
JAVA_TOOL_OPTIONS='-Djava.awt.headless=true' \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --lib real_background_timelines_preserve_motion -- --ignored --nocapture
```

The fixture directory needs the pinned sources saved as `bg32.swf` and
`bg34.swf`. Source hashes are in `assets/charpage/sources.json`. Runtime wrapped
sources are stored under the existing permitted
`dynamic-assets/<dataset>/presentation/` prefix.

## Deployment and verification

The user builds/deploys the pipeline and compose images, then updates the bot.
No new workflow nodes or IAM permissions are required. No ARM build, deployment,
or AWS replay was performed during this change.

After deployment, retry the two job IDs above using `/retry-render`, retaining
the same format, sizes, quality and frame cap for comparison. Verify the sky
effect and running character, then use View render info to check General Info's
background field. Use at least 197 frames if testing the whole W background
cycle; the character and other equipment may require a longer combined cycle.
