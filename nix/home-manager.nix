# The local tab API's server as a socket-activated systemd user service
# (docs/local-api.md). Register the native messaging host with the browsers
# separately: add the flake's `browser-native-host` package to
# programs.firefox.nativeMessagingHosts / programs.chromium.nativeMessagingHosts.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.browser-daemon-local;
  tokenType = lib.types.submodule {
    options = {
      id = lib.mkOption {
        type = lib.types.strMatching "[0-9a-f]+";
        description = "The token's id (the hex part after `bdl_`).";
      };
      sha256 = lib.mkOption {
        type = lib.types.strMatching "[0-9a-f]{64}";
        description = ''
          Hex SHA-256 of the token's secret part. `browser-daemon local token
          create --no-save` prints a token and its entry; keep the token itself
          out of the Nix store (for example in a sops secret for its client).
        '';
      };
      scopes = lib.mkOption {
        type = lib.types.nonEmptyListOf (
          lib.types.enum [
            "tabs:read"
            "tabs:control"
            "media:control"
            "tabs:content"
            "tabs:capture"
            "tabs:debug"
            "events:read"
          ]
        );
        description = "What the token may do.";
      };
    };
  };
  tokenConfig = (pkgs.formats.json { }).generate "browser-daemon-local-tokens.json" {
    tokens = lib.mapAttrsToList (name: t: {
      inherit name;
      inherit (t) id sha256 scopes;
    }) cfg.tokens;
  };
  socketUnit = name: path: {
    Unit.Description = "Browser Daemon local tab API (${name} socket)";
    Socket = {
      ListenStream = path;
      SocketMode = "0600";
      DirectoryMode = "0700";
      FileDescriptorName = name;
      Service = "browser-daemon-local.service";
    };
    Install.WantedBy = [ "sockets.target" ];
  };
in
{
  options.services.browser-daemon-local = {
    enable = lib.mkEnableOption "the Browser Daemon local tab API server";
    package = lib.mkOption {
      type = lib.types.package;
      description = "The browser-daemon package.";
    };
    tokens = lib.mkOption {
      type = lib.types.attrsOf tokenType;
      default = { };
      description = ''
        Tokens the server accepts, by name, in addition to those `browser-daemon
        local token create` saves in ~/.config/browser-daemon/local-tokens.json.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    systemd.user = {
      sockets.browser-daemon-local-api = socketUnit "api" "%t/browser-daemon/local.sock";
      sockets.browser-daemon-local-feeder = socketUnit "feeder" "%t/browser-daemon/feeder.sock";
      services.browser-daemon-local = {
        Unit = {
          Description = "Browser Daemon local tab API";
          Requires = [
            "browser-daemon-local-api.socket"
            "browser-daemon-local-feeder.socket"
          ];
        };
        Service = {
          ExecStart = lib.escapeShellArgs [
            (lib.getExe cfg.package)
            "local"
            "serve"
            "--token-config"
            tokenConfig
          ];
          Sockets = [
            "browser-daemon-local-api.socket"
            "browser-daemon-local-feeder.socket"
          ];
          Restart = "on-failure";
          # It only reads its token files and talks over its sockets.
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
        };
      };
    };
  };
}
