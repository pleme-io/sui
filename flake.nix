{
  description = "Sui (粋) — Rust-native Nix replacement with API-first design";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    crate2nix.url = "github:nix-community/crate2nix";
    flake-utils.url = "github:numtide/flake-utils";
    substrate = {
      url = "github:pleme-io/substrate";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    forge = {
      url = "github:pleme-io/forge";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crate2nix, flake-utils, substrate, forge, ... }: let
    # CLI tool release (4-target GitHub releases)
    toolOutputs = (import "${substrate}/lib/rust-workspace-release-flake.nix" {
      inherit nixpkgs crate2nix flake-utils;
    }) {
      toolName = "sui";
      packageName = "sui";
      src = self;
      repo = "pleme-io/sui";
    };

    # Docker image (substrate pattern — crate2nix, per-crate caching)
    imageOutputs = (import "${substrate}/lib/rust-tool-image-flake.nix" {
      inherit nixpkgs crate2nix flake-utils forge;
    }) {
      toolName = "sui";
      packageName = "sui";
      src = self;
      repo = "pleme-io/sui";
      architectures = [ "amd64" ];
      env = [
        "RUST_LOG=info"
      ];
    };
  in
    toolOutputs
    // {
      # Merge image outputs under namespaced keys
      packages = nixpkgs.lib.genAttrs
        [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ]
        (system:
          (toolOutputs.packages.${system} or {})
          // (let img = imageOutputs.packages.${system} or {}; in {
            dockerImage-amd64 = img.dockerImage-amd64 or null;
          })
        );

      # Image release app
      apps = nixpkgs.lib.genAttrs
        [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ]
        (system:
          (toolOutputs.apps.${system} or {})
          // {
            release-image = (imageOutputs.apps.${system} or {}).release or {
              type = "app";
              program = "echo 'image release not available on ${system}'";
            };
          }
        );
    }
    // {
      # Three module flavors share the same `services.sui.*` namespace
      # but bind to the right service manager for each platform.
      #
      # - homeManagerModules.default → user-level (launchd agent on
      #   darwin, systemd user service on linux). Dev workstations.
      # - nixosModules.default → NixOS system service (DynamicUser,
      #   /var/lib/sui).
      # - darwinModules.default → nix-darwin system launchd daemon
      #   (root, /var/log/sui.log). Survives logout, multi-user.
      homeManagerModules.default = import ./module {
        hmHelpers = import "${substrate}/lib/hm-service-helpers.nix" {
          lib = nixpkgs.lib;
        };
      };
      nixosModules.default = import ./module/nixos.nix;
      darwinModules.default = import ./module/darwin.nix;
    };
}
