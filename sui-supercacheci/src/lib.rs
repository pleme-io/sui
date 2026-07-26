//! `sui-supercacheci` — the typed [`shikumi::TieredConfig`] schema for
//! **`/super-cache-ci`**: service-level [`sui`] (the pure-Rust Nix VM) whose
//! **store lives in Postgres**, whose **hot cache lives in Redis**, and whose
//! **evaluation + build sandbox live in breathe-carved RAM/tmpfs** — so there
//! is no filesystem Nix store to touch and the fleet's *★★ MAGMA-NATIVE*
//! never-touch-durable-disk law is true *by construction* (the store IS the
//! database + RAM; the disk-loss failure class is unrepresentable, not
//! guarded).
//!
//! ## What this crate IS (the shipped increment)
//!
//! The **typed config schema + the fleet tier model** for the convergence,
//! and nothing else. It owns:
//!
//! - [`SuperCacheCiConfig`] — the top-level shape, authorable as
//!   `(defsupercacheci …)` (the Rust + Lisp primary pattern) and configured
//!   via [`shikumi::TieredConfig`] (default ← discovered ← override,
//!   hot-reload) exactly like every other operator-facing pleme-io tool.
//! - The two load-bearing tiers: [`SuperCacheCiConfig::bare`] encodes the
//!   **shipped reality** (on-disk graph store, disk allowed) and
//!   [`SuperCacheCiConfig::prescribed_default`] encodes the **destination
//!   posture** (Postgres store + Redis cache + tmpfs sandbox +
//!   `never_touch_disk = true`).
//!
//! ## What this crate is NOT yet (tier-honest LiveTODOs — never rounded up)
//!
//! No consumer reads this config yet. Each of the following is *designed here
//! as a typed knob* but *unwired in behavior*:
//!
//! - **LiveTODO(store)** — `impl Store for PgStore` behind
//!   `sui-store::Store`; the durable store in Postgres. Today sui's durable
//!   path is the on-disk `redb`+`rkyv` `GraphStore`.
//! - **LiveTODO(cache)** — `impl StorageBackend for RedisBackend` behind
//!   `sui-cache::StorageBackend`; the hot L1. Today the cache backends are
//!   local / redb / S3.
//! - **LiveTODO(tiered)** — the `TieredBackend` super-cache resolver
//!   (Redis L1 → Postgres L2 → object-store L3 → re-derive).
//! - **LiveTODO(sandbox)** — carve the `sui-build` sandbox onto a
//!   breathe-`MemoryBand`-sized tmpfs; only then is `never_touch_disk` a
//!   *realized* invariant rather than a *declared intent*.
//! - **LiveTODO(breathe)** — [`MemoryBand`] here is the operator-facing knob;
//!   binding it to breathe's real band catalog + the on-demand escalation
//!   ladder is the wiring.
//! - **LiveTODO(gen)** — absorb-all-languages: only [`GenDomain::Cargo`] is
//!   shipped; the other domains are the GEN-TYPED-SPEC-CONTRACT M3 arc.
//! - **LiveTODO(coopt)** — the akeyless-nix-images mutual co-optimization
//!   loop is M4; [`CooptCfg::enabled`] is `false` in the prescribed posture
//!   until the promessa + lapidar controller is wired.
//! - **LiveTODO(discovered)** — [`SuperCacheCiConfig::discovered`] is the
//!   trait default (returns `bare()`); cluster `.svc` DNS + breathe-catalog
//!   discovery is unbuilt.
//! - ~~LiveTODO(lisp)~~ **shipped** — [`SuperCacheCiConfig`] carries
//!   `#[derive(DeriveTataraDomain)]` + `#[tatara(keyword = "defsupercacheci")]`;
//!   the workspace's tatara-lisp lib/derive pin skew was already fixed at the
//!   root (`tatara-lisp-derive = "=0.2.2"`), this crate just hadn't been
//!   repointed at it. Parsing an authored `(defsupercacheci …)` form and
//!   feeding it to a runtime `ConfigStore` is still unwired — the derive
//!   makes the struct *authorable*, not yet *authored from*.
//!
//! [`sui`]: https://github.com/pleme-io/sui

