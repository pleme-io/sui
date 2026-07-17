{
  description = "Sui (粋) — Rust-native Nix replacement with API-first design";

  # substrate.rust.workspace dispatches over Cargo.gen.lock (the slim gen delta,
  # reconstructed to the full BuildSpec in pure Nix) — no crate2nix, no Cargo.nix.
  inputs.substrate.url = "github:pleme-io/substrate";

  outputs = { substrate, ... }: substrate.rust.workspace {
    src = ./.;
    member = "sui";
  };
}
