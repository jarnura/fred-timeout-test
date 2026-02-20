docker unpause redis-cluster-2 2>/dev/null; sleep 1

echo "=== CLUSTER TEST | RUST_LOG=fred=debug ==="
cd ~/github/fred-timeout-test

RUST_LOG=fred=debug ./target/debug/cluster 2>&1 | while IFS= read -r line; do
  echo "$line"
  if [[ "$line" == "[READY_FOR_PAUSE]" ]]; then
    docker pause redis-cluster-2 >/dev/null 2>&1
    echo "[HOST] >>> redis-cluster-2 (node2 7002) PAUSED at $(date +%T)"
  fi
  if [[ "$line" == "[READY_FOR_UNPAUSE]" ]]; then
    # Unpause after 15s in background so node2 becomes reachable again
    ( sleep 15 && docker unpause redis-cluster-2 >/dev/null 2>&1 && echo "[HOST] >>> redis-cluster-2 UNPAUSED at $(date +%T)" ) &
  fi
done

echo "=== CLUSTER TEST COMPLETE ==="