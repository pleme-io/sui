# NAR memory bounding, the narhash reverse index, and why L3 eviction is still not in

Status as of 2026-08-02.

- **Part 1 — the streaming NAR path — is SHIPPED and measured.**
- **Part 2 — the narhash → store-hash reverse index — is SHIPPED.** It was named
  here as eviction's blocking prerequisite; it landed on its own, and `delete`
  now resolves the NAR from the narinfo instead of guessing extensions.
- **Part 3 — L3 eviction — is DESIGN ONLY. No code. Do not cite it as existing.**

---

## 0. The incident this came from

`sui` on `camelot-eks` OOMKilled six times in one day (exit 137, cgroup OOM, not
node pressure). Two operator-side mitigations failed to bound it. The root was
not a leak or a tuning error — it was a **type signature**:

```rust
async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;
async fn put_nar(&self, path: &str, data: &[u8])  -> Result<(), StoreError>;
```

Owned bytes in, owned bytes out. Streaming a NAR was **not expressible** through
that trait, so every NAR was fully resident for as long as it took to write or
serve. Measured evidence from the live pod: a single Postgres L2 NAR `INSERT`
took **12.712 s**, with the NAR in the heap that whole time. The pod limit was
6 Gi on a node with 6.66 Gi allocatable, so the peak was set by the largest NAR
in flight and nothing bounded it.

Worse, the resident copies stacked. For one NAR of size `N` through
`PUT /nar/…` on a tiered backend, before this change:

| Where | Cost |
|---|---|
| axum `Bytes` body extractor (frames + concatenation) | up to `2N` |
| `sqlx` bind buffer for the L2 `INSERT` | `N`, held 12.7 s |
| `tokio::fs::write` copying into the blocking closure (L3) | `N` |
| `redis` command buffer (L1 warm) | `N` |

That is `~4–5N` per concurrent upload. Two overlapping 800 MiB NARs is the whole
6 Gi budget.

---

## Part 1 — SHIPPED: the streaming NAR path

### The trait shape, and why it is a *source* and not a `Stream`

```rust
pub const NAR_CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub type NarStream = BoxStream<'static, Result<Bytes, StoreError>>;

#[async_trait]
pub trait NarSource: Send + Sync {
    fn size_hint(&self) -> Option<u64> { None }
    async fn open(&self) -> Result<NarStream, StoreError>;   // re-openable
}

// on StorageBackend:
fn nar_residency(&self) -> NarResidency;                     // REQUIRED, no default
async fn get_nar_stream(&self, path: &str) -> Result<Option<NarStream>, StoreError>;
async fn put_nar_stream(&self, path: &str, src: &dyn NarSource) -> Result<(), StoreError>;
```

The read side is an ordinary one-shot `Stream`. The **write** side is a source
that can be **opened once per destination**, and that is the whole design
decision.

`TieredBackend::put_nar` must write **L2, then L3, then warm L1**, gating on the
two durable tiers via `durable_write_outcome` before the best-effort hot warm,
whose result is discarded. A one-shot `Stream` can be consumed exactly once, so
it would force one of:

- buffer the NAR to fan it out to three tiers — the bug being fixed; or
- interleave chunks across all three tiers concurrently — which changes that
  ordering, and the ordering is load-bearing (a refused L1 warm must never fail
  a build).

A re-openable source keeps `put_nar_stream` **line for line** the previous
`put_nar` with `data: &[u8]` swapped for `src: &dyn NarSource`. Nothing about
the ordering, the durable gate, or the discarded L1 result moved.

### `NarResidency` — why every backend must declare

The streaming verbs carry buffering **defaults**, so a five-line test double
stays five lines. That default is also how a future production backend could
silently reintroduce the OOM. So the declaration is mandatory and has no
default:

```rust
pub enum NarResidency {
    Streaming,        // O(chunk), whatever the NAR size
    Capped(usize),    // O(min(nar, cap)); past the cap it is REFUSED, not buffered
    WholeValue,       // O(nar) — in-memory doubles only
}
```

- Adding a `StorageBackend` without deciding is a **compile error**.
- Shipping a production backend that says `WholeValue` fails
  `every_production_backend_bounds_its_nar_path` in CI.
- `TieredBackend` reports its **weakest tier's** residency, not the resolver's,
  so a streaming resolver in front of a whole-value tier cannot claim a bound it
  does not have.

**Tier-honest:** this is *parse-time-rejected* (the decision cannot be omitted)
plus *CI-gate-caught* (the wrong answer cannot ship from the factory). It is
**not truly-unrepresentable** — `collect_nar` and `BytesNarSource` still exist,
and a hand-constructed backend may use them.

