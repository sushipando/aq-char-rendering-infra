# Embedded render information

The Rust finalizers now embed the same AQW XMP schema the old Python renderer used, extended with `aqw:jobId` and `aqw:renderHash`. Both WebP and AVIF include character name, class, available level/guild, rendered item names, and colors. Item selection follows the resolved appearance, including item overrides, base-item selection, hidden equipment, helm/hair selection, and the ground item (displayed as Rune). Missing level data is left missing rather than guessed as 100.

WebP adds a standard XMP chunk and updates the extended header without decoding or re-encoding image data. AVIF attaches XMP to the original image through libavif and verifies it after final-container parsing. Metadata is limited to 64 KiB. Neither path includes Discord identifiers, credentials, or internal AWS configuration.

The Discord **View Render Info** message command reads either format directly from the delivered file, including attachments, and displays the generating job ID. Older WebP metadata remains readable. The reader parses XML namespaces and element structure instead of relying on whitespace-sensitive regular expressions.

The final cache identity now includes the metadata policy and public character/item details. This prevents old metadata-free results or stale item names, guilds, or levels from satisfying new requests. Upstream vector and raster caches remain reusable. Job ID is deliberately excluded from cache identity: a cache hit keeps the original file and its original generating job ID. `metadata_job_id` in the result identifies that job. Already-delivered old files are not retroactively modified.

Local validation covers actual still/animated WebP and AVIF extraction in the bot, producing both formats through the full Rust raster/compose/finalize pipeline, XML escaping, actual shown-item selection, job IDs, and pixel/alpha/timing preservation. No ARM container build or deployment was performed for this fix.

After owner deployment, render each format with `--no-render-cache`, then use **Apps → View Render Info** on its Discord message. Compare the displayed job ID with the job that generated the file and the listed items with the image. Repeat without bypassing the cache to check that the original generating ID remains intact.

Format references: [WebP XMP container specification](https://developers.google.com/speed/webp/docs/riff_container), [libavif XMP API](https://github.com/AOMediaCodec/libavif/blob/v1.4.2/include/avif/avif.h).
