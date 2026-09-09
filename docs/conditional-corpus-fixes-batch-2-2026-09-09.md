# Conditional-control corpus fixes: bank capes and child idle

Historical batch notes. See [generalized animation support](generalized-swf-animation-support-2026-09-09.md) for the later implementation, current limitations, and validation.

This continues the first conditional-error batch. The baseline before both
conditional batches is commit `d41322c`; batches 1 and 2 remain uncommitted.

## Behavior supported

- The known bank-cape initializer accesses the avatar through three parent levels,
  registers the bank click listener, and stops its own timeline. The complete
  helper and frame body are validated, including the trailing flag reset and
  host-reference assignment. The handler is registered, not executed by export.
- Collection chests and related pets can first explicitly command a named child
  to its literal Idle label, then initialize the bank/shop button and stop the
  parent. Both commands are retained in order: child goto, parent stop. Child
  names are not fixed to CCPet, and play versus stop is preserved.
- Getter/setter shadows, modified initialization helpers, dynamic targets,
  additional controls and unrelated rendering effects remain unsupported.

The timeline resolver retains responsibility for finding the named child,
resolving its actual Idle label, and rejecting missing/ambiguous instances or
unsupported controls. This change does not silently discard child playhead work,
activate shop actions or assume the child should be frozen with its parent.

Export policy: `rust-effective-svg-v11-bank-cape-child-idle`.

## Validation

Selected all 23 timeline-review files in the previous full report whose paths
contain bank, collectionchest or petshop. All 23 now pass the production-parser
sanity checks: 11 capes and 12 pets. This is a targeted subset, not a new full
corpus count. Some can also benefit from first-batch fixes.

Report:
`/var/folders/8s/5w7xsvq16593g_bz_6dfvzg80000gn/T/aqw-swf-sanity-bz8pw7hr/REPORT.md`.

Examples include ALCThroneBankCape, APLichKingMoglinBank, GriefBankCaper1,
16Birthday10kCollectionChest, BF202510kCollectionChest, AlvaroALPetShop,
DmnkNightmareOrbBank, HBMakaiPetBANK and Nulgath202610kCollectionChest.

99 Rust unit tests and six pipeline integration tests pass. New tests check
strict helper validation, preservation of child commands, rejection of getters
and dynamic targets, correct selection of idle artwork instead of walking, and
preservation of animated descendants.

A real-source FFDec/SVG/resvg/cache regression additionally verifies:

- ALCThroneBankCape: root holds frame 1; 12 exported frames produce 12 different
  raster states through nested effects.
- 16Birthday10kCollectionChest: parent holds frame 8, CCPet child settles on its
  authored idle frame 16; 12 exported frames still produce 12 raster states.
- Repeated exports reuse the vector cache for both assets.

All input SWFs were local and left unchanged. No AQW origin requests, full corpus
rerun, ARM builds, deployment or AWS render submissions were performed. Local
checks do not establish production whole-character render success.

## Next families

Generated component setters and automatic frame events remain promising targets,
followed by placement-dependent shoulder/hand/portrait branching. The remaining
AstravianWelkinHatLocks case from the earlier 16-file sample needs broader Animate
layer support rather than bank initialization handling.
