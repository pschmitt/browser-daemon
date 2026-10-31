#!/usr/bin/env bash

# The files rbw's browser extension shares with browser-daemon, vendored into
# vendor/rbw/ so the daemon builds without rbw's sources.
FILES=(passkey-shim.js fill-targets.js)
REPO="pschmitt/rbw"

usage() {
  cat <<EOF
Usage: $(basename "$0") [--check] [DIR|REF]

Copy rbw's shared extension files (${FILES[*]}) into vendor/rbw/ and record the
commit and the files' hashes in vendor/rbw/REV.

  DIR   a local rbw checkout (its HEAD is recorded)
  REF   a commit or tag of ${REPO}, fetched with gh
        (default: the commit the rbw input of flake.lock pins)

  --check        change nothing; without DIR or REF, only verify that the files
                 still match the hashes in REV (no network), else that they
                 equal the source
EOF
}

root() {
  git rev-parse --show-toplevel
}

pinned_rev() {
  jq -er '.nodes.rbw.locked.rev' "$(root)/flake.lock"
}

# Prints the file at $2 of the source $1 (a checkout or a ref).
fetch() {
  local source="$1" name="$2"

  if [[ -d "$source" ]]
  then
    cat "${source}/browser/extension/${name}"
    return
  fi

  gh api -H 'Accept: application/vnd.github.raw' \
    "repos/${REPO}/contents/browser/extension/${name}?ref=${source}"
}

source_rev() {
  if [[ -d "$1" ]]
  then
    git -C "$1" rev-parse HEAD
    return
  fi

  gh api "repos/${REPO}/commits/$1" --jq .sha
}

sync_files() {
  local source="$1" check="$2" dest name text status=0

  dest="$(root)/vendor/rbw"
  mkdir -p "$dest" || return 1

  for name in "${FILES[@]}"
  do
    text="$(fetch "$source" "$name")" || return 1

    if [[ -z "$text" ]]
    then
      printf 'Empty %s from %s\n' "$name" "$source" >&2
      return 1
    fi

    if [[ -n "$check" ]]
    then
      if [[ "$text" != "$(<"${dest}/${name}")" ]]
      then
        printf 'vendor/rbw/%s differs from %s\n' "$name" "$source" >&2
        status=1
      fi
      continue
    fi

    printf '%s\n' "$text" > "${dest}/${name}" || return 1
  done

  if [[ -z "$check" ]]
  then
    {
      source_rev "$source"
      (cd "$dest" && sha256sum "${FILES[@]}")
    } > "${dest}/REV" || return 1
  fi

  return "$status"
}

# Whether the vendored files are the ones REV recorded.
verify_hashes() {
  local dest
  dest="$(root)/vendor/rbw"

  (
    cd "$dest" || exit 1
    sha256sum --check --quiet --strict <(tail -n +2 REV)
  ) || return 1
}

main() {
  local check="" source=""

  while [[ -n "${1:-}" ]]
  do
    case "$1" in
      -h|--help)
        usage
        return 0
        ;;
      --check)
        check=1
        shift
        ;;
      *)
        source="$1"
        shift
        ;;
    esac
  done

  if [[ -z "$source" ]]
  then
    if [[ -n "$check" ]]
    then
      verify_hashes
      return
    fi

    source="$(pinned_rev)" || return 2
  fi

  sync_files "$source" "$check"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
