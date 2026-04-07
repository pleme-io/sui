# sui home-manager module — runs the sui daemon as a user-level service.
#
# Namespace: services.sui.*
#
# Use this module when you want sui to run under your own user account
# (e.g. on a developer workstation). Backed by launchd agents on darwin
# and systemd user services on linux. For multi-user / system-wide
# deployment use the matching nixosModules / darwinModules instead.
{ hmHelpers }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui;
  inherit (lib) mkEnableOption mkOption types mkIf;
  inherit (hmHelpers) mkLaunchdService mkSystemdService;
in
{
  options.services.sui = {
    enable = mkEnableOption "sui Nix management daemon (user-level)";

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

    logDir = mkOption {
      type = types.str;
      default = "${config.home.homeDirectory}/.local/share/sui/logs";
      description = "Log directory.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Extra arguments to pass to `sui serve`.";
    };
  };

  config = mkIf cfg.enable (
    let
      args = [
        "serve"
        "--listen" cfg.listenAddress
        "--grpc-listen" cfg.grpcListenAddress
      ] ++ cfg.extraArgs;
    in
    lib.mkMerge [
      # Darwin (launchd user agent)
      (mkIf pkgs.stdenv.isDarwin (mkLaunchdService {
        name = "sui";
        label = "io.pleme.sui";
        command = "${cfg.package}/bin/sui";
        inherit args;
        logDir = cfg.logDir;
        keepAlive = true;
        runAtLoad = true;
      }))

      # Linux (systemd user service)
      (mkIf pkgs.stdenv.isLinux (mkSystemdService {
        name = "sui";
        description = "Sui Nix Management Daemon";
        command = "${cfg.package}/bin/sui";
        inherit args;
      }))
    ]
  );
}
