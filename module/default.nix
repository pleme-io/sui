{ hmHelpers }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui;
  inherit (lib) mkEnableOption mkOption types mkIf;
  inherit (hmHelpers) mkLaunchdService mkSystemdService;
in
{
  options.services.sui = {
    daemon = {
      enable = mkEnableOption "Sui Nix daemon";
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
    };
  };

  config = mkIf cfg.daemon.enable (
    let
      pkg = cfg.daemon.package;
      logDir = cfg.daemon.logDir;
    in
    lib.mkMerge [
      # Darwin (launchd)
      (mkIf pkgs.stdenv.isDarwin (mkLaunchdService {
        name = "sui-daemon";
        label = "io.pleme.sui";
        command = "${pkg}/bin/sui";
        args = [
          "serve"
          "--listen" cfg.daemon.listenAddress
          "--grpc-listen" cfg.daemon.grpcListenAddress
        ];
        inherit logDir;
        keepAlive = true;
        runAtLoad = true;
      }))

      # Linux (systemd)
      (mkIf pkgs.stdenv.isLinux (mkSystemdService {
        name = "sui-daemon";
        description = "Sui Nix Management Daemon";
        command = "${pkg}/bin/sui";
        args = [
          "serve"
          "--listen" cfg.daemon.listenAddress
          "--grpc-listen" cfg.daemon.grpcListenAddress
        ];
      }))
    ]
  );
}
