# sui ground-truth — ONE full-stack instance

The shortest honest path to a single running `sui` super-cache instance:
**Postgres L2 store + Redis L1 hot cache + tmpfs RAMDISK build sandbox + the
sui daemon** — the never-touch-durable-disk posture, narrowed to one node.

This directory is the **run-recipe + config** for that instance. Author them
here; the run is a follow-on phase.

## Artifacts

| File | What it is |
|---|---|
| `supercache-config.toml` | The **shikumi `SuperCacheCiConfig` posture** (STORE=Postgres, CACHE=Redis L1, sandbox=tmpfs RAMDISK, `never_touch_disk=true`), narrowed to one local node (localhost endpoints, 8 GiB-VM bands, fleet-only surfaces empty). Fed to `sui cache serve --supercache-config`. |
| `tiered-backend.toml` | The **raw `sui_cache::BackendConfig::Tiered`** that `SuperCacheCiConfig::to_backend_config` produces for the localhost posture. Fed to `sui cache serve --backend-config` (one fewer translation hop; identical runtime shape). |
| `breathe-memoryband.yaml` | The **breathe `MemoryBand` CR**, `mode: shadow` (dryRun-first) — the daemon's fleet enrollment. Tier-honest: applies WHEN the node runs on a breathe-managed cluster; locally the band is a static ceiling (see below). |
| `run.sh` | The **host-glue recipe** — brings up pg + redis + tmpfs + the daemon as one podman pod. `up` / `down` / `status` / `smoke`. Host publish port defaults to **5555** (`HOST_PORT`), because macOS reserves `:5000` for the AirPlay Receiver. |
| `measure.sh` | The **measurement harness** — timed PUT/GET narinfo + NAR through the running daemon, confirms write-through (L2-durable-first) + read-through-promotion, reports cold(L2)-vs-warm(L1) latency. Host glue; the typed work is the Rust `TieredBackend`. |
| `Containerfile` | INTERIM in-VM build image for the daemon (`--features tiered`). Build base pinned to `rust:1-slim-bookworm` so its glibc MATCHES the bookworm runtime stage (the floating `rust:1-slim` = trixie/glibc-2.41 links symbols bookworm/2.36 lacks). Destination is the flake's `dockerImage-<arch>` dockerTools output (Pillar 8). |

## Recommended host

The **already-running podman machine VM** (aarch64 Linux, 4 CPU / 8 GiB) — self
contained: no VPN, no rio, no cloud. sui's build sandbox is Linux-only
(`unshare(1)`; macOS gets `NoSandbox`, no tmpfs), so a Linux host is required for
a real RAMDISK sandbox. `postgres:17-alpine` is already pulled locally.

## Run it — RAN 2026-07-06, first real numbers captured

This instance was brought up on the local aarch64 podman VM: the daemon booted,
connected to **real Postgres L2 + Redis L1**, served the Nix cache protocol, and
was measured through the live `TieredBackend`. Write-through (L2-durable-first)
and read-through-with-promotion both confirmed; PgStore promoted
`MockParityProven → LiveClusterProven`. Headline: at NAR scale (1.5 MiB) the
Redis L1 read is **~3.5× faster at p50** than the Postgres L2 fetch; at narinfo
scale (200 B) the delta is in the sub-2 ms axum/loopback noise (honest — no L1
win to claim there). Full numbers + the two blockers fixed (glibc skew, port
5000/AirPlay collision) + the remaining gaps (RAMDISK-build delta, `PUT /nar`
2 MiB body limit) live in the ground-truth numbers writeup.

```sh
# from the sui repo root, on a host with `podman machine` running:
export DOCKER_CONFIG=/tmp/sui-gt-emptydockercfg    # bypass malformed ~/.docker/config.json
HOST_PORT=5555 contrib/ground-truth/run.sh up      # pg + redis + tmpfs + sui cache serve (~4 min cold build)
HOST_PORT=5555 contrib/ground-truth/run.sh smoke   # GET /nix-cache-info → 200 proves the daemon is up
HOST_PORT=5555 contrib/ground-truth/measure.sh     # timed PUT/GET tier measurement
HOST_PORT=5555 contrib/ground-truth/run.sh status
HOST_PORT=5555 contrib/ground-truth/run.sh down
```

The daemon dispatches through the SAME typed `sui_cache::build_backend` factory
as any other backend — the tiered config just hands it `Tiered { l1: Redis,
l2: Pg, l3: Local }` instead of the disk floor. A tiered/redis/pg arm whose
Cargo feature is off returns `CacheError::NotImplemented` — never a silent disk
fallback (why the recipe builds with `--features tiered`).

## What running this MEASURES (software ground-truth)

- Redis L1 hit-rate vs cold; the L1→L2→L3 read-through / write-through behavior
  of the `TieredBackend` resolver (its whole point).
- RAMDISK(tmpfs) sandbox vs on-disk build latency + the `never_touch_disk`
  posture (no durable-disk writes during eval/build).
- Postgres L2 durable-store correctness against a **real Postgres** — this
  promotes `PgStore` from `MockParityProven → LiveClusterProven` (the first
  real-Postgres proof).
- Daemon latency: `cache serve` HTTP request latency; eval + build wall-clock;
  cache warm-vs-cold deltas.
- End-to-end: a real derivation eval+build lands its output in the Postgres
  store via the Redis-fronted tiered path, sandbox on tmpfs.

## What this does NOT measure (needs the fleet — tier-honest)

- Spot / scale breathability, 100%-spot posture, breathe auction, ReplicaBand
  HA, retirada drain-ahead.
- **LIVE breathe `MemoryBand` reconcile.** On one local podman VM there is NO
  breathe controller. The daemon's memory ceiling is a **STATIC cap** applied by
  hand (`--memory 2g` on the daemon, `--maxmemory 2gb` on Redis,
  `--tmpfs …:size=3g` on the sandbox) — the band setpoint applied ONCE, not a
  live reconcile. `breathe-memoryband.yaml` (`mode: shadow`) is the enrollment
  for the fleet path, where the band is live-reconciled. Promote it up the
  ladder (`shadow → shadowConfirmEffect → effect`) only after the shadow window
  holds — the same ladder the rio pangea-database + pangea-operator bands
  followed.
- Multi-node cache sharing, remote build workers, autorevivy live-tuning.

## Code wiring landed for this (all mockable, tested, 0 new warnings)

- `Cargo.toml` — root `[features]` passthrough (`postgres` / `redis-client` /
  `tiered` → the sui-cache/sui-store leaf features), so `--bin sui --features
  tiered` compiles the Redis + Postgres arms. Plus the interim
  `tatara-lisp-derive = "=0.2.2"` pin (a fleet-wide caret-skew hazard;
  destination = the upstream reconcile).
- `sui-supercacheci/src/backend.rs` — the pure, I/O-free
  `SuperCacheCiConfig::to_backend_config` translation (Redis L1 ← cache,
  Postgres L2 ← store, S3-or-local L3 ← object store, write-through). 8 unit
  tests incl. the bare()-floor-is-a-typed-error guard.
- `src/main.rs` — `cache serve --backend-config <file>` /
  `--supercache-config <file>` config-select the tiered backend
  (`resolve_serve_backend`), replacing the hard-coded `BackendConfig::Local`.
