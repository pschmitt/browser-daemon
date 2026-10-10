{
  description = "browser-daemon - Multi-tenant Chromium CDP daemon with virtual display isolation and mobile handoff";

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
      eachSystem = flake-utils.lib.eachDefaultSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          browser-daemon = pkgs.callPackage ./nix/package.nix { };
        in
        {
          packages = {
            default = browser-daemon;
            inherit browser-daemon;
          };

          apps.default = {
            type = "app";
            program = "${browser-daemon}/bin/browser-daemon";
          };

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
        browser-daemon = prev.callPackage ./nix/package.nix { };
      };

      nixosModules.default = import ./nix/module.nix;
      nixosModules.browser-daemon = self.nixosModules.default;
    };
}
