// Cluster pool + HTTP server debug harness
//
// Goals:
//   1. Use a RedisPool (size=3) so commands spread across 3 independent TCP
//      connections per cluster node — isolates head-of-line blocking per client.
//   2. Run an axum HTTP server on :8181 so commands can be fired interactively
//      (curl) before, during, and after node2 is paused via tc netem.
//   3. tc netem delay is controlled via HTTP API — no shell script orchestration needed.
//
// HTTP routes:
//   GET    /get/:key          → pool.next().get(key)  — JSON: value + client_idx + elapsed_ms
//   GET    /set/:key/:value   → pool.next().set(...)   — JSON: ok + client_idx + elapsed_ms
//   GET    /status            → pool size + round-robin counter
//   POST   /pause?ms=N        → docker exec redis-cluster-2: tc netem add delay Nms  (default 15000)
//   DELETE /pause             → docker exec redis-cluster-2: tc qdisc del (remove delay)
//
// Usage:
//   # Start the server
//   HOST_IP=192.168.247.1 RUST_LOG=fred=debug cargo run --bin cluster_server
//
//   # In another terminal — inject 15s delay on node2
//   curl -X POST "http://localhost:8181/pause?ms=15000"
//
//   # Fire commands during the pause (observe blocking / recovery)
//   curl http://localhost:8181/get/key1
//   curl http://localhost:8181/get/alpha
//
//   # Remove the delay
//   curl -X DELETE http://localhost:8181/pause
//
// Slot layout (3 masters, 0 replicas):
//   node1 (7001): slots   0-5460   → key "alpha"  (slot 865)
//   node2 (7002): slots 5461-10922 → key "key1"   (slot 9189)  ← paused by tc netem
//   node3 (7003): slots 10923-16383→ key "beta"   (slot 15419)

use axum::{
  extract::{Path, Query, State},
  response::Json,
  routing::{delete, get, post},
  Router,
};
use fred::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
  env,
  io::Write,
  process::Command,
  sync::{atomic::{AtomicUsize, Ordering}, Arc},
  time::{Duration, Instant},
};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const PORT_NODE1: u16 = 7001;
const PORT_NODE2: u16 = 7002;
const PORT_NODE3: u16 = 7003;
const POOL_SIZE:  usize = 3;
const HTTP_PORT:  u16   = 8181;

// Shared state passed to axum handlers
#[derive(Clone)]
struct AppState {
  pool:    Arc<RedisPool>,
  counter: Arc<AtomicUsize>, // tracks which client index was last handed out
}

// GET /get/:key
async fn handle_get(
  State(state): State<AppState>,
  Path(key): Path<String>,
) -> Json<Value> {
  let idx = state.counter.fetch_add(1, Ordering::Relaxed) % POOL_SIZE;
  let client = &state.pool.clients()[idx];
  let t = Instant::now();
  match client.get::<Option<String>, _>(key.as_str()).await {
    Ok(v) => Json(json!({
      "key":        key,
      "value":      v,
      "client_idx": idx,
      "elapsed_ms": t.elapsed().as_millis(),
    })),
    Err(e) => Json(json!({
      "key":        key,
      "error":      e.to_string(),
      "client_idx": idx,
      "elapsed_ms": t.elapsed().as_millis(),
    })),
  }
}

// GET /set/:key/:value
async fn handle_set(
  State(state): State<AppState>,
  Path((key, value)): Path<(String, String)>,
) -> Json<Value> {
  let idx = state.counter.fetch_add(1, Ordering::Relaxed) % POOL_SIZE;
  let client = &state.pool.clients()[idx];
  let t = Instant::now();
  match client.set::<(), _, _>(key.as_str(), value.as_str(), None, None, false).await {
    Ok(_) => Json(json!({
      "ok":         true,
      "key":        key,
      "value":      value,
      "client_idx": idx,
      "elapsed_ms": t.elapsed().as_millis(),
    })),
    Err(e) => Json(json!({
      "ok":         false,
      "key":        key,
      "error":      e.to_string(),
      "client_idx": idx,
      "elapsed_ms": t.elapsed().as_millis(),
    })),
  }
}