use serde::{Deserialize, Serialize};
use shikumi::TieredConfig;

/// The memory-first core behavior — the pure, deterministic per-tick decision
/// core for the seven memory-first elements of `/super-cache-ci` (preemptive
/// memory via breathe shadowing, memory-tuned spot auction, Redis+PG tuning,
/// always-fresh cache, guardrail-optimal presentation, continuous efficiency,
/// and the tier-honest controller ledger). RAMDISK ⇒ memory is THE resource;
/// every function here optimizes memory + the in-memory cache. Side-effect-free
/// by construction; the Observe/Act loop that drives it is autorevivy's
/// coordinator (a named DESIGN/LiveTODO), not a forked controller.
pub mod memory;

/// The always-fresh cache + Redis/Postgres always-warm tuning surface — the
/// evict/warm/GC maintenance pass (elements 3 + 4 of the memory-first core)
/// lifted into a typed pass that **composes the real sui crates**: the shipped
/// `sui_cache::gc::collect_garbage` root-set GC over `sui_cache::StorageBackend`,
/// and the `sui_store` durable GC contract. The per-entry freshness verdict is
/// pure (in [`memory`]); this module aggregates it and drives it against a
/// mockable [`freshness::CacheMaintenance`] environment (the pleme-io default
/// delivery method). The coordinator that *schedules* the pass is autorevivy's
/// active-cache-maintenance loop — a named LiveTODO, never forked here.
pub mod freshness;

/// The **whole-set** guardrail-like optimal in-memory presentation guard
/// (element 5) + the **whole-stream** lapidar continuous self-tune (element 6).
/// Lifts [`memory`]'s per-entry [`present`](memory::present) and
/// [`evaluate_tune`](memory::evaluate_tune) to the level the build layer
/// consumes — organizing the whole derivation set into the optimal RAMDISK
/// presentation within a finite budget, validating it with a `guardrail`-shaped
/// `Rule → Decision` engine, and folding a stream of shadow-measured tune
/// proposals into a monotone-non-regressing self-tune. Composes both primitives,
/// forks neither.
pub mod presentation;

/// **Element 7 — the controller SKELETON.** The one reconcile loop that folds
/// all six built decision cores into a single per-tick [`TickPlan`](controller::TickPlan),
/// expressed against a [`Controller`](controller::Controller) contract
/// *shape-identical* to `engenho_controllers::Controller`. Tier-honest: the pure
/// Classify/Decide brain ([`decide`](controller::decide)) + the reconcile
/// contract ship + are tested; the Act beat is **shadow**
/// ([`ShadowEnvironment`](controller::ShadowEnvironment) mutates nothing) until
/// the LiveTODO actuator binds to autorevivy's coordinator + the real backends.
/// The live `impl engenho_controllers::Controller` is a thin adapter that must
/// land in engenho (`engenho → sui` is the legal edge; the reverse is a cycle) —
/// a named LiveTODO, never a forked second controller.
pub mod controller;

/// The pure `SuperCacheCiConfig -> sui_cache::BackendConfig` translation — the
/// load-bearing map that turns the typed store/cache posture into the concrete
/// tiered [`sui_cache::BackendConfig`] the `sui cache serve` daemon dispatches.
/// Closes the config-select axis of `LiveTODO(tiered)`: the schema now
/// *produces* the runnable backend, not merely *describes* it. I/O-free and
/// unit-tested against every tier arm; the live-cluster proof is the
/// ground-truth recipe.
pub mod backend;

