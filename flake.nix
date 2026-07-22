{
  description = "Sui (粋) — Rust-native Nix replacement with API-first design";

  # substrate.rust.workspace dispatches over Cargo.gen.lock (the slim gen delta,
  # reconstructed to the full BuildSpec in pure Nix) — no crate2nix, no Cargo.nix.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs = { self, substrate, ... }:
    let
      # The binary crate was renamed `sui` → `pleme-io-sui` (crates.io publish);
      # its `[[bin]]` is still `sui`. mkRustToolFlake picks the spec member by
      # CRATE name (`pleme-io-sui`) and derives the tool/overlay name from the
      # crate's default_bin (`sui`), so the overlay stays `pkgs.sui`.
      base = substrate.rust.workspace { src = ./.; member = "pleme-io-sui"; };

      # sui-dockerfile-wrapper's flake_metadata.default_bin is `gha-entrypoint`
      # (its one [[bin]], src/bin/gha-entrypoint.rs) — pass `toolName`
      # explicitly so the exposed package attribute reads as the crate name,
      # not the binary name (consumers run
      # `nix profile install github:pleme-io/sui#sui-dockerfile-wrapper`,
      # never `#gha-entrypoint`). Same `rust.workspace` shape as `base` above,
      # just a different `member`. First consumer:
      # pleme-io/actions/sui-dockerfile-node-cache (replaces its
      # `cargo install --locked sui-dockerfile-wrapper` runtime-install step —
      # fleet rule: no CI action installs a binary imperatively at job time).
      dockerfileWrapper = substrate.rust.workspace {
        src = ./.;
        member = "sui-dockerfile-wrapper";
        toolName = "sui-dockerfile-wrapper";
      };

      # dockerImage-amd64 output for the fleet's `ghcr.io/pleme-io/sui` image
      # (image-release.yml + the super-cache-ci / camelot-builder / prewarmer /
      # node-cache charts consume it). The substrate `rust` dispatcher exposes
      # only tool|workspace|library|service|binary — none emits a dockerImage-*,
      # and `service` is crate2nix-based (sui is gen-native). The image builder
      # is the separate raw-import `tool-image-flake.nix`; on that path substrate
      # does NOT pre-bind its inputs, so we thread them from `substrate.inputs`.
      # genBuild=true keeps the gen lockfile-builder path (matches Cargo.gen.lock,
      # no Cargo.nix); Entrypoint = /bin/sui (the tiered CLI); no Dockerfile
      # (Pillar 8, dockerTools.buildLayeredImage). `src = self` carries .inputs.
      imageFlake = (import "${substrate}/lib/build/rust/tool-image-flake.nix" {
        nixpkgs = substrate.inputs.nixpkgs;
        flake-utils = substrate.inputs.flake-utils;
        fenix = substrate.inputs.fenix or null;
      }) {
        toolName = "sui";
        src = self;
        repo = "pleme-io/sui";
        genBuild = true;
        packageName = "pleme-io-sui";
        architectures = [ "amd64" ];
      };

      # Deep-merge ONLY dockerImage-amd64 per system so the workspace's own
      # crate-build packages in `base` are preserved (never clobbered).
      mergedPackages = builtins.foldl'
        (acc: sys: acc // {
          ${sys} = (base.packages.${sys} or { }) // {
            inherit (imageFlake.packages.${sys}) dockerImage-amd64;
          };
        })
        (base.packages or { })
        (builtins.attrNames imageFlake.packages);

      # Same fold technique, second pass: fold in sui-dockerfile-wrapper's
      # packages (all 4 default systems — it's a plain `rust.workspace` tool,
      # not an image build restricted to amd64) without clobbering what's
      # already in mergedPackages.
      mergedPackagesWithWrapper = builtins.foldl'
        (acc: sys: acc // {
          ${sys} = (acc.${sys} or { }) // {
            inherit (dockerfileWrapper.packages.${sys}) sui-dockerfile-wrapper;
          };
        })
        mergedPackages
        (builtins.attrNames dockerfileWrapper.packages);
    in
    base // {
      packages = mergedPackagesWithWrapper;

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
