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
| `run.sh` | The **host-glue recipe** — brings up pg + redis + tmpfs + the daemon as one podman pod. `up` / `down` / `status` / `smoke`. |
| `Containerfile` | INTERIM in-VM build image for the daemon (`--features tiered`). Destination is the flake's `dockerImage-<arch>` dockerTools output (Pillar 8). |

## Recommended host

The **already-running podman machine VM** (aarch64 Linux, 4 CPU / 8 GiB) — self
contained: no VPN, no rio, no cloud. sui's build sandbox is Linux-only
(`unshare(1)`; macOS gets `NoSandbox`, no tmpfs), so a Linux host is required for
a real RAMDISK sandbox. `postgres:17-alpine` is already pulled locally.

## Run it (the follow-on phase)

```sh
# from the sui repo root, on a host with `podman machine` running:
contrib/ground-truth/run.sh up      # pg + redis + tmpfs + sui cache serve
contrib/ground-truth/run.sh smoke   # GET /nix-cache-info → 200 proves the daemon is up
contrib/ground-truth/run.sh status
contrib/ground-truth/run.sh down
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
