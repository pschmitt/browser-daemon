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
