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
//! - **LiveTODO(lisp)** — the `(defsupercacheci …)` authoring surface (the
//!   Rust + Lisp primary pattern, `#[derive(DeriveTataraDomain)]`) is designed
//!   but not attached: the sui workspace currently locks a mismatched
//!   `tatara-lisp 0.2.4` / `tatara-lisp-derive 0.2.107` pair that does not
//!   compile (a pre-existing skew, not this crate's). The exact re-attach shape
//!   is preserved as a commented block on [`SuperCacheCiConfig`]; keeping the
//!   derive off here keeps the schema + tests green in isolation rather than
//!   rounding the Lisp surface up to "working".
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
    /// The spot instance families the auction diversifies across.
    pub spot_families: Vec<String>,
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
/// **LiveTODO(lisp):** the config is designed to be authored as
/// `(defsupercacheci :store (…) :cache (…) :sandbox (…) …)` (the Rust + Lisp
/// primary pattern). Re-attaching the derive is exactly this, once the
/// workspace's tatara-lisp lib/derive pin skew is resolved:
///
/// ```ignore
/// use tatara_lisp::DeriveTataraDomain;
///
/// #[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
/// #[tatara(keyword = "defsupercacheci")]
/// pub struct SuperCacheCiConfig { /* … unchanged fields … */ }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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
                        cordel_wake: true,
                    },
                    ArchAsg {
                        arch: Arch::Arm64,
                        min: 0,
                        max: 5,
                        spot_pct: 100,
                        spot_families: vec!["c7g".to_string(), "m7g".to_string()],
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
    fn camelot_posture_is_hundred_percent_spot_scale_to_zero() {
        let p = SuperCacheCiConfig::prescribed_default();
        assert!(!p.builders.arches.is_empty());
        for a in &p.builders.arches {
            assert_eq!(a.spot_pct, 100, "camelot posture is 100% spot");
            assert_eq!(a.min, 0, "scale-to-zero: min replicas 0");
        }
    }
}
