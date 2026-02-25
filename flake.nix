{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: let
    cryonet = { rustPlatform }: rustPlatform.buildRustPackage {
      name = "cryonet";
      RUSTC_BOOTSTRAP = "1";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
    };
    cryonet-wasm = { rustPlatform, wasm-pack, wasm-bindgen-cli, binaryen, lld }: rustPlatform.buildRustPackage {
      name = "cryonet";
      RUSTC_BOOTSTRAP = "1";
      src = ./.;
      cargoLock.lockFile = ./Cargo.lock;
      doCheck = false;
      nativeBuildInputs = [ wasm-pack wasm-bindgen-cli binaryen lld ];
      buildPhase = ''
        HOME=$TMPDIR RUST_LOG=debug wasm-pack build crates/cryonet-lib \
          --target web \
          --release \
          --out-dir $out
      '';
      installPhase = ":";
    };
  in flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, ... }: {
      packages.default = pkgs.callPackage cryonet {};
      packages.wasm = pkgs.callPackage cryonet-wasm {};
      devShells.default = pkgs.mkShell {
        RUSTC_BOOTSTRAP = "1";
        inputsFrom = [ self'.packages.default self'.packages.wasm ];
        buildInputs = with pkgs; [];
        nativeBuildInputs = with pkgs; [
          clippy
        ];
      };
    };
  };
}
