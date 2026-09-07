# Fleki render failure — 2026-09-07

Job: `18a116eb-abed-417d-9cff-54a34bf31855`.

## Confirmed cause

The workflow failed in `ExportSource` (source index 3), about 4.9 seconds after
starting at 03:52:12 PDT. The original Lambda exception was:

```text
cannot resolve idle timeline: sprite 21 (FrostvalWaddlesTheBankDuck):
unsupported control on reachable frame 8:
Some("conditional/early-exit frame control is unsupported")
```

The asset is **Frostval Waddles Bank Pet**, from
`items/pets/FrostvalWaddlesTheBankDuck.swf`. The saved input selected sprite 21,
class `FrostvalWaddlesTheBankDuck`, with idle frame 8. The downloaded source
matches the job's SHA-256:
`641431a984dd3f1cfe84ac0524eebb037796367462d1092d4bae9bc4a0021cab`.

FFDec 26.2.1 decompiled that frame as:

```actionscript
internal function frame8() : * {
    if (!this.petInit) {
        this.petInit = true;
        this.initPet();
    }
    stop();
}
```

`initPet()` obtains the game root/avatar, enables mouse interaction on the
parent, and registers a click listener on `btnBank`, inside an empty-catch
try block. It does not change visual geometry or issue timeline commands.
The click handler opens the bank; it is not invoked by a frame callback.
The constructor registers callbacks for frames 1, 8, 28 and 29. Frame 1 jumps
to `Idle`; frame 28 contains a separate conditional transition to `Walk`.
That walk transition should not be reached after holding idle frame 8.

Before this fix, `services/pipeline-rust/src/script.rs` rejected control flow anywhere
in a callback that contains a timeline command or helper call. Consequently,
the harmless initialization conditional made it reject the unconditional
`stop()` that follows it.

## Correction and validation

This failure occurred before rasterization and final encoding. The request's
4096 raster size, 2048 output size, lossless AVIF setting, and zstd compression
were not the failing stage. The terminal handler released the active-job slot
and delivered the failure notification.

The parser now recognizes the complete bank initialization callback and the
exact `initPet()` helper body, ignoring whitespace/comments. It emits only the
unconditional parent `stop()`. It rejects lookalikes with visual or timeline
mutations, changed conditions, conditional stops, nonempty catches, missing
helpers, or local property accessors. The unrelated conditional walk callback
remains unsupported and unreachable. This is a narrow export policy for this
known bank UI idiom, not a general ActionScript interpreter.

The export policy is now `rust-effective-svg-v5-bank-pet-idle`, so stale vector
exports are not reused. This invalidates existing vector cache entries and may
make initial renders after deployment slower while caches refill.

Local validation on macOS:

- Full pipeline Cargo suite: 77 unit tests and 9 integration tests passed;
  12 environment-dependent tests skipped. The exact bank-duck regression was
  run separately and passed.

- Parser regression covers the accepted callback/helper and rejected variants.
- The exact SHA-verified failed SWF passes the actual FFDec export path, resolves
  sprite 21 to `Hold { frame: 8 }`, and supplies all 120 scheduled frames.
- Its idle export has one unique SVG; that SVG rasterizes with nontransparent
  pixels. The second export is a vector-cache hit.
- Existing timeline regression verifies that holding a parent preserves a plain
  animated child's SWF bytes. The bank pet itself yielded a static idle, so it
  is not evidence of visible child animation.

Reproduce the real-source regression with the locally saved evidence:

```sh
AQW_TEST_FFDEC=/tmp/ffdec/ffdec.jar \
AQW_TEST_BANK_DUCK_SWF=/tmp/aqw-fleki-failure/pet.swf \
JAVA_TOOL_OPTIONS='-Djava.awt.headless=true -Duser.home=/tmp/aqw-ffdec-home' \
cargo test --manifest-path services/pipeline-rust/Cargo.toml \
  --lib real_bank_duck_exports_idle_and_reuses_cache -- --ignored --nocapture
```

No ARM builds, deployments, or AWS render replays were performed. Local results
establish correctness of this failed source, not AWS performance or a full-job
success guarantee. After the user builds/deploys the pipeline image, retry
`18a116eb-abed-417d-9cff-54a34bf31855` using `/retry-render`.

Evidence is under `/tmp/aqw-fleki-failure` (workflow history, prepare input, SWF,
and decompiled scripts).

## Second bank pet: generalized self references

Job `65b26fca-8faf-4fff-8677-cca508e59400` failed at the same stage for
`items/pets/QuibXmasBank2.swf`, class `QuibXmasBank`, sprite 43, idle frame 8.
Source SHA-256:
`2843f1a4d086f49cee561d0de574ec96b730ecd8b93519339e9e6adcdab2ecff`.

The first fix did **not** cover this pet: its decompiled callback and helper
omit `this.` (`petInit`, `initPet()`, `rootClass`, etc.). Otherwise the recognized
bank setup is identical. Matching now accepts explicit, implicit, or mixed self
references at the known self-reference positions, including `stop()` versus
`this.stop()`. It does not strip qualifiers globally or accept other receivers.
Asset/class names are not used to select this parser behavior. Other bank pets
using this idiom are covered; arbitrary bank scripts still require analysis.

Both exact SWFs pass local export, nontransparent rasterization, and warm-cache
reuse with 120 scheduled frames. Quibble produces six unique SVG states; the
regression also checks that their rasterized pixels differ, preserving visible
child animation while the parent holds idle. Negative parser tests include
wrong receivers such as `other.petInit` and `other.this.initPet()`.

To run both real-source tests, add
`AQW_TEST_BANK_QUIBBLE_SWF=/tmp/aqw-quibble-bank.swf` to the environment above
and replace the test filter with `real_bank_`. AWS evidence for this second job
is `/tmp/aqw-bank-second-history.json`; scripts are under
`/tmp/aqw-quibble-bank-export`. No builds for ARM or deployments were performed.

## Third bank pet: already covered

Job `2a825854-afa9-4e3a-acf5-09bc8c89c179` failed before deployment of the fix
for `items/pets/AlvaroPetNXBank.swf`, sprite 104, class `AlvaroPetNXBank`, on
reachable frame 8 with the same conditional-control error. Its source SHA-256
is `5ac2e45bcbde94084b93cdd90dd016174c5ffabf2ea720414d27904a49a97590`.

Its callback and helper already match the pending fix; no additional parser
change was needed. Added and ran `real_bank_alvaro_exports_idle_and_reuses_cache`
against the exact downloaded SWF: parent holds frame 8, all 120 scheduled frames
export, all 59 unique SVG states rasterize with nontransparent pixels, and the
second export hits the vector cache. This validates the failed source locally,
not a full AWS job replay.

Use `AQW_TEST_BANK_ALVARO_SWF=/tmp/aqw-alvaro-bank.swf` with the existing FFDec
and Java environment to rerun that ignored regression. Saved history is
`/tmp/aqw-bank-third-history.json`; scripts are in `/tmp/aqw-alvaro-bank-export`.
No ARM build or deployment was run.
