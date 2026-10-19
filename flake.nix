{
  description = "browser-daemon - remote Chromium and Firefox sessions with CDP multiplexing, virtual displays and mobile handoff";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      # Uncommitted builds report "<sha>-dirty".
      rev = self.rev or self.dirtyRev or null;
      # The module and the browsers it runs are Linux-only.
      eachSystem = flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          browser-daemon = pkgs.callPackage ./nix/package.nix { inherit rev; };
          browser = import ./nix/browser.nix { inherit pkgs; };
          adblock = pkgs.callPackage ./nix/adblock.nix { };
        in
        {
          packages = {
            default = browser-daemon;
            inherit browser-daemon;
            inherit (browser) firefox-addon chromium-addon browser-native-host;
            inherit (adblock) ublock-origin-lite ublock-origin-xpi;
          };

          apps.default = {
            type = "app";
            program = "${browser-daemon}/bin/browser-daemon";
          };

          # Builds the package (which runs the unit tests) and checks the Nix formatting.
          checks = {
            inherit browser-daemon;
            nixfmt =
              pkgs.runCommand "browser-daemon-nixfmt"
                {
                  nativeBuildInputs = [ pkgs.nixfmt ];
                }
                ''
                  cd ${self}
                  nixfmt --check flake.nix nix/*.nix
                  touch $out
                '';
          };

          formatter = pkgs.nixfmt;

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              chromium
              xorg-server
              curl
            ];
          };
        }
      );
    in
    eachSystem
    // {
      overlays.default = _final: prev: {
        browser-daemon = prev.callPackage ./nix/package.nix { inherit rev; };
      };

      # The flake's own package is the default, so the module needs no overlay
      # (and the binary knows its git revision).
      nixosModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.browser-daemon.package =
            lib.mkDefault
              self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      nixosModules.browser-daemon = self.nixosModules.default;
    };
}
