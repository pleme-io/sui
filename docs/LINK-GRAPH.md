# LINK-GRAPH — sui's complete typed-graph model of nix linking

> Grounded 2026-07-11 by a read-only recon of the owning crates (`sui-store`,
> `sui-spec`, `sui-orchestrate`, `sui-graph-store`). Every claim cites the
> `fn`/spec/test it rests on; where a surface is a stub or absent, this doc says
> so and never rounds up. This is the **ACTIVATE stage** of the sui-supremacy
> critical path (`eval → build → store → link-into-place`) and the companion to
> [`SUI-SUPREMACY-ROADMAP.md`](./SUI-SUPREMACY-ROADMAP.md) — read that for the
> module-system blocker (M2.6) that gates the marquee cid proof.

Nix linking is one problem: **place content into the filesystem by reference,
never by copy, such that identity, liveness, and rollback are all functions of a
graph.** Every nix "link" surface — store hardlink dedup, `buildEnv` symlink
farms, profile generations, GC roots, activation `/etc` farms, the `./result`
symlink — is a projection of **one typed structure**: a directed graph whose
nodes are *content* and *filesystem locations* and whose edges are *hardlink* or
*symlink* placements carrying typed metadata. Model it once, seal each invariant
into a type, and the whole class of "dangling link / lost generation / clobbered
file / dead-but-referenced root" bugs becomes unrepresentable (or, at the honest
floor, an eval/CI-caught red gate).

This doc gives: (§1) the typed **LinkGraph** model + its TYPED-SPEC triplet
shape; (§2) the six linking surfaces mapped to that model with the **best-fit
graph algorithm** per operation; (§3) the **invariant → seal-tier** table; (§4)
the **tier-honest coverage scorecard** (sui-real vs gap); (§5) the
**dependency-ordered phased assault**, each phase Parity-Method'd against real
nix; (§6) exactly what linking must hold for the **cid-rebuild activation
proof**.

---

## 1. The LinkGraph model

### 1.1 Nodes, edges, metadata

A **LinkGraph** `G = (V, E)` is a directed graph over two node kinds and one
edge kind carrying typed metadata.

**Nodes `V`** — three sorts, all content-or-location identities:

| Node sort | Identity | Nix instances |
|---|---|---|
| `StoreObject` | `<hash>-<name>` basename (store-relative, prefix-independent) | a `/nix/store/…` path, a `buildEnv` output, a generation's toplevel |
| `FsLocation` | absolute FS path *outside* content-addressing | `/etc/foo`, `~/.nix-profile`, `/run/current-system`, `./result`, a `system-42-link` |
| `ContentClass` | BLAKE3/SHA-256 of file bytes (the dedup equivalence class) | the inode-share group in store-optimise |

**Edges `E`** — a single `Link` edge type, discriminated by *placement mechanism*:

```
Link {
  from: NodeId,        // the referring location (FsLocation) or store object
  to:   NodeId,        // the referent (StoreObject or another FsLocation)
  mech: LinkMech,      // Hardlink | Symlink
  meta: LinkMeta,      // typed placement metadata (below)
}

LinkMech = Hardlink   // same inode; only within one filesystem; content-identity
         | Symlink    // a path pointer; may be relative/absolute/indirect

LinkMeta = {
  priority:   Option<Priority>,   // buildEnv collision precedence (lower wins in nix)
  indirect:   bool,               // GC root through an auto/ indirect pointer
  backup_ext: Option<String>,     // activation clobber → rename target.<ext>
  relative:   bool,               // symlink target stored relative vs absolute
  generation: Option<u32>,        // profile generation number this edge realizes
}
```

The **key modelling move**: nix's six "different" link operations differ only in
*which node sorts the edge connects* and *which `LinkMech`/`LinkMeta` fields are
load-bearing*. Store-optimise is `ContentClass`-keyed `Hardlink` edges; a profile
is a `Symlink` chain `FsLocation → FsLocation(generation) → StoreObject`; a
buildEnv is a fan-in of `Symlink` edges into one merged `StoreObject` tree with
`priority` deciding collisions; a GC root is a `Symlink` from `gcroots/` into a
`StoreObject`, and liveness is *reachability in the reference graph from the
root set*.

