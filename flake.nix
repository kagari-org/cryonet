{
  outputs = inputs@{
    self, nixpkgs, flake-parts,
  }: flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    perSystem = { pkgs, ... }: let
      package = {
        lib,
        buildGoModule,
        buf,
        protoc-gen-go,
        protoc-gen-connect-go,
      }: buildGoModule {
        name = "cryonet";
        src = with lib.fileset; toSource {
          root = ./.;
          fileset = unions [
            ./cryonet
            ./proto
            ./main.go
            ./go.mod
            ./go.sum
            ./Makefile
          ];
        };
        nativeBuildInputs = [ buf protoc-gen-go protoc-gen-connect-go ];
        vendorHash = "sha256-iJO07Vcx0MRe6STyQ/iNKtGbYkpdxNmjuH2ZeaVaehw=";
        overrideModAttrs.preBuild = ''
          export HOME=$(pwd)/.home
          mkdir -p $HOME
          make proto
        '';
        buildPhase = ''
          export HOME=$(pwd)/.home
          mkdir -p $HOME
          make
        '';
        installPhase = ''
          mkdir -p $out/bin
          install -m755 -t $out/bin build/cryonet
        '';
      };
    in {
      packages.default = pkgs.callPackage package {};
      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs; [];
        nativeBuildInputs = with pkgs; [
          buf protoc-gen-go protoc-gen-connect-go
        ];
      };
    };
  };
}