/// The **perpetual cache-warming** decision core — the pure brain that keeps the
/// sui super-cache HOT so a build substitutes pre-built closures (warm) instead
/// of cold-compiling. Answers WHEN to re-warm (cold-start / tracked-input change
/// / cadence), WHICH closures (one [`preheat::WarmTarget`] per dep/service), and
/// HOW to spin the 100%-spot scale-to-zero builder floor **only while warming**
/// ([`preheat::plan_floor`]). Carries the Viggy `(defpromessa)` "cache stays
/// warm" as a typed [`preheat::WarmthPromessa`]. Shadow-first
/// ([`PreheatCfg::dry_run`]); the tick loop that drives it is autorevivy's CLEAN
/// coordinator (a named LiveTODO) with the `camelot-cache-warm` workflow as the
/// running interim — this module ships the brain both derive from, never a
/// second controller.
pub mod preheat;

/// canteiro M0 — a CI run decomposed into one typed content-addressed DAG
/// (`theory/CANTEIRO.md` §5). The decompose morphism + `CiNode` as a shigoto
/// `Job`; in-process wave execution + the in-pod runner entrypoint are M1.
pub mod canteiro;

/// canteiro plane (a) — the multi-worker dispatch crux (`theory/CANTEIRO.md`
/// §5). `emit_gha` projects a decomposed `CiRun` onto a GitHub Actions job
/// graph (one job per node, `needs:` = the DAG edges) that GitHub schedules
/// across separate ARC workers.
pub mod canteiro_gha;

/// canteiro plane (b) — CANTEIRO itself as the cross-worker RUNTIME scheduler
/// (`theory/CANTEIRO.md` §7.1-A, the named crux). `run_distributed` owns the DAG
/// ordering and dispatches each node as a serializable `WorkItem` to a
/// `WorkQueue` that N independent workers claim/run/report — ordering is
/// canteiro's, not GitHub's. M0 proves the protocol in-process; the Postgres
/// queue + a live 2-pod ARC run are the named destination (transport is a trait
/// swap).
pub mod canteiro_dist;

/// canteiro `(defci)` authoring surface (`theory/CANTEIRO.md` §4, leg 2) — the
/// TYPED-SPEC + INTERPRETER TRIPLET for declaring a `CiRun` as typed Lisp data:
/// the `CiSpec` `#[tatara(keyword = "defci")]` border + the `to_ci_run`
/// interpreter. The authoring half of "pleme-io/actions built out of reusable
/// Rust and caixa"; the first-class caixa `:kind Acao` promotion (needs a
/// shareable `CiRun` across repos) is the NAMED DESTINATION, not done here.
pub mod canteiro_spec;

pub use preheat::PreheatCfg;

/// Which durable-store backend realizes the Nix store.
///
/// The prescribed destination is [`Postgres`](StoreBackendKind::Postgres)
/// (never-touch-durable-disk). [`GraphStore`](StoreBackendKind::GraphStore)
/// is the shipped on-disk `redb`+`rkyv` reality and the honest
/// [`bare`](SuperCacheCiConfig::bare) floor.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackendKind {
    /// Durable store in Postgres — content-addressed rows (BLAKE3 key →
    /// metadata + L3 pointer). The never-touch-durable-disk destination.
    Postgres,
    /// The shipped on-disk `redb`+`rkyv` graph store (ZFS-tuned).
    GraphStore,
    /// The local filesystem store (cppnix-compatible layout).
    Local,
}

/// Which hot-cache backend fronts the store.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheBackendKind {
    /// Redis hot L1 — RDB/AOF for warm restart, LRU eviction at the
    /// [`MemoryBand`] ceiling. A cache, not the source of truth: loss
    /// re-warms from the durable store below.
    Redis,
    /// The shipped on-disk `redb` cache backend.
    Redb,
    /// An S3-compatible binary cache backend.
    S3,
}

/// The L3 durable object-store tier (large NAR blobs live here, not as
/// multi-GB Postgres rows) — content-addressed.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStoreKind {
    /// Attic binary cache.
    Attic,
    /// An S3-compatible endpoint.
    S3,
    /// The pleme-io armázem any-storage→S3 gateway.
    Armazem,
    /// No L3 tier (the honest floor — everything durable lives on disk).
    None,
}

