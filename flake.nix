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
        "${nixpkgs}/nixos/modules/profiles/minimal.nix"
        "${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
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

          nix.enable = false;
          security.sudo.enable = false;
          services.lvm.enable = false;

          networking.firewall.enable = false;
          networking.useNetworkd = true;
          networking.usePredictableInterfaceNames = false;
          boot.kernel.sysctl = {
            "net.ipv4.ip_forward" = 1;
            "net.ipv6.conf.all.forwarding" = 1;
            "net.ipv4.conf.all.rp_filter" = 0;
          };

          systemd.services.setup-eth0 = {
            wantedBy = [ "sys-subsystem-net-devices-eth0.device" ];
            bindsTo = [ "sys-subsystem-net-devices-eth0.device" ];
            path = with pkgs; [ jq iproute2 ];
            script = ''
              MAC=$(jq -r '.mac' /info/info)
              IPV4=$(jq -r '.ipv4' /info/info)/32
              IPV6=$(jq -r '.ipv6' /info/info)/128
              ip link set eth0 down
              ip link set eth0 address $MAC
              ip address add $IPV4 dev eth0
              ip address add $IPV6 dev eth0
              ip link set eth0 up
            '';
          };

          systemd.services.bird.serviceConfig.ExecStartPre = "${pkgs.writeScript "generate-bird-config" ''
            #!${pkgs.runtimeShell}
            IPV4=$(${pkgs.jq}/bin/jq -r '.ipv4' /info/info)
            IPV6=$(${pkgs.jq}/bin/jq -r '.ipv6' /info/info)
            cat > /run/bird/static.conf <<EOF
            router id ''${IPV4};
            protocol static {
              route ''${IPV4}/32 via "eth0";
              ipv4 { table igp_v4; };
            }
            protocol static {
              route ''${IPV6}/128 via "eth0";
              ipv6 { table igp_v6; };
            };
            EOF
          ''}";
          services.bird = {
            enable = true;
            checkConfig = false;
            config = ''
              ipv4 table igp_v4;
              ipv6 table igp_v6;
              protocol device {}
              protocol kernel {
                learn;
                ipv4 {
                  import all;
                  export all;
                };
              }
              protocol kernel {
                learn;
                ipv6 {
                  import all;
                  export all;
                };
              }
              protocol pipe {
                  table igp_v4;
                  peer table master4;
                  import none;
                  export all;
              }
              protocol pipe {
                  table igp_v6;
                  peer table master6;
                  import none;
                  export all;
              }
              protocol babel {
                interface "eth0" { type tunnel; };
                ipv4 {
                  table igp_v4;
                  import all;
                  export where (source = RTS_STATIC) || (source = RTS_BABEL);
                };
                ipv6 {
                  table igp_v6;
                  import all;
                  export where (source = RTS_STATIC) || (source = RTS_BABEL);
                };
              }
              include "/run/bird/static.conf";
            '';
          };

          environment.systemPackages = with pkgs; [ iperf3 tcpdump ];
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
