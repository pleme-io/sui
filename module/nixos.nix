# sui NixOS module — system-level systemd service.
#
# Namespace: services.sui.*
#
# Imports as `inputs.sui.nixosModules.default` and runs the daemon
# under a DynamicUser with state in /var/lib/sui and logs in
# /var/log/sui (LogsDirectory). For per-user development, use the
# home-manager module instead.
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
      default = "0.0.0.0:8080";
      description = "REST/GraphQL listen address.";
    };

    grpcListenAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:50051";
      description = "gRPC listen address.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Extra arguments to pass to `sui serve`.";
    };

    user = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = ''
        User to run the daemon as. When `null` (default), systemd
        DynamicUser= is used and a transient `sui` UID is allocated
        per boot.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.services.sui = {
      description = "Sui Nix Management Daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = lib.mkMerge [
        {
          ExecStart = lib.escapeShellArgs ([
            "${cfg.package}/bin/sui"
            "serve"
            "--listen" cfg.listenAddress
            "--grpc-listen" cfg.grpcListenAddress
          ] ++ cfg.extraArgs);
          Restart = "always";
          RestartSec = 5;
          StateDirectory = "sui";
          LogsDirectory = "sui";
          # Hardening
          ProtectSystem = "strict";
          ProtectHome = "tmpfs";
          PrivateTmp = true;
          NoNewPrivileges = true;
        }
        (lib.mkIf (cfg.user == null) {
          DynamicUser = true;
        })
        (lib.mkIf (cfg.user != null) {
          User = cfg.user;
        })
      ];
    };
  };
}