### 1.2 The TYPED-SPEC + INTERPRETER triplet shape

Per the org ★★ TYPED-SPEC + INTERPRETER TRIPLET rule, the LinkGraph ships as
three artifacts. Two link-adjacent domains **already** ship this shape in sui
(`gc`, `activation_script`); the new `link_graph` domain follows them exactly.

**(1) Typed Rust border** — `sui-spec/src/link_graph.rs`:

```rust
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deflink")]
pub struct LinkSpec {
    pub name: String,
    pub mech: LinkMech,           // Hardlink | Symlink (closed enum)
    pub from: LinkEndpoint,       // FsLocation | StoreObject | ContentClass
    pub to:   LinkEndpoint,
    #[serde(default)] pub priority: Option<i64>,
    #[serde(default)] pub indirect: bool,
    #[serde(default)] pub backup_ext: Option<String>,
}

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "deflinkgroup")]
pub struct LinkGroupSpec {
    pub name: String,
    pub kind: LinkGroupKind,      // StoreOptimise | BuildEnv | Profile
                                  //   | GcRootSet | ActivationFarm | ResultRoot
    pub collision: CollisionPolicy,  // FailOnUnpriced | ByPriority | IgnoreCollisions
    pub members: Vec<String>,     // deflink names composing this group
}
```

