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
  };

  outputs = { self, nixpkgs, crate2nix, flake-utils, substrate, ... }:
    (import "${substrate}/lib/rust-workspace-release-flake.nix" {
      inherit nixpkgs crate2nix flake-utils;
    }) {
      toolName = "sui";
      packageName = "sui";
      src = self;
      repo = "pleme-io/sui";
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
