;; Typed named queries over the store.
;;
;; Each (defstore-query …) declares a reusable filter for
;; `sui store find --query <name>`.  Fields compose with AND;
;; leave any field empty/zero to omit that clause.

;; ── large-rust-libs ─────────────────────────────────────────
;;
;; Find Rust library outputs over 100KB.  Useful for tracking
;; bloat in compiled artifacts.

(defstore-query
  :name           "large-rust-libs"
  :description    "Rust library outputs ≥ 100 KB"
  :name-regex     "^rust_.*-lib$"
  :min-size       102400
  :max-size       0
  :contents-regex ""
  :has-reference  "")

;; ── tarballs ────────────────────────────────────────────────
;;
;; Source tarballs (.tar.gz, .tgz, etc.) of any size.

(defstore-query
  :name           "tarballs"
  :description    "Source tarballs by name pattern"
  :name-regex     ".*\\.(tar\\.gz|tgz|tar\\.xz|tar\\.bz2)$"
  :min-size       0
  :max-size       0
  :contents-regex ""
  :has-reference  "")

;; ── potentially-secret-bearing ─────────────────────────────
;;
;; Entries whose contents contain base64-looking runs ≥ 40
;; chars (matches keys, tokens, encoded secrets).  Combined
;; with audit-secrets dry-run.

(defstore-query
  :name           "potentially-secret-bearing"
  :description    "Contains base64 runs ≥ 40 chars (potential secrets)"
  :name-regex     ""
  :min-size       0
  :max-size       0
  :contents-regex "[A-Za-z0-9+/=]{40,}"
  :has-reference  "")

;; ── tiny-drvs-only ─────────────────────────────────────────
;;
;; .drv files under 4KB.  Useful for testing the ATerm parser
;; on a small corpus.

(defstore-query
  :name           "tiny-drvs-only"
  :description    ".drv files under 4 KB"
  :name-regex     ".*\\.drv$"
  :min-size       0
  :max-size       4096
  :contents-regex ""
  :has-reference  "")

;; ── source-trees ───────────────────────────────────────────
;;
;; Anything whose name ends in -source.

(defstore-query
  :name           "source-trees"
  :description    "All -source-suffixed entries"
  :name-regex     ".*-source$"
  :min-size       0
  :max-size       0
  :contents-regex ""
  :has-reference  "")
