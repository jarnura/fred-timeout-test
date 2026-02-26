#!/usr/bin/env bash
# Same tc netem orchestration as test_tc.sh but targets cluster_server binary.
# The HTTP server stays alive after the automated test completes so you can
# fire interactive curl commands to probe pool behavior.
#
# Usage:
#   HOST_IP=192.168.247.1 RUST_LOG=fred=debug bash test_tc_server.sh 2>&1 | tee run_server.log
#
# Then in another terminal:
#   curl http://localhost:8080/get/alpha
#   curl http://localhost:8080/get/key1
#   curl http://localhost:8080/get/beta
#   curl http://localhost:8080/set/foo/bar
#   curl http://localhost:8080/status

set -euo pipefail

DELAY_MS=${DELAY_MS:-15000}

HOST_IP=${HOST_IP:-192.168.247.1}
export HOST_IP

# Ensure tc is available in the container
if ! docker exec redis-cluster-2 which tc >/dev/null 2>&1; then
  echo "[SETUP] Installing iproute2 in redis-cluster-2..."
  docker exec redis-cluster-2 bash -c "apt-get update -qq && apt-get install -y -qq iproute2" >/dev/null
fi

cleanup() {
  docker exec redis-cluster-2 tc qdisc del dev eth0 root 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "=== CLUSTER SERVER TEST (tc netem ${DELAY_MS}ms) | RUST_LOG=${RUST_LOG:-fred=debug} ==="
cd ~/github/fred-timeout-test

RUST_LOG=${RUST_LOG:-fred=debug} ./target/debug/cluster_server 2>&1 | while IFS= read -r line; do
  echo "$line"
  if [[ "$line" == "[READY_FOR_PAUSE]" ]]; then
    docker exec redis-cluster-2 tc qdisc add dev eth0 root netem delay "${DELAY_MS}ms"
    echo "[HOST] >>> tc netem delay ${DELAY_MS}ms applied to redis-cluster-2 eth0 at $(date +%T)"
  fi
  if [[ "$line" == "[READY_FOR_UNPAUSE]" ]]; then
    (
      sleep 20
      cleanup
      echo "[HOST] >>> tc netem removed from redis-cluster-2 at $(date +%T)"
    ) &
  fi
done

echo "=== CLUSTER SERVER TEST COMPLETE ==="
