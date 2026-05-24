# sui nix-wrap migration-bridge nix-darwin module.
#
# Imports as `inputs.sui.darwinModules.nix-wrap`.  Installs the
# typed `nix-wrap` wrapper binary at the system level so every
# `nix <cmd> ...` invocation routes through sui's catalog:
#
#   * Working / SuiNative entries → run on sui
#   * everything else → fall back to cppnix
#
# The operator gets immediate sui-as-nix value for ~85% of the
# cppnix surface while M2.6+ work brings the rest to byte-identity.
# Both engines stay installed — the wrapper is a routing layer,
# not a replacement.
#
# Deploy: in the operator's nix-darwin flake,
#
#   {
#     inputs.sui.url = "github:pleme-io/sui";
#     outputs = { self, nixpkgs, sui, ... }: {
#       darwinConfigurations.cid = darwinSystem {
#         modules = [
#           sui.darwinModules.nix-wrap
#           ({ ... }: { services.sui-nix-wrap.enable = true; })
#         ];
#       };
#     };
#   }
#
# After `darwin-rebuild switch`, `which nix` resolves to nix-wrap.
# `nix --version` reports cppnix's version (routes to cppnix).
# `nix hash to-sri sha256:...` returns sui's output.  Every
# decision is logged to ~/.cache/sui/nix-wrap.log.

{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui-nix-wrap;
  inherit (lib) mkEnableOption mkOption types mkIf mkDefault;
in
{
  options.services.sui-nix-wrap = {
    enable = mkEnableOption "sui nix-wrap migration-bridge wrapper";

    suiPackage = mkOption {
      type = types.package;
      default = pkgs.sui or (throw "sui package not in pkgs overlay — add the sui overlay first");
      description = "The sui binary package to route Working/SuiNative commands to.";
    };

    wrapPackage = mkOption {
      type = types.package;
      default = pkgs.sui-nix-wrap or (throw "sui-nix-wrap package not in pkgs overlay — add the sui overlay first");
      description = "The nix-wrap binary package.";
    };

    cppnixPackage = mkOption {
      type = types.package;
      default = pkgs.nix;
      description = ''
        The cppnix binary package the wrapper falls back to for
        Stub/Partial/Missing commands.  Defaults to nixpkgs's nix.
      '';
    };

    logPath = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Override the routing-decision log path (defaults to
        ~/.cache/sui/nix-wrap.log per the wrapper's built-in
        resolution).  Set to null to use the default.
      '';
    };
  };

  config = mkIf cfg.enable {
    # Install all three binaries.  The wrap binary becomes `nix`
    # at the user's PATH; cppnix is installed as `cppnix` so the
    # wrapper can find it; sui is installed for the wrapper to
    # route Working/SuiNative commands to.
    environment.systemPackages = [
      cfg.wrapPackage   # provides /run/current-system/sw/bin/nix
      cfg.suiPackage    # provides /run/current-system/sw/bin/sui
      # cppnix is renamed at the install path via the symlink farm
      # below; the original `nix` binary is not added directly to
      # avoid colliding with the wrap binary's `nix` symlink.
    ];

    # Symlink-farm a `cppnix` binary at the system PATH pointing at
    # the original nix.  The wrapper falls back to this when its
    # NIX_WRAP_CPPNIX_BIN env var isn't set.
    system.activationScripts.suiNixWrapCppnixSymlink.text = ''
      mkdir -p /run/current-system/sw/bin
      if [ ! -L /run/current-system/sw/bin/cppnix ]; then
        ln -sf ${cfg.cppnixPackage}/bin/nix /run/current-system/sw/bin/cppnix
      fi
    '';

    # Set the env var so the wrapper finds cppnix even when the
    # symlink isn't in PATH (e.g. early system activation, GUI
    # apps, ssh non-interactive shells).
    environment.variables.NIX_WRAP_CPPNIX_BIN =
      "${cfg.cppnixPackage}/bin/nix";

    # Sui binary path likewise — the wrapper consults this env var
    # first, falling back to /run/current-system/sw/bin/sui then to
    # `sui` on PATH.
    environment.variables.NIX_WRAP_SUI_BIN =
      "${cfg.suiPackage}/bin/sui";
  };
}
