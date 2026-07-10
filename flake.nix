{
  description = "dekit (SvenAndBits/mprocs fork) - process TUI with deps, healthchecks, hooks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Version is read from the crate manifest so the flake never drifts.
        cargoToml = builtins.fromTOML (builtins.readFile ./src/Cargo.toml);
      in
      {
        packages = rec {
          dekit = pkgs.rustPlatform.buildRustPackage {
            pname = "dekit";
            version = cargoToml.package.version;

            # `self` pins to this exact flake revision, so no source hash is
            # needed: `nix build github:SvenAndBits/mprocs/<ref>` builds <ref>.
            src = self;

            # Reading Cargo.lock directly fetches each crate by its own hash,
            # which removes the need for a manually-maintained cargoHash.
            # This works only because the lockfile has no git dependencies.
            cargoLock.lockFile = ./Cargo.lock;

            doCheck = false;

            meta = {
              description = "dekit - TUI for running multiple processes with dependency and health gating";
              homepage = "https://github.com/SvenAndBits/mprocs";
              license = pkgs.lib.licenses.mit;
              mainProgram = "dekit";
            };
          };
          default = dekit;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.dekit;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.dekit ];
          packages = with pkgs; [ cargo rustc rust-analyzer clippy rustfmt ];
        };
      });
}