/// One language's typed build encoder domain (the GEN-TYPED-SPEC-CONTRACT
/// triplet: typed encoder → `*.build-spec.json` border → substrate sui
/// interpreter). Only [`Cargo`](GenDomain::Cargo) is shipped.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenDomain {
    /// gen-cargo → substrate `lockfile-builder` (shipped).
    Cargo,
    /// gen-npm (LiveTODO — M3).
    Npm,
    /// gen-pip (LiveTODO — M3).
    Pip,
    /// gen-poetry (LiveTODO — M3).
    Poetry,
    /// gen-gomod (LiveTODO — M3).
    GoMod,
    /// gen-bundler (LiveTODO — M3).
    Bundler,
    /// gen-swift (LiveTODO — M3).
    Swift,
}

/// A runner architecture the camelot in-cluster GHA fleet builds on.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    /// x86_64 (built on the native `rio-builder` runner).
    Amd64,
    /// aarch64 (built on a native arm64 runner).
    Arm64,
}

/// A host + port endpoint. Empty (`host == ""`, `port == 0`) is the honest
/// "unset" floor — [`bare`](SuperCacheCiConfig::bare) uses it because no
/// Postgres/Redis service is selected there.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// DNS name or IP (e.g. a cluster `.svc` name).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl Endpoint {
    /// The honest "unset" endpoint — the `bare()` floor.
    #[must_use]
    pub fn unset() -> Self {
        Self {
            host: String::new(),
            port: 0,
        }
    }

    /// A concrete endpoint.
    #[must_use]
    pub fn at(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
        }
    }
}

/// The operator-facing memory band knob — the RAM the daemon (eval, hot cache)
/// or the tmpfs build sandbox breathes under.
///
/// **LiveTODO(breathe):** this is the *config-facing mirror* of breathe's band
/// vocabulary; binding it to breathe's real `MemoryBand` / band catalog +
/// controller is the wiring. `dry_run = true` is the breathe *shadow-first*
/// safety gate — observe `ShadowWouldApply` before flipping a band LIVE.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBand {
    /// The utilization setpoint the band holds (percent of the ceiling).
    pub target_pct: u8,
    /// Slack above the setpoint before the hard (OOM-cliff) limit.
    pub headroom_pct: u8,
    /// The absolute ceiling the band never carves past, in MiB.
    pub max_mib: u32,
    /// Shadow-first gate: `true` observes `ShadowWouldApply` without mutating.
    pub dry_run: bool,
}

impl MemoryBand {
    /// The zeroed floor (no band opinion).
    #[must_use]
    pub fn unset() -> Self {
        Self {
            target_pct: 0,
            headroom_pct: 0,
            max_mib: 0,
            dry_run: false,
        }
    }
}

/// The L3 durable object-store tier config.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ObjectStoreCfg {
    /// Which object store realizes L3.
    pub kind: ObjectStoreKind,
    /// The object-store endpoint (`None` when [`ObjectStoreKind::None`]).
    pub endpoint: Option<Endpoint>,
    /// The bucket / cache name.
    pub bucket: String,
}

/// The durable store tier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StoreCfg {
    /// Which durable-store backend realizes the Nix store.
    pub backend: StoreBackendKind,
    /// The Postgres endpoint (used when `backend == Postgres`).
    pub pg: Endpoint,
    /// The Postgres connection-pool size.
    pub pg_pool: u32,
    /// The L3 durable object-store tier for large NAR blobs.
    pub l3: ObjectStoreCfg,
}

/// The hot cache tier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CacheCfg {
    /// Which hot-cache backend fronts the store.
    pub backend: CacheBackendKind,
    /// The Redis endpoint (used when `backend == Redis`).
    pub redis: Endpoint,
    /// Persist the hot set (RDB/AOF) for warm restart.
    pub rdb_persist: bool,
    /// The band that sizes the hot cache + drives LRU eviction.
    pub memory_band: MemoryBand,
}

/// The RAM/tmpfs build sandbox tier.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SandboxCfg {
    /// The band that sizes the tmpfs the build sandbox runs on.
    pub tmpfs_band: MemoryBand,
    /// The invariant: when `true`, no build stage touches *durable* disk —
    /// compile, plan, checkpoint, bundle and state all live in RAM + the DB.
    /// The one sanctioned filesystem reach (provider-plugin `exec`, the tmpfs
    /// sandbox) is not durable. `false` is the honest floor (disk allowed).
    pub never_touch_disk: bool,
}

