;; sui-spec/specs/substituters.lisp — the binary-cache substitution
;; protocol authored as Lisp data.  Three canonical substituters
;; today; more can be added as one form each.

;; ── cache.nixos.org (public, NixOS-signed) ───────────────────────

(defsubstituter
  :name        "cache.nixos.org"
  :transport   Https
  :endpoint    "https://cache.nixos.org"
  :auth        None
  :trust-level Trusted
  :phases ((:kind QueryNarInfo)
           (:kind FetchNar           :bind "nar")
           (:kind VerifyNarSignature :from "nar")
           (:kind DecompressNar      :from "nar" :bind "uncompressed")
           (:kind VerifyNarHash      :from "uncompressed")
           (:kind ImportNarToStore   :from "uncompressed")
           (:kind RealizeReferences)))

;; ── attic (Cloudflare-backed authenticated cache) ────────────────

(defsubstituter
  :name        "attic"
  :transport   Attic
  :endpoint    "https://attic.example.com"
  :auth        BearerToken
  :trust-level Trusted
  :phases ((:kind QueryNarInfo)
           (:kind FetchNar           :bind "nar")
           (:kind VerifyNarSignature :from "nar")
           (:kind DecompressNar      :from "nar" :bind "uncompressed")
           (:kind VerifyNarHash      :from "uncompressed")
           (:kind ImportNarToStore   :from "uncompressed")
           (:kind RealizeReferences)))

;; ── local file mirror (untrusted, used as a fast pre-fetch) ──────

(defsubstituter
  :name        "local-mirror"
  :transport   Local
  :endpoint    "file:///var/cache/nix-store-mirror"
  :auth        None
  :trust-level Untrusted
  :phases ((:kind QueryNarInfo)
           (:kind FetchNar         :bind "nar")
           (:kind DecompressNar    :from "nar" :bind "uncompressed")
           (:kind VerifyNarHash    :from "uncompressed")
           (:kind ImportNarToStore :from "uncompressed")
           (:kind RealizeReferences)))

;; ── s3 (AWS S3 / R2 / MinIO direct-object) ───────────────────────

(defsubstituter
  :name        "s3-cache"
  :transport   S3
  :endpoint    "s3://my-binary-cache"
  :auth        AwsSigV4
  :trust-level TrustedUsersOnly
  :phases ((:kind QueryNarInfo)
           (:kind FetchNar           :bind "nar")
           (:kind VerifyNarSignature :from "nar")
           (:kind DecompressNar      :from "nar" :bind "uncompressed")
           (:kind VerifyNarHash      :from "uncompressed")
           (:kind ImportNarToStore   :from "uncompressed")
           (:kind RealizeReferences)))
