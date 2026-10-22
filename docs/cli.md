# CLI

The `browser-daemon` binary is the daemon (`run`, what the systemd service starts) and a command-line client for its API. The client talks to `http://127.0.0.1:3001` unless `--url` (or `BROWSER_DAEMON_URL`) names another node; peers are reached through it. The cluster secret comes from `--secret-file` (or `BROWSER_DAEMON_SECRET_FILE`, which the NixOS module sets from `clusterSecretFile`), else `/run/secrets/browser-daemon/cluster-secret` when readable. `--json` prints JSON instead of tables.

```text
browser-daemon run [OPTIONS]              the daemon (see `run --help`)
browser-daemon session list|show|new|delete (stop)|stop-all|rename|proxy|idle-timeout|record|tabs|open|close-tab|save-as-profile|url|downloads|download
browser-daemon profile list|show|new|set|clone|delete (rm)|clear|backup|export|import|transfer
browser-daemon profile backup list|create|restore|rm PROFILE
browser-daemon recording list|show|keep|unkeep|delete (rm)|merge|export|vacuum
browser-daemon node list|show|set|add|remove (rm)
browser-daemon completions bash|zsh|fish   shell completions (the package installs them)
browser-daemon mcp                        MCP server for AI agents (see below)
browser-daemon tui                        terminal UI (see above)
browser-daemon secret-agent               serve the live views' vault (passkeys, logins) from rbw (docs/rbw.md)
```

Sessions, profiles and recordings are named by id or, for sessions and profiles, by name. An exact id wins over an exact name, which wins over the start of an id (at least 4 characters); `--node` narrows a name that exists on several nodes. Commands with several targets (`session stop`, `recording rm|keep|unkeep`) try each one and report the ones that failed. Commands that destroy data (`profile rm|clear`, `profile backup restore|rm`, `profile import --overwrite`, `profile transfer --move|--overwrite`, `recording rm`, `session stop-all`) ask first, or take `-y`. Exports (`profile export`, `recording export`) stream into a temporary file that replaces the target when complete, and refuse to overwrite an existing file without `--force`.

Exit status: 0 success, 1 failure, 2 bad command line, 3 not found, 4 node unreachable, 5 cancelled (or not confirmed), 6 unauthorized.

```sh
browser-daemon session list
browser-daemon session new --engine firefox --profile hrworks --node rofl-14   # prints the id, WebSocket URL on stderr
browser-daemon session open quiet-otter https://example.org/
browser-daemon session record quiet-otter pause
browser-daemon session stop-all --node rofl-13 -y
browser-daemon profile backup create hrworks before-cleanup
browser-daemon profile transfer hrworks --to rofl-14 --move
browser-daemon recording export 1b2c3d4e -o rec.jsonl --force
```

## `browser-daemon run`

```text
Run the daemon (what the systemd service does)

Usage: browser-daemon run [OPTIONS]

Options:
  -b, --bind <BIND>
          [default: 127.0.0.1:3001]
      --chromium <CHROMIUM>
          [default: chromium]
      --firefox <FIREFOX>

      --display <DISPLAY>

      --max-sessions <MAX_SESSIONS>
          [default: 8]
      --profile-dir <PROFILE_DIR>
          [default: /var/lib/browser-daemon/profiles]
      --default-width <DEFAULT_WIDTH>
          [default: 1600]
      --default-height <DEFAULT_HEIGHT>
          [default: 1000]
      --node-name <NODE_NAME>

      --public-url <PUBLIC_URL>

      --peer <PEERS>

      --cluster-secret <CLUSTER_SECRET>
          Shared cluster secret for peer-to-peer authentication
      --cluster-secret-file <CLUSTER_SECRET_FILE>
          Path to file containing shared cluster secret
      --allowed-host <ALLOWED_HOSTS>
          A `Host` (HOST[:PORT]) the node answers to besides its public URL's and loopback's (repeat for several); other hosts get 403
      --recordings-dir <RECORDINGS_DIR>
          [default: /var/lib/browser-daemon/recordings]
      --record-sessions <RECORD_SESSIONS>
          Record new sessions unless they opt out (`--record-sessions false` to turn off) [default: true] [possible values: true, false]
      --record-audio <RECORD_AUDIO>
          Record the session's sound with its recording (needs --pulse-server); a recording can still be started without (`POST .../record?audio=false`) [default: true] [possible values: true, false]
      --recording-max-storage-mb <RECORDING_MAX_STORAGE_MB>
          [default: 2048]
      --recording-max-age-hours <RECORDING_MAX_AGE_HOURS>
          [default: 168]
      --recording-max-count <RECORDING_MAX_COUNT>
          [default: 100]
      --max-session-memory-mb <MAX_SESSION_MEMORY_MB>
          Maximum memory per Chromium browser process in MB (sets --js-flags=--max-old-space-size=N)
      --idle-timeout-minutes <IDLE_TIMEOUT_MINUTES>
          Stop sessions nobody used for this many minutes (0 = never) [default: 30]
      --chromium-adblock <CHROMIUM_ADBLOCK>
          Unpacked ad blocker extension (uBlock Origin Lite) for Chromium
      --firefox-adblock <FIREFOX_ADBLOCK>
          Ad blocker add-on (uBlock Origin .xpi) for Firefox
      --adblock-default <ADBLOCK_DEFAULT>
          Block ads in new sessions unless they opt out [default: true] [possible values: true, false]
      --proxies-file <PROXIES_FILE>
          JSON list of upstream proxies sessions can use: [{"id", "name", "url"}]
      --pulse-server <PULSE_SERVER>
          PulseAudio socket for session audio (per-session sinks, streamed on /audio)
      --pactl <PACTL>
          [default: pactl]
      --ffmpeg <FFMPEG>
          [default: ffmpeg]
      --display-mode <DISPLAY_MODE>
          X display per session (own Xvfb, needed for video streaming) or one shared --display [default: shared] [possible values: shared, per-session]
      --xvfb <XVFB>
          Xvfb binary for --display-mode per-session [default: Xvfb]
      --xdotool <XDOTOOL>
          xdotool, to park the X pointer on per-session displays
      --video-encoder <VIDEO_ENCODER>
          Offer the live view as H.264 video (needs --display-mode per-session) [possible values: vaapi, x264]
      --video-ffmpeg <VIDEO_FFMPEG>
          ffmpeg with x11grab (ffmpeg-full) for --video-encoder [default: ffmpeg]
      --vaapi-device <VAAPI_DEVICE>
          [default: /dev/dri/renderD128]
      --chromium-gpu <CHROMIUM_GPU>
          Render Chromium on the GPU: vulkan (ANGLE on Vulkan), or none (software) [default: none] [possible values: none, vulkan]
  -h, --help
          Print help
```
