# Agent notes

## Releases

Before every release, try to land the open dependency PRs (Renovate,
Dependabot): `gh pr list`, merge those whose checks are green (Renovate
rebases the ones that conflict; wait for it), and let CI finish on the
resulting `main` before tagging. Leave out PRs with failing checks or major
bumps that need real work, and say which.

Release with `just release-prepare X.Y.Z`, then `just release-tag` (see
`tools/release.sh`); the tag must be GPG-signed, so unlock GPG first. The
release workflow signs the browser extension, publishes it and then commits
its hashes to `browser/packages/metadata.json` on `main`. Any extension source
change needs a new version, because AMO signs each version only once.

## Code shared with rbw

rbw's browser extension and this repository share two self-contained files,
`passkey-shim.js` (the fake WebAuthn credential and the platform-authenticator
answers) and `fill-targets.js` (which input is the login's username, password
and one-time-code field). They live in rbw (`browser/extension/`) and are
vendored in `vendor/rbw/`; do not edit the copies. After a change in rbw, run
`tools/sync-rbw-shared.sh ../rbw.git` (or `tools/sync-rbw-shared.sh REF`),
which also records the rbw commit and the hashes in `vendor/rbw/REV`; CI checks
the hashes. The daemon puts the shim into its Chromium hook (`agent_passkeys.rs`),
the live view (`handoff.html`) and the node extension, and the targets into
both vault fills (`autofill.rs`, `fillLogin` in `background.js`).
