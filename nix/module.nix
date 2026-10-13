{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.browser-daemon;
  adblockPkgs = pkgs.callPackage ./adblock.nix { };
  # The screen is larger than any session window (up to 4K), and the X pointer is
  # parked in the margin outside all of them. Headful Chromium reports the real
  # pointer as a mouse move whenever it handles a mouse press; with the pointer
  # over a window that move cancels drags, so text could not be selected in handoff.
  screenWidth = lib.max cfg.defaultWidth 3840 + 16;
  screenHeight = lib.max cfg.defaultHeight 2160 + 16;
  parkPointer = pkgs.writeShellScript "browser-daemon-park-pointer" ''
    for _ in $(seq 50); do
      DISPLAY=${cfg.display} ${lib.getExe pkgs.xdotool} mousemove ${toString (screenWidth - 1)} ${toString (screenHeight - 1)} && exit 0
      sleep 0.1
    done
    exit 1
  '';
in
{
  options.services.browser-daemon = {
    enable = lib.mkEnableOption "native remote browser daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.browser-daemon or (pkgs.callPackage ./package.nix { });
      description = "browser-daemon package to run.";
    };

    chromiumPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.chromium;
      description = "Chromium package to spawn for sessions.";
    };

    firefoxPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = pkgs.firefox;
      description = "Firefox package to spawn for sessions. Set to null to disable Firefox engine.";
    };

    xvfbPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.xorg-server;
      description = "Xvfb server package.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3001";
      description = "Listen address and port for the HTTP/WebSocket daemon.";
    };

    display = lib.mkOption {
      type = lib.types.str;
      default = ":98";
      description = "X11 virtual display to use.";
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/browser-daemon";
      description = "State directory for browser profiles and runtime data.";
    };

    maxSessions = lib.mkOption {
      type = lib.types.ints.positive;
      default = 8;
      description = "Maximum number of concurrent browser processes.";
    };

    defaultWidth = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1600;
      description = "Default window width in pixels.";
    };

    defaultHeight = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1000;
      description = "Default window height in pixels.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "browser-daemon";
      description = "User account under which browser-daemon runs.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "browser-daemon";
      description = "Group under which browser-daemon runs.";
    };

    nodeName = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      description = "Node identifier for peer-to-peer mesh.";
    };

    publicUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Public or mesh base URL of this node.";
    };

    peers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "List of peer browser-daemon base URLs for cluster meshing.";
    };

    clusterSecret = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Shared cluster secret string for peer-to-peer authentication.";
    };

    clusterSecretFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Path to file containing shared cluster secret for peer-to-peer authentication.";
    };

    recordingsDir = lib.mkOption {
      type = lib.types.str;
      default = "${cfg.dataDir}/recordings";
      description = "Directory where session screencast recordings are saved.";
    };

    recordSessions = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to automatically record browser sessions by default.";
    };

    recordingMaxStorageMB = lib.mkOption {
      type = lib.types.ints.positive;
      default = 2048;
      description = "Maximum storage in MB allocated for session recordings before auto-vacuum.";
    };

    recordingMaxAgeHours = lib.mkOption {
      type = lib.types.ints.positive;
      default = 168; # 7 days
      description = "Maximum age in hours before recordings are pruned by auto-vacuum.";
    };

    recordingMaxCount = lib.mkOption {
      type = lib.types.ints.positive;
      default = 100;
      description = "Maximum number of recordings kept before auto-vacuum.";
    };

    sessionMemoryMaxMB = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = "Maximum memory per Chromium browser process in MB (sets --js-flags=--max-old-space-size=N).";
    };

    adblock = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Make the ad blocker available to sessions (uBlock Origin Lite in Chromium, uBlock Origin in Firefox).";
      };
      default = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Block ads in new sessions unless they opt out with `adblock: false`.";
      };
      chromiumExtension = lib.mkOption {
        type = lib.types.package;
        default = adblockPkgs.ublock-origin-lite;
        description = "Unpacked Chromium extension to load as the ad blocker.";
      };
      firefoxAddon = lib.mkOption {
        type = lib.types.path;
        default = "${adblockPkgs.ublock-origin-xpi}/ublock-origin.xpi";
        description = "Firefox add-on (.xpi) to install as the ad blocker.";
      };
    };

    proxies = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            id = lib.mkOption {
              type = lib.types.strMatching "[A-Za-z0-9._-]+";
              description = "Stable key that sessions and profiles refer to.";
            };
            name = lib.mkOption {
              type = lib.types.str;
              description = "Name shown in the dashboard.";
            };
            url = lib.mkOption {
              type = lib.types.str;
              example = "socks5://proxy.example:1080";
              description = ''
                Proxy URL: `socks5://`, `socks5h://`, `socks4://`, `http://` or `https://`.
                Credentials are not supported.
              '';
            };
          };
        }
      );
      default = [ ];
      description = "Upstream proxies sessions can be routed through, per session or as a profile default.";
    };

    idleTimeoutMinutes = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 30;
      description = "Stop sessions nobody used for this many minutes (0 = never). Sessions can override it.";
    };

    memoryMax = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Global systemd MemoryMax limit for browser-daemon service (e.g. '4G').";
    };

    memoryHigh = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Global systemd MemoryHigh threshold for browser-daemon service (e.g. '3.5G').";
    };

    cpuQuota = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Global systemd CPUQuota limit for browser-daemon service (e.g. '200%').";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      inherit (cfg) group;
      home = cfg.dataDir;
    };
    users.groups.${cfg.group} = { };

    fonts.packages = [
      pkgs.dejavu_fonts
      pkgs.noto-fonts
      pkgs.noto-fonts-color-emoji
      pkgs.noto-fonts-cjk-sans
    ];

    systemd.services.browser-daemon-xvfb = {
      description = "Xvfb virtual display for browser-daemon";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${cfg.xvfbPackage}/bin/Xvfb ${cfg.display} -screen 0 ${toString screenWidth}x${toString screenHeight}x24 -nolisten tcp -noreset";
        ExecStartPost = parkPointer;
        Restart = "always";
        RestartSec = 2;
        User = cfg.user;
        Group = cfg.group;
      };
    };

    systemd.services.browser-daemon = {
      description = "Native remote browser daemon";
      wantedBy = [ "multi-user.target" ];
      wants = [ "browser-daemon-xvfb.service" ];
      after = [ "browser-daemon-xvfb.service" ];
      path = [
        pkgs.gnutar
        pkgs.gzip
      ];
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " (
          [
            "${cfg.package}/bin/browser-daemon"
            "--bind"
            cfg.bind
            "--chromium"
            (lib.getExe cfg.chromiumPackage)
            "--display"
            cfg.display
            "--profile-dir"
            "${cfg.dataDir}/profiles"
            "--recordings-dir"
            cfg.recordingsDir
            "--recording-max-storage-mb"
            (toString cfg.recordingMaxStorageMB)
            "--recording-max-age-hours"
            (toString cfg.recordingMaxAgeHours)
            "--recording-max-count"
            (toString cfg.recordingMaxCount)
            "--max-sessions"
            (toString cfg.maxSessions)
            "--default-width"
            (toString cfg.defaultWidth)
            "--default-height"
            (toString cfg.defaultHeight)
            "--node-name"
            cfg.nodeName
            "--idle-timeout-minutes"
            (toString cfg.idleTimeoutMinutes)
            "--adblock-default"
            (lib.boolToString cfg.adblock.default)
          ]
          ++ lib.optionals cfg.adblock.enable [
            "--chromium-adblock"
            "${cfg.adblock.chromiumExtension}"
            "--firefox-adblock"
            "${cfg.adblock.firefoxAddon}"
          ]
          ++ lib.optionals (cfg.proxies != [ ]) [
            "--proxies-file"
            "${(pkgs.formats.json { }).generate "browser-daemon-proxies.json" cfg.proxies}"
          ]
          ++ lib.optionals (cfg.publicUrl != null) [
            "--public-url"
            cfg.publicUrl
          ]
          ++ lib.concatMap (p: [
            "--peer"
            p
          ]) cfg.peers
          ++ lib.optionals (cfg.clusterSecretFile != null) [
            "--cluster-secret-file"
            (toString cfg.clusterSecretFile)
          ]
          ++ lib.optionals (cfg.clusterSecret != null) [
            "--cluster-secret"
            cfg.clusterSecret
          ]
          ++ lib.optionals cfg.recordSessions [
            "--record-sessions"
          ]
          ++ lib.optionals (cfg.firefoxPackage != null) [
            "--firefox"
            (lib.getExe cfg.firefoxPackage)
          ]
          ++ lib.optionals (cfg.sessionMemoryMaxMB != null) [
            "--max-session-memory-mb"
            (toString cfg.sessionMemoryMaxMB)
          ]
        );
        Restart = "always";
        RestartSec = 2;
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "browser-daemon";
        WorkingDirectory = cfg.dataDir;
        MemoryMax = lib.mkIf (cfg.memoryMax != null) cfg.memoryMax;
        MemoryHigh = lib.mkIf (cfg.memoryHigh != null) cfg.memoryHigh;
        CPUQuota = lib.mkIf (cfg.cpuQuota != null) cfg.cpuQuota;
      };
    };
  };
}
