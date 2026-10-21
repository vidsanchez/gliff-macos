# Attribution and licenses

Gliff for macOS derives from [Gliff by Kevin McConnell](https://github.com/omacom/gliff),
upstream version 0.4.0. It retains the MIT license, copyright, artwork, and SVG/PNG
icons. The Mac changes and version `0.1.0-alpha.1` belong to this fork; they do
not imply affiliation with or endorsement by the original author. See
[LICENSE](LICENSE) and [NOTICE](NOTICE).

## Redistributed dependencies

The bundle includes `Contents/Resources/licenses/`: available license texts for
**each Homebrew formula actually copied**, versions, source URLs, metadata/receipts,
and the `third-party.json` inventory. Missing license/source evidence blocks
packaging. The licenses of crates in `Cargo.lock` are also copied from the local
Cargo cache; an unrecognized missing license blocks the package rather than
being assumed MIT.

The project's own license does not replace dependency licenses. Per-component
license texts from source archives are included, not only each formula's root
COPYING file: for example, libintl/liblzma have different terms from their
associated utilities. Cairo is rebuilt without LZO to avoid that GPL dependency;
the hicolor fallback is original metadata, not a copy of the upstream theme's
GPL index.

GTK, GLib, libadwaita, and other dependencies include LGPL software, distributed
as replaceable dynamic libraries. Exact versions and sources are recorded in the
inventory, together with source hashes where Homebrew supplies them. Rebuild
using those sources and the included formula recipe, not an arbitrary different
version. Public links do not replace corresponding-source obligations:
**before distribution**, the maintainer must retain/publish the corresponding
source archives and recipes for the redistributed LGPL versions. See the
distribution gate.

You may modify/replace libraries in `Gliff.app/Contents/Frameworks` with
ABI-compatible versions and ad-hoc sign the libraries and bundle again using
`codesign --force --sign -`. The validator checks paths, architecture, and the
minimum OS after changes. This invalidates the original signature/checksum;
do not represent a modified package as the original. Reverse engineering to
debug modifications of LGPL libraries is not prohibited.

## Native decoding

The client uses the VideoToolbox, CoreMedia, and CoreVideo frameworks supplied
with macOS. It does not redistribute or download OpenH264 or another codec module.
Apple selects the decoder; hardware acceleration is not guaranteed. Removing
that codec from the package is **not** a patent license or royalty-coverage
guarantee. Distributors and users must assess applicable obligations separately.

Test bitstreams contain project-owned synthetic images. Their generator and
provenance are documented in `crates/gliff-sw/tests/fixtures/README.md`; they do
not contain source code from the encoder temporarily used to generate them.
