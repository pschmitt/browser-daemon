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
  pulseDir = "/run/browser-daemon-pulse";
  # A bare PulseAudio for the browsers: an idle default sink, and the daemon
  # adds a null sink per session through pactl.
  pulseConfig = pkgs.writeText "browser-daemon-pulse.pa" ''
    load-module module-native-protocol-unix socket=${pulseDir}/native auth-anonymous=1
    load-module module-null-sink sink_name=browserd_idle sink_properties=device.description=browser-daemon
    set-default-sink browserd_idle
  '';
  # StateDirectory is relative to /var/lib.
  stateDirectory =
    if lib.hasPrefix "/var/lib/" cfg.dataDir then lib.removePrefix "/var/lib/" cfg.dataDir else null;
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
      defaultText = lib.literalExpression "pkgs.browser-daemon (the flake's package when the module comes from the flake)";
      description = "browser-daemon package to run.";
    };

    chromiumPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.chromium;
      defaultText = lib.literalExpression "pkgs.chromium";
      description = "Chromium package to spawn for sessions.";
    };

    firefoxPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = pkgs.firefox;
      defaultText = lib.literalExpression "pkgs.firefox";
      description = "Firefox package to spawn for sessions. Set to null to disable Firefox engine.";
    };

    displayMode = lib.mkOption {
      type = lib.types.enum [
        "shared"
        "per-session"
      ];
      default = "shared";
      description = ''
        Where browser windows live: one shared Xvfb (`display`), or an Xvfb per
        session, sized to its window, which video streaming needs (each session's
        screen can then be captured on its own).
      '';
    };

    video = {
      enable = lib.mkEnableOption "the handoff live view as H.264 video (needs displayMode = \"per-session\")";
      encoder = lib.mkOption {
        type = lib.types.enum [
          "vaapi"
          "x264"
        ];
        default = "x264";
        description = "H.264 encoder: VAAPI (GPU, needs a VAAPI driver, e.g. intel-media-driver) or x264 (CPU).";
      };
      vaapiDevice = lib.mkOption {
        type = lib.types.str;
        default = "/dev/dri/renderD128";
        description = "Render node for VAAPI encoding.";
      };
      ffmpegPackage = lib.mkOption {
        type = lib.types.package;
        default = pkgs.ffmpeg-full;
        defaultText = lib.literalExpression "pkgs.ffmpeg-full";
        description = "ffmpeg with x11grab (the default ffmpeg lacks it).";
      };
    };

    gpu.enable = lib.mkEnableOption ''
      rendering Chromium on the GPU (ANGLE on Vulkan, through the render node)
      instead of in software. Pages then see the real GPU in WebGL rather than
      a software renderer, which bot detectors flag. Needs a Vulkan driver
      (hardware.graphics)
    '';

    xvfbPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.xorg-server;
      defaultText = lib.literalExpression "pkgs.xorg-server";
      description = "Xvfb server package.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3001";
      example = "127.0.0.1:3001";
      description = "Listen address and port (IP literal, `[::1]:3001` for IPv6) for the HTTP/WebSocket daemon.";
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
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "Node identifier for peer-to-peer mesh.";
    };

    publicUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Public or mesh base URL of this node.";
    };

    shareUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://browser.example.org";
      description = ''
        Base URL of public share links (`<shareUrl>/share/<node>/<token>`):
        the name the public reaches the mesh by. Serve its `/share/` path
        without a login in the reverse proxy; the daemon checks the token.
        `publicUrl` when null.
      '';
    };

    peers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "List of peer browser-daemon base URLs for cluster meshing.";
    };

    allowedHosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "browser.example.org" ];
      description = ''
        Host names (optionally with `:port`) the daemon accepts in the `Host`
        header, besides the one of `publicUrl` and loopback. Add the names a
        reverse proxy or a mesh peer reaches this node by.
      '';
    };

    clusterSecret = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Shared cluster secret string. Deprecated: it ends up in the world-readable
        Nix store and on the command line. Use `clusterSecretFile`.
      '';
    };

    clusterSecretFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/browser-daemon/cluster-secret";
      description = ''
        Absolute path of a file with the shared cluster secret. Give a string,
        not a Nix path literal (that would copy the secret into the Nix store).
        The CLI and TUI read it from `$BROWSER_DAEMON_SECRET_FILE` when set.
      '';
    };

    recordingsDir = lib.mkOption {
      type = lib.types.str;
      default = "${cfg.dataDir}/recordings";
      defaultText = lib.literalExpression ''"''${config.services.browser-daemon.dataDir}/recordings"'';
      description = "Directory where session screencast recordings are saved.";
    };

    browserForgetAfterHours = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 24;
      description = "Forget remembered browsers (extension nodes) that have not connected for this many hours, revoking their key. 0 keeps them until revoked.";
    };

    recordSessions = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to automatically record browser sessions by default.";
    };

    recordAudio = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether recordings take the session's sound too (needs `audio.enable`); a recording can still be started without.";
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
        defaultText = lib.literalExpression "(pkgs.callPackage ./adblock.nix { }).ublock-origin-lite";
        description = "Unpacked Chromium extension to load as the ad blocker.";
      };
      firefoxAddon = lib.mkOption {
        type = lib.types.path;
        default = "${adblockPkgs.ublock-origin-xpi}/ublock-origin.xpi";
        defaultText = lib.literalExpression ''"''${(pkgs.callPackage ./adblock.nix { }).ublock-origin-xpi}/ublock-origin.xpi"'';
        description = "Firefox add-on (.xpi) to install as the ad blocker.";
      };
    };

    audio = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Give sessions audio: a PulseAudio server for the browsers, streamed to handoff on /audio.";
      };
      pulseaudioPackage = lib.mkPackageOption pkgs "pulseaudio" { };
      ffmpegPackage = lib.mkPackageOption pkgs "ffmpeg" { };
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

    keepSessionsOnRestart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Leave the browsers (and their displays) running when the service stops or
        restarts, e.g. on a deploy: the next daemon adopts them, so sessions and
        their tabs survive. Sets `KillMode=process`. `systemctl stop` then leaves
        the sessions running too; stop them from the dashboard first, or turn
        this off to have every restart end them.
      '';
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
    assertions = [
      {
        assertion = cfg.clusterSecret == null || cfg.clusterSecretFile == null;
        message = "services.browser-daemon: set only one of clusterSecret and clusterSecretFile.";
      }
      {
        assertion =
          cfg.clusterSecretFile == null
          || (
            lib.hasPrefix "/" cfg.clusterSecretFile
            && !(lib.hasPrefix "${builtins.storeDir}/" cfg.clusterSecretFile)
          );
        message = "services.browser-daemon.clusterSecretFile must be an absolute path outside the Nix store (a string, not a path literal).";
      }
      {
        assertion = builtins.match "([[][0-9a-fA-F:.]+[]]|[0-9.]+):[0-9]{1,5}" cfg.bind != null;
        message = "services.browser-daemon.bind must be IP:PORT (or [IPv6]:PORT), got `${cfg.bind}`.";
      }
      {
        assertion = cfg.video.enable -> cfg.displayMode == "per-session";
        message = "services.browser-daemon.video needs displayMode = \"per-session\" (each session's screen is captured on its own).";
      }
    ];

    warnings =
      lib.optional (cfg.clusterSecret != null)
        "services.browser-daemon.clusterSecret is deprecated: it lands in the Nix store and on the command line. Use clusterSecretFile."
      ++ lib.optional (
        cfg.peers != [ ] && cfg.clusterSecret == null && cfg.clusterSecretFile == null
      ) "services.browser-daemon.peers is set without a cluster secret: the mesh will not authenticate.";

    # A dataDir outside /var/lib cannot be a StateDirectory.
    systemd.tmpfiles.rules = lib.mkIf (stateDirectory == null) [
      "d ${cfg.dataDir} 0700 ${cfg.user} ${cfg.group} -"
    ];

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

    # `browser-daemon tui` for the people on the host.
    environment.systemPackages = [ cfg.package ];
    environment.variables = lib.mkIf (cfg.clusterSecretFile != null) {
      BROWSER_DAEMON_SECRET_FILE = cfg.clusterSecretFile;
    };

    systemd.services = {
      browser-daemon-pulse = lib.mkIf cfg.audio.enable {
        description = "PulseAudio for browser-daemon sessions";
        wantedBy = [ "multi-user.target" ];
        environment = {
          HOME = pulseDir;
          PULSE_RUNTIME_PATH = pulseDir;
          PULSE_STATE_PATH = "${pulseDir}/state";
        };
        serviceConfig = {
          ExecStart = lib.concatStringsSep " " [
            "${cfg.audio.pulseaudioPackage}/bin/pulseaudio"
            "--daemonize=no"
            "--exit-idle-time=-1"
            "--use-pid-file=no"
            "--realtime=no"
            "--high-priority=no"
            "--log-target=stderr"
            "-n"
            "-F"
            "${pulseConfig}"
          ];
          Restart = "always";
          RestartSec = 2;
          User = cfg.user;
          Group = cfg.group;
          RuntimeDirectory = "browser-daemon-pulse";
          RuntimeDirectoryMode = "0750";
        };
      };

      browser-daemon-xvfb = lib.mkIf (cfg.displayMode == "shared") {
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

      browser-daemon = {
        description = "Native remote browser daemon";
        wantedBy = [ "multi-user.target" ];
        wants =
          lib.optional (cfg.displayMode == "shared") "browser-daemon-xvfb.service"
          ++ lib.optional cfg.audio.enable "browser-daemon-pulse.service";
        after =
          lib.optional (cfg.displayMode == "shared") "browser-daemon-xvfb.service"
          ++ lib.optional cfg.audio.enable "browser-daemon-pulse.service";
        path = [
          pkgs.gnutar
          pkgs.gzip
        ];
        serviceConfig = {
          ExecStart = lib.concatStringsSep " " (
            [
              "${cfg.package}/bin/browser-daemon"
              "run"
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
              "--record-audio"
              (lib.boolToString cfg.recordAudio)
              "--record-sessions"
              (lib.boolToString cfg.recordSessions)
              "--browser-forget-after-hours"
              (toString cfg.browserForgetAfterHours)
              "--ffmpeg"
              (lib.getExe cfg.audio.ffmpegPackage)
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
            ++ lib.optionals (cfg.displayMode == "per-session") [
              "--display-mode"
              "per-session"
              "--xvfb"
              "${cfg.xvfbPackage}/bin/Xvfb"
              "--xdotool"
              (lib.getExe pkgs.xdotool)
            ]
            ++ lib.optionals cfg.video.enable [
              "--video-encoder"
              cfg.video.encoder
              "--video-ffmpeg"
              (lib.getExe cfg.video.ffmpegPackage)
              "--vaapi-device"
              cfg.video.vaapiDevice
            ]
            ++ lib.optionals cfg.gpu.enable [
              "--chromium-gpu"
              "vulkan"
            ]
            ++ lib.optionals cfg.audio.enable [
              "--pulse-server"
              "${pulseDir}/native"
              "--pactl"
              "${cfg.audio.pulseaudioPackage}/bin/pactl"
            ]
            ++ lib.optionals (cfg.proxies != [ ]) [
              "--proxies-file"
              "${(pkgs.formats.json { }).generate "browser-daemon-proxies.json" cfg.proxies}"
            ]
            ++ lib.optionals (cfg.publicUrl != null) [
              "--public-url"
              cfg.publicUrl
            ]
            ++ lib.optionals (cfg.shareUrl != null) [
              "--share-url"
              cfg.shareUrl
            ]
            ++ lib.concatMap (p: [
              "--peer"
              p
            ]) cfg.peers
            ++ lib.concatMap (h: [
              "--allowed-host"
              h
            ]) cfg.allowedHosts
            ++ lib.optionals (cfg.clusterSecretFile != null) [
              "--cluster-secret-file"
              cfg.clusterSecretFile
            ]
            ++ lib.optionals (cfg.clusterSecret != null) [
              "--cluster-secret"
              cfg.clusterSecret
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
          KillMode = lib.mkIf cfg.keepSessionsOnRestart "process";
          User = cfg.user;
          Group = cfg.group;
          StateDirectory = lib.mkIf (stateDirectory != null) stateDirectory;
          StateDirectoryMode = "0700";
          UMask = "0077";
          # Hardening. User namespaces stay allowed (the Chromium sandbox needs
          # them), as does W^X-violating JIT memory. /dev is not restricted, so
          # the GPU render node (/dev/dri) and audio keep working.
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ReadWritePaths = [
            cfg.dataDir
            "-${cfg.recordingsDir}"
            "-${pulseDir}"
            # The real /tmp, not a private one: browsers and their X displays
            # outlive a daemon restart (keepSessionsOnRestart), and systemd would
            # empty a private /tmp when the old daemon stops, under their feet
            # (uploads, throwaway profiles, X locks). The daemon's own files there
            # are 0700/0600 (UMask) instead.
            "/tmp"
            "/var/tmp"
          ];
          ProtectHome = true;
          PrivateTmp = false;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectKernelLogs = true;
          ProtectControlGroups = true;
          RestrictAddressFamilies = [
            "AF_UNIX"
            "AF_INET"
            "AF_INET6"
            "AF_NETLINK"
          ];
          LockPersonality = true;
          RestrictRealtime = true;
          SystemCallArchitectures = "native";
          CapabilityBoundingSet = "";
          WorkingDirectory = cfg.dataDir;
          MemoryMax = lib.mkIf (cfg.memoryMax != null) cfg.memoryMax;
          MemoryHigh = lib.mkIf (cfg.memoryHigh != null) cfg.memoryHigh;
          CPUQuota = lib.mkIf (cfg.cpuQuota != null) cfg.cpuQuota;
        };
      };
    };
  };
}