### Per-backend

| Tier | Residency | Mechanism |
|---|---|---|
| `LocalStorage` (L3) | `Streaming` | chunked read; chunked write to a **per-write scratch file**, then `rename` |
| `PgStorageBackend` (L2) | `Streaming` | NAR stored as `NAR_CHUNK_BYTES` rows in `sui_cache_nar_chunk`, published by a completeness marker; legacy whole-value rows served via windowed `substr` |
| `RedisBackend` (L1) | `Capped(64 MiB)` | Redis has no streaming `SET`; collection **stops at the cap** and refuses |
| `S3Storage` | `Streaming` | `WriteMultipart` with 5 MiB parts and ≤2 in flight; `abort` on fault |
| `TieredBackend` | weakest tier | opens the source once per tier, ordering unchanged |

Two invariants were introduced *because* streaming removed atomicity that the
whole-value write got for free:

- **Local: write-then-rename.** A chunked write that dies partway (or hits
  `ENOSPC` three chunks in — the exact live failure on the full tmpfs) would
  otherwise leave a **truncated NAR at the real path**. A truncated NAR is
  silent corruption, strictly worse than the OOM. Writing aside and renaming
  means a partial write leaves nothing and the next read is a clean miss.
- **Postgres: a completeness marker.** `N` chunk `INSERT`s are not atomic the
  way one was. A marker row at `seq = -1`, holding the chunk count, is written
  **last** and read **first**. A process killed mid-write publishes no marker,
  so the key reads as a miss. A chunk gap *under* a published marker is a typed
  error, never a short read.

Scratch and spool names are unique **per write, not per key**: concurrent pushes
of the same content-addressed key are routine, and a shared name would let two
writers interleave into one file and rename a spliced NAR into place.

### The HTTP boundary

- `GET /nar/{path}` wires the backend's chunk stream straight to the socket.
- `PUT /nar/{path}` takes `Body` (not `Bytes`), spools it in bounded chunks via
  `spool_or_buffer`, and hands the backend a re-openable `SpooledNarSource`.

The spool directory is `std::env::temp_dir()`, i.e. **`TMPDIR`** — an operator
retargets it without a code change. If a spool file cannot be *created* (no
dir, no permission, full volume) the ingest falls back to an in-memory buffer
**hard-capped** at `DEFAULT_INGEST_MEMORY_CAP` (256 MiB), refusing past it. The
choice is made *before any bytes are read*, because a spool that fails halfway
has already consumed a one-shot stream and cannot restart.

That fallback is the one place a whole NAR can still be resident, and it is
bounded, not unbounded. It is the deliberate trade against the alternative,
which is 500ing every push on a pod with no usable `TMPDIR`.

### Measured

`sui-cache/tests/nar_memory_bound.rs` pushes a **256 MiB** NAR through the real
`PUT` handler over a three-tier `TieredBackend`, then serves it back, measuring
peak **live heap bytes** with a counting `GlobalAlloc`.

```
nar=268435456  budget=67108864
control_peak=268501032   write_peak=8394909   read_peak=6292056
```

- **write: 8.0 MiB** for a 256 MiB NAR — **32×** below the NAR, and flat in NAR
  size (two 4 MiB chunks plus wire frames).
- **read: 6.0 MiB** — **42×** below.
- **control: 256 MiB.** The same stream collected into one buffer, asserted to
  *exceed* the budget. Per the repo's INSTRUMENT RULE, the gate's green only
  means something because the control's red proves the meter can see a resident
  NAR. A meter stuck at zero fails there first.

The test also checksums the served bytes, so "moved 256 MiB cheaply" cannot be
satisfied by "did nothing cheaply".

**What the number is, honestly.** It is peak *live heap* (allocated minus
freed), not peak RSS. It counts every `Vec`/`Bytes`/`BytesMut`/`String` held at
once — which is exactly where a resident NAR lives. It does **not** count
allocator fragmentation, unreturned arenas, thread stacks, `mmap`'d pages, or
the kernel page cache behind the spool file, so absolute RSS is somewhat higher.
What the gate proves is that **the peak no longer scales with NAR size**, which
is the property that was violated. Portable peak-RSS needs a platform crate;
this is the honest instrument that runs everywhere the suite runs.

### What is still unbounded after Part 1