/// The per-language typed build domains fed to sui.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GenCfg {
    /// Enabled language domains. Only [`GenDomain::Cargo`] is shipped.
    pub domains: Vec<GenDomain>,
    /// Fail CI on a stale committed `*.build-spec.json` (GEN-TYPED-SPEC I2).
    pub ci_stale_check: bool,
}

/// One camelot runner architecture's scale-set posture.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchAsg {
    /// The runner architecture.
    pub arch: Arch,
    /// Minimum replicas (`0` = scale-to-zero when idle).
    pub min: u32,
    /// Maximum replicas.
    pub max: u32,
    /// Percent of capacity on spot (camelot posture is `100`).
    pub spot_pct: u8,
    /// The spot instance families the auction diversifies across (the flat
    /// family-name list; the *memory-axis* posture over them lives in
    /// [`memory_auction`](ArchAsg::memory_auction)).
    pub spot_families: Vec<String>,
    /// The **memory-tuned auction config** — the typed 100%-spot posture on the
    /// memory axis (floor, reclaim ceiling, diversification cap) that
    /// [`memory::plan_memory_auction`] runs over the live spot candidates to
    /// produce the memory-ranked input breathe's `BandLeiloeiro` consumes.
    /// super-cache-ci is memory-bound (RAMDISK ⇒ MiB is the buy), so the
    /// auction bids on MiB-per-dollar, never vCPU.
    pub memory_auction: memory::MemoryAuctionCfg,
    /// Wake capacity on demand via cordel (JIT builder wake).
    pub cordel_wake: bool,
}

/// The camelot in-cluster GHA runner fleet posture.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BuildersCfg {
    /// One entry per built architecture (amd64 + arm64).
    pub arches: Vec<ArchAsg>,
}

/// The breathe on-demand escalation ladder — how far the fleet may climb off
/// spot when reclaim pressure spikes.
///
/// **LiveTODO(breathe):** the real ladder lives in breathe; this is the knob.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OnDemandLadder {
    /// Whether on-demand escalation is permitted at all.
    pub enabled: bool,
    /// The maximum on-demand replicas the ladder may climb to.
    pub max_ondemand: u32,
}

/// The lifecycle-breath posture for the daemon + runner fleet.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BreatheCfg {
    /// The breathe band this workload is registered under.
    pub band_ref: String,
    /// Seconds idle before scaling the fleet to zero.
    pub idle_scale_zero_secs: u32,
    /// The on-demand escalation ladder.
    pub escalation: OnDemandLadder,
}

/// The akeyless-nix-images mutual co-optimization toggles.
///
/// **LiveTODO(coopt):** M4 — [`enabled`](CooptCfg::enabled) is `false` in the
/// prescribed posture until the promessa + lapidar controller is wired.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CooptCfg {
    /// Master toggle for the co-optimization loop.
    pub enabled: bool,
    /// The images super-cache-ci builds + co-optimizes with.
    /// **LiveTODO:** populate from the akeyless-nix-images image manifest
    /// (kept empty here rather than fabricate the microservice image names).
    pub images: Vec<String>,
    /// Git refs whose change re-triggers a co-optimized build.
    pub rebuild_on: Vec<String>,
    /// Let lapidar auto-tune the bands from build telemetry each tick.
    pub lapidar: bool,
    /// The Viggy `(defpromessa)` that holds hit-rate / p50 / on-demand
    /// (`None` until wired).
    pub promessa_ref: Option<String>,
}

