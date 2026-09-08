# Standalone source-fetch AWS test — September 7, 2026

Deployed and tested successfully in account `538522204887`, region `us-west-2`.
This was the standalone fetch stack only; the main rendering workflow and bot
were not deployed or invoked.

- Stack: `aqw-char-source-fetch-test-dev`
- Lambda: `aqw-char-dev-source-fetch-test`
- Runtime: Python 3.13 image, ARM64, 1024 MB, 300-second timeout
- Proxy configuration: SSM SecureString `/aqw-char/dev/brightdata`
- Transport: curl_cffi 0.15.0, Chrome 136 profile, per-job sticky proxy session
- Deployment verification: Lambda `Active`, last update `Successful`

Credentials were uploaded from the user-provided local configuration without
printing their contents. The tests invoked Lambda directly and wrote acquisition
artifacts to S3; they did not create render admissions or Discord messages.

## Results

All five invocations returned successfully with no Lambda `FunctionError`.
Times are AWS Lambda REPORT durations, not Mac measurements or complete render
timings. Each row is a single observation, not a latency benchmark.

| Test | Lambda duration | Sources | Result |
| --- | ---: | ---: | --- |
| Queen Iona, first job | 4.643 s + 6.111 s cold initialization | 4 | Three cached sources; downloaded `cp-bg32.swf` through proxy |
| Yoshino | 4.067 s | 6 | Five cached sources; downloaded `cp-bg34.swf` through proxy |
| Repeat original Queen Iona job | 0.193 s | Saved acquisition | Returned exactly the same enriched request |
| Queen Iona, fresh job ID | 3.162 s | 4 | Fresh character acquisition; all four SWFs were S3 hits |
| Fleki, current charpage | 2.973 s | 5 | Four cached sources; downloaded `cp-bg23.swf` through proxy |

The first invocation's billed duration was 10.754 seconds including initialization.
Maximum reported memory usage across these invocations was 207 MB. No inference
about optimal Lambda memory sizing is justified by this small sample.

Fleki was selected because an earlier bank-pet failure belonged to that character,
but the current appearance reports `strPetFile=none`. This test therefore does
not establish bank-pet rendering coverage. No rasterization was performed.

## Binary integrity and cache evidence

Read back the following new objects from S3 and verified their actual bytes,
SWF signatures, S3 SHA-256 metadata, and pinned catalog hashes:

| Source | Bytes | SHA-256 |
| --- | ---: | --- |
| `etc/chardetail/bgs/cp-bg32.swf` | 100,286 | `2ab6ba230ef678d92059e9e21603792a6bcbda77754898d72928baa68320b6c5` |
| `etc/chardetail/bgs/cp-bg34.swf` | 66,295 | `22991199910e5d25f408b09db0aa284668f9b90ff0ffdd9367765be0cb735d98` |

These were real source-cache misses on the first tests. Queen Iona's fresh job
then recorded `cache_hit: true` for the previously missing background. No shared
source assets were deleted to manufacture cache misses.

The repeated original job returned the identical enriched request, confirming
the completed-acquisition reuse path. The deployed transport routes AQW fetching
through the configured proxy; this test did not independently observe the exit IP
or prove its identity remained constant at the provider across every request.

## Job references

| Test | Job ID |
| --- | --- |
| Queen Iona original and repeat | `60e4aefa-7e42-4b01-881a-0d35ac9d03e1` |
| Yoshino | `1bb28a03-b959-4693-ae5c-1fac93de5cbd` |
| Queen Iona fresh | `49d96e48-86a0-4740-9603-7efe72de3f3c` |
| Fleki | `32722128-d327-41ec-b5e7-e5c56b5133f4` |

Work bucket: `aqw-char-rendering-dev-workresultbucket6323ca9d-ango9x59ui8e`.
Each job has `jobs/<job_id>/fetch/appearance.json`, `request.json`, and
`sources.json`. These standalone test IDs are not render jobs in DDB or Step
Functions. Local invocation outputs and timing reports are under
`/tmp/aqw-source-fetch-*`.

The next integration step is deploying the main rendering stack with FetchSources
first, followed by the updated bot. Standalone deployment does not activate that
workflow change.