1. **Concurrency.** Each in-flight upload costs ~8 MiB and each download ~6 MiB,
   but nothing caps *how many* run at once. `DefaultBodyLimit::disable()` is
   still set, so a single NAR may also be arbitrarily large on the wire (its
   bytes now land on the spool, not the heap). A concurrency limit —
   `tower::limit::ConcurrencyLimitLayer` or a semaphore on the NAR routes — is
   the natural next bound and is **not** in this change.
2. **The in-memory ingest fallback**, 256 MiB per concurrent upload on a pod
   with no usable spool dir. Bounded, but far above the streaming path.
3. **Legacy Postgres rows** are served windowed, which bounds *this* process;
   `substr` over a compressed TOAST value still detoasts server-side, so the
   Postgres pod's own memory is unchanged for those rows. They drain as the
   cache turns over.
4. **`get_nar`/`put_nar`** (the whole-value verbs) still exist and are still
   `O(nar)`. They are no longer on the server path, but `push.rs`, `gc.rs`,
   `agent.rs` and `sui-registry` still use them.

---

## Part 2 — SHIPPED: the narhash → store-hash reverse index

This is the piece Part 3 named as its blocking prerequisite. It landed on its
own, because `delete`'s extension-guessing was already a latent bug independent
of eviction.

### The problem it closes

The two halves of a binary cache are keyed differently:

| Artifact | Key |
|---|---|
| narinfo | the 32-char **store-path** hash |
| NAR blob | the **narhash** — `nar/<filehash>.nar.xz`, from the narinfo's `URL:` |

Forward is easy (read the narinfo, take `URL:`). Backward — from a NAR to the
narinfo(s) advertising it — was not expressible, and `delete` worked around it
by best-effort-deleting `nar/{store-hash}.{xz,zst,nar}`. That guess is wrong on
both sides: those three keys are normally *other paths' NARs or nothing*, and
the real NAR survived.

Two narinfos genuinely can advertise one NAR: a NAR serializes a store path's
*contents*, not its name, so two paths with byte-identical contents produce one
narhash and one `URL:`. So the index is a **set** of referrers, not one.

### The shape

One persisted edge per `(nar_path, store_hash)` pair — never a set-valued
record. Recording is then a blind write of a key that names its own content, so
two concurrent narinfo pushes cannot lose each other's edge the way a
read-modify-write of a shared set would (and S3 offers no compare-and-swap to
fix that with).

```rust
#[async_trait]
pub trait NarRefIndex: Send + Sync {
    async fn record(&self, nar_path: &str, hash: &str)   -> Result<(), StoreError>;
    async fn forget(&self, nar_path: &str, hash: &str)   -> Result<(), StoreError>;
    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError>;
}

// on StorageBackend — REQUIRED, no default:
fn nar_ref_index(&self) -> &dyn NarRefIndex;
```

The key is one typed `Display` surface (`NarRefKey` / `NarRefScan`), so the four
key-value tiers cannot drift into four encodings of the same edge.

| Tier | Where the edge lives | Lookup |
|---|---|---|
| `LocalStorage` (L3) | empty file at `<root>/nar-refs/<nar path>/<hash>` | one `read_dir` |
| `PgStorageBackend` (L2) | zero-value row in `sui_cache_nar_ref`, keyed by `NarRefKey` | primary-key range scan via `starts_with` |
| `RedisBackend` (L1) | `sui:nar-refs/<nar path>/<hash>`, **no TTL** | `SCAN MATCH` |
| `S3Storage` | zero-byte object at `nar-refs/<nar path>/<hash>` | `LIST` under the prefix |
| `TieredBackend` | fan-out; durable-gated on L2/L3, best-effort L1 | **union** of all three |

### Which way each choice rounds

Over-reporting a referrer keeps a NAR that could have been reclaimed — a leak.
Under-reporting deletes a NAR another narinfo still advertises — an outage.
**Every decision rounds toward over-reporting**, and the ordering is chosen for
it:

- `put_narinfo` records the edge **before** writing the narinfo record. A crash
  between the two leaves an edge with no narinfo (leak). The other order leaves
  a narinfo with no edge (strand).
- `delete` removes the narinfo record **first**, then forgets the edge. A crash
  between leaves a stale edge (leak), not a live narinfo with no edge (strand).
- `TieredBackend::referrers` unions **all three** tiers, including the partial
  hot one — the opposite of `list_narinfos`, which reads only the authoritative
  tiers because there a hot tier's partial view would *under*-report.
- A tier whose referrer read *fails* propagates the error. "This tier is down"
  must never resolve to "nobody advertises this NAR" for something about to
  delete.