/// The typed `/super-cache-ci` config.
///
/// Resolved via [`shikumi::TieredConfig`] (default ← discovered ← override,
/// hot-reload). See the crate docs for the tier contract and the LiveTODO
/// ledger.
///
/// Authored as `(defsupercacheci :store (…) :cache (…) :sandbox (…) …)` (the
/// Rust + Lisp primary pattern) via [`tatara_lisp::DeriveTataraDomain`].
#[derive(tatara_lisp::DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[tatara(keyword = "defsupercacheci")]
pub struct SuperCacheCiConfig {
    /// The durable store tier (Postgres at the destination).
    pub store: StoreCfg,
    /// The hot cache tier (Redis at the destination).
    pub cache: CacheCfg,
    /// The RAM/tmpfs build sandbox tier.
    pub sandbox: SandboxCfg,
    /// The per-language typed build domains.
    ///
    /// Named `gen_domains` (not `gen`) because `gen` is a reserved keyword in
    /// Rust edition 2024.
    pub gen_domains: GenCfg,
    /// The camelot in-cluster runner fleet posture.
    pub builders: BuildersCfg,
    /// The lifecycle-breath posture.
    pub breathe: BreatheCfg,
    /// The akeyless co-optimization toggles.
    pub coopt: CooptCfg,
    /// The **perpetual cache-warming** posture — keeps the super-cache HOT so a
    /// build substitutes warm instead of cold-compiling (re-warm on
    /// tracked-input change or cadence, spinning the builder floor only while
    /// warming). Shadow-first. See [`preheat`].
    pub preheat: PreheatCfg,
}

impl TieredConfig for SuperCacheCiConfig {
    /// Tier 0 — the documented floor: **sui exactly as it ships today**.
    /// On-disk graph store, on-disk redb cache, disk allowed. No
    /// Postgres/Redis/tmpfs selected, no runners, no co-opt. This tier is the
    /// tier-honest ground truth: it does not claim the never-touch-disk
    /// destination.
    fn bare() -> Self {
        Self {
            store: StoreCfg {
                backend: StoreBackendKind::GraphStore,
                pg: Endpoint::unset(),
                pg_pool: 0,
                l3: ObjectStoreCfg {
                    kind: ObjectStoreKind::None,
                    endpoint: None,
                    bucket: String::new(),
                },
            },
            cache: CacheCfg {
                backend: CacheBackendKind::Redb,
                redis: Endpoint::unset(),
                rdb_persist: false,
                memory_band: MemoryBand::unset(),
            },
            sandbox: SandboxCfg {
                tmpfs_band: MemoryBand::unset(),
                // Honest floor: disk is the current reality.
                never_touch_disk: false,
            },
            gen_domains: GenCfg {
                domains: Vec::new(),
                ci_stale_check: false,
            },
            builders: BuildersCfg { arches: Vec::new() },
            breathe: BreatheCfg {
                band_ref: String::new(),
                idle_scale_zero_secs: 0,
                escalation: OnDemandLadder {
                    enabled: false,
                    max_ondemand: 0,
                },
            },
            coopt: CooptCfg {
                enabled: false,
                images: Vec::new(),
                rebuild_on: Vec::new(),
                lapidar: false,
                promessa_ref: None,
            },
            // Honest floor: no perpetual warming.
            preheat: preheat::PreheatCfg::off(),
        }
    }

