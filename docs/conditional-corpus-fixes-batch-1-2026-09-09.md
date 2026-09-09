# Conditional-control corpus fixes: first batch

Historical batch notes. See [generalized animation support](generalized-swf-animation-support-2026-09-09.md) for the later implementation, current limitations, and validation.

Committed all preceding work as `d41322c` before starting this batch. Unrelated
local logs were excluded. The full scan had 516 files with conditional/early-exit
errors, the largest diagnostic family. This batch addresses repeated semantics,
not asset filenames.

## Supported patterns

1. **Once-only random start for a looping effect.** A complete generated class
   with an undefined flag, a sole frame-1 callback, and guarded
   `gotoAndPlay(Math.ceil(Math.random()*totalFrames))` now exports from phase 1
   and keeps playing. Recognition checks the entire class and rejects initialized
   flags, extra methods/statements, reset flags, changed random formulas and
   shadowed built-ins. Names of the class, flag and callback may vary.

   This is a deterministic export policy, not random-number emulation. Multiple
   copies of an effect can start in sync; preserving distinct randomized phases
   would require per-instance timeline handling. Geometry and authored frames are
   preserved; this does not convert the effect into a still image.

2. **Optional game attack registration followed by idle stop.** The exact known
   frame setup that obtains the game host, conditionally registers the weapon via
   `registerAttackFrame(this)`, then stops now resolves to that final stop. This is
   the renderer's host-free idle policy, not general handling of host callbacks.
   Other methods, changed controls, appended statements and shadowed accessors
   remain unsupported. Later conditional attack frames are not blanket-approved.

3. **Empty pet initialization helper.** The known guarded `initPet()` setup is
   also accepted when the helper is completely empty. The final stop is retained.
   Nonempty helpers with rendering effects remain blocked unless separately
   recognized by the existing bank-pet policy.

The export policy is `rust-effective-svg-v10-loop-phase-idle-weapon`, invalidating
older script metadata and vector-export cache identities.

## Validation and impact

Selected the first four conditional-review files from each of capes, helms,
pets and swords in the existing report: 16 previously flagged SWFs. This is a
targeted diagnostic sample, not a random statistical sample.

After the fixes, **12 pass and four remain timeline reviews**. Report:
`/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-arbo4y2e/REPORT.md`.

New passes:

- 2016DarkCasterCaper1, ALCThroneCape
- 2016DCBlackSpiritHead, BlackVCFlamingHeadr1, DageLegionWarbringerH5
- 2016DCFlamingHeadPet, 2016DCTransformingPet, 8bitMemetPet
- AQHeartsCrusherSword, ApocrphyalGreatsword, ApocryphyalAltSword, ApocryphyalKatana

Remaining sample reviews:

- ALCThroneBankCape, APLichKingMoglinBank
- AstravianWelkinHatLocks
- 16Birthday10kCollectionChest

The old full scan has 86 conditional-failure files whose paths mention the
BasicFireBC family. That is a useful next validation set, **not a claim that all
86 now pass**. Other scripts in the same asset can still cause rejection.

96 Rust unit tests and six pipeline integration tests passed. The additional
real-source export regression uses FFDec, source metadata, timeline normalization,
SVG export, resvg and cache reuse:

- 2016DCBlackSpiritHead: 12 exported frames produced 12 distinct raster states;
  the flame clip is not normalized to a hold.
- ApocrphyalGreatsword: its Weapon_2 controller holds frame 1; its visible SVG
  rasterizes correctly and a repeated export hits the vector cache.

No full corpus rerun, ARM build, deployment or AQW origin requests were performed.
Local source SWFs were not modified. This does not establish production whole-job
success or exact agreement with a live Flash reference.

## Next work

Continue with repeated guarded bank/cape initialization and generated interactive
component setters, then placement-dependent shoulder/hand/portrait conditions.
Those require their own semantics and tests; do not remove all conditionals.