- Redis edges carry **no TTL** even when its narinfo/NAR writes do, so an edge
  cannot expire out from under a live narinfo.

### What `delete` does now

1. Resolve the advertised NAR from the narinfo's own `URL:` — never guess.
2. Remove the narinfo record.
3. Forget this path's edge.
4. Remove the NAR **only if** no other referrer remains.

Step 4 is the whole point: resolving without it would be a regression, because
the first of two co-referring paths to be deleted would take the shared NAR with
it.

### Two smaller things that fell out, and are load-bearing

- **`URL:` is read directly, not through `NarInfo::parse`.** The strict parser
  requires `FileHash`/`FileSize` and rejects the whole document without them — but
  such a narinfo *still advertises a NAR*, and a client still hard-fails on a 404.
  Indexing through the strict parse would have left exactly those narinfos out of
  the index, and an unindexed narinfo sharing a narhash with an indexed one gets
  stranded when the indexed one is deleted.
- **An unaddressable `URL:` is refused at the boundary.** A narinfo arrives over
  `PUT /<hash>.narinfo`, and its `URL:` is used as a key *and* joined onto the
  cache root. `URL: ../../etc/passwd` is now a 400 at the HTTP handler and a typed
  `StoreError::NarInfo` at the backend. That hole predates this change —
  `LocalStorage::delete` already joined the raw `URL:` onto the root.

### Tier honesty

- **Parse-time-rejected** — `nar_ref_index()` is required with no default, on the
  `nar_residency()` precedent. An empty default index does not read as "unknown";
  it reads as "nobody advertises this NAR", which is exactly the answer that
  authorizes deleting a NAR out from under a live narinfo.
- **Only-mitigated** — `put_narinfo` and `delete` are *provided* methods, so a
  backend implements only the raw record verbs and cannot forget to index. A
  backend that overrides them can still get it wrong; that is caught by
  `every_production_backend_pairs_its_nar_with_its_narinfo` in CI, not by the
  type system.
- **A stated migration gap** — a store written by a pre-index binary has no
  edges, so deleting one of two co-referring paths strands the other until
  `reindex_nar_refs()` has run once. Both halves are asserted in
  `an_unindexed_store_can_strand_until_reindexed`, so the gap can be neither
  quietly forgotten nor quietly claimed closed.

---

## Part 3 — DESIGN ONLY: L3 eviction

**Nothing here is implemented.** The local tier still has no eviction: the tmpfs
fills once and then rejects every write forever, which is why its size barely
mattered. `atatame` names `sui cache watch` as the GC and it is not shipped.

### What is left, now that the index exists

The four reasons this was not small were, in order:

1. ~~**Evicting a NAR can strand its narinfo.**~~ **Closed by Part 2** for the
   `delete(store_hash)` direction: `delete` will not remove a NAR another
   narinfo advertises. What eviction adds is the *other* direction — starting
   from a NAR and removing it — and that now has a legal way to run:
   `nar_ref_index().referrers(nar_path)` gives the narinfos to remove first (or
   the reason to skip this candidate). **The mechanism exists; nothing calls it
   from a NAR-first sweep yet.**
2. ~~**The reverse index does not exist.**~~ **Closed by Part 2.**
3. **LRU needs an access time the filesystem may not keep.** Unchanged. tmpfs and
   most production mounts run `relatime` or `noatime`. For content-addressed
   files mtime is effectively creation time, so an mtime-ordered policy is
   **FIFO, not LRU** — and calling it LRU would be exactly the rounding-up this
   repo forbids. Real recency needs an in-process hit counter, which is lost on
   every pod roll.
4. **Concurrency.** Unchanged. Byte accounting must survive concurrent writers,
   and eviction needs single-flight so N simultaneous over-watermark writes do
   not each scan and delete.

Two more that Part 2 surfaced rather than closed:

5. **A NAR-first enumeration.** A sweep needs "every NAR in this tier, with its
   size and mtime". `LocalStorage` can walk `nar/`; the trait has no verb for it,
   and the other tiers have no equivalent. Eviction is currently only sensible
   for L3, so an inherent `LocalStorage` method is the honest scope — a trait
   verb would be inventing a contract for tiers that will not implement it.
6. **`reindex_nar_refs()` must have run.** A sweep on an unindexed store reads
   every NAR as unreferenced and deletes the lot. Eviction must refuse to start
   until the index is known-seeded — a persisted marker, not a comment.

### The shape it should take

