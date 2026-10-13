{
  lib,
  rustPlatform,
  # Git commit of the flake, shown in the dashboard's About (null: unknown).
  rev ? null,
}:
rustPlatform.buildRustPackage {
  pname = "browser-daemon";
  version = "0.1.0";

  env = lib.optionalAttrs (rev != null) { BROWSER_DAEMON_REV = rev; };

  src = lib.cleanSourceWith {
    src = ./..;
    filter =
      path: _type:
      let
        base = baseNameOf path;
      in
      !(builtins.elem base [
        ".git"
        "target"
        "result"
      ]);
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  meta = {
    description = "Multi-tenant Chromium CDP daemon with virtual display isolation and mobile handoff";
    homepage = "https://github.com/pschmitt/browser-daemon";
    license = lib.licenses.gpl3Only;
    maintainers = [ ];
    mainProgram = "browser-daemon";
  };
}
