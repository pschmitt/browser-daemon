# NixOS module

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

### SOCKS5 egress

A node can offer its network to the rest of the mesh as a plain SOCKS5 proxy:

```nix
services.browser-daemon.socksProxy = {
  listen = "10.0.0.5:1080";           # not loopback: needs allow and/or credentialsFile
  allow = [ "10.0.0.0/8" ];           # clients outside are dropped without an answer
  credentialsFile = "/run/secrets/browser-daemon/socks-credentials";   # user:password
};
```

(`--socks-listen`, `--socks-allow`, `--socks-advertise`, `--socks-credentials-file`.) With a cluster secret that is all there is to set up: the node tells its peers about the listener (`egress` in its node info, at the address in `advertise`, else the host they know the node by), they log in with a login derived from the secret, and each of them offers it as a proxy named `node:<name>` next to its own `proxies`, for sessions, profiles and `GET /api/proxies`. The list follows the mesh (peers are asked every 30 seconds, a node that stops answering loses its entry), and the login is never shown. A session restored after a restart whose proxy has not been found yet gets a connection that refuses, never a direct one. `credentialsFile` adds a login for other SOCKS5 clients; a proxy of a node outside the mesh goes into `proxies` with the login in a secret file, so it stays out of the Nix store (`{ id = "exit"; name = "Exit node"; urlFile = "/run/secrets/browser-daemon/exit-url"; }`, the file holding `socks5://user:password@10.0.0.5:1080`; `--proxies-file` entries take `url_file` the same way). It speaks CONNECT only (no UDP), names are resolved on the serving node, and it refuses what a session's relay refuses: this machine (loopback), link-local addresses and the mesh's daemons, however they are reached. Private LAN ranges are reachable: that is the point of exiting from that node. A listener that would be open to anyone (not loopback, no `allow`, no credentials) does not start.

### HTTP proxy for your real browser

A paired [browser extension](../browser/README.md) can send its whole browser's traffic out through one of the node's proxies (its `proxies`, a peer's `node:<name>`, or the node's own network):

```nix
services.browser-daemon.httpProxy = {
  listen = "10.0.0.5:3128";           # not loopback: needs a cluster secret or passwordFile
  allow = [ "10.0.0.0/8" ];           # clients outside are dropped without an answer
};
```

