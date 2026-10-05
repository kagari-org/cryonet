{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  outputs = inputs@{
    self, nixpkgs, flake-parts, rust-overlay,
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
    cryonet-android = { makeRustPlatform, rust-bin, androidenv, ktlint }: let
      rust = rust-bin.stable.latest.default.override {
        targets = [ "aarch64-linux-android" ];
      };
      rustPlatform = makeRustPlatform { cargo = rust; rustc = rust; };
      ndk = androidenv.androidPkgs.ndk-bundle;
      bin = "${ndk}/libexec/android-sdk/ndk/${ndk.version}/toolchains/llvm/prebuilt/linux-x86_64/bin";
      clang = "${bin}/aarch64-linux-android24-clang";
    in rustPlatform.buildRustPackage {
      inherit src;
      name = "cryonet-android";
      RUSTC_BOOTSTRAP = "1";
      cargoLock.lockFile = ./Cargo.lock;
      doCheck = false;
      nativeBuildInputs = [ ktlint ];
      CC_aarch64_linux_android = clang;
      CXX_aarch64_linux_android = "${clang}++";
      AR_aarch64_linux_android = "${bin}/llvm-ar";
      RANLIB_aarch64_linux_android = "${bin}/llvm-ranlib";
      CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = clang;
      buildPhase = ''
        cargo build --offline --release --package cryonet-lib --target aarch64-linux-android
        cargo build --offline --release --package uniffi-bindgen
      '';
      installPhase = ''
        mkdir -p $out/jniLibs/arm64-v8a $out/kotlin
        cp target/aarch64-linux-android/release/libcryonet_lib.so $out/jniLibs/arm64-v8a/
        target/release/uniffi-bindgen generate \
          --library $out/jniLibs/arm64-v8a/libcryonet_lib.so \
          --language kotlin \
          --out-dir $out/kotlin
      '';
    };
  in flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, system, ... }: {
      _module.args.pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config.allowUnfree = true;
      };
      imports = [ ./web ];
      packages.default = pkgs.callPackage cryonet {};
      packages.static = pkgs.pkgsStatic.callPackage cryonet {};
      packages.wasm = pkgs.callPackage cryonet-wasm {};
      packages.android = pkgs.callPackage cryonet-android {};
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
