{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.browser-daemon;
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
      pkgs.noto-fonts-emoji
      pkgs.noto-fonts-cjk-sans
    ];

    systemd.services.browser-daemon-xvfb = {
      description = "Xvfb virtual display for browser-daemon";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${cfg.xvfbPackage}/bin/Xvfb ${cfg.display} -screen 0 ${toString cfg.defaultWidth}x${toString cfg.defaultHeight}x24 -nolisten tcp -noreset";
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
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/browser-daemon --bind ${cfg.bind} --chromium ${lib.getExe cfg.chromiumPackage} --display ${cfg.display} --profile-dir ${cfg.dataDir}/profiles --max-sessions ${toString cfg.maxSessions} --default-width ${toString cfg.defaultWidth} --default-height ${toString cfg.defaultHeight}";
        Restart = "always";
        RestartSec = 2;
        User = cfg.user;
        Group = cfg.group;
        StateDirectory = "browser-daemon";
        WorkingDirectory = cfg.dataDir;
      };
    };
  };
}
