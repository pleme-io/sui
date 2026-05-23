;; sui-spec/specs/eval_cache.lisp — typed eval-cache formats.

;; ── cppnix eval cache (SQLite + sha256) ──────────────────────────

(defeval-cache-format
  :name         "cppnix-eval-cache-v5"
  :version      5
  :backend      SQLite
  :hash-algo    Sha256
  :key-input    ExprPlusInputs
  :default-path "var/nix/eval-cache-v5/")

;; ── sui eval cache (redb + BLAKE3) — same key shape ──────────────

(defeval-cache-format
  :name         "sui-eval-cache-v1"
  :version      1
  :backend      Redb
  :hash-algo    Blake3
  :key-input    ExprPlusInputs
  :default-path "var/sui/eval-cache-v1/")
