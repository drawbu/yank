{
  description = "yank: a peer-to-peer clipboard daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      # Linux only: yank reads the clipboard through the Wayland
      # data-control protocols, which is all it knows how to do.
      forEachSystem = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};

          yank = pkgs.rustPlatform.buildRustPackage {
            pname = "yank";
            version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;

            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            # The tests bring up two daemons and pair them over hermetic
            # iroh endpoints, which needs more of a network than the build
            # sandbox has.
            doCheck = false;

            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installShellCompletion --cmd yank \
                --bash <(COMPLETE=bash $out/bin/yank) \
                --fish <(COMPLETE=fish $out/bin/yank) \
                --zsh <(COMPLETE=zsh $out/bin/yank)
            '';

            meta = {
              description = "Peer-to-peer clipboard daemon";
              license = nixpkgs.lib.licenses.wtfpl;
              mainProgram = "yank";
              platforms = nixpkgs.lib.platforms.linux;
            };
          };
        in
        {
          default = yank;
          inherit yank;
        }
      );

      overlays.default = final: prev: {
        inherit (self.packages.${final.stdenv.hostPlatform.system}) yank;
      };

      homeModules = rec {
        yank = import ./nix/home-module.nix self;
        default = yank;
      };

      nixosModules = rec {
        yank = import ./nix/nixos-module.nix self;
        default = yank;
      };

      devShells = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.yank ];
            env.RUSTUP_TOOLCHAIN = pkgs.rustc.version;
            packages = with pkgs; [
              rustup
              wl-clipboard
            ];
          };
        }
      );

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt-tree);
    };
}
