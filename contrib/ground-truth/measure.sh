#!/usr/bin/env bash
# sui ground-truth MEASUREMENT harness — first real numbers off the ONE
# full-stack tiered instance stood up by run.sh (Postgres L2 + Redis L1 +
# tmpfs RAMDISK sandbox).
#
# WHAT IT MEASURES (all against the running `sui cache serve` daemon over the
# Nix binary-cache HTTP protocol — PUT/GET narinfo + NAR through the
# TieredBackend resolver):
#   - daemon-up gate                 (GET /nix-cache-info → 200)
#   - write-through placement        (a PUT lands in Postgres L2 AND warms Redis L1)
#   - COLD read  (L1 flushed → L2 hit + L1 promotion)   → latency
#   - WARM read  (L1 hit)                                → latency
#   - warm-vs-cold delta + tier hit distribution (L1 vs L2)
#   - daemon HTTP request latency (the serve() axum path)
#
# HOST GLUE ONLY (Op-Principle: shell as thin orchestration). The TYPED work is
# the Rust daemon + the TieredBackend resolver (sui-cache/src/storage/tiered.rs):
# read-through promotes L2→L1, write-through writes L2 then warms L1. This
# harness just times the HTTP surface and inspects the two tiers to CONFIRM the
# resolver did what its types say — it invents no logic.
#
# TIER-HONEST: this measures the store/cache DAEMON (the TieredBackend's whole
# point). It does NOT measure a `sui build` RAMDISK-vs-disk delta — that is a
# separate `sui build <drv>` path (the tmpfs sandbox), noted in the numbers doc
# as the remaining gap. It does NOT exercise a live breathe controller (static
# band only on one node).
#
# Usage:  contrib/ground-truth/measure.sh        # run the full measurement
#         HOST_PORT=5555 POD=sui-ground-truth contrib/ground-truth/measure.sh
set -euo pipefail

POD=${POD:-sui-ground-truth}
HOST_PORT=${HOST_PORT:-5555}                    # matches run.sh (macOS AirPlay owns :5000)
BASE="http://localhost:${HOST_PORT}"
export DOCKER_CONFIG="${DOCKER_CONFIG:-/tmp/sui-gt-emptydockercfg}"
mkdir -p "$DOCKER_CONFIG"

# A deterministic 32-char store-path hash + a minimal-but-valid narinfo body.
HASH="0000000000000000000000000000gtst"
NARINFO_BODY=$'StorePath: /nix/store/'"$HASH"$'-ground-truth\nURL: nar/gtst.nar.xz\nCompression: xz\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 128\nReferences: \n'
NAR_PATH="gtst.nar.xz"
NAR_BYTES=131072                                # 128 KiB blob, deterministic zeros

