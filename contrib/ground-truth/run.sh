#!/usr/bin/env bash
# sui ground-truth run-recipe — ONE full-stack instance on a Linux host.
#
# Brings up the never-touch-durable-disk super-cache posture on a single node:
#   - Postgres L2 durable store        (postgres:17-alpine)
#   - Redis   L1 hot cache             (redis:7-alpine, LRU at the static band cap)
#   - tmpfs   RAMDISK build sandbox    (--tmpfs /build, never touches durable disk)
#   - the sui daemon                   (sui cache serve --features tiered)
#
# WHY A LINUX HOST: sui's build sandbox is Linux-only (unshare(1); on macOS you
# get NoSandbox — no isolation, no tmpfs). The recommended host is the podman
# machine VM already running on the aarch64-darwin workstation (self-contained:
# no VPN, no rio, no cloud). The three containers share ONE podman POD so they
# reach each other over localhost — matching the 127.0.0.1 endpoints in
# tiered-backend.toml.
#
# This is HOST GLUE (Op-Principle: shell only as thin orchestration, not logic).
# All the TYPED work is in the committed Rust:
#   - the config-select serve path  (src/main.rs `resolve_serve_backend`)
#   - the tiered BackendConfig       (contrib/ground-truth/tiered-backend.toml)
#   - the shikumi posture + xlate    (supercache-config.toml + to_backend_config)
#
# TIER-HONEST: the MemoryBand here is a STATIC ceiling (--memory / --maxmemory /
# --tmpfs …:size=), i.e. the band setpoint applied ONCE by hand — there is no
# breathe controller on a single local node. The live-reconciled band is the
# fleet path (breathe-memoryband.yaml, mode: shadow). This run measures the
# SOFTWARE (tiers, resolver, RAMDISK sandbox, PgStore-vs-real-Postgres); the
# fleet measures the breathability.
#
# Usage:
#   contrib/ground-truth/run.sh up        # start pg + redis + serve the daemon
#   contrib/ground-truth/run.sh down       # stop + remove the pod
#   contrib/ground-truth/run.sh status     # show pod + container state
#   contrib/ground-truth/run.sh smoke      # curl the nix-cache-info endpoint
#
# Prereqs: `podman machine` running (Linux VM), ~2.5 GiB free RAM for the stack.
set -euo pipefail

# ── config knobs (match tiered-backend.toml + the MemoryBand ceiling) ──────────
POD=sui-ground-truth
PG_IMAGE=postgres:17-alpine
REDIS_IMAGE=redis:7-alpine
SUI_IMAGE=${SUI_IMAGE:-sui-ground-truth:local}   # built by `build_sui_image` below
LISTEN_PORT=5000                # the daemon's IN-CONTAINER listen port (Nix cache proto)
# HOST publish port. Defaults to 5555 because macOS reserves :5000 for the
# AirPlay Receiver (ControlCenter binds *:commplex-main) — a pod publishing
# :5000 fails infra-container start with `bind: address already in use`.
# Override with HOST_PORT if 5555 is taken. VERIFIED on the aarch64 podman VM.
HOST_PORT=${HOST_PORT:-5555}
PG_PASSWORD=sui                 # tiered-backend.toml: postgres://sui:sui@…/sui
REDIS_MAXMEM=2gb                # == supercache-config.toml [cache.memory_band].max_mib
TMPFS_SIZE=3g                   # == [sandbox.tmpfs_band].max_mib (3072 MiB)
DAEMON_MEM=2g                   # static MemoryBand ceiling for the daemon container
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# The workstation's ~/.docker/config.json has a malformed `auths` array that
# makes bare `podman run` error on JSON unmarshal. Bypass with an empty config
# dir (cosmetic; no operator needed). See sui-ground-truth-plan.md §2.
export DOCKER_CONFIG="${DOCKER_CONFIG:-/tmp/sui-gt-emptydockercfg}"
mkdir -p "$DOCKER_CONFIG"

log() { printf '\033[36m[ground-truth]\033[0m %s\n' "$*"; }

