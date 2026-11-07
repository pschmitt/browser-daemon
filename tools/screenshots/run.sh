#!/usr/bin/env bash
# README screenshots of the dashboard, from a throwaway demo mesh: two nodes
# (alpha, bravo) with made-up profiles and sessions on public pages only, in
# their own temporary directories, so none of your own data can end up in a
# picture. Light and dark, at 2x.
#
# Needs a browser-daemon binary, Chromium, Xvfb, Node.js and npm (network, for
# playwright-core and the public pages). For example, from the repository root
# on a build host:
#
#   cargo build
#   nix shell nixpkgs#chromium nixpkgs#xvfb nixpkgs#nodejs nixpkgs#curl nixpkgs#coreutils \
#     -c tools/screenshots/run.sh
#
# Writes dashboard.png, dashboard-profiles.png (desktop), dashboard-phone.png,
# dashboard-phone-sheet.png and live-phone.png (phone, 390x844 at 2x), each
# with a -dark twin, to the output directory (default: the repository root). Ports, the binary and the browser can be set with the variables below.

usage() {
  cat <<EOF
Usage: $(basename "$0") [-o OUTDIR]

Environment:
  BROWSER_DAEMON  the binary (default: target/debug/browser-daemon, else PATH)
  CHROMIUM        the browser (default: chromium)
  PORT            alpha's port; bravo takes PORT+1 (default: 13101)
  KEEP            set to leave the nodes and the temporary directory in place
EOF
}

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
here="$root/tools/screenshots"
tmp=""
pids=()

cleanup() {
  local pid attempt=0
  for pid in "${pids[@]}"
  do
    kill "$pid" 2>/dev/null
  done
  wait "${pids[@]}" 2>/dev/null
  if [[ -n "${KEEP:-}" || -z "$tmp" ]]
  then
    return 0
  fi
  # The browsers may still be writing their profiles for a moment.
  while [[ -e "$tmp" && "$attempt" -lt 10 ]]
  do
    rm -rf "$tmp" 2>/dev/null
    attempt=$((attempt + 1))
    sleep 1
  done
}

# bd COMMAND ARGS...: the client against alpha, with the demo's secret.
bd() {
  "$BROWSER_DAEMON" "$1" "${@:2}" --url "http://127.0.0.1:$PORT" --secret-file "$tmp/secret" 2>&1
}

start_node() {
  local name="$1" port="$2" peer="$3"
  mkdir -p "$tmp/$name/profiles" "$tmp/$name/recordings"
  "$BROWSER_DAEMON" run \
    --bind "127.0.0.1:$port" \
    --node-name "$name" \
    --peer "http://127.0.0.1:$peer" \
    --cluster-secret-file "$tmp/secret" \
    --profile-dir "$tmp/$name/profiles" \
    --recordings-dir "$tmp/$name/recordings" \
    --proxies-file "$tmp/proxies.json" \
    --chromium "$CHROMIUM" \
    --display "$DISPLAY" \
    --max-sessions 12 \
    --record-audio false \
    --adblock-default false \
    > "$tmp/$name.log" 2>&1 &
  pids+=("$!")
}

wait_for_mesh() {
  local attempt=0
  while [[ "$attempt" -lt 60 ]]
  do
    if [[ "$(bd node list --json | grep -c '"name"')" -ge 2 ]]
    then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  printf 'The demo nodes did not come up; see %s/*.log\n' "$tmp" >&2
  return 1
}

# seed: made-up profiles and sessions, public pages only.
seed() {
  bd profile new "Work (alice@example.com)" --node alpha --proxy office \
    --tab https://www.python.org/ > /dev/null
  bd profile new "Personal" --node alpha --proxy vpn-exit \
    --tab https://www.openstreetmap.org/ > /dev/null
  bd profile new "Research" --node bravo > /dev/null
  bd profile new "Shopping" --node bravo \
    --tab https://en.wikipedia.org/wiki/Online_shopping > /dev/null

  bd session new --node alpha --name keen-heron --no-default-tabs > /dev/null
  bd session open keen-heron https://www.openstreetmap.org/ > /dev/null
  bd session new --node bravo --name release-notes --no-default-tabs > /dev/null
  bd session open release-notes https://www.rust-lang.org/ > /dev/null
  bd session new --node alpha --name spry-quokka --profile "Work (alice@example.com)" > /dev/null

  # Two short sessions that end, so some recordings are finished.
  bd session new --node bravo --name checkout-flow --profile Shopping > /dev/null
  bd session new --node alpha --name docs-crawl --no-default-tabs > /dev/null
  bd session open docs-crawl https://www.rust-lang.org/learn > /dev/null
  sleep 12
  bd session delete checkout-flow > /dev/null
  bd session delete docs-crawl > /dev/null
}

main() {
  local outdir="$root"
  while [[ -n "${1:-}" ]]
  do
    case "$1" in
      -h|--help)
        usage
        return 0
        ;;
      -o|--out)
        outdir="${2:-}"
        shift 2
        ;;
      *)
        usage >&2
        return 2
        ;;
    esac
  done

  PORT="${PORT:-13101}"
  CHROMIUM="${CHROMIUM:-chromium}"
  if [[ -z "${BROWSER_DAEMON:-}" ]]
  then
    if [[ -x "$root/target/debug/browser-daemon" ]]
    then
      BROWSER_DAEMON="$root/target/debug/browser-daemon"
    else
      BROWSER_DAEMON="$(command -v browser-daemon)" || return 2
    fi
  fi

  tmp="$(mktemp -d)"
  trap cleanup EXIT
  head -c 24 /dev/urandom | base64 > "$tmp/secret"
  cat > "$tmp/proxies.json" <<JSON
[
  {"id": "office", "name": "Office", "url": "http://127.0.0.1:$((PORT + 2))"},
  {"id": "vpn-exit", "name": "VPN exit", "url": "http://127.0.0.1:$((PORT + 2))"}
]
JSON

  # Stands in for the upstream proxies, so the pages load through them.
  node "$here/proxy.mjs" "$((PORT + 2))" &
  pids+=("$!")

  DISPLAY=":$((PORT % 100 + 50))"
  export DISPLAY
  Xvfb "$DISPLAY" -screen 0 1600x1000x24 > "$tmp/xvfb.log" 2>&1 &
  pids+=("$!")
  sleep 1

  start_node alpha "$PORT" "$((PORT + 1))"
  start_node bravo "$((PORT + 1))" "$PORT"
  wait_for_mesh || return 1
  seed || return 1
  # Let the pages load and the dashboard collect their thumbnails.
  sleep 15

  mkdir -p "$tmp/pw"
  (cd "$tmp/pw" && npm install --silent playwright-core > /dev/null 2>&1) || return 1
  mkdir -p "$outdir"
  PLAYWRIGHT_DIR="$tmp/pw" URL="http://127.0.0.1:$PORT" OUT="$outdir" CHROMIUM="$(command -v "$CHROMIUM")" \
    node "$here/shots.mjs" || return 1
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]
then
  main "$@"
fi

# vim: set ft=sh et ts=2 sw=2 :
