# sui-as-nix — NixOS module that replaces cppnix with the `sui-as-nix`
# symlink-farm package as the system-wide `nix` binary.
#
# Imports as `inputs.sui.nixosModules.default-as-nix`.  Enabling this
# module sets `nix.package` to the sui binary surface; cppnix is no
# longer the dispatch target for `nix`, `nix-build`, `nix-store`, …
#
# ## ⚠ Readiness
#
# sui-eval's module-system fixed-point (`lib.evalModules` under
# `nixosConfigurations.<host>.config.system.build.toplevel`) is the
# M2.6 lattice item in `pleme-io/sui` and DIVERGES today on real
# NixOS flakes.  Enabling this module on a production host will
# break `nixos-rebuild` until M2.6 lands.  Track via
# `sui-sweep --corpus rebuild` against the host's flake; cut over
# only when the rebuild corpus is Match-clean.
#
# The wiring itself is sound: when M2.6 lands, the operator flips
# `services.sui-as-nix.enable = true;`, rebuilds once with cppnix
# (transition build), and every subsequent rebuild is sui-driven
# with cppnix bytecode-isolated to the bootloader-selectable
# previous generations.
#
# ## Usage (operator)
#
#   {
#     inputs.sui.url = "github:pleme-io/sui";
#     outputs = { self, nixpkgs, sui, ... }: {
#       nixosConfigurations.rio = nixpkgs.lib.nixosSystem {
#         system = "x86_64-linux";
#         modules = [
#           ./configuration.nix
#           sui.nixosModules.default-as-nix
#           ({ ... }: {
#             services.sui-as-nix.enable = true;
#             services.sui-as-nix.package =
#               sui.packages.x86_64-linux.sui-as-nix;
#           })
#         ];
#       };
#     };
#   }

{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui-as-nix;
  inherit (lib) mkEnableOption mkOption types mkIf;
in
{
  options.services.sui-as-nix = {
    enable = mkEnableOption ''
      sui as the system-wide nix replacement (no cppnix fallback)

      WARNING: requires M2.6 lattice completion in pleme-io/sui;
      enabling on a host where `sui-sweep --corpus rebuild` is not
      Match-clean will break `nixos-rebuild`.
    '';

    package = mkOption {
      type = types.package;
      description = ''
        The sui-as-nix package — a symlinkJoin of the sui binary with
        per-legacy-name symlinks (`nix`, `nix-build`, `nix-store`,
        `nix-env`, `nix-shell`, `nix-instantiate`,
        `nix-collect-garbage`, `nix-hash`, `nix-copy-closure`,
        `nix-channel`, `nix-daemon`, `nix-prefetch-url`) all pointing
        at `bin/sui`.  argv[0] dispatch in sui rewrites each legacy
        invocation into the modern `sui <subcommand>` form.
      '';
      default = pkgs.sui-as-nix or
        (throw "sui-as-nix package not in pkgs overlay — set services.sui-as-nix.package explicitly");
    };
  };

  config = mkIf cfg.enable {
    # Single load-bearing line: cppnix gives way to sui across every
    # NixOS reference to `nix.package` (system PATH, activation
    # scripts, profile management, GC, daemon).
    nix.package = cfg.package;
  };
}
