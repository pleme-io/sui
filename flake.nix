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
      homeManagerModules.default = import ./module {
        hmHelpers = import "${substrate}/lib/hm-service-helpers.nix" {
          lib = nixpkgs.lib;
        };
      };
    };
}
