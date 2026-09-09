# Local SWF sanity checker

Existing work was committed before adding this tool: infra `fbe28ba`, companion
bot `1444fa3`. The new tool is `scripts/sanity_check_swfs.py`, backed by the native
Rust example `services/pipeline-rust/examples/swf_sanity.rs`.

Discovery found approximately 56,000 SWFs under `~/Projects/aq`, primarily in:

```text
~/Projects/aq/aq-image-search/bot/assets/swf_item_index/swf_assets
```

Run these from the infra repo. Python 3.11+, Cargo and Java are required for the
full check. The existing FFDec 26.2.1 JAR from our investigation is at the path
below; replace it if that temporary directory has been removed.

Start with 50 files to check your setup:

```bash
python3 scripts/sanity_check_swfs.py \
  ../aq-image-search/bot/assets/swf_item_index/swf_assets \
  --ffdec /tmp/aqw-background-animation/ffdec/ffdec.jar --limit 50 --workers 2
```

Then check everything locally under the AQ projects directory:

```bash
python3 scripts/sanity_check_swfs.py ~/Projects/aq \
  --ffdec /tmp/aqw-background-animation/ffdec/ffdec.jar --workers 2
```

A faster structural pass, without Java/decompilation:

```bash
python3 scripts/sanity_check_swfs.py ~/Projects/aq --structural-only --workers 2
```

The full pass starts FFDec once per unique SWF and can take many hours on this
corpus. Default concurrency is two to limit CPU/memory pressure. `--workers` accepts
1–8. The first invocation builds only the native debug helper; it does not build
ARM images or deploy anything. `--no-build` reuses that helper (rebuild after parser
changes). Existing SWFs and sibling projects are read-only.

Every invocation prints a fresh `/tmp/aqw-swf-sanity-*` output directory. It writes
`results.jsonl` after each result and produces `summary.json` and `REPORT.md` on
completion. Files with identical contents share a check and list all paths;
background and ordinary-asset policies remain separate. An explicit `--output`
must name a new directory, preventing accidental report overwrites. There is no
resume mode; interrupted JSONL remains readable, but restarting starts a new run.
Per-file failures and timeouts do not abort the corpus. A nonzero final exit status
means the report contains failures or cases needing review. A process-group timeout
kills both the helper and FFDec. Default timeout is 120 seconds (FFDec itself 90).

## What the results establish

- `ok`: structural parser passed; in full mode the decompiled scripts and exported
  sprite probes also passed. This is not a successful render or pixel-quality test.
- `parser_failed`: at least one FFDec script failed the production metadata parser;
  the result includes the script-relative path and error.
- `timeline_review`: individual exported sprites failed the production timeline
  resolver. Every exported sprite is probed independently at its idle/ready/default
  frame. A real job may never select some UI/walking symbols, or may select several
  roots together, so these are candidates for investigation, not confirmed job failures.
- `no_exported_roots`: full-mode parsing finished but there were no exported sprite
  roots to probe. That does not establish playback compatibility.
- `failed`, `process_failed`, `timeout`, `unreadable`, `checker_failed`: read the error
  and path in JSONL. Structural limitations (for example unsupported compression)
  and FFDec errors are reported rather than silently skipped.

All nested sprites get structural timeline inspection. Backgrounds named
`cp-bg*.swf` use the production background wrapper and AVM1 approval policy.
Arbitrarily renamed background SWFs cannot be identified from their filename and
are treated as ordinary assets. There is no SVG export, rasterization, full-job
validation or AWS timing comparison. SDK/application SWFs included by the broad
root may have unsupported features that are irrelevant to character assets.

Send back the printed output directory when your run finishes; the per-file JSONL
contains the details needed to group failures.
