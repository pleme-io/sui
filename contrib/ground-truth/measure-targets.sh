#!/usr/bin/env bash
# sui ground-truth REAL-BUILD-TARGET measurement matrix — {cold,hot}x{apart,together}
# per real build target, run against the ONE full-stack tiered instance stood up
# by run.sh (Postgres L2 + Redis L1 + tmpfs RAMDISK, `sui cache serve`).
#
# WHAT THIS MEASURES (the matrix the store/cache daemon actually optimizes):
#   The sui daemon is a Nix BINARY CACHE (sui cache serve), not a build worker.
#   So a "real build target through the optimized stack" is:
#     BUILD on the host (real compiler) → nix copy --to the daemon (write-through
#     TieredBackend: L2 Postgres durable + L1 Redis warm) → delete locally →
#     nix copy --from the daemon (read-through: L1 hit, or L2 hit + L1 promotion).
#
#   cold-apart    = real from-source COMPILE of each target, alone   → build baseline
#   hot-apart     = target output already in sui; delete local; pull FROM sui alone
#                   → cache-hit fetch latency (the optimized win)
#   cold-together = all targets built in one pass, sui empty; push all → shared-
#                   closure dedup on write-through
#   hot-together  = all targets in sui; one pull-all pass            → steady state
#   + L1/L2 tier hit distribution after the run
#
# TARGETS: real Rust + real Go compiles with SMALL outputs (output NAR <= 2 MiB so
# the whole target round-trips the current serve path — axum DefaultBodyLimit is
# 2 MiB, larger NARs are rejected; see the numbers doc). These are SUBSTITUTES for
# the larger service images we actually target, which are toolchain-gated (one
# of them needs go1.26; host has go1.25) — stated plainly, never faked.
#
# TIER-HONEST: measures the SOFTWARE (tiers, resolver, write/read-through, real
# closure round-trip). Deps (rustc/go/stdenv) come from cache.nixos.org normally
# — they are not what the sui daemon optimizes here; the target's OWN compile +
# round-trip is. Does NOT exercise a live breathe controller (static band, one node).
#
# HOST GLUE ONLY (Op-Principle: shell as thin orchestration). The typed work is the
# Rust daemon + the TieredBackend resolver. Prereqs: run.sh up; host nix; the two
# scratch derivations (gt-rust.nix + gt-go.nix) present at $TARGETS_DIR.
set -uo pipefail

HOST_PORT=${HOST_PORT:-5555}
POD=${POD:-sui-ground-truth}
BASE="http://localhost:${HOST_PORT}?compression=none"
TARGETS_DIR=${TARGETS_DIR:?set TARGETS_DIR to the dir holding gt-rust.nix + gt-go.nix}

log()  { printf '\033[36m[targets]\033[0m %s\n' "$*"; }
redis() { podman exec "$POD-redis" redis-cli "$@" 2>/dev/null; }
pg()    { podman exec "$POD-pg" psql -U sui -d sui -tAc "$1" 2>/dev/null | tr -d ' '; }

# real wall-clock of a command, seconds 6dp, to stdout (stderr passes through)
timed() { local s e; s=$(python3 -c 'import time;print(time.time())'); "$@" >/dev/null 2>&1; e=$(python3 -c 'import time;print(time.time())'); python3 -c "print(f'{$e-$s:.4f}')"; }

drv_out() { nix eval --impure --raw --expr "(import $1).outPath" 2>/dev/null; }

RUST=$TARGETS_DIR/gt-rust.nix
GO=$TARGETS_DIR/gt-go.nix
cd "$TARGETS_DIR"

del_local() { for o in "$@"; do nix store delete "$o" >/dev/null 2>&1 || true; done; }
# FORCE from-source compile, rooted via an out-link so the path stays valid for push.
build_rust() { nix build --impure --rebuild --expr "(import $RUST)" -o "$TARGETS_DIR/r-rust"; }
build_go()   { nix build --impure --rebuild --expr "(import $GO)"   -o "$TARGETS_DIR/r-go"; }
push()       { nix copy --to "$BASE" "$@"; }
pull()       { nix copy --from "$BASE" --no-check-sigs "$@"; }
unroot()     { rm -f "$TARGETS_DIR/r-rust" "$TARGETS_DIR/r-go"; }

RUST_OUT=$(drv_out "$RUST"); GO_OUT=$(drv_out "$GO")
log "targets: RUST=$RUST_OUT  GO=$GO_OUT"

echo; log "════════ COLD-APART (real from-source compile, each alone) ════════"
del_local "$RUST_OUT" "$GO_OUT"
C_RUST=$(timed build_rust); log "cold-apart rust compile = ${C_RUST}s"
C_GO=$(timed build_go);     log "cold-apart go   compile = ${C_GO}s"

log "push both target closures to sui (write-through)"
push "$RUST_OUT" 2>&1 | tail -1 || true
push "$GO_OUT"   2>&1 | tail -1 || true

echo; log "════════ HOT-APART (delete local, pull each FROM sui alone) ════════"
unroot
del_local "$RUST_OUT"; H_RUST=$(timed pull "$RUST_OUT"); log "hot-apart rust pull = ${H_RUST}s  (present=$(test -e "$RUST_OUT" && echo Y || echo N))"
del_local "$GO_OUT";   H_GO=$(timed pull "$GO_OUT");     log "hot-apart go   pull = ${H_GO}s  (present=$(test -e "$GO_OUT" && echo Y || echo N))"

echo; log "════════ COLD-TOGETHER (rebuild both one pass; push all together) ════════"
del_local "$RUST_OUT" "$GO_OUT"
CT=$(timed sh -c "nix build --impure --rebuild --expr '(import $RUST)' -o '$TARGETS_DIR/r-rust'; nix build --impure --rebuild --expr '(import $GO)' -o '$TARGETS_DIR/r-go'")
log "cold-together build (both) = ${CT}s"
PT=$(timed push "$RUST_OUT" "$GO_OUT"); log "cold-together push-all (dedup) = ${PT}s"

echo; log "════════ HOT-TOGETHER (delete both, one pull-all pass) ════════"
del_local "$RUST_OUT" "$GO_OUT"
HT=$(timed pull "$RUST_OUT" "$GO_OUT"); log "hot-together pull-all = ${HT}s  (rust=$(test -e "$RUST_OUT" && echo Y||echo N) go=$(test -e "$GO_OUT" && echo Y||echo N))"

echo; log "════════ TIER DISTRIBUTION (post-run) ════════"
log "L2 Postgres narinfo rows = $(pg 'SELECT count(*) FROM sui_cache_narinfo;')   nar rows = $(pg 'SELECT count(*) FROM sui_cache_nar;')"
log "L1 Redis narinfo keys    = $(redis KEYS 'sui:narinfo:*' | wc -l | tr -d ' ')   nar keys = $(redis KEYS 'sui:nar:*' | wc -l | tr -d ' ')"

echo; log "════════ SUMMARY (seconds) ════════"
printf '  %-16s rust=%-10s go=%-10s\n' "cold-apart" "$C_RUST" "$C_GO"
printf '  %-16s rust=%-10s go=%-10s\n' "hot-apart"  "$H_RUST" "$H_GO"
printf '  %-16s both=%-10s push=%-10s\n' "cold-together" "$CT" "$PT"
printf '  %-16s pull-all=%-10s\n' "hot-together" "$HT"
