{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
  # Git commit of the flake, shown in `--version` and the dashboard's About (null: unknown).
  rev ? null,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "browser-daemon";
  inherit (cargoToml.package) version;

  env = lib.optionalAttrs (rev != null) { BROWSER_DAEMON_REV = rev; };

  # Everything but the docs, the Nix files and VCS/build leftovers, so that
  # editing those does not rebuild the binary.
  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.difference ./.. (
      lib.fileset.unions [
        ../README.md
        ../LICENSE
        ../dashboard.png
        ../dashboard-profiles.png
        ../nix
        ../browser
        ../flake.nix
        ../flake.lock
        ../justfile
        (lib.fileset.maybeMissing ../.github)
        (lib.fileset.maybeMissing ../.git)
        (lib.fileset.maybeMissing ../target)
        (lib.fileset.maybeMissing ../result)
      ]
    );
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  nativeBuildInputs = [ installShellFiles ];

  # CI builds aarch64 under qemu-user, which fakes /proc: the relay's
  # socket -> process lookup cannot find the test process there.
  checkFlags = lib.optionals stdenv.hostPlatform.isAarch64 [
    "--skip=relay::tests::serves_only_the_browsers_processes"
    "--skip=relay::tests::a_strict_relay_refuses_loopback_with_a_ruleset_reply"
  ];

  postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    for shell in bash zsh fish; do
      $out/bin/browser-daemon completions $shell > browser-daemon.$shell
    done
    installShellCompletion --cmd browser-daemon \
      --bash browser-daemon.bash \
      --zsh browser-daemon.zsh \
      --fish browser-daemon.fish
  '';

  meta = {
    description = "Multi-tenant Chromium CDP daemon with virtual display isolation and mobile handoff";
    homepage = "https://github.com/pschmitt/browser-daemon";
    license = lib.licenses.gpl3Only;
    maintainers = [ lib.maintainers.pschmitt ];
    platforms = lib.platforms.linux;
    mainProgram = "browser-daemon";
  };
}
