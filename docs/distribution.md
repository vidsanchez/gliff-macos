# Experimental distribution

**Status:** the pipeline is prepared; QA of the downloaded bundle on a Mac
without Homebrew remains pending. Synthetic tests do not establish proven
portability. There is no Developer ID or notarization. CI does not create or
publish a release.

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

## Sources and licenses: a separate gate

`source-lock.json` uses **installed** recipes and their stable URL/SHA pairs,
including resources/patches, not current HEAD formulas. `third-party-sources.tar.gz`
contains verified sources, embedded recipes/patches, receipts, licenses, and the
Cargo inventory. Unpinned HTTPS sources fail the process. CI generates a lock
for **its own** dependencies; it does not reuse another Mac's lock.

Before distribution, review recipe-to-installed-bottle correspondence and notices.
Upload the ZIP, `SHA256SUMS`, report, lock, and complete corresponding-source
archive to the **same release**. The app sources/scripts must match the published
tag exactly. This provides sources alongside binaries, not a written three-year offer.

Normative references: [LGPL 2.1, §§4 and 6](https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html)
and [LGPL 3, §4](https://www.gnu.org/licenses/lgpl-3.0.html). Compatible replacement
libraries can be used; re-sign ad-hoc and validate afterwards. This is a technical
checklist, not a claim of professional legal review.

## Manual gate before publishing the app

- [ ] Independent diff review and green CI for the exact commit/tag.
- [ ] Download **that** ZIP, verify its checksum, and extract/move it to a path
      containing spaces on an Apple Silicon Mac without Homebrew (or a clean VM),
      running at least the report's measured minimum. Open from Finder, not cargo/Terminal.
- [ ] Keep Gatekeeper protections. Only if the source is trusted, use Apple's
      [Open Anyway procedure](https://support.apple.com/102445). Do not remove
      quarantine in bulk or disable Gatekeeper.
- [ ] Check the window/address/recents, icons/fonts, preconfigured SSH, video/mouse,
      reconnects, keyboard, and synthetic-text clipboard in both directions.
      Command+C/V in foot/Omarchy was confirmed in development, not in this ZIP.
- [ ] Without the flag: no prompt/tap. Manually test opt-in capture, identity/
      permissions, and Shift+Esc using [security](security.md), never CI tests.
- [ ] Make exact sources/recipes/notices available in the same release; verify
      SHA256SUMS for both the app and `third-party-sources.tar.gz`.
- [ ] Record actual macOS, architecture, GTK version, and results. Keep outstanding
      limits in release notes; never mark an untested item as passed.

Without clean-machine QA, describe only an experimental candidate with explicit
warnings; do not advertise proven operation without Homebrew.

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
The cask has no `postflight`, `zap`, quarantine bypass, or TCC writes. Until these
steps are complete, the README's installation commands are future instructions,
not a promise of current availability.