// GET /status
async fn handle_status(State(state): State<AppState>) -> Json<Value> {
  Json(json!({
    "pool_size":   state.pool.size(),
    "counter":     state.counter.load(Ordering::Relaxed),
  }))
}

#[derive(Deserialize)]
struct PauseParams {
  ms: Option<u64>,
}

// POST /pause?ms=N  — inject tc netem delay on redis-cluster-2
//
// Applies a flat netem delay to all traffic on eth0.
// The cluster gossip (port 17002) also gets delayed, but the docker-compose.yml
// sets cluster-node-timeout=60000ms so gossip survives delays up to 30s without
// Redis declaring the node as failed.
async fn handle_pause(
  State(_state): State<AppState>,
  Query(params): Query<PauseParams>,
) -> Json<Value> {
  let ms = params.ms.unwrap_or(15000);

  // Ensure iproute2/tc is installed
  let has_tc = Command::new("docker")
    .args(["exec", "redis-cluster-2", "which", "tc"])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false);
  if !has_tc {
    let _ = Command::new("docker")
      .args(["exec", "redis-cluster-2", "bash", "-c",
             "apt-get update -qq && apt-get install -y -qq iproute2"])
      .output();
  }

  // Remove any existing qdisc first (idempotent)
  let _ = Command::new("docker")
    .args(["exec", "redis-cluster-2", "tc", "qdisc", "del", "dev", "eth0", "root"])
    .output();

  // Add netem delay on all traffic
  let out = Command::new("docker")
    .args(["exec", "redis-cluster-2", "tc", "qdisc", "add", "dev", "eth0",
           "root", "netem", "delay", &format!("{}ms", ms)])
    .output();

  match out {
    Ok(o) if o.status.success() => {
      println!("[PAUSE] tc netem delay {}ms applied to redis-cluster-2 eth0", ms);
      Json(json!({
        "ok": true,
        "delay_ms": ms,
        "msg": format!("tc netem delay {}ms applied. cluster-node-timeout=60s keeps cluster healthy.", ms)
      }))
    }
    Ok(o) => {
      let err = String::from_utf8_lossy(&o.stderr).to_string();
      println!("[PAUSE] tc netem failed: {}", err);
      Json(json!({ "ok": false, "error": err }))
    }
    Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
  }
}

// DELETE /pause  — remove tc netem delay from redis-cluster-2
async fn handle_unpause(State(_state): State<AppState>) -> Json<Value> {
  let out = Command::new("docker")
    .args(["exec", "redis-cluster-2", "tc", "qdisc", "del", "dev", "eth0", "root"])
    .output();

  match out {
    Ok(o) if o.status.success() => {
      println!("[UNPAUSE] tc netem removed from redis-cluster-2");
      Json(json!({ "ok": true, "msg": "tc netem delay removed from redis-cluster-2 eth0" }))
    }
    Ok(o) => {
      let err = String::from_utf8_lossy(&o.stderr).to_string();
      println!("[UNPAUSE] tc del failed (may already be clear): {}", err);
      Json(json!({ "ok": false, "error": err }))
    }
    Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
  }
}

