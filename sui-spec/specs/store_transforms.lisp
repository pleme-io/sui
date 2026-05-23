;; Typed store-content transformations.
;;
;; Each (defstore-transform …) declares a pure-Rust mutation
;; applied to a ParsedNar tree.  Three kinds:
;;
;;   FileContents       — regex match-and-replace over file bytes
;;   StorePathReference — graft (replace) hash prefixes everywhere
;;                        (file contents + symlink targets)
;;   EntryName          — rename top-level directory entries
;;
;; Operators reference transforms by name when invoking
;; `sui store transform <slice> <transform>`.

;; ── redact-base64-secrets ─────────────────────────────────────
;;
;; Long base64-ish blobs in file contents are usually accidental
;; secrets (signing keys, tokens).  This transform redacts every
;; ≥40-char base64 run.

(defstore-transform
  :name        "redact-base64-secrets"
  :description "Replace any base64-looking run ≥40 chars with [redacted]"
  :match-kind  FileContents
  :pattern     "[A-Za-z0-9+/=]{40,}"
  :replacement "[redacted]")

;; ── strip-comments ───────────────────────────────────────────
;;
;; Drop shell-style `# …\n` comment lines from file contents.
;; Useful when normalizing scripts for diff.

(defstore-transform
  :name        "strip-shell-comments"
  :description "Strip `# …` comment lines from file contents (line-mode regex)"
  :match-kind  FileContents
  :pattern     "(?m)^#[^\\n]*\\n"
  :replacement "")

;; ── rewrite-store-prefix-aaaaa-to-bbbbb ──────────────────────
;;
;; Example StorePathReference transform — replaces every
;; reference to /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-*
;; with /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-*.
;; Used by the test suite + as a working template for graft ops.

(defstore-transform
  :name        "graft-example-a-to-b"
  :description "Demonstration graft: rewrite aaaa…-prefixed refs to bbbb…"
  :match-kind  StorePathReference
  :pattern     "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  :replacement "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")

;; ── lowercase-entry-names ────────────────────────────────────
;;
;; EntryName transform — leaves contents untouched but normalises
;; the directory tree's entry names.  Demonstrates the third
;; transform kind.

(defstore-transform
  :name        "downcase-share-doc"
  :description "Rename `share/Doc` → `share/doc` (case-normalisation)"
  :match-kind  EntryName
  :pattern     "^Doc$"
  :replacement "doc")
