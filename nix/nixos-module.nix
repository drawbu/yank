# NixOS module: yank as a user service, for systems not using Home Manager.
#
# It installs the package and the unit; the configuration file lives in
# each user's home, so `config.toml` is theirs to write. Use the Home
# Manager module instead when you want it managed declaratively.
#
# Do not also run `yank service install`: that writes a second unit into
# the user's own systemd directory, which shadows this one.

self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.yank;
in
{
  options.services.yank = {
    enable = lib.mkEnableOption "yank, a clipboard shared across your machines";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.yank;
      defaultText = lib.literalMD "the package built by the yank flake";
      description = "The yank package providing the daemon and the CLI.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "yank=info";
      description = "Value of `RUST_LOG` for the daemon.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    systemd.user.services.yank = {
      description = "yank clipboard daemon";
      after = [ "graphical-session.target" ];
      wantedBy = [ "default.target" ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 5;
        Environment = [ "RUST_LOG=${cfg.logLevel}" ];
        LimitCORE = 0;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateMounts = true;
        ProtectKernelLogs = true;
        RestrictSUIDSGID = true;
      };
    };
  };
}
