;; Typed border for the L1 LockfileGraph — the parsed, follows-
;; resolved, content-addressed representation of a `flake.lock`.
;;
;; The corresponding Rust types live at sui-spec/src/lockfile_graph.rs
;; and carry `#[derive(DeriveTataraDomain, Archive, Serialize,
;; Deserialize)]`. The Archive derive is what makes a LockfileGraph
;; mmap-and-castable as bytes in sui-graph-store; this Lisp form is
;; what makes a LockfileGraph readable / writable as data when an
;; operator hands it around as text.
;;
;; Invariants the type enforces by construction:
;;
;;   - `version` is always 7 (current cppnix-flake-lock major).
;;   - `root_id` is always 0 (root is interned first; ids are dense u32s
;;     assigned in topological discovery order).
;;   - Every entry in `nodes[i].inputs` is a `(name, id)` pair — the
;;     follows chain has been chased at parse time, so resolution is
;;     O(1) hash lookup at runtime.
;;   - `canonical_hash` is the BLAKE3 of the rkyv archive bytes; it
;;     identifies the graph in `sui-graph-store` and is the cache key
;;     for every downstream eval.
;;
;; This file holds *fixture* instances used by the lockfile_graph
;; tests (minimal graph, follows-resolution graph, deeply-nested
;; substrate-style graph). Production graphs are never read from a
;; .lisp file — they're built in-memory by `LockfileGraph::from_flake_lock`
;; and persisted as rkyv blobs in sui-graph-store.

;; ── Fixture 1: minimal lockfile graph (just root + one direct input). ──

(deflockfile-graph-fixture
  :name            "minimal-one-input"
  :version         7
  :root-id         0
  :nodes           ((:id   0
                     :name "root"
                     :kind RootNode
                     :inputs (("nixpkgs" . 1))
                     :locked Empty
                     :original Empty)
                    (:id   1
                     :name "nixpkgs"
                     :kind GithubFlake
                     :inputs ()
                     :locked (:owner "NixOS"
                              :repo "nixpkgs"
                              :rev "abc1234567890abc1234567890abc1234567890ab"
                              :nar-hash "sha256-deadbeefdeadbeefdeadbeefdeadbeefdeadbeef0=")
                     :original (:owner "NixOS"
                                :repo "nixpkgs"
                                :rev-or-ref None)))
  :notes           "Smallest possible non-trivial graph. Used to test the root-only walk path.")

;; ── Fixture 2: follows-resolution graph. ──
;; `flake-utils` declares `nixpkgs` as an input and `follows` it to the
;; root's `nixpkgs`. After resolution, flake-utils.inputs.nixpkgs is the
;; *same* node id (1) as root.inputs.nixpkgs — never re-resolved at
;; eval time.

(deflockfile-graph-fixture
  :name            "follows-resolved-at-parse"
  :version         7
  :root-id         0
  :nodes           ((:id   0
                     :name "root"
                     :kind RootNode
                     :inputs (("nixpkgs"     . 1)
                              ("flake-utils" . 2))
                     :locked Empty
                     :original Empty)
                    (:id   1
                     :name "nixpkgs"
                     :kind GithubFlake
                     :inputs ()
                     :locked (:owner "NixOS"
                              :repo "nixpkgs"
                              :rev "abc1234567890abc1234567890abc1234567890ab"
                              :nar-hash "sha256-deadbeefdeadbeefdeadbeefdeadbeefdeadbeef0=")
                     :original (:owner "NixOS"
                                :repo "nixpkgs"
                                :rev-or-ref None))
                    (:id   2
                     :name "flake-utils"
                     :kind GithubFlake
                     :inputs (("nixpkgs" . 1))      ;; ← resolved at parse, points at id 1
                     :locked (:owner "numtide"
                              :repo "flake-utils"
                              :rev "0011223344556677889900112233445566778899"
                              :nar-hash "sha256-cafebabecafebabecafebabecafebabecafebabe0=")
                     :original (:owner "numtide"
                                :repo "flake-utils"
                                :rev-or-ref None)))
  :notes           "Validates that the follows-chase happens at parse time, not at eval time.")

;; The rio flake's lockfile is too large to express as a Lisp fixture
;; (~1 M lines / 109 inputs). Its parity is exercised by the
;; lockfile_graph_real_world integration test in sui-spec/tests/,
;; which reads the live nix/flake.lock from disk and asserts the
;; graph's invariants without enumerating every node here.
