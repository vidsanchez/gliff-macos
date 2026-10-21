# Generated from the matching ZIP/build-report.json; do not hand-edit hashes.
# Commit under Casks/ only after this exact release asset is available.
cask "gliff-macos" do
  version "0.1.0-alpha.1"
  sha256 "da4c9ec13f3273575ce01db597ed0d1954e82bb41a1cd58d452294521e2fca61"

  url "https://github.com/vidsanchez/gliff-macos/releases/download/v0.1.0-alpha.1/Gliff-0.1.0-alpha.1-arm64.zip"
  name "Gliff for macOS"
  desc "Experimental Hyprland remote desktop client over SSH"
  homepage "https://github.com/vidsanchez/gliff-macos"

  depends_on arch: :arm64
  depends_on macos: :sequoia

  # Homebrew's dependency DSL takes named major releases. Retain the exact
  # measured minor/patch requirement with a read-only preflight comparison.
  preflight do
    if MacOS.full_version < Version.new("15.0")
      raise "Gliff requires macOS 15.0 or newer (measured bundle minimum)"
    end
  end

  app "Gliff.app"

  caveats <<~EOS
    Experimental app, ad-hoc signed; no Developer ID or Apple notarization.
    Gatekeeper may block opening. Only if you trust the source, follow Apple's
    per-app Open Anyway instructions: https://support.apple.com/102445
    Do not disable Gatekeeper or quarantine globally.
    Clean-machine QA and current limitations: https://github.com/vidsanchez/gliff-macos/blob/main/docs/distribution.md
  EOS
end
