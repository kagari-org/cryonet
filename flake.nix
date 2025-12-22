{
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: let
    cryonet = { rustPlatform }: rustPlatform.buildRustPackage {
      name = "cryonet";
      RUSTC_BOOTSTRAP = "1";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
    };
  in flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, ... }: {
      packages.default = pkgs.callPackage cryonet {};
      devShells.default = pkgs.mkShell {
        RUSTC_BOOTSTRAP = "1";
        inputsFrom = [ self'.packages.default ];
        buildInputs = with pkgs; [];
        nativeBuildInputs = with pkgs; [];
      };
    };
  };
}
