#!/usr/bin/env bash
# Simulate a slow (not frozen) Redis node2 via tc netem inside the container.
# No docker pause, no macOS sudo — just docker exec.
#
# Requires redis-cluster-2 to have CAP_NET_ADMIN (set in docker-compose.yml).
# iproute2 is installed on first run if missing.
#
# IMPORTANT: tc netem delays ALL traffic on eth0 — including new TCP handshakes
# during fred's cluster reconnect/sync loop. To keep those succeeding, the fred
# config in cluster.rs must set:
#   connection_timeout / internal_command_timeout > DELAY_MS * 2
#     (each packet is delayed one-way, so a full request/response RTT = DELAY_MS*2)
#   unresponsive_timeout < DELAY_MS
#     (so the watchdog fires before the delayed response arrives)
# Default: DELAY_MS=3000ms, unresponsive_timeout=2s, connection_timeout=15s

set -euo pipefail
# DELAY_MS must satisfy two constraints:
#   1. DELAY_MS > unresponsive_timeout (2s) — so the watchdog fires before response arrives
#   2. DELAY_MS * 2 < connection_timeout (15s) AND internal_command_timeout (15s)
#      — so new TCP connections during fred's reconnect loop can complete (each
#        packet is delayed one-way, so a full RTT = DELAY_MS*2)
# Default: 3s satisfies both (3s > 2s, and 6s RTT < 15s timeouts).
DELAY_MS=${DELAY_MS:-3000}

# Ensure tc is available in the container
if ! docker exec redis-cluster-2 which tc >/dev/null 2>&1; then
  echo "[SETUP] Installing iproute2 in redis-cluster-2..."
  docker exec redis-cluster-2 bash -c "apt-get update -qq && apt-get install -y -qq iproute2" >/dev/null
fi

cleanup() {
  docker exec redis-cluster-2 tc qdisc del dev eth0 root 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "=== CLUSTER TEST (tc netem ${DELAY_MS}ms) | RUST_LOG=fred=debug ==="
cd ~/github/fred-timeout-test

RUST_LOG=fred=debug ./target/debug/cluster 2>&1 | while IFS= read -r line; do
  echo "$line"
  if [[ "$line" == "[READY_FOR_PAUSE]" ]]; then
    docker exec redis-cluster-2 tc qdisc add dev eth0 root netem delay "${DELAY_MS}ms"
    echo "[HOST] >>> tc netem delay ${DELAY_MS}ms applied to redis-cluster-2 eth0 at $(date +%T)"
  fi
  if [[ "$line" == "[READY_FOR_UNPAUSE]" ]]; then
    (
      sleep 15
      cleanup
      echo "[HOST] >>> tc netem removed from redis-cluster-2 at $(date +%T)"
    ) &
  fi
done

echo "=== CLUSTER TEST COMPLETE ==="
