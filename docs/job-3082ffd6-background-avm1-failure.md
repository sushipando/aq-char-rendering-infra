# Bryce charpage failure: 3082ffd6-b6e8-4c15-83a7-b9d4cb7468ba

The job failed in `ExportSourceFrames`, iteration 15, while exporting background
`etc/chardetail/bgs/cp-bg18.swf` (background I / index 18).
`FetchSources` and `PrepareResolve` both succeeded. This is not an AQW 403, proxy,
S3 cache, AVIF, or rasterization failure.

The terminal exception ends with:

```text
sprite 39 (): AVM1 timeline actions are unsupported
```

`services/pipeline-rust/src/timeline.rs`, `Clip::read`, rejects all DoAction (12)
and DoInitAction (59) tags before examining their instructions.

Downloaded the cached wrapped SWF into `/tmp/aqw-debug-3082-bg18.swf` and inspected
its tags and FFDec-decompiled scripts. Its FileAttributes indicate AVM1, not AS3.
The SWF contains three DoAction scripts, all on frame 1:

- Sprites 39 and 44 call `MovieClip(this.stage.getChildAt(0)).mcSetColor(this,"Trim","None")`
  inside a try/catch.
- Sprite 72 sets `isProp=true`, `mouseEnabled=false`, `mouseChildren=false`.
- No playback-control commands appear in those scripts.

Decompiled scripts are under `/tmp/aqw-debug-3082-scripts/scripts/`.

The immediate issue is the resolver's blanket AVM1 rejection. A proper fix needs
narrow support/classification for these callbacks and metadata assignments, with
an explicit decision about the color callback's visible effect. Ignoring all AVM1
actions would be unsafe for rendering correctness because other SWFs can use them
for actual playback or display changes. No renderer code was changed during this
investigation, and nothing was deployed or retried.

Temporary workaround: retry with `presentation.background=false`, or use the
character-only view. Disabling the border alone does not skip the background SWF.
The workflow correctly marked the job FAILED, released its slot, and delivered
the Discord failure notification.

## Follow-up: apparent static background

The wrapped SWF has 43 sprites: 42 have one frame, and sprite 52 has 50 frames.
Exported all 50 frames of sprite 52 using FFDec into `/tmp/aqw-bg18-motion*`.
Its local artwork is small (roughly 28 by 21 units), showing red/orange curved
marks. That establishes a nested authored animation; it does not establish that
it is noticeable or even visible in the live character-page presentation.

The rejection is caused by the three scripts described above, not by detecting
motion. Even a visually static SWF can contain those scripts. Calling the entire
background visibly animated without inspecting its presentation would overstate
what the failure tells us.

## Fix implemented (2026-09-07; not deployed)

The exporter now recognizes the two complete FFDec program patterns above and
binds each approval to the SHA-256 of its original DoAction bytes. Sprite/frame
paths must resolve to exactly one action tag. Cached script metadata retains
these approvals. Only AVM1 background exports may use them; character assets,
AS3 sources, unknown scripts, added playback/display instructions, and
DoInitAction remain subject to rejection. This is deliberately narrow support,
not an AVM1 interpreter or a blanket removal of scripts.

Color policy: preserve this background's authored colors. The guarded callback
uses the AS3 host API from AVM1 content; the exporter does not emulate that host
or recolor this background to the character's Trim color. Interaction setup is
irrelevant to image export. Recognition currently requires the exact known
Trim/None callback or the complete isProp/mouse setup program, apart from lexical
whitespace/comments; other patterns need separate review.

Local regression `real_escaped_scythe_and_avm1_background_export` passes against
the original cached wrapped background. It exports 50 frames / 49 SVG states,
rasterizes every state to nontransparent pixels, and verifies the warm cache.
Normalization leaves the **entire source SWF byte-for-byte unchanged**, including
the 50-frame nested timeline. This validates preservation, not a claim about how
noticeable the motion is on the live charpage. Negative tests verify that unknown
actions and DoInitAction still fail.

Export policy is now `rust-effective-svg-v6-escaped-names-avm1-background`, which
invalidates old export/script metadata cache identities. No deployment, ARM build,
or new AWS render was performed for this fix. Retry the failed job after deployment.