build_sui_image() {
  # Build the sui daemon FOR linux/arm64, tiered features on. Two paths:
  #   1. If a linux nix build is available, `nix build .#dockerImage-arm64` and
  #      `podman load` it (preferred — matches the fleet AUTOBUMP image).
  #   2. Otherwise build in-VM with cargo (the ground-truth shortcut): a tiny
  #      Containerfile that `cargo build --release --bin sui --features tiered`.
  if podman image exists "$SUI_IMAGE"; then
    log "sui image $SUI_IMAGE already present — skipping build"
    return
  fi
  log "building sui daemon image ($SUI_IMAGE) with --features tiered for linux/arm64"
  # In-VM cargo build. The Containerfile lives next to this recipe.
  podman build \
    --tag "$SUI_IMAGE" \
    --file "$HERE/Containerfile" \
    "$REPO_ROOT"
}

up() {
  build_sui_image
  log "creating pod $POD (shared localhost, publish host :$HOST_PORT → container :$LISTEN_PORT)"
  podman pod exists "$POD" || \
    podman pod create --name "$POD" --publish "$HOST_PORT:$LISTEN_PORT"

  log "starting Postgres L2 ($PG_IMAGE) — role/db 'sui'"
  podman container exists "$POD-pg" || podman run -d \
    --pod "$POD" --name "$POD-pg" \
    -e POSTGRES_USER=sui \
    -e POSTGRES_PASSWORD="$PG_PASSWORD" \
    -e POSTGRES_DB=sui \
    "$PG_IMAGE"

  log "starting Redis L1 ($REDIS_IMAGE) — maxmemory $REDIS_MAXMEM, allkeys-lru (static band cap)"
  podman container exists "$POD-redis" || podman run -d \
    --pod "$POD" --name "$POD-redis" \
    "$REDIS_IMAGE" \
    redis-server --maxmemory "$REDIS_MAXMEM" --maxmemory-policy allkeys-lru --save "" --appendonly no

  log "waiting for Postgres to accept connections…"
  for _ in $(seq 1 30); do
    if podman exec "$POD-pg" pg_isready -U sui -d sui >/dev/null 2>&1; then break; fi
    sleep 1
  done

  log "starting the sui daemon — cache serve --features tiered, tmpfs RAMDISK /build:$TMPFS_SIZE"
  # --tmpfs /build = the never-touch-durable-disk build sandbox (RAMDISK).
  # --memory     = the static MemoryBand ceiling for the daemon.
  # --backend-config = the committed tiered BackendConfig (Redis L1 + Postgres L2).
  # SUI_BUILD_DIR points LocalBuilder at the tmpfs mount (SandboxCfg.never_touch_disk).
  podman container exists "$POD-daemon" || podman run -d \
    --pod "$POD" --name "$POD-daemon" \
    --memory "$DAEMON_MEM" \
    --tmpfs "/build:size=$TMPFS_SIZE,mode=1777" \
    -e SUI_BUILD_DIR=/build \
    -v "$HERE/tiered-backend.toml:/etc/sui/tiered-backend.toml:ro" \
    "$SUI_IMAGE" \
    sui cache serve \
      --listen "0.0.0.0:$LISTEN_PORT" \
      --backend-config /etc/sui/tiered-backend.toml

  log "up. daemon on localhost:$HOST_PORT (Redis L1 → Postgres L2, RAMDISK sandbox)."
  log "smoke:  contrib/ground-truth/run.sh smoke"
}

down() {
  log "tearing down pod $POD"
  podman pod rm -f "$POD" 2>/dev/null || true
}

status() {
  podman pod ps --filter "name=$POD" || true
  echo
  podman ps -a --pod --filter "pod=$POD" || true
}

smoke() {
  # The Nix binary cache protocol serves /nix-cache-info; a 200 proves the
  # tiered daemon is up and answering (Redis L1 fronting Postgres L2).
  log "GET http://localhost:$HOST_PORT/nix-cache-info"
  curl -fsS "http://localhost:$HOST_PORT/nix-cache-info" && echo
  log "(a real ground-truth build: sui build a small drv against this cache-url —"
  log " the output lands in Postgres via the Redis-fronted tiered path, sandbox on tmpfs.)"
}

case "${1:-}" in
  up)     up ;;
  down)   down ;;
  status) status ;;
  smoke)  smoke ;;
  *)
    echo "usage: $0 {up|down|status|smoke}" >&2
    exit 2
    ;;
esac
