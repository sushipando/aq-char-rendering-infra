# Battleon background linkage metadata

Job `d759622c-50e0-4ea4-a4a5-2b1e8db318c6` failed during background export,
`etc/chardetail/bgs/cp-battleon2.swf`, at sprite 548. Its only AVM1 script is:

```actionscript
var isProp = true;
var strLinkage = "BushZ";
mouseEnabled = false;
mouseChildren = false;
```

The background-only AVM1 classifier now accepts this complete setup pattern with
any literal string for `strLinkage`. It does not execute expressions or allow
additional statements. Existing byte-hash binding, sprite/frame association,
AVM1-only and background-only checks remain in effect. A linkage value does not
change the exported display list in this recognized setup.

Positive/negative unit tests cover varied string literals, dynamic expressions,
concatenation, changed assignments and appended playback instructions. A local
FFDec regression uses the exact S3-cached wrapped SWF (SHA-256
`57a1fe4b69af41a4e1bb66fb1997fa93e9c01a7b1fdbedad2a49524520cac6b7`) to verify
script classification and production timeline normalization, unchanged SWF bytes,
and continued rejection when the same asset is requested as a non-background.

Export policy is `rust-effective-svg-v9-background-linkage`, invalidating earlier
metadata/export cache identities. No source SWFs were changed, no AQW origin
requests were made, and no ARM build or deployment was performed. Retry the job
after deployment to validate the complete render.
