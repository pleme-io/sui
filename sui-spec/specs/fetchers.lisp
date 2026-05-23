;; sui-spec/specs/fetchers.lisp — typed border for cppnix's ingest
;; layer.  Five fetchers, each authored as a phase pipeline.

;; ── fetchurl — single-file HTTP GET ────────────────────────────────

(deffetcher
  :name        "fetchurl"
  :transport   Http
  :hash-mode   Flat
  :output-kind FixedOutput
  :phases ((:kind ValidateUrl)
           (:kind CacheLookup)
           (:kind FetchBytes      :bind "bytes")
           (:kind CheckHash       :from "bytes")
           (:kind WriteToStore    :from "bytes")
           (:kind EmitNarHash     :from "bytes")))

;; ── fetchTarball — HTTP + unpack ──────────────────────────────────

(deffetcher
  :name        "fetchTarball"
  :transport   Http
  :hash-mode   Recursive
  :output-kind FixedOutput
  :phases ((:kind ValidateUrl)
           (:kind CacheLookup)
           (:kind FetchBytes      :bind "bytes")
           (:kind Unpack          :from "bytes" :bind "tree")
           (:kind CheckHash       :from "tree")
           (:kind WriteToStore    :from "tree")
           (:kind EmitNarHash     :from "tree")))

;; ── fetchGit — git protocol clone + checkout ──────────────────────

(deffetcher
  :name        "fetchGit"
  :transport   Git
  :hash-mode   Recursive
  :output-kind FixedOutput
  :phases ((:kind ValidateUrl)
           (:kind ResolveRegistryRef)
           (:kind CacheLookup)
           (:kind FetchBytes      :bind "checkout")
           (:kind CheckHash       :from "checkout")
           (:kind WriteToStore    :from "checkout")
           (:kind EmitNarHash     :from "checkout")))

;; ── fetchTree — polymorphic dispatcher ────────────────────────────
;;
;; fetchTree dispatches on URL scheme to one of the others.
;; The phase pipeline reflects the common shape; the
;; transport-specific phases are subsumed by FetchBytes.

(deffetcher
  :name        "fetchTree"
  :transport   Tree
  :hash-mode   Recursive
  :output-kind FixedOutput
  :phases ((:kind ValidateUrl)
           (:kind ResolveRegistryRef)
           (:kind CacheLookup)
           (:kind FetchBytes      :bind "tree")
           (:kind CheckHash       :from "tree")
           (:kind WriteToStore    :from "tree")
           (:kind EmitNarHash     :from "tree")))

;; ── path — local filesystem copy ──────────────────────────────────
;;
;; builtins.path copies a local directory/file into the store after
;; computing its NAR hash.  No URL validation needed (path is a
;; filesystem reference, not a URL).

(deffetcher
  :name        "path"
  :transport   LocalPath
  :hash-mode   Recursive
  :output-kind FixedOutput
  :phases ((:kind CacheLookup)
           (:kind FetchBytes      :bind "tree")
           (:kind CheckHash       :from "tree")
           (:kind WriteToStore    :from "tree")
           (:kind EmitNarHash     :from "tree")))
