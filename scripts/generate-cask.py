#!/usr/bin/env python3
"""Generate a candidate cask ONLY from a matching real ZIP/build report.

Output belongs in release artifacts until that exact ZIP is published and the
maintainer commits it under Casks/. Does not tap, install or contact Homebrew.
"""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import zipfile

REPO = "https://github.com/vidsanchez/gliff-macos"
VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?")
# Audited against Homebrew MacOSVersion::SYMBOLS at 8f10429b. A new
# major must be reviewed explicitly rather than emitted as a numeric string.
MACOS_SYMBOLS = {11: "big_sur", 12: "monterey", 13: "ventura", 14: "sonoma",
                 15: "sequoia", 26: "tahoe", 27: "golden_gate"}
MINIMUM = re.compile(r"[1-9][0-9]\.(?:0|[1-9][0-9]?)(?:\.(?:0|[1-9][0-9]?))?")


def render(report):
    version = report.get("version", "")
    minimum = report.get("minimum_macos", "")
    digest = report.get("archive_sha256", "")
    if not isinstance(version, str) or not VERSION.fullmatch(version):
        raise ValueError("Invalid release version/tag")
    if not isinstance(minimum, str) or not MINIMUM.fullmatch(minimum) or int(minimum.split(".")[0]) < 11:
        raise ValueError("Invalid measured minimum macOS version")
    major = int(minimum.split(".")[0])
    if major not in MACOS_SYMBOLS:
        raise ValueError("Unknown Homebrew macOS major; review supported DSL symbol before publishing")
    symbol = MACOS_SYMBOLS[major]
    if report.get("architecture") != "arm64":
        raise ValueError("Only the arm64 app is supported")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest) or len(set(digest)) == 1:
        raise ValueError("Invalid real ZIP SHA-256")
    expected = f"{REPO}/releases/download/v{version}/Gliff-{version}-arm64.zip"
    if report.get("archive_url") != expected or report.get("tag") != "v" + version:
        raise ValueError("Release URL/tag must match immutable version and expected repository")
    return f'''# Generated from the matching ZIP/build-report.json; do not hand-edit hashes.
# Commit under Casks/ only after this exact release asset is available.
cask "gliff-macos" do
  version "{version}"
  sha256 "{digest}"

  url "{expected}"
  name "Gliff for macOS"
  desc "Experimental Hyprland remote desktop client over SSH"
  homepage "{REPO}"

  depends_on arch: :arm64
  depends_on macos: :{symbol}

  # Homebrew's dependency DSL takes named major releases. Retain the exact
  # measured minor/patch requirement with a read-only preflight comparison.
  preflight do
    if MacOS.full_version < Version.new("{minimum}")
      raise "Gliff requires macOS {minimum} or newer (measured bundle minimum)"
    end
  end

  app "Gliff.app"

  caveats <<~EOS
    Experimental app, ad-hoc signed; no Developer ID or Apple notarization.
    Gatekeeper may block opening. Only if you trust the source, follow Apple's
    per-app Open Anyway instructions: https://support.apple.com/102445
    Do not disable Gatekeeper or quarantine globally.
    Clean-machine QA and current limitations: {REPO}/blob/main/docs/distribution.md
  EOS
end
'''


def generate(report_path, archive, output):
    report = json.loads(report_path.read_text())
    text = render(report)
    expected_name = f"Gliff-{report['version']}-arm64.zip"
    if archive.name != expected_name:
        raise ValueError("ZIP filename does not match release version")
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if digest != report["archive_sha256"]:
        raise ValueError("ZIP checksum differs from build report")
    with zipfile.ZipFile(archive) as source:
        info = plistlib.loads(source.read("Gliff.app/Contents/Info.plist"))
        if (info.get("GliffReleaseVersion") != report["version"] or
                info.get("LSMinimumSystemVersion") != report["minimum_macos"] or
                info.get("CFBundleIdentifier") != "io.github.vidsanchez.gliff-macos"):
            raise ValueError("ZIP Info.plist does not match the release report")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        generate(args.report, args.archive, args.output)
    except (ValueError, KeyError, OSError, zipfile.BadZipFile) as error:
        parser.exit(1, f"Cask generation failed closed: {error}\n")


if __name__ == "__main__":
    main()
