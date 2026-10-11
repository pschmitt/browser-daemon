# browser-daemon

A fast, lightweight, native remote browser daemon written in Rust for CDP automation, AI agents, Firefox WebDriver BiDi, screencasting, and interactive browser handoff across a peer-to-peer cluster mesh.

![Browser Daemon Dashboard](dashboard.png)

## Features

- **Multi-Tenant Process Isolation:** Spawns an isolated browser instance per incoming connection (`/chromium` for CDP, `/firefox` for WebDriver BiDi). Connections never interfere with one another.
- **Dual Engine Support:** Runs Chromium with native CDP and Firefox with WebDriver BiDi (`/firefox`).
- **Cluster Mesh & Profile Migration:** Discovers peer nodes across Tailscale/wireguard meshes and transfers persistent browser profiles between nodes with live progress toasts.
- **Bot Detection Immunity:** Runs headful Chromium inside an X11 virtual display (`Xvfb`) with `--disable-blink-features=AutomationControlled` natively. Cleanly passes Google Botguard, Cloudflare Turnstile, and other bot detection systems.
- **Session Screencasting & Playback:** Built-in lightweight screencast recorder capturing JPEG frames and metadata, with an interactive scrubbing video player, IndexedDB background downloading, and client-side WebM export.
- **Mobile-Friendly Live Handoff:** Embedded `/handoff/` web interface with interactive screen streaming, touch-to-click, touch-to-scroll, mobile text input, and clipboard synchronization (copy & paste from/to remote pages).
- **Resource Limits & Governance:** Configurable per-session memory limits (`--max-session-memory-mb`, V8 heap limits) and global systemd service limits (`MemoryMax`, `MemoryHigh`, `CPUQuota`).
- **Named Profile Persistence:** Optional persistent profiles via `?profile=<name>` query parameter (saved under `/var/lib/browser-daemon/profiles/<name>`), alongside ephemeral auto-cleaned sessions.
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
  };
}
```

## CLI Usage

```text
Usage: browser-daemon [OPTIONS]

Options:
  -b, --bind <BIND>                      [default: 127.0.0.1:3001]
      --chromium <CHROMIUM>              [default: chromium]
      --display <DISPLAY>
      --max-sessions <MAX_SESSIONS>      [default: 8]
      --profile-dir <PROFILE_DIR>        [default: /var/lib/browser-daemon/profiles]
      --default-width <DEFAULT_WIDTH>    [default: 1600]
      --default-height <DEFAULT_HEIGHT>  [default: 1000]
  -h, --help                             Print help
  -V, --version                          Print version
```

## License

MIT