(`--http-proxy-listen`, `--http-proxy-allow`, `--http-proxy-advertise`, `--http-proxy-password-file`.) The password browsers log in with (Basic) is derived from the cluster secret, or read from `passwordFile`; a paired extension is handed it, with the list of proxies it may name, over its connection (`registered`, and the list again in every heartbeat), so nothing is configured in the browser. A paired extension is issued its own login (its id) and tells the daemon which proxy it picked over its connection, so the pick can change at any time (Firefox remembers the first login it gives an HTTP proxy). For any other client the user part of the login names the proxy (`direct` or empty for the node's own network). The same port also speaks SOCKS5 (the first byte tells): there the user is the proxy's name for each connection, which is how Firefox gives every tab its own proxy. A proxy that is unknown or down fails the connection with a 502, never falls back to a direct one. It serves CONNECT (HTTPS, WebSockets) and plain `http://` requests, which it forwards one at a time, and refuses what a session's relay refuses: this machine, link-local addresses and the mesh's daemons. A listener that would be open to anyone (not loopback, no password) does not start. Open the port in the firewall yourself.

In the extension's popup, **Route this browser through** lists the choices; nothing is routed until one is picked, and the extension asks for the optional `proxy` permission (Chromium also `webRequest` and `webRequestAuthProvider`, to answer the proxy's login) at that moment. On Firefox, right-click a tab or its page: **Proxy for this tab** (default, no proxy, the daemon's own network or any proxy) and a **Share tab** checkbox; the popup says which proxy the tab in front goes through. Chromium has no per-tab proxies: its pick is for the whole browser. Routing lasts only while the browser is connected to the daemon: leaving it, or a restart of the extension, gives the browser its direct connection back. Local names and private networks (and the daemons themselves) always stay direct. The daemon whose proxy is picked can see the traffic that is not HTTPS-protected, so pick the daemon you trust.

The flake's own package is the module's default, so no overlay is needed. More options (see [`nix/module.nix`](../nix/module.nix) for all of them):

| Option | What it does |
| --- | --- |
| `displayMode` | `"shared"` (one Xvfb, `display`) or `"per-session"` (an Xvfb per session, sized to its window) |
| `video.enable`, `video.encoder`, `video.vaapiDevice` | H.264 live view (needs `per-session`); `x264` on the CPU or `vaapi` on the GPU |
| `gpu.enable` | render Chromium on the GPU (ANGLE on Vulkan) instead of in software |
| `audio.enable` | a PulseAudio server for the browsers, streamed to the handoff and recorded |
| `run.enable` | serve [`POST /api/run`](run.md): Playwright scripts and steps run on the node (Node.js and playwright-core from nixpkgs, `run.nodePackage`, `run.playwrightCorePackage`); off by default, needs a cluster secret |
| `adblock.enable`, `adblock.default` | uBlock Origin (Lite) for the sessions, on by default |
| `recordSessions`, `recordAudio` | record new sessions (with sound); `recordingMax{StorageMB,AgeHours,Count}` are the cleanup limits |
| `maxSessions`, `sessionMemoryMaxMB`, `idleTimeoutMinutes` | per-node and per-session limits |
| `memoryMax`, `memoryHigh`, `cpuQuota` | systemd limits of the service |
| `keepSessionsOnRestart` | leave the browsers running when the service restarts (the next daemon adopts them) |
| `allowedHosts` | `Host` header values accepted besides `publicUrl` and loopback (`--allowed-host`) |
| `shareUrl` | base URL of public share links (`--share-url`); its reverse proxy serves `/share/` without a login |
| `secretAgent.enable` | serve a user's rbw vault to the live views (see [Secret agent](#secret-agent)); off by default |
| `liveViewExtensions` | [live-view extensions](extensions.md) to load (`--extension-dir`): directories or packages with a `manifest.json`; defaults to rbw's vault (`packages.rbw-extension`) |

At startup the daemon kills browsers and session displays that no session record claims (leftovers of a crash). Only its own are touched, so several daemons of one user can coexist: every browser and Xvfb it starts carries `BROWSER_DAEMON_INSTANCE=<id derived from the state directory>`, and the sweep kills only processes with its id. Processes without a tag (started by an older version) are killed only when their command line names a path under the profiles or state directory (or a `browserd-*` temporary profile), or, for an Xvfb, when its display number appears in this daemon's session records.

The service runs hardened: `ProtectSystem=strict` (writes go to `dataDir`, `recordingsDir`), `ProtectHome`, `NoNewPrivileges`, no capabilities, a state directory of mode 0700 and `UMask=0077`. User namespaces stay allowed for the Chromium sandbox, and `/dev` is not restricted so the GPU render node and audio work.

## Mesh

Nodes find each other through `peers` (base URLs) and authenticate with a shared secret kept in a file, outside the Nix store (for example a sops-nix secret):

```nix
services.browser-daemon = {
  nodeName = "rofl-13";
  publicUrl = "https://browser-13.example.org";
  peers = [ "https://browser-14.example.org" ];
  clusterSecretFile = "/run/secrets/browser-daemon/cluster-secret"; # a string, not a path literal
};
```

`clusterSecret` (a plain string) still works but is deprecated: it lands in the Nix store and on the command line. Set only one of the two. `peers` without a secret warns. The module exports the file's path as `$BROWSER_DAEMON_SECRET_FILE` to the CLI, TUI and MCP server on the host. `bind` must be `IP:PORT`. The package installs shell completions for bash, zsh and fish.

## Secret agent

The live view's vault button and passkey sign-ins need a [secret agent](rbw.md#secret-agent) running as the vault's user. It is optional and off by default; without it, nodes have no vault. It does not need `enable`: a host without a node can serve the vault to other nodes.

```nix
services.browser-daemon.secretAgent = {
  enable = true;
  user = "alice"; # whose rbw vault: the agent and rbw run as them
  # This host's node by default (http://<bind>); list every node to serve.
  nodes = [
    "https://browser-13.example.org"
    "https://browser-14.example.org"
  ];
  # Optional: ask the phone to unlock rbw, and say when a login is filled into
  # a site its entry is not saved for.
  unlockWebhook = "http://homeassistant:8123/api/webhook/rbw_unlock_request";
  notifyWebhook = "http://homeassistant:8123/api/webhook/browser_daemon_any_site_fill";
};
```

| Option | What it does |
| --- | --- |
| `user` | the vault's user (required); rbw and its config are theirs |
| `nodes` | base URLs of the nodes to serve; this host's node (`http://<bind>`) when `enable` is set, else none |
| `clusterSecretFile` | the nodes' cluster secret; defaults to `clusterSecretFile`. systemd passes it as a credential, so `user` need not be able to read it |
| `rbwPackage` | the rbw to run (`pkgs.rbw`) |
| `unlockWebhook`, `unlockHost` | where to ask for an rbw unlock (POST `{"host", "agent"}`) and the host it is for (`networking.hostName`); without a webhook a locked vault fails the request |
| `notifyWebhook` | told (POST `{"message"}`) about logins filled into a site their entry is not saved for |

The agent runs as the system service `browser-daemon-secret-agent` with `User=<user>`. rbw reaches the user's rbw-agent in `/run/user/<uid>`, so that directory has to exist: enable lingering (`users.users.<user>.linger = true`) on a host where the user is not logged in. If no rbw-agent runs, rbw starts one inside the service.

## Resource limits

Configurable per-session memory limits (`--max-session-memory-mb`, V8 heap limits) and global systemd service limits (`MemoryMax`, `MemoryHigh`, `CPUQuota`).
