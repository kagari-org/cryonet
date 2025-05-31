{
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { pkgs, self', ... }: let

      cryonet = { lib, rustPlatform, openssl, pkg-config, ... }: rustPlatform.buildRustPackage {
        name = "cryonet";
        RUSTC_BOOTSTRAP = true;
        cargoLock.lockFile = ./Cargo.lock;
        buildInputs = [ openssl ];
        nativeBuildInputs = [ pkg-config ];
        src = with lib.fileset; toSource {
          root = ./.;
          fileset = unions [
            ./src
            ./Cargo.toml
            ./Cargo.lock
          ];
        };
      };

    in {
      packages.default = pkgs.callPackage cryonet {};
      packages.static = pkgs.pkgsStatic.callPackage cryonet {};
      devShells.default = pkgs.mkShell {
        RUSTC_BOOTSTRAP = true;
        inputsFrom = [ self'.packages.default ];
      };
    };
  };
}
