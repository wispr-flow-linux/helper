{
  description = "Clean-room Wispr Flow Linux helper";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      packageFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "wispr-flow-linux-helper";
          version = "0.1.3";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
        };
    in
    {
      packages = forEachSystem (system: {
        default = packageFor system;
      });

      checks = forEachSystem (system: {
        unit-tests = packageFor system;
      });

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
