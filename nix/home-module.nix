# Home Manager module: yank as a user service.
#
# A user service, not a system one: the daemon needs the session to reach
# the compositor, and the clipboard belongs to the session. It is not bound
# to graphical-session.target on purpose, so it keeps replicating while the
# session is restarting and picks the compositor back up on its own.

self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.yank;
  tomlFormat = pkgs.formats.toml { };

  # config.toml is only managed when there is something to put in it.
  manageSettings = cfg.settings != { };
  settingsFile = tomlFormat.generate "yank-config.toml" cfg.settings;
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

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          # A machine with no graphical session.
          clipboard = false;
          history-limit = 200;
          secret-ttl = "45s";
        }
      '';
      description = ''
        Written to {file}`$XDG_CONFIG_HOME/yank/config.toml`. Left
        unmanaged, and editable by hand, when empty.

        Run `yank service restart` after changing it, or let the service
        restart itself: the unit is retriggered when this file changes.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "yank=info";
      example = "yank=debug";
      description = "Value of `RUST_LOG` for the daemon.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."yank/config.toml" = lib.mkIf manageSettings {
      source = settingsFile;
    };

    # Recorded so `yank service install` refuses to replace a unit it does
    # not own.
    xdg.configFile."yank/service.toml".source = tomlFormat.generate "yank-service.toml" {
      installer = "home-manager";
      label = "yank";
    };

    systemd.user.services.yank = {
      Unit = {
        Description = "yank clipboard daemon";
        # Wanted, not required: with no compositor the daemon still
        # replicates, and it connects to one as soon as there is one.
        After = [ "graphical-session.target" ];
      }
      // lib.optionalAttrs manageSettings {
        X-Restart-Triggers = [ (toString settingsFile) ];
      };

      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 5;
        Environment = [ "RUST_LOG=${cfg.logLevel}" ];

        # The daemon holds clipboard contents, which may be passwords.
        # None of this is a sandbox, but it keeps them out of a core dump
        # and off swap.
        LimitCORE = 0;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateMounts = true;
        ProtectKernelLogs = true;
        RestrictSUIDSGID = true;
      };

      Install.WantedBy = [ "default.target" ];
    };
  };
}
