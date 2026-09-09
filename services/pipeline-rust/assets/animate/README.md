# Recognition templates for Adobe Animate advanced layers

These are FFDec 26.2.1 decompilations of generated code from the locally cached
BoAEnergy SWF, used as **recognition templates**, not executed or embedded in
output images. `script.rs` compares complete class bodies as lexical tokens,
ignoring package/import/Embed wrappers and normalizing generated class,
constructor, layer-field and setter names. Templates are parsed once per process.

Recognition alone never authorizes ignoring scripts. `animate.rs` verifies the
source display lists before replacing the matched script control metadata with
inert programs. Any template changes require reviewing the runtime semantics,
updating the placement proof/tests, and bumping EXPORT_POLICY.

The current templates cover the generated property runtime, a controller without
layer properties, and a controller with one neutral object/property pair. Extra
layer pairs, other runtime versions, custom companion scripts, cameras, nonzero
layer depth, timeline synchronization, or differing effects remain unsupported.
