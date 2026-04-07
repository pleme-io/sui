# sui nix-darwin module — system-level launchd daemon.
#
# Namespace: services.sui.*
#
# Imports as `inputs.sui.darwinModules.default`. Runs the daemon
# under launchd at the system level (root) so it survives logout
# and is shared across users on the box. For per-user development
# use the home-manager module instead.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui;
  inherit (lib) mkEnableOption mkOption types mkIf;
in
{
  options.services.sui = {
    enable = mkEnableOption "sui Nix management daemon (system-level)";

    package = mkOption {
      type = types.package;
      default = pkgs.sui or (throw "sui package not in pkgs overlay");
      description = "The sui package to use.";
    };

    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      description = "REST/GraphQL listen address.";
    };

    grpcListenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:50051";
      description = "gRPC listen address.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Extra arguments to pass to `sui serve`.";
    };

    logFile = mkOption {
      type = types.str;
      default = "/var/log/sui.log";
      description = "Path for combined stdout/stderr.";
    };

    label = mkOption {
      type = types.str;
      default = "io.pleme.sui";
      description = "launchd label (also the service identifier).";
    };
  };

  config = mkIf cfg.enable {
    launchd.daemons.sui = {
      serviceConfig = {
        Label = cfg.label;
        ProgramArguments = [
          "${cfg.package}/bin/sui"
          "serve"
          "--listen" cfg.listenAddress
          "--grpc-listen" cfg.grpcListenAddress
        ] ++ cfg.extraArgs;
        RunAtLoad = true;
        KeepAlive = true;
        ProcessType = "Adaptive";
        StandardOutPath = cfg.logFile;
        StandardErrorPath = cfg.logFile;
        EnvironmentVariables = {
          PATH = lib.makeBinPath [ cfg.package ];
        };
      };
    };
  };
}
