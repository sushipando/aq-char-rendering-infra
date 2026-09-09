# Adobe Animate static advanced-layer support

Job `f11ba87c-815a-4f35-a0e4-8f76121616c8` failed exporting `BoAEnergy.swf`, SHA-256
`5597b4544d0bd72afccb7e8cd7d7c785f81dc7f31aa9bf9de1eebeeb6c5e8b62`.
Its generated frame callback invokes layer processing and registers Event.ADDED;
the earlier click-listener fix does not cover this behavior.

## Implementation

The parser recognizes complete known Adobe Animate generated class bodies,
normalizing class/constructor names, layer names and generated setter names.
This is not keyed to the weapon name, SWF hash, sprite ID or package name.
Recognition templates live in `services/pipeline-rust/assets/animate/`.

Before permitting these scripts, `animate.rs` checks the complete source:

- The stage and every sprite have one frame; all companion classes match the
  known generated family or a trivial MovieClip constructor.
- Property clips have no artwork/actions and bind to their recognized controller.
- Placement depths/names are unique; there are no removal actions, changing
  placements, named cameras or generated mask instances.
- Each supported controller has zero property pairs or one object/property pair.
  Its initializer has zero layer depth, no camera/mask attachment and default
  layer index. These values are verified by full template comparison.
- Target/property placements have identity matrices, no color transforms or
  masking/ratio/clip actions, and identical authored effect bytes. Differing
  filters or blend modes are rejected, rather than silently dropped.

Under those constraints, frame synchronization stays at frame 1. Applying the
property effects reproduces the target's existing filters/blend, and projection
is identity. The hidden property sprite contains no artwork. Event.ADDED's
bookkeeping/deduplication does not change this verified static display list.
We can therefore retain every SWF byte and allow FFDec's normal static export.
The same classification/validation is used by AWS export and the sanity checker.

This is a bounded static compatibility path, **not general advanced-layer or
ActionScript execution**. Animated layers, cameras, depth, multiple property
pairs, other generated runtime versions, custom companion classes and changed
effects continue to fail. Supporting those requires explicit synchronization or
property transfer, not a blanket event-listener exemption. Ordinary assets
without this generated family retain the existing parser behavior.

## Validation

- The original BoAEnergy SWF passes the full local sanity checker.
- A real FFDec export regression passes through `export_source`, script metadata,
  timeline normalization, SVG export, resvg rasterization and warm vector-cache reuse.
- The normalized SWF is byte-for-byte identical to the source, preserving authored
  geometry, glow filters and blend modes. The SVG contains filters and rasterizes
  to visible sword artwork. Preview: `/tmp/aqw-boa-validated.png`.
- Negative tests reject runtime edits, nonzero layer depth, animated timelines,
  property artwork, nonidentity matrices, differing blend effects, duplicate
  instance names and cameras. Renaming generated classes and layer fields still
  recognizes the template.
- 91 Rust unit tests and six pipeline integration tests pass, plus the explicit
  real-asset export regression. This is not a production full-character render or
  a comparison against a live Flash/charpage reference.

Export policy is `rust-effective-svg-v8-static-animate-layers`; deployment will
invalidate old export/script metadata cache identities. No ARM builds, deployment,
full corpus scan, AWS render submission or AQW origin requests were performed.
All source inspection used existing local files.

After deployment, retry `f11ba87c-815a-4f35-a0e4-8f76121616c8`. The diagnosed export
failure is covered; subsequent stages can only be confirmed by that full render.
For another sanity run, omit `--no-build` so the native checker includes this fix.
