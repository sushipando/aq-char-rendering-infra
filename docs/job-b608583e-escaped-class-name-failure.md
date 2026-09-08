# Pei render failure: b608583e-48a3-4a3e-b776-3d3c7ef0e4b1

FetchSources and PrepareResolve succeeded. ExportSourceFrames iteration 14 failed
on weapon `items/polearms/013BlackSkullsScythe.swf`, character ID 14, with:

```text
decompiled script .../§013BlackSkullsScythe§.as: missing class name
```

Downloaded the cached source and reproduced FFDec's decompilation locally. Its
class and constructor are written as `§013BlackSkullsScythe§`, because the original
SWF class name starts with digits and needs escaping in FFDec's source notation.
The SWF export request correctly identifies the class as `013BlackSkullsScythe`.

In `services/pipeline-rust/src/script.rs`, `lex` treats `§` as punctuation.
`parse` requires a Word token immediately after `class`, encounters the opening
`§`, and reports that the class name is missing. This is a parser compatibility
bug, not a missing class in the source SWF.

The appropriate fix is to tokenize FFDec escaped identifiers into identifier
words while preserving their underlying names, consistently for declarations,
constructors and references. It should not be a special case for this weapon.
This differs from Bryce's unsupported AVM1 background scripts.

Investigation artifacts: `/tmp/aqw-debug-b608-history.json`,
`/tmp/aqw-debug-b608-scythe.swf`, and `/tmp/aqw-debug-b608-scripts/`.
No code changes, deployments, or retries were made during this investigation.

## Fix implemented (2026-09-07; not deployed)

The ActionScript lexer now recognizes FFDec's `§identifier§` notation, preserving
the underlying name for class, constructor, package and callback lookup. Quoted
identifiers are distinguished from declaration keywords before normalization.
This handles names with leading digits, spaces, punctuation and Unicode without
special-casing the scythe or relaxing reachable script checks. Malformed names,
unsupported escape sequences and `§§` pseudoinstructions remain errors. FFDec's
[identifier notation documentation](https://github.com/jindrapetrik/jpexs-decompiler/wiki/FAQ)
distinguishes quoted names from those pseudoinstructions.

Local regression `real_escaped_scythe_and_avm1_background_export` passes against
`013BlackSkullsScythe.swf`: script metadata contains the correct underlying class,
the single-frame SVG exports and rasterizes nontransparent, and a repeat uses the
vector cache. Unit tests cover constructor/callback resolution for several kinds
of quoted names, keyword-like package names, and malformed input.

The shared export policy bumped to
`rust-effective-svg-v6-escaped-names-avm1-background`. No ARM build or deployment
was performed. Retry the failed job after deployment.
