# Home-manager module for (not)Handy speech-to-text
#
# Provides a systemd user service for autostart.
# Usage: imports = [ handy.homeManagerModules.default ];
#        services.handy.enable = true;
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.handy;
in
{
  options.services.handy = {
    enable = lib.mkEnableOption "(not)Handy speech-to-text user service";

    package = lib.mkOption {
      type = lib.types.package;
      defaultText = lib.literalExpression "handy.packages.\${system}.handy";
      description = "The (not)Handy package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.handy = {
      Unit = {
        Description = "(not)Handy speech-to-text";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/not-handy";
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
