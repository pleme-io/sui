# sui-converge — a nix-darwin module that runs the continuous system-reconcile
# loop as a KeepAlive launchd daemon. The node is kept **always rebuilt into
# place**: `sui system converge --watch` streams FSEvents on the flake source +
# a drift-catch interval and converges the live system to its declared toplevel
# whenever they drift.
#
# This is the pleme-io shape (generation over composition): the launchd plist is
# GENERATED from this typed module, never hand-authored. It lives in `sui/contrib/`
# as the reusable surface; ENABLING it on a host (e.g. cid) is a change to that
# host's PRIVATE nix config (the `nix` repo) — import this module + set the flake:
#
#   imports = [ inputs.sui + "/contrib/launchd/sui-converge.nix" ];
#   services.sui-converge = {
#     enable  = true;
#     package = inputs.sui.packages.${pkgs.system}.default;   # the `sui` binary
#     flake   = "path:/Users/you/code/github/pleme-io/nix#${config.networking.hostName}";
#     # action = "dry-activate";   # start in SHADOW (observe, never converge) to trust it first
#   };
#
# NixOS peer: the same shape is a `systemd.services.sui-converge` unit — a small
# follow-up when the first NixOS node adopts it.

{ config, lib, pkgs, ... }:

let
  cfg = config.services.sui-converge;
in
{
  options.services.sui-converge = {
    enable = lib.mkEnableOption "the sui continuous system-reconcile daemon (always rebuilt into place)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The sui package providing the `sui` binary.";
    };

    flake = lib.mkOption {
      type = lib.types.str;
      example = "path:/Users/you/code/github/pleme-io/nix#cid";
      description = ''
        The flake reference (INCLUDING the host attribute) this node is kept in
        place against — e.g. `path:/…/nix#cid`. This is the same reference
        `sui system rebuild --flake` takes.
      '';
    };

    action = lib.mkOption {
      type = lib.types.enum [ "switch" "boot" "test" "dry-activate" "build" ];
      default = "switch";
      description = ''
        The converge action taken on drift. `switch` (the default) keeps the
        node live-in-place. `dry-activate` is the SHADOW posture — it builds +
        diffs the desired toplevel but activates NOTHING; run the loop this way
        first to watch what it *would* do before trusting it to converge.
      '';
    };

    intervalSecs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = ''
        The drift-catch interval, in seconds — how often the loop re-checks even
        when no source file changed (catches out-of-band drift). Source changes
        fire a reconcile immediately via FSEvents regardless of this.
      '';
    };

    logPath = lib.mkOption {
      type = lib.types.str;
      default = "/var/log/sui-converge.log";
      description = "Where the daemon's stdout+stderr are written.";
    };
  };

  config = lib.mkIf cfg.enable {
    launchd.daemons.sui-converge = {
      serviceConfig = {
        ProgramArguments = [
          "${cfg.package}/bin/sui"
          "system"
          "converge"
          "--flake"
          cfg.flake
          "--watch"
          "--interval-secs"
          (toString cfg.intervalSecs)
          "--action"
          cfg.action
        ];
        # The loop must always run — restart it if it ever exits.
        KeepAlive = true;
        RunAtLoad = true;
        ProcessType = "Background";
        StandardOutPath = cfg.logPath;
        StandardErrorPath = cfg.logPath;
        # A launchd DAEMON (not a user agent) runs as root — which the mutating
        # converge requires: it advances the root-owned system profile and runs
        # the activate script. `sui`'s fail-closed root gate means a non-root run
        # would refuse to touch the live system anyway, so root is mandatory for
        # a `switch`/`boot`/`test` action (a `dry-activate` shadow run needs none).
      };
    };
  };
}
