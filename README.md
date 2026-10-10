# browser-daemon

A fast, lightweight, native remote Chromium daemon written in Rust for CDP automation, AI agents, and interactive browser handoff.

## Features

- **Multi-Tenant Process Isolation:** Spawns an isolated Chromium instance per incoming CDP WebSocket connection (`/chromium`). Connections never interfere with one another.
- **Bot Detection Immunity:** Runs headful Chromium inside an X11 virtual display (`Xvfb`) with `--disable-blink-features=AutomationControlled` natively. No prototype patching or fake user-agents; cleanly passes Google Botguard, Cloudflare Turnstile, and other bot detection systems.
- **Mobile-Friendly Live Handoff:** Embedded `/handoff/` web interface with interactive screen streaming, touch-to-click, touch-to-scroll, and mobile text input when human intervention (passkeys, 2FA, CAPTCHAs) is required.
- **Named Profile Persistence:** Optional persistent profiles via `?profile=<name>` query parameter (saved under `/var/lib/browser-daemon/profiles/<name>`), alongside ephemeral auto-cleaned sessions.
- **Drop-In CDP Compatibility:** Standard Chrome DevTools Protocol endpoints:
  - `/chromium` (WebSocket for Playwright, Puppeteer, CDP clients)
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
