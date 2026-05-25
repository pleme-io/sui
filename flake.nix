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
        (system: let
          pkgs = import nixpkgs { inherit system; };
          suiPkg = (toolOutputs.packages.${system} or {}).default or null;
          # The legacy `nix-*` basenames sui's argv[0] dispatcher
          # (`src/legacy.rs::LegacyCmd`) understands.  Keep in sync.
          legacyNixNames = [
            "nix"
            "nix-build"
            "nix-store"
            "nix-env"
            "nix-shell"
            "nix-instantiate"
            "nix-collect-garbage"
            "nix-hash"
            "nix-copy-closure"
            "nix-channel"
            "nix-daemon"
            "nix-prefetch-url"
          ];
          # `sui-as-nix`: symlinkJoin of the sui binary + one symlink
          # per legacy name → `bin/sui`.  When a symlink is invoked,
          # sui's argv[0] dispatcher rewrites the legacy CLI into the
          # modern `sui <subcommand>` form.  This is the durable
          # form-factor for `nix.package = sui-as-nix`.
          suiAsNix =
            if suiPkg == null then null
            else pkgs.symlinkJoin {
              name = "sui-as-nix";
              paths = [ suiPkg ];
              postBuild = ''
                cd $out/bin
                for n in ${pkgs.lib.escapeShellArgs legacyNixNames}; do
                  ln -sf sui "$n"
                done
              '';
            };
        in
          (toolOutputs.packages.${system} or {})
          // (let img = imageOutputs.packages.${system} or {}; in {
            dockerImage-amd64 = img.dockerImage-amd64 or null;
          })
          // (if suiAsNix == null then {} else { sui-as-nix = suiAsNix; })
        );

      # App index: the substrate-produced toolOutputs apps + two
      # sui-specific wrappers.
      #
      # - `shadow-rebuild` runs `sui rebuild-shadow` — the canonical
      #   way to test sui as a full nix replacement.  `nix run
      #   github:pleme-io/sui#shadow-rebuild -- --corpus rebuild`
      #   on any host gets a typed JSON shadow report without
      #   `cargo install`.
      # - `sui-sweep` runs the standalone differential parity runner
      #   (same library, different operator surface).
      # - `release-image` is the substrate's image release plumbing.
      apps = nixpkgs.lib.genAttrs
        [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ]
        (system: let
          pkgs = import nixpkgs { inherit system; };
          suiPkg = (toolOutputs.packages.${system} or {}).default or null;
        in
          (toolOutputs.apps.${system} or {})
          // {
            shadow-rebuild = {
              type = "app";
              program = "${pkgs.writeShellScript "sui-shadow-rebuild" ''
                exec ${suiPkg}/bin/sui rebuild-shadow "$@"
              ''}";
            };
            sui-sweep = {
              type = "app";
              program = "${suiPkg}/bin/sui-sweep";
            };
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
      # `services.sui-as-nix.enable = true;` sets `nix.package` to
      # the sui-as-nix symlink farm.  See module/nixos-as-nix.nix
      # for the readiness caveat (gated on M2.6 lattice completion).
      nixosModules.default-as-nix = import ./module/nixos-as-nix.nix;
      # nix-wrap migration-bridge: routes Working/SuiNative `nix`
      # commands to sui, falls back to cppnix for the rest.
      # `imports = [ inputs.sui.darwinModules.nix-wrap ]` +
      # `services.sui-nix-wrap.enable = true;` is the operator's
      # one-line opt-in.
      darwinModules.nix-wrap = import ./module/nix-wrap.nix;
    };
}
