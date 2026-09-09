# Full local SWF corpus: trends

Source: `aqw-swf-sanity-6793ar3x/results.jsonl`, completed September 8, 2026.
55,934 paths yielded 55,925 unique content/policy checks. This report groups
existing results and inspects representative local scripts; no corpus rerun or
renderer modifications were made.

| Result | Unique checks | Share |
|---|---:|---:|
| ok | 54,726 | 97.856% |
| timeline_review | 705 | 1.261% |
| no_exported_roots | 473 | 0.846% |
| failed | 21 | 0.038% |

There are no parser_failed, timeout or process_failed results. The prior female
and male 13ClawSuit and 20thFancyDress files, BoAEnergy and FClericSwordsBack all
pass. This is parsing/timeline coverage, not a measured full-render success rate.
The report does not record an exact Git revision/helper digest, so reproducing
its precise tool version would require additional provenance.

## Dominant diagnostic families

Count each SWF once per family, even when several root probes produce that error.
Families overlap, so their counts must not be summed as distinct failing files.

| Family | SWFs |
|---|---:|
| Conditional / early-exit control | 516 |
| Parameterized helper | 86 |
| Advanced Animate layers, total | 61 |
| Implicit transition into another animation state | 42 |
| Indirect helper | 13 |
| Unknown frame labels | 11 |
| Dynamic target / scene argument | 8 |
| Not a standalone call | 6 |

The advanced-layer total consists of 37 unrecognized companion-script cases and
24 frame-synchronization cases. These are the explicit limits of the bounded
static BoAEnergy support, not evidence that the BoAEnergy fix regressed.

The 705 review files produce 1,765 root diagnostics. Counting log lines would
substantially exaggerate the number of independent problems. Similarly, 118
armor review files correspond to 64 filenames, with 54 represented in both
male and female variants.

## Representative script inspection

These observations are examples, not estimates of how all 516/86 cases divide.

- `classes/F/ArchDoomFiend2023.swf`: a shoulder clip branches on whether an ancestor
  is named `backshoulder` and selects `Arm2`. This needs placement context, not
  simply unconditional removal of the conditional.
- `classes/F/BBrigadeArmorr1.swf`: a portrait-specific early return precedes a
  collection of animation-event registrations and a final stop. Distinguishing
  portrait and body contexts can make this resolvable.
- `classes/F/DarkBloodEviscerater.swf`: its constructor registers a
  `FRAME_CONSTRUCTED` handler, and generated setters use the current frame to
  configure interactive component properties. The parameterized-helper message
  is partly the same registration-versus-execution distinction seen earlier,
  but automatic frame events cannot be treated as inert clicks.
- `items/polearms/AbysallBloodSpear.swf`: generated advanced-layer code includes
  a controller with multiple property layers and additional synchronization
  helpers, outside the one-pair static template.
- `items/Capes/CCGothicGrimskullRune.swf`: the root holds initially and has a later
  `Move` branch, while the reported rejection occurs deeper in its child timeline.
  More precise state/child interpretation is needed; do not globally freeze it.

## Concentration by asset type

Pets account for 214/705 reviews (30.4%), despite being only 3,531/55,925 checks
(6.3%). Their review rate is 6.1%, versus 0.43% for helms (69/16,230), 1.21% for
capes (89/7,353), and 0.85% for armor (118/13,883). Bows have 13/248 reviews (5.2%),
but far fewer absolute cases. Interactive pet/shop/bank behavior and animation
state machines are strong investigation candidates; filenames alone do not
establish the exact cause for every pet.

## Coverage gaps and hard failures

All 21 hard failures are `empty root timeline`, raised by the checker's eager
inspection of every sprite, not just requested/reachable roots. This does not
establish that the file is corrupt or that a real render would visit that sprite.
For example, ArchFMageSword contains zero-frame sprites with placement tags;
they are not simply empty byte arrays. Classify reachability and intended empty
clip behavior before permitting or rejecting them globally. These failures occur
before FFDec script inspection, so they can conceal later issues.

All 473 no-exported-root cases have zero discovered symbols and zero exported
scripts. They are all weapon categories. A sampled Barbedclub1 has shapes,
sprites, and a stage placement, with no SymbolClass tag. This exposes a
stage-based asset coverage gap: the checker currently probes exported symbols,
not the top-level stage. It is not evidence that those SWFs contain no artwork.
The sample does not prove all 473 use exactly the same layout.

## Suggested order

1. Improve report coverage: distinguish unreachable zero-frame sprites, probe
   stage-based assets, identify actual render roots, and record helper revision.
   This makes subsequent compatibility counts more trustworthy.
2. Group and support common conditional behavior using known placement/render
   context, prioritizing high-frequency pet and armor families.
3. Separate listener registration from callback execution for additional events;
   implement automatic frame-event effects explicitly where they matter.
4. Extend advanced-layer support to multiple pairs, then synchronized animation.
   Keep camera/depth/effect correctness constraints explicit.
5. Address state-transition, label and child-playhead edge cases using targeted
   job requests and visual regressions.

The pattern supports investing in a few reusable semantic capabilities rather
than adding filename-specific exceptions. No AQW origin requests were made; the
representative scripts were decompiled from existing local files using FFDec.
