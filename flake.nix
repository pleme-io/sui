{
  description = "Sui (粋) — Rust-native Nix replacement with API-first design";

  # substrate.rust.workspace dispatches over Cargo.gen.lock (the slim gen delta,
  # reconstructed to the full BuildSpec in pure Nix) — no crate2nix, no Cargo.nix.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs = { substrate, ... }:
    let
      base = substrate.rust.workspace { src = ./.; member = "sui"; };
    in
    base // {
      # Re-attached after the bare substrate.rust.workspace migration (b1b9e09)
      # dropped the module trio. `overlays.default` + `packages` are still auto-emitted
      # by the builder; only the modules were lost. The fleet nix repo consumes
      # darwinModules.default (nix/darwinConfigurations + the darwin-developer profile).
      # These module files are plain nix-darwin/NixOS modules ({config,lib,pkgs,…}), so
      # they re-attach arg-free. homeManagerModules.default is intentionally deferred —
      # it needs hmHelpers built from nixpkgs.lib (absent in this bare flake's scope) and
      # nothing in the fleet consumes sui's HM module; restore it if a consumer appears.
      darwinModules.default = import ./module/darwin.nix;
      darwinModules.nix-wrap = import ./module/nix-wrap.nix;
      nixosModules.default = import ./module/nixos.nix;
      nixosModules.default-as-nix = import ./module/nixos-as-nix.nix;
    };
}
