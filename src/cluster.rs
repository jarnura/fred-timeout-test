// Cluster scenario: unresponsive_timeout with a 3-node Redis cluster
//
// Key insight: unresponsive detection is PER-NODE.
// When one cluster node goes unresponsive:
//   - Only that node's reader task is interrupted
//   - Only commands routed to that node are affected
//   - The other two nodes keep serving requests normally
//   - fred sends RouterCommand::SyncCluster (not Reconnect) to re-sync topology
//
// Slot layout (3 masters, 0 replicas):
//   node1 (7001): slots   0-5460   → key "alpha"  (slot 865)
//   node2 (7002): slots 5461-10922 → key "key1"   (slot 9189)  ← we pause this node
//   node3 (7003): slots 10923-16383→ key "beta"   (slot 15419)
//
// Host IP: 192.168.215.1 (what CLUSTER SLOTS reports; reachable from host)
// Container names: redis-cluster-1/2/3 (docker pause targets)

use fred::prelude::*;
use std::{
  io::Write,
  time::{Duration, Instant},
};

const HOST_IP: &str = "192.168.215.1";
const PORT_NODE1: u16 = 7001; // owns "alpha"
const PORT_NODE2: u16 = 7002; // owns "key1"  ← will be paused
const PORT_NODE3: u16 = 7003; // owns "beta"

#[tokio::main]
async fn main() -> Result<(), RedisError> {
  env_logger::init();

  println!("=== Cluster unresponsive_timeout test ===");
  println!("  node1 {}:{} → key 'alpha'  (slot 865)", HOST_IP, PORT_NODE1);
  println!("  node2 {}:{} → key 'key1'   (slot 9189)  ← will be paused", HOST_IP, PORT_NODE2);
  println!("  node3 {}:{} → key 'beta'   (slot 15419)", HOST_IP, PORT_NODE3);
  println!();

  let config = RedisConfig {
    server: ServerConfig::new_clustered(vec![
      (HOST_IP, PORT_NODE1),
      (HOST_IP, PORT_NODE2),
      (HOST_IP, PORT_NODE3),
    ]),
    ..Default::default()
  };

  let perf = PerformanceConfig {
    // Correct config: 60s >> unresponsive_timeout(2s) + cluster sync time
    // For the misconfigured scenario, set this to e.g. 3s (< netem_delay*2=6s)
    default_command_timeout: Duration::from_secs(60),
    ..Default::default()
  };

  let conn = ConnectionConfig {
    // unresponsive_timeout must be < netem_delay (3s) so the watchdog fires
    // before the delayed response arrives.
    unresponsive_timeout:     Duration::from_secs(2),
    max_command_attempts:     3,
    // connection_timeout and internal_command_timeout must be > netem_delay*2 (6s)
    // so that new TCP connections during the reconnect loop succeed.
    connection_timeout:       Duration::from_secs(15),
    internal_command_timeout: Duration::from_secs(15),
    ..Default::default()
  };

  let policy = ReconnectPolicy::new_constant(10, 1000);
  let client = RedisClient::new(config, Some(perf), Some(conn), Some(policy));

  // Per-node event tracking
  let _unresponsive_task = client.on_unresponsive(|server| {
    println!(
      "[EVENT][unresponsive] node {}:{} went unresponsive",
      server.host, server.port
    );
    Ok(())
  });
  let _reconnect_task = client.on_reconnect(|server| {
    println!(
      "[EVENT][reconnect] reconnected to {}:{}",
      server.host, server.port
    );
    Ok(())
  });
  let _error_task = client.on_error(|err| {
    println!("[EVENT][error] {}", err);
    Ok(())
  });

  client.connect();
  client.wait_for_connect().await?;
  println!("[OK] Connected to cluster (3 nodes).");

  // Write one key to each node
  let _: () = client.set("alpha", "value-from-node1", None, None, false).await?;
  let _: () = client.set("key1",  "value-from-node2", None, None, false).await?;
  let _: () = client.set("beta",  "value-from-node3", None, None, false).await?;
  println!("[OK] SET alpha→node1, key1→node2, beta→node3");

  // Verify all reads work before the test
  let a: Option<String> = client.get("alpha").await?;
  let k: Option<String> = client.get("key1").await?;
  let b: Option<String> = client.get("beta").await?;
  println!("[OK] Pre-pause reads: alpha={:?} key1={:?} beta={:?}", a, k, b);
  println!();

  // Signal orchestration script to pause node2 only
  println!("[READY_FOR_PAUSE]");
  std::io::stdout().flush().ok();
  tokio::time::sleep(Duration::from_secs(1)).await;

  println!("[T=0s] node2 (7002) is now paused. Starting concurrent reads...");
  println!("       → GET alpha (node1)  should succeed immediately");
  println!("       → GET key1  (node2)  should block, then recover after ~10-12s");
  println!("       → GET beta  (node3)  should succeed immediately");
  println!();

  let t0 = Instant::now();

  // Fire all three GETs concurrently
  let client1 = client.clone();
  let client2 = client.clone();
  let client3 = client.clone();

  let h_alpha = tokio::spawn(async move {
    let t = Instant::now();
    match client1.get::<Option<String>, _>("alpha").await {
      Ok(v)  => println!("[T={}ms] GET alpha (node1) → {:?}  ✓ unaffected", t.elapsed().as_millis(), v),
      Err(e) => println!("[T={}ms] GET alpha (node1) → ERROR: {}", t.elapsed().as_millis(), e),
    }
  });

  let h_key1 = tokio::spawn(async move {
    let t = Instant::now();
    match client2.get::<Option<String>, _>("key1").await {
      Ok(v)  => println!("[T={}ms] GET key1  (node2) → {:?}  ← recovered after reconnect", t.elapsed().as_millis(), v),
      Err(e) => println!("[T={}ms] GET key1  (node2) → ERROR: {}", t.elapsed().as_millis(), e),
    }
  });

  let h_beta = tokio::spawn(async move {
    let t = Instant::now();
    match client3.get::<Option<String>, _>("beta").await {
      Ok(v)  => println!("[T={}ms] GET beta  (node3) → {:?}  ✓ unaffected", t.elapsed().as_millis(), v),
      Err(e) => println!("[T={}ms] GET beta  (node3) → ERROR: {}", t.elapsed().as_millis(), e),
    }
  });

  // Signal to unpause node2 after 15s so the reconnect can succeed
  println!("[READY_FOR_UNPAUSE]");
  std::io::stdout().flush().ok();

  let _ = tokio::join!(h_alpha, h_key1, h_beta);

  println!();
  println!("[T={}ms] All GETs completed.", t0.elapsed().as_millis());
  println!("[DONE] Exiting.");
  client.quit().await?;
  Ok(())
}
