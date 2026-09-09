# Fixes from the first 50-SWF sanity sample

The original sample in `aqw-swf-sanity-2zi11wfp` reported 48 passes, one timeline
review (`13ClawSuit.swf`), and one size mismatch (`20thFancyDress.swf`).

## Click-listener callbacks

Seven nested buttons in `13ClawSuit.swf` register
`this.btnButton.addEventListener(MouseEvent.CLICK,this.onButtonPress,false,0,true)`
and then call `stop()`. The parser incorrectly treated the reference to
`onButtonPress` as immediate helper execution. These buttons were reachable from
chest, hand and shoulder roots; the 14 diagnostics were overlapping paths to the
same pattern.

The parser now recognizes literal child click-listener registration, leaving the
callback unexecuted and retaining `stop()`. This does not approve arbitrary event
handlers, frame/timer events, computed callback expressions, or direct calls to
the handler. Regression tests verify those distinctions.

## Missing final End tag

`20thFancyDress.swf` declares 22,302 uncompressed bytes, but its complete zlib
stream produces 22,300. All top-level tags are complete and end on ShowFrame;
the final two-byte End tag is missing.

Compatibility repair is allowed only for an exact two-byte deficit, with a
complete compressed stream (for CWS), no existing top-level End tag, a complete
final ShowFrame, and matching declared root frame count. The missing End is
appended in memory. FFDec receives a repaired FWS copy, and timeline normalization
retains that repair even when no sprites require rewriting. Local source files
and S3 source objects/hashes are unchanged. Other mismatches, truncated tags,
incomplete compressed streams, and incorrect frame counts remain errors.

The export policy is now `rust-effective-svg-v7-click-listeners-swf-end` to prevent
reuse of metadata/export cache identities from before these fixes.

## Validation

- 87 native Rust unit tests passed, including new positive/negative regressions.
- Rechecked only the two affected original assets with FFDec 26.2.1 and the native
  sanity checker: both `ok`, zero parser or timeline errors.
- `13ClawSuit.swf`: 35 scripts and 35 exported sprite roots checked.
- `20thFancyDress.swf`: 32 scripts and 32 exported sprite roots checked.
- Targeted report: `/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-zvpfbmjf/REPORT.md`.

No full corpus scan, ARM build, AWS deployment, or production render was run.
These checks establish parser/timeline compatibility, not pixel-level correctness.
Rebuild the sanity helper before the next corpus scan (omit `--no-build`).
