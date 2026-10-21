# Experimental distribution

**Status:** 0.1.0-alpha.1 is an **experimental prerelease**, not a stable or
proven-portable release. The downloaded `b7b1417f` bundle passed user checks for
opening, connecting to Linux, video, typing, and copy/paste on Apple Silicon
macOS 27.0.1. The latest test did not record an exact server version or keyboard
layout. Reconnects and opt-in system capture/permissions were not newly tested.
**A clean Mac without Homebrew remains untested.** The bundle's measured minimum
is macOS 15.0, not a claim of human testing on macOS 15. There is no Developer ID
or notarization. CI does not create or publish a release.

[CI run 39587341227](https://github.com/vidsanchez/gliff-macos/actions/runs/39587341227)
built exact source commit `b7b1417fd0e8b2d51b20847a3d5b8e5bf8e8a4a8`:
52 Python tests and 184 Rust tests passed, including four native VideoToolbox
integration tests; one Rust test was ignored. The release tag stays on that
source commit; later documentation/cask commits on main do not retag the app.
Build reports and validation assets remain immutable build-time evidence;
release notes record subsequent user QA and publication status. Their pending
fields must not be edited to relabel later checks.

## Create a candidate

From the root of this independent checkout, with dependencies already installed:

```sh
cargo build --offline --locked --release -p gliff
python3 -m unittest discover -s scripts -p 'test_*.py' -v
# Additional development tools: Meson and Ninja.
python3 scripts/build-cairo.py --plan-source dist/cairo-reference-lock.json
python3 scripts/fetch-sources.py dist/cairo-reference-lock.json --output dist/sources --download
python3 scripts/build-cairo.py --source-dir dist/sources --reference-lock dist/cairo-reference-lock.json --output dist/cairo-build
python3 scripts/package-macos.py --plan-sources --output dist/source-plan --cairo-prefix dist/cairo-build/prefix
# This explicit action uses the network for sources, never installation/execution.
python3 scripts/fetch-sources.py dist/source-plan/source-lock.json --output dist/sources --download
python3 scripts/package-macos.py --output dist/release --source-dir dist/sources --cairo-prefix dist/cairo-build/prefix
cmp dist/source-plan/source-lock.json dist/release/source-lock.json
python3 scripts/fetch-sources.py dist/release/source-lock.json --output dist/sources \
  --archive dist/release/third-party-sources.tar.gz \
  --recipes dist/release/Gliff.app/Contents/Resources/licenses
(cd dist/release && shasum -a 256 -c SHA256SUMS)
```

Build/package output directories must be **new**; a previous app is not deleted
or overwritten. `--brew-prefix` selects another prefix. The packager does not
download: the separate fetch step verifies the lock's SHA-256 before reading
licenses and retains exact source archives. Cached Cargo crates are reused.
It does not extract arbitrary tar/ZIP paths or execute downloaded contents.

## What is checked

- Recursive Mach-O copying and relocation of non-Apple libraries and installed
  GdkPixbuf/GIO/GTK modules. Absolute Homebrew imports and external rpaths are
  forbidden. Missing dependencies, basename collisions, and escaping symlinks fail.
- arm64 only. The largest `minos`/`LC_VERSION_MIN_MACOSX` across **all** Mach-O
  files, including helpers/loaders, determines `LSMinimumSystemVersion` and the
  report. The executable's target can be lower than its libraries' targets.
- Cairo rebuilt from **the installed reference's exact source** with
  `-Dlzo=disabled`, retaining the
  script interpreter and three consistent libraries. The build checks every
  exported symbol against the original installation and records source/options.
  It does not modify Homebrew. LZO and unreviewed formula families are rejected.
- Compiled GSettings, an **original** hicolor fallback index and original icon
  (not the GPL hicolor index or a third-party theme), and fontconfig for system
  fonts. Apple fonts are not redistributed. The original PNG becomes `.icns`
  without redesigning the artwork.
- A **native C Mach-O launcher**, identifier `io.github.vidsanchez.gliff-macos`,
  sets bundle-relative resource paths and executes the client. A bundled helper
  generates loader caches in a private 0700 per-version directory under
  `~/Library/Caches/io.github.vidsanchez.gliff-macos/`. Each launch creates a new
  0600 file, supporting moved apps and spaces without writing the signed bundle.
  Caches contain no clipboard/keystroke data and may be removed with Gliff closed.
  SSH uses the normal macOS PATH to find `/usr/bin/ssh`; no keys are bundled.
- Ad-hoc signing of each binary and the complete bundle, strict verification,
  resource-preserving `ditto` ZIP packaging, SHA-256, and a license inventory.
  Ad-hoc signing is **not** Developer ID, notarization, or a security guarantee.
- Glycin in the closure **rejects** the package rather than guessing its loader
  configuration/sandbox. The inspected local GdkPixbuf formula explicitly
  disables it; dependency upgrades can require another audit.

Cairo is rebuilt because GTK links `libcairo-script-interpreter`, whose local
bottle pulls in GPL LZO. Deleting that dylib or disabling the interpreter would
break GTK; disabling optional LZO preserves its API. Review source/hash/API
explicitly before changing the lock for a different Cairo version.

The installed receipt, keg revision, recipe, and three reference-library hashes
are bound into `cairo-reference-lock.json`. Only audited source versions 1.18.4
and 1.18.6 are accepted; Homebrew revisions such as `1.18.6_1` are distinguished
from the source version. Installed recipe URL/SHA must match the pinned official
release, without evaluating Ruby or consulting a newer formula API. Unknown
versions, patches/resources, ambiguous metadata, or changed reference evidence
fail closed with the actual keg/version in the diagnostic. CI prints this
reference before expensive compilation. The final inventory records that same
source version/hash; it never labels a 1.18.6 build as 1.18.4 source.

An older Cairo build report without this reference binding is not a valid
candidate. Rebuild from the selected verified source; do not retrofit its
metadata by hand or bypass the version/source guards. `--audit-only` only
revalidates a report that already has matching reference evidence.

Official source/hash and optional-LZO evidence:
[Cairo 1.18.4 release](https://cairographics.org/news/cairo-1.18.4/),
[Cairo 1.18.6 release](https://www.cairographics.org/news/cairo-1.18.6/).

Static analysis does not prove arbitrary `dlopen` paths, rendering/fonts,
Finder/TCC, SSH connections, file pickers, or clean-machine behavior. That limit
remains even when all known imports resolve inside the bundle.

## Additional runtime components

The libadwaita 1.9.3 build links AppStream; its library links libfyaml and libxmlb.
Only these source versions and **resolved** library filenames are admitted:

| Source | Library | License evidence |
| --- | --- | --- |
| AppStream 1.2.0 | `libappstream.5.dylib` | [LGPL-2.1-or-later declaration](https://github.com/ximion/appstream/blob/v1.2.0/meson.build), [license](https://github.com/ximion/appstream/blob/v1.2.0/COPYING) |
| libfyaml 0.9.6 | `libfyaml.0.9.6.dylib` | [MIT license](https://github.com/pantoniou/libfyaml/blob/v0.9.6/LICENSE) |
| libxmlb 0.3.29 | `libxmlb.2.dylib` | [LGPL-2.1-or-later source notice](https://github.com/hughsie/libxmlb/blob/0.3.29/src/xb-node.c), [license](https://github.com/hughsie/libxmlb/blob/0.3.29/LICENSE) |

Numeric Homebrew keg revisions such as `1.2.0_2` are distinct from the source
version. Different versions, ABIs, tools, compose/Qt libraries, and other files
from these families require another audit. In particular, libfyaml's
`libfyaml.0.dylib` symlink resolves to its full-version filename; checking just an
install-name alias would not enforce this boundary. This is not a licensing
claim about every component shipped by those projects.

The installed recipe, source/patch URLs and SHA-256 values, actual license texts,
and receipts still enter the corresponding-source and notice gates below.
No AppStream utilities, extra data directories, or networking helpers are added.
Compatible-library replacement and ad-hoc re-signing remain available as described
below; the component allowlist does not waive license obligations.

### WebP and SharpYUV

The complete CI graph also identified `libtiff` → WebP → SharpYUV. Only WebP
source **1.6.0** and resolved `libwebp.7.2.0.dylib` / `libsharpyuv.0.1.2.dylib`
are admitted, including numeric Homebrew keg revisions. Mux/demux libraries,
command-line tools, other ABIs, and other source versions remain rejected.

The installed recipe must also match the audited SHA-256
`c2daf79de310f5aa1a3c8a2e412c6fcec1cc1ff47ba506e4bbddcaffbf7dc90c`.
Any recipe change, even metadata or build-time `inreplace`, requires another
audit. Numeric keg-revision normalization does not waive this recipe pin.
Ruby recipe behavior is not inferred from a token blacklist.

The [official source archive](https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-1.6.0.tar.gz)
is pinned to SHA-256 `e4ab7009bf0629fd11982d4c2aa83964cf244cffba7347ecd39019a9e38c4564`.
Its [COPYING](https://chromium.googlesource.com/webm/libwebp/+/refs/tags/v1.6.0/COPYING)
contains BSD-3-Clause terms; SharpYUV source notices refer to that same license.
The app retains exact `COPYING`, `PATENTS`, `AUTHORS`, and source-file copyright
notices; the corresponding-source asset retains the complete unmodified archive.
[Google's PATENTS grant](https://chromium.googlesource.com/webm/libwebp/+/refs/tags/v1.6.0/PATENTS)
is limited to specified Google-licensable necessary claims, excludes certain
modification-only claims, and has litigation termination conditions. It is not
universal patent clearance, a third-party patent license, or a legal warranty.

## Sources and licenses: a separate gate

`source-lock.json` uses **installed** recipes and their stable URL/SHA pairs,
including resources/patches, not current HEAD formulas. `third-party-sources.tar.gz`
contains verified sources, embedded recipes/patches, receipts, licenses, and the
Cargo inventory. Unpinned HTTPS sources fail the process. CI generates a lock
for **its own** dependencies; it does not reuse another Mac's lock.

Before distribution, review recipe-to-installed-bottle correspondence and notices.
Upload all eight verified assets to the **same release**: the ZIP, `SHA256SUMS`,
`build-report.json`, `relocated-validation.json`, `source-lock.json`,
`third-party-sources.tar.gz`, `icon-validation.json`, and generated `gliff-macos.rb`. The app sources/scripts must match the published
tag exactly. This provides sources alongside binaries, not a written three-year offer.

Normative references: [LGPL 2.1, §§4 and 6](https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html)
and [LGPL 3, §4](https://www.gnu.org/licenses/lgpl-3.0.html). Compatible replacement
libraries can be used; re-sign ad-hoc and validate afterwards. This is a technical
checklist, not a claim of professional legal review.

## Startup icon regression gate

The first candidate passed static checks but failed user startup QA: the original
minimal hicolor index hid GTK's embedded `16x16/status/image-missing.png`.
GTK 4.22.4 retries that missing fallback recursively. The original metadata now
exposes that resource directory; no third-party theme/index/artwork is copied.
The monitor button uses an existing GTK built-in symbol instead of assuming an
external desktop icon theme supplies it. The original application artwork is unchanged.

CI now runs `validate-icons.py` against the **relocated bundled GTK**, in isolated
subprocesses with a forced unavailable display backend, no display/window and
15-second timeouts. It safely rejects the old index before a recursive lookup,
then requires every actual UI icon to resolve by its own name at multiple sizes
and scales; an unknown icon must resolve to `image-missing`. Results are retained
in `icon-validation.json`. This no-window check does not replace Finder/rendering,
connection, or clean-machine QA. The old `0a268eab` candidate was never released
and must never be published; the replacement `b7b1417f` passed the scoped user
checks recorded above.

## Manual gate before publishing the app

- [x] Independent diff review and green CI for the exact release-source commit.
- [ ] Download **that** ZIP, verify its checksum, and extract/move it to a path
      containing spaces on an Apple Silicon Mac without Homebrew (or a clean VM),
      running at least the report's measured minimum. Open from Finder, not cargo/Terminal.
- [ ] Keep Gatekeeper protections. Only if the source is trusted, use Apple's
      [Open Anyway procedure](https://support.apple.com/102445). Do not remove
      quarantine in bulk or disable Gatekeeper.
- [x] User confirmed the downloaded replacement opens, connects to Linux, displays
      video, and supports typing and copy/paste on Apple Silicon macOS 27.0.1.
- [ ] Extend QA to recents, fonts, mouse, reconnects, both clipboard directions,
      and a recorded server/layout matrix. The confirmation above does not
      establish these details. Earlier foot/Omarchy Command+C/V testing was a
      separate development check, not a universal terminal-shortcut guarantee.
- [ ] Without the flag: no prompt/tap. Manually test opt-in capture, identity/
      permissions, and Shift+Esc using [security](security.md), never CI tests.
- [x] Published exact sources/recipes/notices in the same experimental prerelease.
      Anonymous downloads of all eight release assets matched the verified CI
      bytes, sizes, and SHA-256 values; `SHA256SUMS` verified both archives.
- [ ] Record actual macOS, architecture, GTK version, and results. Keep outstanding
      limits in release notes; never mark an untested item as passed.

Outstanding checks above remain open. Publication without clean-machine QA is
limited to an explicitly experimental prerelease with these warnings, not a
stable release or a promise of operation without Homebrew.

## CI

The [workflow](../.github/workflows/macos.yml) uses public `macos-15`, asserts
arm64, grants only `contents: read`, pins actions by commit, and uploads one
artifact containing the app, sources, and checksums. It has no publication tokens
or secrets. [GitHub documents runner architectures](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Runners and Homebrew change: the artifact report is authoritative, not the runner
name or an arbitrary `MACOSX_DEPLOYMENT_TARGET`.

## Inspect a rejected dependency graph

CI runs `scripts/audit-runtime.py` after the exact Cairo rebuild and before
source planning. It reads the same executable/module/helper seeds as production
and follows every resolvable non-Apple Mach-O import, including rejected families.
Unknown components remain rejected: discovery is not redistribution approval.

On failure, the separate `gliff-macos-runtime-diagnostic` artifact retains
`runtime-closure.json` for seven days. It contains resolved paths, parent/import
edges, all policy rejections, traversal errors, and bounded installed recipe and
receipt snapshots with SHA-256. It does not capture the environment, credentials,
or arbitrary user files. `complete: false` means some evidence or graph traversal
failed; even `complete: true` covers only known imports/seeds, not arbitrary
`dlopen` or GUI resource behavior.

For a local report, with a verified Cairo prefix and a release binary present:

```sh
python3 scripts/audit-runtime.py --output dist/closure-diagnostics --cairo-prefix dist/cairo-build/prefix
```

The command writes only a **fresh JSON diagnostic directory**, never an app,
source archive, signature, or approval file. Any rejection/error exits nonzero.
It does not execute inspected binaries, evaluate recipes, install, or download.
The normal package/source-plan gates are unchanged; review the exact reported
source version, recipes, licenses, and components before editing an allowlist.

## Homebrew

This same repository can serve as a tap with an explicit URL. No additional
repository is required, and inclusion in official Homebrew Cask is not claimed.
See the [tap format](https://docs.brew.sh/Taps.html) and [Cask Cookbook](https://docs.brew.sh/Cask-Cookbook).

Install the experimental Apple Silicon app (macOS 15.0+) with:

```sh
brew tap vidsanchez/gliff-macos https://github.com/vidsanchez/gliff-macos
brew install --cask vidsanchez/gliff-macos/gliff-macos
```

### Cask maintenance

```sh
python3 scripts/generate-cask.py dist/release/build-report.json \
  --archive dist/release/Gliff-0.1.0-alpha.1-arm64.zip \
  --output dist/release/gliff-macos.rb
ruby -c dist/release/gliff-macos.rb
```

The generator checks repository/version/tag/URL, arm64, measured minimum, the
actual ZIP SHA, and the bundled Info.plist identity/version/minimum. It rejects
`no_check`, latest URLs, and arbitrary Ruby text. The macOS dependency uses a
supported major-release symbol (`:sequoia`, `:golden_gate`, etc.). A read-only
preflight compares `MacOS.full_version` with the exact measured minor/patch
minimum without rounding down. An unknown major blocks generation until its
DSL symbol is reviewed. Tests evaluate a narrow faithful parser model and reject
the incompatible numeric format. CI uploads only a **candidate** outside `Casks/`,
without tapping, installing, or publishing locally.

Only after the gates pass, **that ZIP** is published, and its remote checksum is
verified may the maintainer add the generated file as `Casks/gliff-macos.rb` and
commit it. Never publish an example hash or a cask with an unavailable URL.
The cask has no `postflight`, `zap`, quarantine bypass, or TCC writes. The initial
cask matches the anonymously verified v0.1.0-alpha.1 ZIP exactly; retain this
verification gate for every future update.
