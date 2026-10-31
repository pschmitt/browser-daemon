# Project specifics for tools/release.sh: the Python client (python/) is
# released with the daemon, under the same version.

# The default (Cargo.toml and Cargo.lock), kept under another name.
eval "cargo_bump_version() $(declare -f bump_version | tail -n +2)"

bump_version() {
  cargo_bump_version "$1"
  sed -i "0,/^version = \".*\"$/s//version = \"$1\"/" "$(repo_root)/python/pyproject.toml"
}

release_files() {
  printf '%s\n' Cargo.toml Cargo.lock python/pyproject.toml
}
