{
  lib,
  stdenv,
  runCommand,
  # crane's library for this nixpkgs (`crane.mkLib pkgs`).
  craneLib,
  installShellFiles,
  # Git commit of the flake, shown in `--version` and the dashboard's About (null: unknown).
  rev ? null,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
  inherit (cargoToml.package) version;
  # The package's own version, normalized out of everything the dependency build
  # sees, so a release bump keeps that derivation (and the caches holding it).
  normalized =
    file:
    builtins.toFile (baseNameOf file) (
      builtins.replaceStrings
        [ "name = \"browser-daemon\"\nversion = \"${version}\"" ]
        [ "name = \"browser-daemon\"\nversion = \"0.0.0\"" ]
        (builtins.readFile file)
    );
  depsLock = normalized ../Cargo.lock;
  common = {
    pname = "browser-daemon";
    inherit version src;
    strictDeps = true;
    # Vendored crates don't depend on the package's version: shared by both builds.
    cargoVendorDir = craneLib.vendorCargoDeps { cargoLock = depsLock; };
    cargoExtraArgs = "--locked";
  };
  # Only the normalized manifests (eval-time files) and a stub main, unlike crane's
  # mkDummySrc, which also copies the real ones.
  depsSrc = runCommand "browser-daemon-deps-src" { } ''
    mkdir -p $out/src
    cp ${normalized ../Cargo.toml} $out/Cargo.toml
    cp ${depsLock} $out/Cargo.lock
    echo 'fn main() {}' > $out/src/main.rs
  '';
  # The dependencies, built once: source changes and version bumps reuse them.
  deps = craneLib.buildDepsOnly (
    # dummySrc replaces src; passing both makes crane warn.
    builtins.removeAttrs common [ "src" ]
    // {
      pname = "browser-daemon-deps";
      version = "0";
      dummySrc = depsSrc;
    }
  );
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
in
craneLib.buildPackage (
  common
  // {
    cargoArtifacts = deps;
    env = lib.optionalAttrs (rev != null) { BROWSER_DAEMON_REV = rev; };

    nativeBuildInputs = [ installShellFiles ];

    # The unit tests run here, reusing the built dependencies.
    doCheck = true;
    # CI builds aarch64 under qemu-user, which fakes /proc: the relay's
    # socket -> process lookup cannot find the test process there.
    cargoTestExtraArgs = lib.optionalString stdenv.hostPlatform.isAarch64 (
      "-- --skip=relay::tests::serves_only_the_browsers_processes"
      + " --skip=relay::tests::a_strict_relay_refuses_loopback_with_a_ruleset_reply"
    );

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
)
