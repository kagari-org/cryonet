{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: let
    src = with nixpkgs.lib.fileset; toSource {
      root = ./.;
      fileset = unions [
        ./crates
        ./.cargo
        ./Cargo.toml
        ./Cargo.lock
      ];
    };
    cryonet = { rustPlatform }: rustPlatform.buildRustPackage {
      inherit src;
      name = "cryonet";
      RUSTC_BOOTSTRAP = "1";
      cargoLock.lockFile = ./Cargo.lock;
    };
    cryonet-wasm = { rustPlatform, wasm-pack, wasm-bindgen-cli, binaryen, lld }: rustPlatform.buildRustPackage {
      inherit src;
      name = "cryonet";
      RUSTC_BOOTSTRAP = "1";
      cargoLock.lockFile = ./Cargo.lock;
      doCheck = false;
      nativeBuildInputs = [ wasm-pack wasm-bindgen-cli binaryen lld ];
      buildPhase = ''
        HOME=$TMPDIR RUST_LOG=debug wasm-pack build crates/cryonet-lib \
          --mode no-install \
          --target web \
          --release \
          --out-dir $out
      '';
      installPhase = ":";
    };
  in flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, ... }: {
      imports = [ ./web ];
      packages.default = pkgs.callPackage cryonet {};
      packages.static = pkgs.pkgsStatic.callPackage cryonet {};
      packages.wasm = pkgs.callPackage cryonet-wasm {};
      packages.apk = pkgs.runCommand "cryonet-apk" {
        nativeBuildInputs = [ pkgs.apk-tools pkgs.fakeroot ];
      } ''
        mkdir -p $out root/usr/bin root/lib/netifd/proto root/www/luci-static/resources/protocol
        cp ${self'.packages.static}/bin/* root/usr/bin/
        install -m755 ${./openwrt/proto/cryonet.sh} root/lib/netifd/proto/cryonet.sh
        install -m644 ${./openwrt/luci/cryonet.js} root/www/luci-static/resources/protocol/cryonet.js
        fakeroot sh -c 'chown -R 0:0 root; exec apk mkpkg "$@"' sh --files root \
          --info name:cryonet \
          --info version:0.1.0 \
          --info description:'Cryonet mesh networking daemon and OpenWrt integration' \
          --output $out/cryonet.apk
      '';
      devShells.default = pkgs.mkShell {
        RUSTC_BOOTSTRAP = "1";
        inputsFrom = [ self'.packages.default self'.packages.wasm ];
        buildInputs = with pkgs; [];
        nativeBuildInputs = with pkgs; [
          clippy rustfmt yarn
        ];
        shellHook = ''
          rm -rf web/packages/cryonet-lib
          ln -sf ${self'.packages.wasm} web/packages/cryonet-lib
        '';
      };
    };
  };
}
