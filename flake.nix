{
  description = "yank: a peer-to-peer clipboard daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
  };

  outputs =
    inputs@{ self, flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake = {
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
      };

      perSystem =
        {
          lib,
          pkgs,
          self',
          ...
        }:
        let
          gitRev = self.rev or self.dirtyRev or null;
          packageVersion = (lib.importTOML ./Cargo.toml).package.version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
            ];
          };
          yank = pkgs.rustPlatform.buildRustPackage {
            pname = "yank";
            version = "${packageVersion}-unstable-${if gitRev != null then gitRev else "dirty"}";

            inherit src;
            cargoLock.lockFile = ./Cargo.lock;

            useNextest = true;

            env = {
              NIX_YANK_GIT_HASH = gitRev;
            };

            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installShellCompletion --cmd yank \
                --bash <(COMPLETE=bash $out/bin/yank) \
                --fish <(COMPLETE=fish $out/bin/yank) \
                --zsh <(COMPLETE=zsh $out/bin/yank)
            '';

            meta = {
              description = "Peer-to-peer clipboard daemon";
              license = lib.licenses.wtfpl;
              mainProgram = "yank";
              platforms = lib.platforms.linux;
            };
          };
        in
        {

          packages.default = self'.packages.yank;
          packages.yank = yank;
          packages.yank-stable = yank.overrideAttrs {
            __intentionallyOverridingVersion = true;
            version = packageVersion;
          };

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo-nextest
              rustup
              wl-clipboard
            ];
          };

          formatter = pkgs.nixfmt-tree;
        };
    };
}
