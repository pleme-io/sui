;; sui-spec/specs/hash.lisp — typed border for nix hash algorithms
;; + encodings.  Five algorithms × four encodings = 20 concrete
;; renderings; each (algorithm, encoding) pair is a typed conversion
;; the spec interpreter will support once M3 lands.

;; ── Algorithms ────────────────────────────────────────────────────

(defhash-algorithm
  :name       "sha1"
  :bit-length 160
  :weakness   Deprecated
  :nix-prefix "sha1")

(defhash-algorithm
  :name       "sha256"
  :bit-length 256
  :weakness   Strong
  :nix-prefix "sha256")

(defhash-algorithm
  :name       "sha512"
  :bit-length 512
  :weakness   Strong
  :nix-prefix "sha512")

(defhash-algorithm
  :name       "md5"
  :bit-length 128
  :weakness   Broken
  :nix-prefix "md5")

(defhash-algorithm
  :name       "blake3"
  :bit-length 256
  :weakness   Strong
  :nix-prefix "blake3")

;; ── Encodings ─────────────────────────────────────────────────────

(defhash-encoding
  :name     "base16"
  :alphabet "0123456789abcdef"
  :preferred-by-nix-for ())

(defhash-encoding
  :name     "nix-base32"
  :alphabet "0123456789abcdfghijklmnpqrsvwxyz"
  :preferred-by-nix-for (NarHash StorePathHash NarSignature))

(defhash-encoding
  :name     "base64"
  :alphabet "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
  :preferred-by-nix-for ())

(defhash-encoding
  :name     "sri"
  :alphabet "sha256-<base64>="
  :preferred-by-nix-for (FlakeInputSri FodOutputHash))
