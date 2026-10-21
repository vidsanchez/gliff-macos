# Owned synthetic AVC fixtures

These test images are authored for this project (MIT); no screen, camera,
clipboard, server or other user data was captured. The `.h264` files contain
encoded images, not encoder source or a redistributed codec module.

`generate.rs` was run in an isolated temporary Cargo project using the cached
`openh264 = { version = "=0.9.9", features = ["source"] }` on Apple Silicon.
Its generated Cargo.lock and codec build stay outside this repository; neither
OpenH264 nor its sys crate is a regular/dev/build dependency of the client.
For reproduction, put the generator in a temporary project's `src/main.rs`,
add that dependency to its Cargo.toml, and run `cargo run --offline -- <output>`.
This is an explicit maintainer-only regeneration step, not part of builds,
installation, CI or normal tests; it requires an existing cached encoder.

Configuration follows the original Gliff CPU encoder: H.264 Baseline,
ScreenContentRealTime, CBR 1 Mbit/s, 60 fps, QP12–35, BT.709 limited-range VUI,
no frame skipping. One encoder thread and 512-byte slice limit make small
fixtures. ScreenContentRealTime forces scene-change detection back on; the
64×48 and 34×18 sequences therefore change only the center quarter so frames
1/2 are genuine P slices without SPS/PPS. Small 16×16 and patterned 128×96
streams may emit IDRs; the latter has six slices in its first access unit.

The base image is red (YUV 63,102,240); center patches change to blue
(32,240,118), then green (173,42,26). 128×96 is an 8-pixel color checkerboard.
The native test compares interior BGRA against the portable BT.709 reference
within 12 levels/channel, allowing H.264 loss and Apple's conversion rounding.
34×18 exercises right/bottom SPS cropping from padded macroblocks; 16×16 is
one complete macroblock. 80×64 exercises a changed StreamConfig.

SHA-256 manifests below identify the bytes used by native/parser tests.

```text
ccc72405350d1b47194f6f49c23d8bfe594bd41deb3ed41355b2498440039a8b  128x96-0.h264
4761aa3b1ca163f29de0fc8161a92b59393fa4484c52f2cbfa69837eb57da571  128x96-1.h264
666c68b3378be1d4fae6a90e46ea99c37ca3cf019837bef9818d8c34207ae8f4  128x96-2.h264
46af209fa2a3369497ef20df43f655fa24853c69435887b2e16659a3884de95b  16x16-0.h264
795170894395c92a84b286ad4cd6a31a8ad90449a992aa082c0f8410cfecc562  16x16-1.h264
56b262c3697e2d48faf25ca684207f84affe7f3172928a2da2fd71309e0b279b  16x16-2.h264
29525f586fd93ef10b60c7ef4f76e5be5678931a7a7cf3462aa9aadc70e55df3  34x18-0.h264
c291124eb071378afc51f069a8f084a35686b05c4d1c5b4204a2b3fe30e556d5  34x18-1.h264
4f61ebd308ea5bc30484757f3e7862ac81aa2443d8c749fb67875981de1f5139  34x18-2.h264
0067b5df7331390c6403bd4b334b134d22c51978a16ea21a9c137802071f3742  64x48-0.h264
adab46e3df389776fef5180ea7bb6e86bfe641a739d1f1d64a11e4bf207c479d  64x48-1.h264
00ecbca2a260d4f74a157639f17617360f382a6a0f0b9302080a8bda2afb2bed  64x48-2.h264
f6658b236f346948916caa10301607a641328d8f1f58eedc82cb7dcfe658e422  80x64-0.h264
ed903e6179ccdecc4bfe74f494fd62de35ed9aa77c061915e6511c52d976ffc7  80x64-1.h264
55721ccceb29410aeaeb175718cfc46814acc5eeadc16cd80267e488418ee0fa  80x64-2.h264
```
