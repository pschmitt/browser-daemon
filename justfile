set shell := ["bash", "-cu"]

default:
  @just --list

build:
  cargo build --release

check:
  cargo check
  cargo clippy --all-targets -- -D warnings
  cargo fmt --all --check

fmt:
  cargo fmt --all
  nixfmt flake.nix nix/*.nix

test:
  cargo test

# Packages are generated from the same source/version, as in rbw's extension.
browser-build:
  python3 browser/build.py

browser-check:
  node --check browser/extension/background.js
  node --check browser/extension/popup.js
  node --check assets/extension.js
  node --test browser/check.mjs
  python3 browser/check_companion.py

browser-smoke:
  node browser/smoke_chromium.mjs
  python3 browser/smoke_firefox.py
  python3 browser/smoke_firefox_roundtrip.py
  python3 browser/smoke_firefox_roundtrip.py --dom-only
  node browser/smoke_agent_passkey.mjs

# Bump to VERSION (X.Y.Z), date the changelog, commit and push.
release-prepare version:
  tools/release.sh prepare '{{ version }}'

# Push a GPG-signed vVERSION tag once build and test are green for HEAD;
# the release workflow publishes binaries and the signed browser extension.
release-tag:
  tools/release.sh tag
