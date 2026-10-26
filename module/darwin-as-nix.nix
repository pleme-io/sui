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

    compatibleNixVersion = mkOption {
      type = types.str;
      default = cfg.cppnixPackage.version;
      defaultText = lib.literalExpression "config.services.sui-as-nix.cppnixPackage.version";
      description = "The nix version sui answers for: the version nix-darwin reads from nix.package for its feature checks (isNixAtLeast).";
    };

    cppnixAlias.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Install cppnixPackage as the `cppnix` command, the oracle beside sui.";
    };

    nixConsumers = {
      enable = mkEnableOption "build the packages that call nix as a CLI against sui instead of cppnix";
      packages = mkOption {
        type = types.listOf types.str;
        default = [ "nix-direnv" "nix-index" "bundix" "nurl" "nixos-option" ];
        description = "nixpkgs attributes whose `nix` argument is set to the sui-as-nix package.";
      };
    };

    daemon = mkOption {
      type = types.enum [ "cppnix" "sui" ];
      default = "cppnix";
      description = "The engine behind the org.nixos.nix-daemon launchd job, the store's only writer.";
    };
  };

  config = mkIf cfg.enable {
    nix.package = cfg.package.overrideAttrs (_: { version = cfg.compatibleNixVersion; });
    system.tools.darwin-rebuild.enable = false;
    environment.etc."sui/cppnix-passthrough".text = "off\n";
    environment.systemPackages = lib.optional cfg.cppnixAlias.enable cppnixAlias;
    nixpkgs.overlays = lib.optional cfg.nixConsumers.enable (
      _final: prev: lib.genAttrs cfg.nixConsumers.packages (name: prev.${name}.override { nix = cfg.package; })
    );
    launchd.daemons.nix-daemon.command = mkForce (
      if cfg.daemon == "cppnix" then
        lib.getExe' cfg.cppnixPackage "nix-daemon"
      else
        "${lib.getExe' cfg.package "sui"} daemon --socket /nix/var/nix/daemon-socket/socket"
    );
  };
}
