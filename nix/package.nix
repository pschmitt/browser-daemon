{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "browser-daemon";
  version = "0.1.0";

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
