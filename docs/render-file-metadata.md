# Embedded render information

The Rust finalizers now embed the same AQW XMP schema the old Python renderer used, extended with `aqw:jobId` and `aqw:renderHash`. Both WebP and AVIF include character name, class, available level/guild, rendered item names, and colors. Item selection follows the resolved appearance, including item overrides, base-item selection, hidden equipment, helm/hair selection, and the ground item (displayed as Rune). Missing level data is left missing rather than guessed as 100.

WebP adds a standard XMP chunk and updates the extended header without decoding or re-encoding image data. AVIF attaches XMP to the original image through libavif and verifies it after final-container parsing. Metadata is limited to 64 KiB. Neither path includes Discord identifiers, credentials, or internal AWS configuration.

The Discord **View Render Info** message command reads either format directly from the delivered file, including attachments, and displays the generating job ID. Older WebP metadata remains readable. The reader parses XML namespaces and element structure instead of relying on whitespace-sensitive regular expressions.

The final cache identity now includes the metadata policy and public character/item details. This prevents old metadata-free results or stale item names, guilds, or levels from satisfying new requests. Upstream vector and raster caches remain reusable. Job ID is deliberately excluded from cache identity: a cache hit keeps the original file and its original generating job ID. `metadata_job_id` in the result identifies that job. Already-delivered old files are not retroactively modified.

Local validation covers actual still/animated WebP and AVIF extraction in the bot, producing both formats through the full Rust raster/compose/finalize pipeline, XML escaping, actual shown-item selection, job IDs, and pixel/alpha/timing preservation. No ARM container build or deployment was performed for this fix.

After owner deployment, render each format with `--no-render-cache`, then use **Apps → View Render Info** on its Discord message. Compare the displayed job ID with the job that generated the file and the listed items with the image. Repeat without bypassing the cache to check that the original generating ID remains intact.

Format references: [WebP XMP container specification](https://developers.google.com/speed/webp/docs/riff_container), [libavif XMP API](https://github.com/AOMediaCodec/libavif/blob/v1.4.2/include/avif/avif.h).

Metadata policy v3 also embeds exact final file size and preparation-through-encoding elapsed time. **View Render Info** displays these in **Render Info**. Timing excludes initial queue time and final upload/delivery; old files show missing values as `N/A`. See [Discord retry and statistics](discord-retry-render.md) for field definitions and deployment checks.

### Animation metadata (2026-09-07)

The `aqw-xmp-v4-animation` policy adds `frameCount` (generated logical timeline
frames, before identical-frame duration merging) and `loopStatus` to both WebP
and AVIF. `View Render Info` displays these as **Frames** and **Complete loop**.

`loopStatus` is `complete`, `truncated`, `unknown`, or `still`. It describes
completion of the renderer's detected animation cycle, not the container's
infinite-repeat flag or the requested `complete_loop` option. Completion requires
an integer number of detected item periods, completion of the blink (which the
renderer plays once and then holds), and full ground/pet ping-pong round trips.
The actual output count after the component frame cap is used. Missing cycle
detection reports Unknown; a single frame reports N/A (still image).

For example, 120 generated frames with an item period of 40 and a blink completed
by frame 100 can be complete; 120 frames with an item period of 50 are truncated.
These labels rely on the existing schedule-based cycle detector, not a visual
seam assessment. Older files show Frames: N/A and Complete loop: Unknown.
The metadata policy changes the final render cache key so newly requested renders
receive the new fields after deployment.
