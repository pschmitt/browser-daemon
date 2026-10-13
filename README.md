# browser-daemon

A fast, lightweight, native remote browser daemon written in Rust for CDP automation, AI agents, Firefox WebDriver BiDi, screencasting, and interactive browser handoff across a peer-to-peer cluster mesh.

![Browser Daemon Dashboard](dashboard.png)

## Features

- **Multi-Tenant Process Isolation:** Spawns an isolated browser instance per incoming connection (`/chromium` for CDP, `/firefox` for WebDriver BiDi). Connections never interfere with one another.
- **Dual Engine Support:** Runs Chromium with native CDP and Firefox with WebDriver BiDi (`/firefox`). Firefox only allows one WebDriver session per process, so the daemon owns it and multiplexes it: any number of BiDi clients (automation, handoff, recorder) can attach to the same Firefox session (`/firefox?session_id=<id>`).
- **Cluster Mesh & Profile Migration:** Discovers peer nodes across Tailscale/wireguard meshes and transfers persistent browser profiles between nodes with live progress toasts.
- **Bot Detection Immunity:** Runs headful Chromium inside an X11 virtual display (`Xvfb`) with `--disable-blink-features=AutomationControlled` natively. Cleanly passes Google Botguard, Cloudflare Turnstile, and other bot detection systems.
- **Session Screencasting & Playback:** Built-in lightweight screencast recorder capturing JPEG frames and metadata, with an interactive scrubbing video player, IndexedDB background downloading, and client-side WebM export.
- **Mobile-Friendly Live Handoff:** Embedded `/handoff/` web interface with interactive screen streaming, touch-to-click, touch-to-scroll, mobile text input, and clipboard synchronization (copy & paste from/to remote pages). Works for both engines: Chromium streams a CDP screencast, Firefox streams WebDriver BiDi screenshots.
- **Resource Limits & Governance:** Configurable per-session memory limits (`--max-session-memory-mb`, V8 heap limits) and global systemd service limits (`MemoryMax`, `MemoryHigh`, `CPUQuota`).
- **Named Profile Persistence:** Optional persistent profiles via `?profile=<name or id>` query parameter, alongside ephemeral auto-cleaned sessions. Each profile lives in `/var/lib/browser-daemon/profiles/<uuid>/` with a `profile.json` holding its free-text name (e.g. `Google (p@example.com)`) and engine; the API reports the directory as `storage_dir`. Name-based directories from older versions are migrated on startup.
- **Upstream Proxies:** Route a session through a SOCKS or HTTP proxy configured on its node (`services.browser-daemon.proxies`). Pick one per session (`"proxy": "<id>"` when creating it, `?proxy=<id>` on `/chromium` and `/firefox`, `none` for direct) or store a default on a profile (`PATCH /api/profiles/<name> {"proxy": "<id>"}`). DNS goes through SOCKS5 proxies, and WebRTC is kept from bypassing them. `GET /api/proxies` lists a node's proxies.
- **Installable PWA:** Web App Manifest with standalone display mode, badging, and offline service worker caching.
- **Drop-In CDP & BiDi Compatibility:** Standard endpoints:
  - `/chromium` (WebSocket for Playwright, Puppeteer, CDP clients)
  - `/firefox` (WebSocket for Firefox WebDriver BiDi clients)
  - `/json/version`
  - `/json/list` (aggregates active pages across all active sessions)
  - `/devtools/page/:page_id` (WebSocket for individual page inspection)
  - `/health`

## NixOS Module

Add the flake input:

```nix
inputs = {
  browser-daemon = {
    url = "github:pschmitt/browser-daemon";
    inputs.nixpkgs.follows = "nixpkgs";
  };
};
```

Enable the service in your NixOS configuration:

```nix
{ inputs, ... }:
{
  imports = [
    inputs.browser-daemon.nixosModules.default
  ];

  services.browser-daemon = {
    enable = true;
    bind = "127.0.0.1:3001";
    maxSessions = 8;
    # Optional: proxies sessions and profiles can choose from.
    proxies = [
      {
        id = "vpn";
        name = "VPN exit";
        url = "socks5://10.0.0.1:1080";
      }
    ];
  };
}
```

## Terminal UI

`browser-daemon tui` shows the whole mesh in the terminal: sessions (with their tabs), profiles, recordings and nodes, refreshed every few seconds.

- Sessions: take over in the browser, stop, start or stop recording, start a throwaway session, show the WebSocket URL.
- Profiles: launch (or take over a running one), cycle the proxy, delete.
- Recordings: play in the dashboard, delete.

It talks to `http://127.0.0.1:3001` unless `--url` (or `BROWSER_DAEMON_URL`) names another node; peers are reached through it. The cluster secret comes from `--secret-file`, the NixOS module's secret when readable, or otherwise the dashboard of that node. Press `?` for the keys.

## CLI Usage

```text
Usage: browser-daemon [OPTIONS] [COMMAND]

Commands:
  tui   Terminal UI for the mesh: sessions, profiles, recordings and nodes
  help  Print this message or the help of the given subcommand(s)

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
      --recordings-dir <RECORDINGS_DIR>
          [default: /var/lib/browser-daemon/recordings]
      --record-sessions
          
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
  -h, --help
          Print help
  -V, --version
          Print version
```

## License

GPL-3.0-only. See [LICENSE](LICENSE).