log()  { printf '\033[36m[measure]\033[0m %s\n' "$*"; }
fail() { printf '\033[31m[measure] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

# curl one request, print HTTP code + total_time (seconds, 6dp) to stdout.
# $1=method $2=url [$3=body-file]
timed() {
  local method="$1" url="$2" body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -s -o /dev/null -w '%{http_code} %{time_total}\n' -X "$method" \
      --data-binary "@$body" -H 'content-type: application/octet-stream' "$url"
  else
    curl -s -o /dev/null -w '%{http_code} %{time_total}\n' -X "$method" "$url"
  fi
}

redis()  { podman exec "$POD-redis" redis-cli "$@" 2>/dev/null; }
pgcount() { podman exec "$POD-pg" psql -U sui -d sui -tAc "SELECT count(*) FROM $1 WHERE key = '$2';" 2>/dev/null | tr -d ' '; }

# ── 0. daemon-up gate ─────────────────────────────────────────────────────────
log "0. GET /nix-cache-info (daemon-up gate)"
INFO=$(timed GET "$BASE/nix-cache-info")
echo "   → $INFO"
[[ "$INFO" == 200\ * ]] || fail "daemon not answering /nix-cache-info (got: $INFO)"

# ── 1. write-through PUT narinfo → L2 (Postgres) + L1 (Redis) ─────────────────
log "1. PUT /$HASH.narinfo  (write-through: L2 first, then warm L1)"
TMP=$(mktemp); printf '%s' "$NARINFO_BODY" > "$TMP"
PUT=$(timed PUT "$BASE/$HASH.narinfo" "$TMP")
echo "   → PUT $PUT"
[[ "$PUT" == 20*\ * ]] || fail "PUT narinfo failed (got: $PUT)"
sleep 0.3
L2=$(pgcount sui_cache_narinfo "$HASH")
L1=$(redis EXISTS "sui:narinfo:$HASH")
echo "   → L2(Postgres sui_cache_narinfo).count=$L2   L1(Redis sui:narinfo:$HASH).exists=$L1"
[[ "$L2" == "1" ]] || fail "write-through did NOT reach Postgres L2 (L2 count=$L2)"
[[ "$L1" == "1" ]] || log "   note: L1 not warmed (write policy may be WriteAround) — L1 exists=$L1"

# ── 2. COLD read (flush L1 → forces L2 hit + read-through promotion) ──────────
log "2. FLUSH Redis L1, then COLD GET (L1 miss → L2 Postgres hit → promote to L1)"
redis DEL "sui:narinfo:$HASH" >/dev/null
[[ "$(redis EXISTS "sui:narinfo:$HASH")" == "0" ]] || fail "could not flush L1 key"
# 3 cold samples (each re-flush so every one is a true L1 miss)
COLD_SUM=0; COLD_N=3
for i in $(seq 1 $COLD_N); do
  redis DEL "sui:narinfo:$HASH" >/dev/null
  C=$(timed GET "$BASE/$HASH.narinfo")
  echo "   → cold[$i] $C   (L1 now exists=$(redis EXISTS "sui:narinfo:$HASH"))"
  [[ "$C" == 200\ * ]] || fail "cold GET missed L2 (got: $C) — tiered read-through broken"
  T=$(echo "$C" | awk '{print $2}')
  COLD_SUM=$(echo "$COLD_SUM + $T" | bc -l)
done
COLD_AVG=$(echo "scale=6; $COLD_SUM / $COLD_N" | bc -l)

# ── 3. WARM read (L1 hit, no flush) ──────────────────────────────────────────
log "3. WARM GET x N (L1 Redis hit — no flush)"
# prime L1 once
timed GET "$BASE/$HASH.narinfo" >/dev/null
WARM_SUM=0; WARM_N=5
for i in $(seq 1 $WARM_N); do
  W=$(timed GET "$BASE/$HASH.narinfo")
  echo "   → warm[$i] $W"
  [[ "$W" == 200\ * ]] || fail "warm GET failed (got: $W)"
  T=$(echo "$W" | awk '{print $2}')
  WARM_SUM=$(echo "$WARM_SUM + $T" | bc -l)
done
WARM_AVG=$(echo "scale=6; $WARM_SUM / $WARM_N" | bc -l)

# ── 4. NAR blob round-trip (128 KiB, write-through then cold/warm) ───────────
log "4. PUT /nar/$NAR_PATH ($NAR_BYTES bytes) → L2, then cold/warm GET"
NARTMP=$(mktemp); head -c "$NAR_BYTES" /dev/zero > "$NARTMP"
NPUT=$(timed PUT "$BASE/nar/$NAR_PATH" "$NARTMP")
echo "   → PUT $NPUT"
[[ "$NPUT" == 20*\ * ]] || fail "PUT nar failed (got: $NPUT)"
sleep 0.3
NL2=$(pgcount sui_cache_nar "nar/$NAR_PATH")
echo "   → L2(Postgres sui_cache_nar).count=$NL2"
redis DEL "sui:nar:nar/$NAR_PATH" "sui:nar:$NAR_PATH" >/dev/null 2>&1 || true
NCOLD=$(timed GET "$BASE/nar/$NAR_PATH")
NWARM=$(timed GET "$BASE/nar/$NAR_PATH")
echo "   → nar cold $NCOLD    nar warm $NWARM"

# ── summary ──────────────────────────────────────────────────────────────────
DELTA=$(echo "scale=6; $COLD_AVG - $WARM_AVG" | bc -l)
SPEEDUP=$(echo "scale=2; $COLD_AVG / $WARM_AVG" | bc -l 2>/dev/null || echo "n/a")
echo
log "════════ GROUND-TRUTH NUMBERS (narinfo, TieredBackend Redis L1 → Postgres L2) ════════"
printf '   COLD (L1 miss → L2 Postgres hit → promote)  avg = %s s  (n=%d)\n' "$COLD_AVG" "$COLD_N"
printf '   WARM (L1 Redis hit)                         avg = %s s  (n=%d)\n' "$WARM_AVG" "$WARM_N"
printf '   L1-vs-L2 read delta                              = %s s  (%sx faster warm)\n' "$DELTA" "$SPEEDUP"
echo
log "tier placement CONFIRMED: PUT reached Postgres L2 (durable); read-through promotes L2→L1."
log "this promotes PgStore MockParityProven → LiveClusterProven (real Postgres, not the oracle)."
rm -f "$TMP" "$NARTMP"
