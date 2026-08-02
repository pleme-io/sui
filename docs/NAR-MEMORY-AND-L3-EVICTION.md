# NAR memory bounding, and why L3 eviction is not in this change

Status as of 2026-08-01.

- **Part 1 — the streaming NAR path — is SHIPPED and measured.**
- **Part 2 — L3 eviction — is DESIGN ONLY. No code. Do not cite it as existing.**

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

## Part 2 — DESIGN ONLY: L3 eviction

**Nothing here is implemented.** The local tier still has no eviction: the tmpfs
fills once and then rejects every write forever, which is why its size barely
mattered. `atatame` names `sui cache watch` as the GC and it is not shipped.

### Why it is not in this change

The instruction was to implement it only if it were small, and to write the
design rather than half-build a GC. It is not small, for four reasons — the
first of which is a **build-breaking correctness hazard**, not a size problem:

1. **Evicting a NAR can strand its narinfo, and a stranded narinfo is worse
   than a miss.** A client fetches `…​.narinfo` (200 OK, advertising
   `URL: nar/…`), then fetches that NAR and gets 404. Nix treats a *missing
   advertised NAR* as a hard failure, not a cache miss — the same class of
   outage as the 2026-07-26 incident, where 500s from a substituter failed every
   build on the cluster. So eviction must remove the narinfo **and** its NAR
   together.
2. **The reverse index does not exist.** narinfo is keyed by the 32-char
   store-path hash; the NAR is keyed by its **narhash**. There is no
   narhash → store-hash map anywhere in `sui-castore` — three backends carry a
   comment saying exactly this, and `delete` works around it by best-effort
   guessing `nar/{hash}.{xz,zst,nar}`. Pairwise eviction needs that index built
   and maintained, which is the actual body of work.
3. **LRU needs an access time the filesystem may not keep.** tmpfs and most
   production mounts run `relatime` or `noatime`. For content-addressed files
   mtime is effectively creation time, so an mtime-ordered policy is **FIFO, not
   LRU** — and calling it LRU would be exactly the rounding-up this repo forbids.
   Real recency needs an in-process hit counter, which is lost on every pod roll.
4. **Concurrency.** Byte accounting must survive concurrent writers, and
   eviction needs single-flight so N simultaneous over-watermark writes do not
   each scan and delete.

### The shape it should take

- `LocalStorage::with_capacity(root, high_water, low_water)`; absent capacity
  keeps today's unbounded behaviour, so the change is opt-in and reversible
  (★★ MODULARIZE, DON'T DELETE — a typed field, never a deletion).
- One `AtomicU64` of tracked bytes, seeded by a single scan at construction and
  maintained by write/delete. No per-write directory scan.
- A `tokio::sync::Mutex` single-flight around the sweep; crossing `high_water`
  triggers one sweep down to `low_water`.
- **Pairwise eviction over a persisted `narhash → store-hash` index.** This is
  the real prerequisite and should land first, on its own, because `delete`'s
  extension-guessing is already a latent bug independent of eviction.
- A typed `EvictionPolicy` (`Fifo` by mtime is honest and cheap;
  `Lru` only once a hit counter exists) rather than a bool.
- Gates: a capacity test that writes past the high-water mark and asserts the
  directory settles at/below it; and a test that eviction **never** leaves a
  narinfo whose advertised NAR is gone.

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

Two of these were **green on the first attempt**, and the cause was the break,
not the gate — recorded because it is the INSTRUMENT RULE working:

- the marker break edited `put_nar_stream`, but the test writes orphan chunks
  directly through the conn seam and never calls it; the gate's subject is the
  **read** side;
- the rename break aimed the write at the final path but left the failure
  cleanup, which then deleted the very partial file the test exists to catch.

Both went red once the break matched what the gate actually asserts.