**(2) Authored Lisp spec** — `sui-spec/specs/link_graph.lisp`: one
`(deflinkgroup …)` per canonical linking operation (the six surfaces), each
referencing `(deflink …)` members. This is the operator-facing authoring
surface, and both the tree-walker and any future engine drive the *same*
authored data — drift is impossible (the CATALOG REFLECTION invariant already
enforced across sui's 22 catalog domains).

**(3) Working interpreter behind a mockable FS `Environment`** —
`apply<E: LinkEnvironment>(group, args, env) -> Result<LinkOutcome, SpecError>`.
The `LinkEnvironment` trait abstracts *every* filesystem side effect so tests
mock it (the exact pattern `gc::GcEnvironment` already uses —
`sui-spec/src/gc.rs:129`):

```rust
pub trait LinkEnvironment {
    fn symlink(&self, from: &str, to: &str, relative: bool) -> Result<(), String>;
    fn hardlink(&self, from: &str, to: &str) -> Result<(), String>;
    fn read_link(&self, at: &str) -> Result<Option<String>, String>;
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;   // atomic swap
    fn content_hash(&self, path: &str) -> Result<String, String>;   // dedup class
    fn read_dir(&self, at: &str) -> Result<Vec<String>, String>;
    fn nlink(&self, path: &str) -> Result<u64, String>;             // already-linked skip
}
```

The trait **is** the testability contract; the `local.rs` real impl satisfies it
against `/nix/store`, and unit tests drive a `MockFs` (mirroring
`gc.rs`'s `MockEnv`). Where a real store `Store` already exists, the interpreter
consumes it rather than re-rolling FS access.

---

## 2. Per-surface: node/edge shape + best-fit algorithm

### 2.1 Store hardlink optimization (store-optimise) — DSU dedup graph

**Graph.** Nodes are every regular file across all store paths; the edge relation
is *content-equality* → group files into `ContentClass` equivalence classes; one
representative per class holds the inode, every other member becomes a `Hardlink`
edge to it.

**Best-fit algorithm — Disjoint-Set Union (union-find) over the content hash.**
The dedup is a partition problem: files with equal content are one set, and each
set collapses to one inode. sui's current `optimise_store`
(`sui-store/src/local.rs:544`) implements the degenerate DSU where the *hash map*
`seen: HashMap<contenthash, PathBuf>` **is** the union-find with the
first-seen file as the fixed set representative — `O(N)` over N files with
`O(1)` amortized find (the hash lookup) + `O(1)` union (replace-with-hardlink).
This is optimal: any dedup must hash every file once (`Ω(total bytes)`), and DSU
adds only near-constant overhead per file. The `nlink() > 1` skip
(`local.rs:574`) is the "already in a set" short-circuit.

**Complexity.** `O(B + N·α(N))` where B = total store bytes hashed, N = file
count, α = inverse-Ackermann (≈ constant). Current impl: `O(B + N)` (flat map,
no rank/path-compression needed since the representative is pinned to first-seen).

**Sealed to Done** per the roadmap (Wave 2 / Phase C3): `optimise_store` +
`nlink` skip + content-hash dedup are REAL and CLI-wired (`main.rs:4861`); the
named gaps are the `links/`-dir parity with cppnix's
`/nix/var/nix/db/links` and the reflink/CoW fast path (ZFS/btrfs).

### 2.2 buildEnv / symlinkJoin — three-way merge with precedence resolution — **GAP**

**Graph.** N input `StoreObject` trees fan **in** to one output `StoreObject`
tree. Each input contributes `Symlink` edges (`out/bin/foo → inputⱼ/bin/foo`).
When two inputs both provide `bin/foo`, that is a **collision node** — two edges
targeting the same `from` location — and the `priority` metadata (the `.priority`
file / `meta.priority`) or `ignoreCollisions` decides the winner.

**Best-fit algorithm — priority-ordered three-way tree merge.** Walk the N input
trees in a *stable priority order* (nix: lower `priority` int wins; ties broken
by input order), unioning directory subtrees and, at each leaf, applying the
CollisionPolicy:
- `FailOnUnpriced` — two providers, no distinguishing priority → **typed error**
  (this is the seal: last-writer-wins is *unrepresentable*, §3).
- `ByPriority` — the lower-priority provider's leaf wins deterministically.
- `IgnoreCollisions` — first-in-order wins, collision recorded but not fatal.

This is a k-way merge of sorted trees: `O(Σ|treeⱼ| · log k)` with a priority
heap over the k current heads, or `O(Σ|treeⱼ|)` if inputs are pre-sorted and
merged pairwise left-fold. Optimal — every input leaf is visited exactly once.

**Status: ABSENT.** sui has **no buildEnv / symlinkJoin / makeBinPath /
makeWrapper primitive at all** — confirmed by grep across `sui-spec/specs`,
`sui-spec/src`, `sui-store/src` (the only `priority`/`buildEnv` hits are
module-system *option* priority, `mkForce`/`mkDefault`, an unrelated concept).
`store_recipe.rs` + `store_transform.rs` are a NAR *transform/materialize*
pipeline (regex-replace over file bytes, store-path grafting) — **not** a
symlink-farm merge. This is the single largest link-graph gap and the first
net-new `deflinkgroup` to author.

### 2.3 Profiles + generations — a versioned singly-linked list; atomic-swap head

**Graph.** A profile is a chain: `FsLocation(profile) → FsLocation(gen-N-link) →
StoreObject(toplevel)`. Generations form a **monotone singly-linked list**
indexed by number; the profile symlink is the *head pointer*; rollback moves the
head to a lower-numbered node; the list is append-only (new gen = `max+1`).

**Best-fit algorithm — atomic rename-swap for the head pointer + linear scan for
list ops.** The head move MUST be atomic so no reader ever observes a
half-switched profile: create `profile-tmp-link → gen-N-link`, then
`rename(tmp, profile)` — POSIX `rename(2)` is atomic within one filesystem. This
is exactly `ProfileManager::atomic_switch` (`sui-store/src/profile.rs:218`):
`symlink(target, tmp)` then `fs::rename(tmp, profile)`. List ops (list/rollback/
next-number) are `O(#generations)` directory scans — optimal, the generation set
is small.

**Status: REAL.** `ProfileManager` (`profile.rs`) implements set / list /
current / switch / rollback / delete with the atomic-swap invariant + the
"cannot delete current" guard, 20+ unit tests. The typed border
`ProfileFormat`/`ProfileKind` (`sui-spec/src/profile.rs`) + `profile.lisp` name
the three cppnix formats (System / User / Ephemeral). The gap is the **JSON
manifest** (`manifest.json` for `nix profile` post-2.4) — `ProfileFormat` names
`manifest_path` but `ProfileManager` doesn't read/write it (it manages the
symlink chain only, which is what `nix-env` / system profiles need).

### 2.4 GC roots — reachability (BFS) over the reference graph

**Graph.** Two overlaid graphs: (a) the **root set** — `Symlink` edges from
`gcroots/` + `profiles/` into `StoreObject`s (and `indirect` roots: a symlink in
`gcroots/auto/` pointing to an out-of-store symlink — e.g. a `./result` — that
in turn points into the store); (b) the **reference graph** — `StoreObject →
StoreObject` edges from the DB `Refs` table (runtime deps). *Liveness = the set
reachable from the root set in the reference graph.* Garbage = everything else.

**Best-fit algorithm — multi-source BFS/DFS reachability (transitive closure
from roots).** Seed a frontier with every root, then flood the reference graph;
the visited set is the live set; the complement is dead. This is exactly:
- `gc::apply`'s `ComputeLiveSet` phase (`sui-spec/src/gc.rs:192`) — BFS from
  roots over `StorePathInfo::references`, `HashMap`-indexed, `O(V + E)`.
- `LocalStore::collect_garbage` (`sui-store/src/local.rs:384`) — the real
  mark-and-sweep: `find_gc_roots` scans roots, walks the reachable closure in
  the **basename domain** (prefix-independent — the fix that makes a `--store
  <chroot>` GC correct), computes the dead set, deletes.

`O(V + E)` is optimal for reachability (each node/edge visited once). The BFS is
correct-by-construction only if roots are complete — see the indirect-root gap.

**Status: REAL (mislabelled InProgress; closer to Done).** `find_gc_roots`
(`local.rs:671`) is **store-relative** (derives `<state>/gcroots` +
`<state>/profiles` from the store dir via `state_dir_for_store`, not a hardcoded
`/nix/var/nix`) and follows symlinks into the store. **Named gaps:** (1)
**indirect / `auto/` roots** — `find_gc_roots` follows a symlink *one hop* and
takes the first store component; it does **not** chase an out-of-store symlink
(the `./result` → store two-hop indirect-root case) — `gc.rs`'s doc-comment
names indirect roots but `local.rs` doesn't resolve them. (2) No **store lock**
around the critical section (the `LockStore`/`UnlockStore` phases exist in the
`gc.lisp` spec + `GcEnvironment` trait but `collect_garbage` doesn't take a real
global lock — a concurrent build could race the dead-set computation).

### 2.5 Activation linking — topological compose + atomic generation swap — SPEC-ONLY realizer

**Graph.** The richest surface. A generation's activation links a whole
*bundle* of `FsLocation → StoreObject` edges into place: the `/etc` symlink farm
(`environment.etc.*`), systemd unit / launchd plist trees, the `home-files`
tree, and the `/run/current-system` pointer. Two graph structures compose:
1. **The `/etc` (and home-files) farm** is a buildEnv-shaped fan-in (§2.2) — N
   modules contribute `/etc/*` entries with **collision + backup** semantics
   (an existing non-nix `/etc/foo` is renamed `/etc/foo.backup` before the
   symlink lands — the `backup_ext` metadata).
2. **The activation *script* itself** is composed in **topological order** over
   the module dependency graph — each module's activation snippet may declare
   `before`/`after` deps on others (home-manager's `dag` / NixOS's
   `system.activationScripts.*.deps`), so the compose is a **topological sort**
   of the activation-snippet DAG.

**Best-fit algorithms.**
- **Topological sort (Kahn / DFS post-order)** for snippet ordering — `O(V + E)`
  over the activation-DAG; a cycle in the deps is a **typed error** (the
  `dependency-cycle` seal, §3), not a nondeterministic order.
- **Atomic generation swap** for `/run/current-system` — same rename-swap as
  §2.3 (the profile head move); the whole new generation appears atomically, so
  no half-linked system state is ever observable.
- **Three-way merge with backup** for the `/etc` farm — §2.2's merge, plus the
  clobber→backup rename when a target pre-exists outside nix's ownership.

**Status: SPEC-ONLY realizer / REAL orchestration-by-shelling.** Two honest
tiers here:
- **The typed spec is REAL:** `activation_script.rs` +
  `activation_script.lisp` name the three algorithms (NixOS/Darwin/HomeManager)
  with the correct phase pipelines (`ResolveSystemBuildToplevel` →
  `GenerateEtcSymlinks` / `GenerateSystemdUnits` / `GenerateLaunchdPlists` →
  `ResolveSecretRefs` → `ComposeActivationScript` → `WriteActivationDerivation`),
  and the M3.0 `apply` interpreter (`activation_script.rs:155`) walks the
  pipeline producing the **script TEXT** — but it does **no filesystem linking**:
  `GenerateEtcSymlinks` only *emits comment lines* naming the intended
  `/etc/x → source` edges (`activation_script.rs:209`); `WriteActivationDerivation`
  records a **placeholder** store path (`:236`); there is no real `/etc` farm
  build, no topological snippet ordering, no atomic `/run/current-system` swap.
- **The actual activation is REAL but shells out:** `SystemOrchestrator::
  activate_system` (`sui-orchestrate/src/system.rs:280`) sets the profile
  natively (`ProfileManager::set` — §2.3, REAL) then **runs the nix-generated
  `{system_path}/activate` (+ `/activate-user`) script** via `CommandRunner`.
  So today sui *delegates* the actual `/etc`-farm linking + `/run/current-system`
  swap to the closure's own baked-in activate script — it does not itself
  construct those link edges. That is correct for the cid proof (the activate
  script is part of the byte-identical closure) but means the **activation
  link-graph interpreter is not yet sui-native**.

### 2.6 The `./result` symlink + indirect GC roots — a single indirect edge

**Graph.** `./result` is one `FsLocation → StoreObject` symlink (`indirect =
false` at the `result` node itself), but it doubles as an **indirect GC root**:
nix registers `gcroots/auto/<hash> → /path/to/result` (an out-of-store symlink),
and the GC must chase `auto/<hash> → result → /nix/store/…` (two hops, the
middle hop leaving the store) to root the output. This is a special case of
§2.4's root set — an edge with `indirect = true`.

**Best-fit algorithm — two-hop symlink resolution during root collection.** When
`collect_gc_roots` finds a symlink under `gcroots/auto/`, resolve it; if the
target is itself a symlink *outside* the store, resolve *that* to find the store
object it ultimately roots. `O(1)` per indirect root (bounded hops).

**Status: GAP** (same gap as §2.4's indirect-root note). `find_gc_roots` does not
create `./result` on build (no `sui build` result-symlink write found), and does
not chase `auto/` two-hop indirect roots.

---

## 3. Invariants → seal tiers (honest, never rounded up)

Per ★★ UNREPRESENTABILITY, a `Result::Err` is **mitigation**; a compile
error / absent method / parse-boundary rejection is **unrepresentability**.
Each linking invariant is stated with its *honest achievable tier*.

| # | Invariant | Seal mechanism | Tier | Where |
|---|---|---|---|---|
| I1 | **`LinkMech` is closed** — a link is Hardlink or Symlink, no third kind | Rust `enum LinkMech` (no `_` arm) | **truly-unrep** | new `link_graph.rs` |
| I2 | **buildEnv collision without a priority is an error, not last-writer-wins** | `CollisionPolicy::FailOnUnpriced` → interpreter returns `SpecError::Interp{phase:"buildenv-unpriced-collision"}` | **parse/eval-rejected** (a `Result::Err` at merge time — cannot be a compile error, the collision is data-dependent) | new `deflinkgroup BuildEnv` interp |
| I3 | **No dangling symlink** — every `Symlink` edge's `to` node exists | interpreter verifies `env.read_link`/`stat` target before/after placement; `dangling-symlink` error | **runtime-gated** (target existence is a runtime FS fact; cannot be a type) | new interp + a `sui link verify` gate |
| I4 | **No cycle in the activation-snippet ordering** | topological sort detects back-edge → `SpecError::Interp{phase:"dependency-cycle"}` | **eval-rejected** (`gc::apply` already names `dependency-cycle` defensively, `gc.rs:166`) | activation compose interp |
| I5 | **Generation switch is atomic — no half-linked profile state observable** | tmp-symlink + `rename(2)` (atomic within one FS) | **runtime-invariant via POSIX atomicity** — as strong as the FS gives; not a *type* (a crash between symlink+rename leaves a stale `-tmp-link`, cleaned on next set) | `ProfileManager::atomic_switch` (**REAL**, `profile.rs:218`) |
| I6 | **A GC root is never dead while referenced** — liveness = reachability, so a rooted path is live *by construction* of the BFS | multi-source BFS closure; garbage = complement of reachable | **algorithmic-invariant** (correct iff the root set is complete — see I6a) | `collect_garbage` (**REAL**, `local.rs:384`) |
| I6a | **Root set completeness** — indirect/`auto` roots included | *not yet sealed* — `find_gc_roots` misses two-hop indirect roots | **GAP** (a live path rooted only through `./result` could be wrongly collected) | `find_gc_roots` (`local.rs:671`) |
| I7 | **Store-optimise never corrupts on link failure** — a file is never lost if `hard_link` fails after `remove_file` | current impl is **only-mitigated**: `remove_file` then `hard_link`; on hard_link failure the file is already gone (`local.rs:590` comment admits "best-effort") | **only-mitigated** (the correct seal: link-to-tmp-then-rename-over, so the original is never removed until the replacement is in place) | `optimise_store` — **remediation-queue item** |
| I8 | **Activation clobber is backed up, never silently overwritten** | `backup_ext` metadata → rename `target → target.<ext>` before linking | **design (spec-only)** — the activate script does this; sui's interp doesn't yet | activation farm interp (**GAP**) |
| I9 | **A store path's on-disk basename ⇄ node identity is prefix-independent** | GC + refs keyed on basename, not `/nix/store`-hardcoded absolute | **truly-unrep-adjacent** (the basename IS the identity; already the shipped design) | `collect_garbage` basename domain (**REAL**, `local.rs:399`) |

**The two honest floors to name loudly:** I7 (store-optimise is only-mitigated —
a mid-link crash can lose a file; the link-tmp-then-rename fix is owed) and I6a
(indirect-root completeness is a real correctness gap that could wrongly GC a
`./result`-rooted output). Neither is rounded up to "sealed."

---

## 4. Coverage scorecard — sui-real vs gap

Legend: **REAL** = impl + tests, verified by reading · **SPEC-ONLY** = typed
border + parser + M3.0 text interp, no FS realizer · **PARTIAL** = works for the
common case, named gap · **GAP** = absent.

| Surface | Graph algorithm | sui today | Verdict | Owning code |
|---|---|---|---|---|
| Store hardlink optimise | DSU dedup | REAL, CLI-wired; needs `links/`-dir parity + reflink + I7 fix | **REAL** (Wave-2 Done-candidate) | `sui-store/local.rs:544` |
| buildEnv / symlinkJoin | k-way priority tree merge | **absent — no primitive** | **GAP** (largest) | — |
| Profiles + generations | atomic-swap head + linear list | REAL (set/list/switch/rollback/delete + atomicity); manifest.json gap | **REAL** | `sui-store/profile.rs` + `sui-spec/profile.rs` |
| GC roots liveness | multi-source BFS reachability | REAL mark-and-sweep, store-relative roots | **REAL** (mislabelled InProgress) | `sui-store/local.rs:384` + `sui-spec/gc.rs` |
| — indirect / `auto` roots | two-hop resolution | not chased | **GAP** (I6a) | `local.rs:671` |
| — store lock during GC | mutual exclusion | spec has phases; impl takes no real lock | **PARTIAL** | `gc.rs` vs `local.rs` |
| Activation `/etc` farm + swap | topo compose + atomic swap + backup | typed spec REAL; **realizer emits comment text only**, real activation shells out to closure's `/activate` | **SPEC-ONLY realizer / REAL-by-delegation** | `sui-spec/activation_script.rs` + `orchestrate/system.rs:280` |
| `./result` + indirect root | single indirect edge | no result-symlink write; no two-hop root | **GAP** | — |

**Reuse-first note:** three phases below *extend* an existing sui primitive
rather than build fresh — store-optimise's DSU (§2.1), `find_gc_roots`
reachability (§2.4), and the atomic-swap in `ProfileManager` (§2.3) are the
reusable cores a buildEnv merge, an indirect-root chase, and an activation-farm
interpreter respectively lean on.

---

## 5. Phased assault — dependency-ordered, each Parity-Method'd

Sequenced so `sui-spec` execution follows the **module-system wave** (Phase A of
`SUI-SUPREMACY-ROADMAP.md`): any phase whose realizer needs the evaluated module
config (activation, home-files farm) is **flagged `[gated: M2.6]`** — it can be
*authored* (typed border + Lisp + mock-tested interp) now, but its live parity
against real nix is blocked until the module-system fixpoint lands.

### Phase L0 — Store-optimise → Done (independent; hardening) — **S**
Not gated. Extends the REAL `optimise_store`.
- **Fix I7** (link-tmp-then-rename, so a mid-link crash never loses a file) +
  add cppnix `/nix/var/nix/db/links` `links/`-dir parity.
- **Oracle:** `nix-store --optimise` freed-bytes + `links/` inode set vs
  `sui store optimise`. (Already has the differential test per roadmap Wave 2.)

### Phase L1 — GC root completeness → Done (independent) — **S**
Not gated. Extends the REAL `find_gc_roots` + `collect_garbage`.
- Chase two-hop **indirect / `auto/` roots** (seal I6a) + take a real store lock
  around the critical section (wire the `LockStore`/`UnlockStore` phases to a
  real flock, closing the PARTIAL).
- **Oracle:** `nix-store --gc --print-dead` dead-set == sui's dead-set, on a
  store containing a `./result`-only-rooted output (the case indirect roots fix).

### Phase L2 — buildEnv / symlinkJoin primitive (independent) — **M** *(largest net-new)*
Not gated (a buildEnv merges *already-built* store paths — no module eval needed).
- Author `(deflinkgroup :kind BuildEnv …)` + the k-way priority-merge interp +
  `CollisionPolicy` sealing I2 (unpriced collision → typed error, never LWW) +
  `makeBinPath`/`makeWrapper` as thin consumers.
- **Oracle:** build a `buildEnv`/`symlinkJoin` under nix, `find`-diff its symlink
  tree (target-for-target) against sui's; assert the `.priority`-collision
  winner matches and an unpriced collision is a red error in both.

### Phase L3 — `./result` symlink + indirect-root write (depends on `sui build`) — **S**
- On `sui build`, write the `./result` symlink + register the
  `gcroots/auto/<hash>` indirect root (the write half of L1's read fix).
- **Oracle:** `nix build` `./result` target == `sui build` `./result` target;
  the auto-root keeps the output live across a `sui store gc`.

### Phase L4 — Activation link-graph interpreter `[gated: M2.6]` — **L**
Authorable now against a mock config; live parity gated on the module system.
- Replace the M3.0 *text-emitting* `activation_script::apply` with a real
  `LinkEnvironment` interpreter: build the `/etc` (and home-files) farm as a real
  buildEnv-merge (reuse L2) with backup semantics (seal I8), order snippets by
  **topological sort** of the activation-DAG (seal I4), and perform the atomic
  `/run/current-system` swap (reuse §2.3's rename-swap, seal I5). This makes
  activation **sui-native** instead of shelling to the closure's `/activate`.
- **Oracle:** activate under nix, snapshot the `/etc` symlink farm + the
  `/run/current-system` target + the activation-snippet order; diff against sui's
  interpreter output. **`[gated]`** — needs the evaluated module config that M2.6
  unblocks.

### Phase L5 — Full generation manifest (`manifest.json`) — **S**
- Teach `ProfileManager` to read/write the post-2.4 `nix profile` JSON manifest
  (the `ProfileFormat::manifest_path` field is already named).
- **Oracle:** `nix profile list --json` == sui's manifest read; a `nix profile
  install` generation is listable by sui.

**Ordering rationale:** L0/L1/L2/L3 are the *store-side* link graph — all
independent of the module system, all parallelizable with the in-flight M2.6
work, each flipping a scorecard row to Done for the cost of a differential test
(+ the L2 net-new primitive). L4 is the *activation-side* graph — the one phase
that genuinely needs M2.6's evaluated config, so it's sequenced last and flagged.

---

## 6. What linking must hold for the cid-rebuild activation proof

The marquee proof — `sui system rebuild switch --flake .#cid` activates cid
byte/behavior-identical to `darwin-rebuild switch` — is the ACTIVATE stage's
acceptance test. From the link graph, **exactly these must be true**:

1. **The toplevel is a store object with the identical basename** — i.e. `sui
   system rebuild build`'s `system_path` basename == `darwin-rebuild build`'s.
   This is *not* a linking fact; it's the eval→build→store fact gated on M2.6
   (`SUI-SUPREMACY-ROADMAP.md` §2 #1–8). Linking rides on top of it.
2. **The system profile head links to that toplevel atomically** —
   `ProfileManager::set` creates `system-<N+1>-link → <toplevel>` and
   `rename`-swaps `system → system-<N+1>-link` (seal I5). **REAL today**
   (`profile.rs`); this is the `/nix/var/nix/profiles/system` edge.
3. **`/run/current-system` swaps to the new toplevel atomically** — today this
   is done *by the closure's own `/activate` script* that sui shells out to
   (`system.rs:280`), which is correct-by-delegation for the proof (the activate
   script is part of the byte-identical closure). **Phase L4** would make it
   sui-native, but the proof does **not** require L4 — it requires that sui runs
   the *identical* activate script against the *identical* profile edge.
4. **The `/etc` symlink farm + launchd plists land identically** — likewise done
   by the delegated `/activate`; sui must not perturb the farm. The proof's
   linking obligation is therefore **negative**: sui's profile-set + activate-run
   must produce a `/run/current-system` target and an `/etc` farm *equal to
   nix's*, which it does by (a) setting the same toplevel via the REAL
   `ProfileManager` and (b) running the same closure-baked activate script.

**The crisp statement:** for the cid proof, the *only sui-native linking on the
critical path is the profile head swap* (#2, REAL) — everything else
(`/run/current-system`, `/etc` farm, launchd) is linked by the closure's own
activate script that sui invokes verbatim. So **the cid proof needs zero net-new
linking work** beyond what's shipped; the linking gaps in §4 (buildEnv,
indirect roots, native activation interp) are *supremacy-completeness* work, not
*cid-proof-blocking* work. The one linking fact the proof depends on — atomic
profile-set to the correct toplevel — is REAL and tested today. The proof's
sole blocker remains M2.6 (getting the *correct toplevel* to set), exactly as
the roadmap states.

---

## 7. Composition with the org doctrines

- **★★ TYPED-SPEC + INTERPRETER TRIPLET** — `link_graph` is one more sui-spec
  triplet (border + `.lisp` + mockable-`Environment` interp), following `gc` and
  `activation_script` verbatim.
- **★★ UNREPRESENTABILITY** — I1 (closed `LinkMech`) is truly-unrep; I2/I4
  (unpriced-collision, dep-cycle) are eval-rejected; I3/I5/I6 are runtime/POSIX/
  algorithmic invariants honestly *below* a compile-time type; I7 is a named
  only-mitigated remediation item. The doc never calls a mitigation a proof.
- **★★ CATALOG REFLECTION** — the six linking operations become catalog entries;
  the substrate-invariant test (every domain has a catalog row + a loadable
  module) already enforced across sui's 22 domains extends to `link_graph`.
- **★★ CONQUER** (corner → make-impossible → strengthen) — each phase corners one
  link surface, seals its worst state to the highest honest tier, then the next
  phase strengthens (L2's merge → L4's activation farm reuses it; L1's read →
  L3's write).
- **SUI-SUPREMACY-ROADMAP** — this doc is the ACTIVATE-stage detail of that
  roadmap's Phase B/C; L0/L1 *are* roadmap Waves 2–3, and L4 is the activation
  half that its Phase A (M2.6) unblocks.
