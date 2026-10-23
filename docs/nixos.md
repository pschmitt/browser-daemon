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

The flake's own package is the module's default, so no overlay is needed. More options (see [`nix/module.nix`](../nix/module.nix) for all of them):

| Option | What it does |
| --- | --- |
| `displayMode` | `"shared"` (one Xvfb, `display`) or `"per-session"` (an Xvfb per session, sized to its window) |
| `video.enable`, `video.encoder`, `video.vaapiDevice` | H.264 live view (needs `per-session`); `x264` on the CPU or `vaapi` on the GPU |
| `gpu.enable` | render Chromium on the GPU (ANGLE on Vulkan) instead of in software |
| `audio.enable` | a PulseAudio server for the browsers, streamed to the handoff and recorded |
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
