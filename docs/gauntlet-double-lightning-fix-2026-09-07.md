# Reno's doubled gauntlet lightning

Job: `1416ac18-15b2-4fcc-a68c-ed92cc8a232c`.

Compared the supplied desktop files: `reno_double_lightning` (an AVIF without a filename extension, 2048×1549, 119 physical frames) and `reno_gt.webp` (1080×800, 76 frames). Their animation phases and framing differ, so individual-frame pixel equality is not an appropriate whole-image test.

## Cause

The weapon is **Fury of the Risen**, `items/Gauntlets/FurryofRisen.swf`. Its root contains two placements:

- `FurryofRisen_fla.WeaponBackhand_2`, character 12: on its first frame, hides itself when `MovieClip(parent.parent).name == "fronthand"`.
- `FurryofRisen_fla.WeaponForehand_5`, character 13: hides itself when that holder is `"backhand"`.

The game's AvatarMC inserts a gauntlet instance into each hand. These scripts select one of the two authored lightning shapes per instance. Our exported weapon SVG retained both placements, and component rendering applied the whole SVG to each hand. That produced the two slightly offset lightning shapes. This is a script visibility issue, not an AVIF encoding artifact or a need to remove one entire hand's gauntlet.

## Fix

Recognize the exact registered first-frame hand-holder visibility pattern and carry a class-to-hidden-hand map through parsed source metadata, source manifests and prepared parts. At SVG import, `gauntlet_front` uses the `fronthand` context and `gauntlet_back` uses `backhand`. Remove only matching direct child placements before color transforms and rasterization. Keep both gauntlet instances, their transforms, the surviving clip's animation, and the armor hands.

The implementation does not guess from names such as “Forehand” and does not generalize arbitrary ActionScript conditions. It requires the supported complete first-frame callback; nested placements inside another SWF character are not assigned the outer hand context.

Export policy `rust-effective-svg-v4-hand-visibility` prevents reuse of metadata and final render identities produced without these rules. Hand-dependent parts also bypass the shared no-color-customization raster cache, whose keys do not include script visibility. Existing bounds remain conservative; this change does not promise tighter cropping.

## Validation and owner replay

Local regression checks cover the script recognizer, correct direct-child removal for both hands, preservation of deeper placements, and cache eligibility. An actual-source check reads the downloaded gauntlet's FFDec scripts and saved job SVG, verifies two placements become one for each hand, and renders before/front/back PNGs under `/tmp/aqw-gauntlet-review/`.

After building and deploying the updated **pipeline and component-raster images**:

```bash
scripts/render-character --restart 1416ac18-15b2-4fcc-a68c-ed92cc8a232c --no-render-cache
```

Or use `/retry-render job:1416ac18-15b2-4fcc-a68c-ed92cc8a232c render_cache:false`. Check that each hand retains one lightning pattern and compare several animation frames. No ARM builds, deployments or AWS render replays were performed during this investigation.

The actual-source raster check passed. Visual inspection of its `before.png`, `front.png` and `back.png` confirms that the original double outline separates into the two distinct authored single outlines, one per holder. The 33 component-raster unit tests passed.

The complete standard pipeline suite also passed: 69 unit tests and 8 integration tests. Environment-dependent ignored checks remain opt-in; the actual gauntlet check above was explicitly run.
