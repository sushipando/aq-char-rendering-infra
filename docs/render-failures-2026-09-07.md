# Export failures: Steampunk Landship and characterB getters

Read-only AWS execution histories and saved prepare inputs were captured under `/tmp/aqw-failed-job-review/`. All three jobs failed inside `ExportSourceFrames`, before rasterization, composition or final encoding. The undeployed zstd-only Discord change is unrelated. The requests already selected zstd, but this stage never reached the image handoff.

| Job | Character | Recorded cause |
|---|---|---|
| `52ead8e4-f8ac-4407-92ec-f03f9dd55209` | joeismymom | Unsupported conditional/early-exit frame control on frame 2 of sprite 469, `SteampunkLandshipUnitArmorMHead` |
| `a06856b4-5781-49c8-a38a-7ccb823410d3` | pppinkkk1 | `AvatarMC.as: duplicate function get` |
| `2ee51509-71f4-46de-80b6-e06c3fcbf309` | aly | Same frame-2 rejection in `SteampunkLandshipUnitArmorFHead` |

## Fixes

The Landship head script registers animation listeners inside a try block with an empty catch, then executes an unconditional `stop()`. Our parser saw references to callback methods containing conditional animation changes and treated registration as immediate execution. It also rejected the whole callback because of the try/catch syntax.

The parser now recognizes the specific literal `MovieClip(parent.parent.parent).addAnimationListener(...)` registration-only block with an empty catch. It excludes registrations from immediate timeline commands and preserves the unconditional stop. It does not execute Walk/Attack callbacks or pretend arbitrary conditional control is supported. Nonempty catches, timeline commands inside the try block, dynamic arguments/receivers and conditional stops remain unsupported.

The second job exports a characterB symbol and inspects its decompiled classes. `AvatarMC` contains multiple getters such as `get helmName` and `get armorName`. The parser had named every getter `get`. It now distinguishes accessor kind and property name; duplicate definitions still fail. A reachable property evaluation remains unsupported rather than silently ignoring possible accessor side effects.

The export policy changes to `rust-effective-svg-v3-script-syntax`, invalidating cached parsed source metadata and exports produced under the previous parser. It also changes dependent render identity. The first render after deployment may consequently perform additional export work.

## Validation

Regression cases cover multiple getters/setters, duplicate accessors, reachable property use, registration-only try/catch and unsafe variants. Local real-source checks use downloaded male/female Landship SWFs and characterB, including the full shared normalization request set for each source. Additional source-corpus checks decompile and normalize the other downloaded inputs from pppinkkk1's request.

These checks validate the failing parser/normalization stage on actual source bytes. They are not full AWS render replays or M1-to-AWS performance measurements. No ARM build, deployment or new AWS render was performed.

## Owner verification after build/deployment

Retry the saved requests through the normal workflow:

```bash
scripts/render-character --restart 52ead8e4-f8ac-4407-92ec-f03f9dd55209 --no-render-cache
scripts/render-character --restart a06856b4-5781-49c8-a38a-7ccb823410d3 --no-render-cache
scripts/render-character --restart 2ee51509-71f4-46de-80b6-e06c3fcbf309 --no-render-cache
```

Alternatively use `/retry-render job:<id> render_cache:false` once that command is deployed. Preserve the original lossless and format settings for this verification. The CLI path does not post to Discord. Inspect the Landship idle appearance after completion; this fix preserves its authored stopped state rather than selecting walking or attack frames.

Validation completed: 68 Rust unit tests passed, the eight-source real FFDec corpus check passed, and the saved-script full-context check passed for both 11-root armor sources plus characterB. Both armor head decisions explicitly resolve to `Hold { frame: 2 }`. Full AWS replays remain owner-run after deployment.
