{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui-as-nix;
  inherit (lib) mkEnableOption mkOption types mkIf mkForce;
  cppnixAlias = pkgs.linkFarm "cppnix" [
    {
      name = "bin/cppnix";
      path = "${cfg.cppnixPackage}/bin/nix";
    }
  ];
in
{
  options.services.sui-as-nix = {
    enable = mkEnableOption "sui as the nix-darwin nix: the CLI, the legacy nix-* tools, darwin-rebuild and gitops";

    package = mkOption {
      type = types.package;
      description = "The sui-as-nix package: sui linked as nix and as every legacy nix-* name.";
    };

    cppnixPackage = mkOption {
      type = types.package;
      default = pkgs.nix;
      description = "The cppnix package kept installed as `cppnix`, and the daemon when `daemon` is \"cppnix\".";
    };

    daemon = mkOption {
      type = types.enum [ "cppnix" "sui" ];
      default = "cppnix";
      description = "The engine behind the org.nixos.nix-daemon launchd job, the store's only writer.";
    };
  };

  config = mkIf cfg.enable {
    nix.package = cfg.package;
    system.tools.darwin-rebuild.enable = false;
    environment.etc."sui/cppnix-passthrough".text = "off\n";
    environment.systemPackages = [ cppnixAlias ];
    launchd.daemons.nix-daemon.command = mkIf (cfg.daemon == "cppnix") (
      mkForce (lib.getExe' cfg.cppnixPackage "nix-daemon")
    );
  };
}