    /// Tier 2 — the prescribed destination posture: **service-level sui,
    /// never-touch-durable-disk by construction**. Postgres store + Redis
    /// cache + tmpfs sandbox (`never_touch_disk = true`); 100%-spot,
    /// scale-to-zero, multi-arch camelot runners; the shipped Cargo gen
    /// domain; the co-opt loop pre-shaped but OFF (M4).
    ///
    /// The bands start `dry_run = true` — breathe's shadow-first safety gate
    /// (observe `ShadowWouldApply` before flipping a band LIVE).
    fn prescribed_default() -> Self {
        Self {
            store: StoreCfg {
                backend: StoreBackendKind::Postgres,
                pg: Endpoint::at("postgres.super-cache-ci.svc.cluster.local", 5432),
                pg_pool: 16,
                l3: ObjectStoreCfg {
                    kind: ObjectStoreKind::Attic,
                    endpoint: Some(Endpoint::at("attic.super-cache-ci.svc.cluster.local", 8080)),
                    bucket: "sui-super-cache".to_string(),
                },
            },
            cache: CacheCfg {
                backend: CacheBackendKind::Redis,
                redis: Endpoint::at("redis.super-cache-ci.svc.cluster.local", 6379),
                rdb_persist: true,
                memory_band: MemoryBand {
                    target_pct: 80,
                    headroom_pct: 20,
                    max_mib: 8192,
                    dry_run: true,
                },
            },
            sandbox: SandboxCfg {
                tmpfs_band: MemoryBand {
                    target_pct: 90,
                    headroom_pct: 10,
                    max_mib: 16384,
                    dry_run: true,
                },
                // The destination invariant — realized once the tmpfs sandbox
                // carve ships (LiveTODO(sandbox)).
                never_touch_disk: true,
            },
            gen_domains: GenCfg {
                // Only the shipped domain. Absorb-all-languages is M3.
                domains: vec![GenDomain::Cargo],
                ci_stale_check: true,
            },
            builders: BuildersCfg {
                arches: vec![
                    ArchAsg {
                        arch: Arch::Amd64,
                        min: 0,
                        max: 10,
                        spot_pct: 100,
                        spot_families: vec![
                            "c7i".to_string(),
                            "m7i".to_string(),
                            "r6i".to_string(),
                        ],
                        // Memory-tuned: reject <8 GiB / >20% reclaim, diversify top 3.
                        memory_auction: memory::MemoryAuctionCfg::camelot(),
                        cordel_wake: true,
                    },
                    ArchAsg {
                        arch: Arch::Arm64,
                        min: 0,
                        max: 5,
                        spot_pct: 100,
                        spot_families: vec!["c7g".to_string(), "m7g".to_string()],
                        memory_auction: memory::MemoryAuctionCfg::camelot(),
                        cordel_wake: true,
                    },
                ],
            },
            breathe: BreatheCfg {
                band_ref: "super-cache-ci".to_string(),
                idle_scale_zero_secs: 300,
                escalation: OnDemandLadder {
                    enabled: true,
                    max_ondemand: 4,
                },
            },
            coopt: CooptCfg {
                // OFF until the M4 promessa + lapidar controller is wired.
                enabled: false,
                images: Vec::new(),
                rebuild_on: vec!["akeyless-nix-images".to_string()],
                lapidar: false,
                promessa_ref: None,
            },
            // Perpetual cache-warming: ENABLED (the cache must stay warm) but
            // SHADOW-FIRST (dry_run = true) — the plan is computed + observed;
            // the builder floor is not spun and nothing is warmed until the
            // operator flips the band LIVE. 6 h cadence, 99% warm-fraction
            // objective, scale-to-zero at rest. Targets are supplied by the
            // chart values / discovered tier — an empty set yields an honest
            // empty plan, never a claimed-warm cache.
            preheat: preheat::PreheatCfg::camelot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shikumi::{ConfigTier, TieredConfig};

    #[test]
    fn bare_is_the_honest_on_disk_floor() {
        let b = SuperCacheCiConfig::bare();
        // The floor documents sui AS IT SHIPS: on-disk graph store, disk allowed.
        assert_eq!(b.store.backend, StoreBackendKind::GraphStore);
        assert_eq!(b.cache.backend, CacheBackendKind::Redb);
        assert!(
            !b.sandbox.never_touch_disk,
            "bare() must NOT claim the never-touch-disk destination"
        );
    }

    #[test]
    fn prescribed_default_is_the_never_touch_disk_destination() {
        let p = SuperCacheCiConfig::prescribed_default();
        assert_eq!(p.store.backend, StoreBackendKind::Postgres);
        assert_eq!(p.cache.backend, CacheBackendKind::Redis);
        assert!(
            p.sandbox.never_touch_disk,
            "prescribed_default() IS the never-touch-durable-disk posture"
        );
        // breathe shadow-first: the prescribed bands start in dry-run.
        assert!(p.cache.memory_band.dry_run);
        assert!(p.sandbox.tmpfs_band.dry_run);
    }

    #[test]
    fn prescribed_default_differs_from_bare() {
        assert_ne!(
            SuperCacheCiConfig::bare(),
            SuperCacheCiConfig::prescribed_default()
        );
    }

    #[test]
    fn default_tier_resolves_through_shikumi_machinery() {
        // Exercises the real TieredConfig::resolve_tier default impl.
        let via_tier = SuperCacheCiConfig::resolve_tier(ConfigTier::Default);
        assert_eq!(via_tier, SuperCacheCiConfig::prescribed_default());
    }

    #[test]
    fn serde_round_trips_through_the_shikumi_diff() {
        // diff_against serializes BOTH sides to YAML inside shikumi; an empty
        // self-diff proves Serialize + DeserializeOwned + the tier bounds hold
        // end to end (the type is a valid Custom-tier overlay target).
        let p = SuperCacheCiConfig::prescribed_default();
        assert!(p.diff_against(&p).is_empty_diff());
    }

    #[test]
    fn only_the_shipped_gen_domain_is_prescribed() {
        // Absorb-all-languages is M3. prescribed_default enables ONLY the
        // shipped domain (Cargo); this guard FAILS if a language is added to
        // the prescribed posture without its interpreter being wired.
        let p = SuperCacheCiConfig::prescribed_default();
        assert_eq!(p.gen_domains.domains, vec![GenDomain::Cargo]);
        assert!(p.gen_domains.ci_stale_check);
    }

    #[test]
    fn coopt_is_off_until_wired() {
        // The akeyless co-optimization loop is M4 (unwired). prescribed_default
        // must NOT claim it is running.
        let p = SuperCacheCiConfig::prescribed_default();
        assert!(!p.coopt.enabled);
        assert!(!p.coopt.lapidar);
    }

    #[test]
    fn preheat_is_off_in_the_bare_floor() {
        let b = SuperCacheCiConfig::bare();
        assert!(!b.preheat.enabled, "bare() must not claim perpetual warming");
        assert!(b.preheat.targets.is_empty());
    }

    #[test]
    fn preheat_is_enabled_but_shadow_first_in_the_destination() {
        let p = SuperCacheCiConfig::prescribed_default();
        assert!(p.preheat.enabled, "the destination keeps the cache warm");
        assert!(
            p.preheat.dry_run,
            "perpetual warming is SHADOW-first (observe before spinning the fleet)"
        );
        // scale-to-zero at rest — cost at rest is zero.
        assert_eq!(p.preheat.floor_spin.idle_floor, 0);
        // 6 h cadence matches the camelot-cache-warm workflow.
        assert_eq!(p.preheat.cadence_secs, 21_600);
    }

    #[test]
    fn camelot_posture_is_hundred_percent_spot_scale_to_zero() {
        let p = SuperCacheCiConfig::prescribed_default();
        assert!(!p.builders.arches.is_empty());
        for a in &p.builders.arches {
            assert_eq!(a.spot_pct, 100, "camelot posture is 100% spot");
            assert_eq!(a.min, 0, "scale-to-zero: min replicas 0");
        }
    }

    #[test]
    fn camelot_auction_is_memory_tuned_on_every_arch() {
        // The 100%-spot posture is refined ON THE MEMORY AXIS per arch — a
        // memory floor, a reclaim ceiling, and a never-single-family cap. This
        // guard FAILS if an arch ships the memory-blind `unset()` auction.
        let p = SuperCacheCiConfig::prescribed_default();
        for a in &p.builders.arches {
            assert_eq!(a.memory_auction, crate::memory::MemoryAuctionCfg::camelot());
            assert!(
                a.memory_auction.min_mib > 0,
                "memory floor: never bid a starved instance (memory is the resource)"
            );
            assert!(
                a.memory_auction.max_interrupt_rate < 100,
                "reclaim ceiling: reject churny families (a reclaim wastes the in-RAM build)"
            );
            assert!(
                a.memory_auction.diversify_top_n >= 2,
                "100%-spot never rides a single family"
            );
        }
    }
}
