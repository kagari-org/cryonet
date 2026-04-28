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
    iso = (nixpkgs.lib.nixosSystem {
      system = "i686-linux";
      modules = [
        "${nixpkgs}/nixos/modules/installer/cd-dvd/iso-image.nix"
        ({ pkgs, ... }: {
          system.stateVersion = "25.11";
          boot.initrd = {
            systemd.enable = true;
            # https://github.com/copy/v86/issues/146#issuecomment-297871746
            kernelModules = [ "vfat" "nls_cp437" "nls_iso8859_1" "atkbd" "i8042" "virtio_net" ];
          };
          fileSystems."/info" = {
            device = "/dev/sda1";
            neededForBoot = true;
          };
          users.users.root.password = "root";
          services.getty.autologinUser = "root";
          networking.useNetworkd = true;
          networking.usePredictableInterfaceNames = false;

          systemd.services.setup-eth0 = {
            wantedBy = [ "sys-subsystem-net-devices-eth0.device" ];
            bindsTo = [ "sys-subsystem-net-devices-eth0.device" ];
            path = with pkgs; [ jq iproute2 ];
            script = ''
              MAC=$(jq -r '.mac' /info/info)
              ADDRESS=$(jq -r '.address' /info/info)/24
              ip link set eth0 down
              ip link set eth0 address $MAC
              ip address add $ADDRESS dev eth0
              ip link set eth0 up
            '';
          };

          environment.systemPackages = with pkgs; [ iperf3 ];
        })
      ];
    }).config.system.build.isoImage;
  in flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { self', pkgs, ... }: {
      packages.default = pkgs.callPackage cryonet {};
      packages.wasm = pkgs.callPackage cryonet-wasm {};
      packages.iso = iso;
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