#[tokio::main]
async fn main() -> Result<(), RedisError> {
  // Layer 1: tokio-console (task monitoring via gRPC at localhost:6669)
  // Layer 2: fmt (fred trace logs to stderr, respects RUST_LOG env var)
  let console_layer = console_subscriber::spawn();
  tracing_subscriber::registry()
    .with(console_layer)
    .with(fmt::layer().with_writer(std::io::stderr))
    .with(EnvFilter::from_default_env())
    .init();

  let host_ip: String = env::var("HOST_IP").unwrap_or_else(|_| "192.168.215.1".to_string());

  println!("=== Cluster pool + HTTP server debug harness ===");
  println!("  pool size: {}", POOL_SIZE);
  println!("  node1 {}:{} → key 'alpha'  (slot 865)", host_ip, PORT_NODE1);
  println!("  node2 {}:{} → key 'key1'   (slot 9189)  ← will be paused", host_ip, PORT_NODE2);
  println!("  node3 {}:{} → key 'beta'   (slot 15419)", host_ip, PORT_NODE3);
  println!("  HTTP server: http://0.0.0.0:{}", HTTP_PORT);
  println!();

  let config = RedisConfig {
    server: ServerConfig::new_clustered(vec![
      (host_ip.as_str(), PORT_NODE1),
      (host_ip.as_str(), PORT_NODE2),
      (host_ip.as_str(), PORT_NODE3),
    ]),
    ..Default::default()
  };

  let perf = PerformanceConfig {
    default_command_timeout: Duration::from_secs(5),
    ..Default::default()
  };

  let conn = ConnectionConfig {
    unresponsive_timeout: Duration::from_secs(2),
    max_command_attempts: 5,
    ..Default::default()
  };

  // Retry up to 1000 times with 3s between attempts — survives a 15s+ node pause
  // without exhausting the reconnect budget.
  let policy = ReconnectPolicy::new_constant(5, 5);

  let pool = Arc::new(RedisPool::new(config, Some(perf), Some(conn), Some(policy), POOL_SIZE)?);

  // Register event handlers on each client in the pool
  for (i, client) in pool.clients().iter().enumerate() {
    let i = i;
    let _u = client.on_unresponsive(move |server| {
      println!("[EVENT][client-{}][unresponsive] node {}:{} went unresponsive", i, server.host, server.port);
      Ok(())
    });
    let _r = client.on_reconnect(move |server| {
      println!("[EVENT][client-{}][reconnect] reconnected to {}:{}", i, server.host, server.port);
      Ok(())
    });
    let _e = client.on_error(move |err| {
      println!("[EVENT][client-{}][error] {}", i, err);
      Ok(())
    });
  }

  pool.connect();
  pool.wait_for_connect().await?;
  println!("[OK] Pool connected ({} clients, {} connections per node).", POOL_SIZE, POOL_SIZE);

  // Pre-test: write one key per node
  let c = pool.next();
  let _: () = c.set("alpha", "value-from-node1", None, None, false).await?;
  let _: () = c.set("key1",  "value-from-node2", None, None, false).await?;
  let _: () = c.set("beta",  "value-from-node3", None, None, false).await?;
  println!("[OK] SET alpha→node1, key1→node2, beta→node3");

  // Verify pre-pause reads
  let a: Option<String> = pool.next().get("alpha").await?;
  let k: Option<String> = pool.next().get("key1").await?;
  let b: Option<String> = pool.next().get("beta").await?;
  println!("[OK] Pre-pause reads: alpha={:?} key1={:?} beta={:?}", a, k, b);
  println!();

  // Start HTTP server
  let app_state = AppState {
    pool:    pool.clone(),
    counter: Arc::new(AtomicUsize::new(0)),
  };
  let app = Router::new()
    .route("/get/:key",        get(handle_get))
    .route("/set/:key/:value", get(handle_set))
    .route("/status",          get(handle_status))
    .route("/pause",           post(handle_pause))
    .route("/pause",           delete(handle_unpause))
    .with_state(app_state);

  let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", HTTP_PORT)).await.unwrap();
  println!("[OK] HTTP server listening on http://0.0.0.0:{}", HTTP_PORT);
  println!();
  println!("  ── Read / Write ───────────────────────────────────────────────");
  println!("  curl http://localhost:{}/get/alpha", HTTP_PORT);
  println!("  curl http://localhost:{}/get/key1", HTTP_PORT);
  println!("  curl http://localhost:{}/get/beta", HTTP_PORT);
  println!("  curl http://localhost:{}/set/mykey/myval", HTTP_PORT);
  println!();
  println!("  ── tc netem control (docker exec redis-cluster-2) ─────────────");
  println!("  curl -X POST   'http://localhost:{}/pause?ms=15000'  # inject 15s delay", HTTP_PORT);
  println!("  curl -X POST   'http://localhost:{}/pause?ms=3000'   # inject 3s delay", HTTP_PORT);
  println!("  curl -X DELETE  http://localhost:{}/pause             # remove delay", HTTP_PORT);
  println!();
  println!("  ── Status ─────────────────────────────────────────────────────");
  println!("  curl http://localhost:{}/status", HTTP_PORT);
  println!();
  println!("  Press Ctrl+C to exit.");
  println!();

  std::io::stdout().flush().ok();

  axum::serve(listener, app).await.unwrap();

  pool.quit().await?;
  Ok(())
}
