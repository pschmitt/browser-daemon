# Project specifics for tools/release.sh: the Python client (python/) and the
# Home Assistant integration (custom_components/browser_daemon) are released
# with the daemon, under the same version.

# The default (Cargo.toml and Cargo.lock), kept under another name.
eval "cargo_bump_version() $(declare -f bump_version | tail -n +2)"

bump_version() {
  cargo_bump_version "$1"
  sed -i "0,/^version = \".*\"$/s//version = \"$1\"/" "$(repo_root)/python/pyproject.toml"
  sed -i "s/^  \"version\": \".*\"$/  \"version\": \"$1\"/" \
    "$(repo_root)/custom_components/browser_daemon/manifest.json"
  # The integration needs the client of this very release: pinned ahead of its publication
  # (the release workflow uploads it to PyPI), so Renovate never has to follow.
  sed -i "s/\"browser-daemon-client==[^\"]*\"/\"browser-daemon-client==$1\"/" \
    "$(repo_root)/custom_components/browser_daemon/manifest.json"
}

release_files() {
  printf '%s\n' Cargo.toml Cargo.lock python/pyproject.toml \
    custom_components/browser_daemon/manifest.json
}
