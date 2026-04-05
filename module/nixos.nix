{ config, lib, pkgs, ... }:
let
  cfg = config.services.sui;
  inherit (lib) mkEnableOption mkOption types mkIf;
in
{
  options.services.sui = {
    enable = mkEnableOption "Sui Nix daemon (system-level)";
    package = mkOption {
      type = types.package;
      default = pkgs.sui or (throw "sui package not in pkgs overlay");
      description = "The sui package to use.";
    };
    listenAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:8080";
      description = "REST/GraphQL listen address.";
    };
    grpcListenAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:50051";
      description = "gRPC listen address.";
    };
  };

  config = mkIf cfg.enable {
    systemd.services.sui = {
      description = "Sui Nix Management Daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/sui serve --listen ${cfg.listenAddress} --grpc-listen ${cfg.grpcListenAddress}";
        Restart = "always";
        RestartSec = 5;
        DynamicUser = true;
        StateDirectory = "sui";
        LogsDirectory = "sui";
      };
    };
  };
}