- `LocalStorage::with_capacity(root, high_water, low_water)`; absent capacity
  keeps today's unbounded behaviour, so the change is opt-in and reversible
  (★★ MODULARIZE, DON'T DELETE — a typed field, never a deletion).
- One `AtomicU64` of tracked bytes, seeded by a single scan at construction and
  maintained by write/delete. No per-write directory scan.
- A `tokio::sync::Mutex` single-flight around the sweep; crossing `high_water`
  triggers one sweep down to `low_water`.
- Per candidate NAR: `referrers(nar_path)` → `delete_narinfo_record` for each,
  then `delete_nar_record` — narinfo first, so a crash mid-eviction leaves an
  orphan NAR (reclaimable next sweep) and never a stranded narinfo.
- A typed `EvictionPolicy` (`Fifo` by mtime is honest and cheap;
  `Lru` only once a hit counter exists) rather than a bool.
- Gates: a capacity test that writes past the high-water mark and asserts the
  directory settles at/below it; and a NAR-first sibling of
  `every_production_backend_pairs_its_nar_with_its_narinfo` proving a sweep
  never leaves a narinfo whose advertised NAR is gone.

**Not obviously trivial even with the index.** Points 3–6 are each real work, and
5 is a new enumeration surface. The index removed the *correctness blocker*, not
the body of the job.

### What Part 1 already improved for a full tmpfs

Not eviction, but worth recording so the two are not confused:

- `ENOSPC` mid-write now leaves **nothing** behind. Previously a partial file
  would occupy space *and* be servable as a truncated NAR; write-then-rename
  makes a failed write a no-op.
- A full L3 does not fail a push: `durable_write_outcome` succeeds if **either**
  durable tier accepted the write, and logs the lost redundancy at `WARN`.

---

## Red runs

Every gate added or changed was broken deliberately and observed to fail for the
right reason, then restored.

| Gate | Break applied | Result |
|---|---|---|
| `nar_memory_bound` (write) | handler collects the body and calls `put_nar` | RED |
| `nar_memory_bound` (positive control) | counting allocator made a no-op | RED |
| `every_production_backend_bounds_its_nar_path` | `LocalStorage` declares `WholeValue` | RED |
| `streamed_put_writes_l2_then_l3_then_warms_l1` | L1 warm moved before the durable gate | RED |
| `a_write_killed_before_the_marker_reads_as_a_miss…` | read ignores the marker; chunk gap ends the stream quietly | RED |
| `a_write_that_dies_mid_stream_publishes_nothing_at_all` | scratch file + failure cleanup both removed | RED |
| `an_over_cap_stream_of_unknown_length_stops_at_the_cap` | cap dropped from `collect_nar` | RED |
| `every_production_backend_pairs_its_nar_with_its_narinfo` | `delete` stops consulting the index before removing the NAR | RED — `STRANDED — pathB's narinfo advertises …, which is gone` |
| `every_production_backend_pairs_its_nar_with_its_narinfo` | extension-guessing fan-out restored alongside the resolve | RED — `GUESSED — delete removed nar/pathA.nar.zst …` |
| `delete_resolves_the_nar_from_the_narinfo_instead_of_guessing` (pg / redis / s3 / tiered) | same guessing break | RED, all four |
| `an_unindexed_store_can_strand_until_reindexed` | `delete` stops consulting the index | RED on the *healed* half |
| `a_traversal_url_is_refused_rather_than_stored` | `is_addressable_nar_path` short-circuited to `true` | RED |
| `a_narinfo_the_strict_parser_rejects_still_advertises_its_nar` | `advertised_url_line` routed back through `NarInfo::parse` | RED |
| `a_scan_prefix_does_not_reach_a_longer_neighbour` | trailing `/` dropped from `NarRefScan`'s `Display` | RED |
| `the_index_is_a_directory_of_edge_files` + 5 others | `put_narinfo` stops recording the edge | RED |
| `a_failed_narinfo_delete_must_not_take_the_nar_with_it` | tiered `delete_narinfo_record` goes back to swallowing per-tier failures | RED — `the NAR must be untouched: its narinfo is still servable` |

Two of these were **green on the first attempt**, and the cause was the break,
not the gate — recorded because it is the INSTRUMENT RULE working:

- the marker break edited `put_nar_stream`, but the test writes orphan chunks
  directly through the conn seam and never calls it; the gate's subject is the
  **read** side;
- the rename break aimed the write at the final path but left the failure
  cleanup, which then deleted the very partial file the test exists to catch.

Both went red once the break matched what the gate actually asserts.
