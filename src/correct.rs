// Scenario B: CORRECTLY CONFIGURED
// default_command_timeout (60s) >> unresponsive_timeout (10s)
// The watchdog fires at ~10-12s, reconnects, retries the command transparently.
// Result: caller gets Ok(value) after reconnect — no error seen.

use fred::prelude::*;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), RedisError> {
  // Initialize env_logger so fred's internal log::debug!/warn!/trace! calls are visible.
  // Control verbosity with RUST_LOG env var, e.g.:
  //   RUST_LOG=fred=debug   → fred internals only
  //   RUST_LOG=fred=trace   → everything including frame-level detail
  //   RUST_LOG=debug        → all crates
  env_logger::init();
  let config = RedisConfig {
    server: ServerConfig::new_centralized("127.0.0.1", 6379),
    ..Default::default()
  };

  let perf = PerformanceConfig {
    // CORRECT: 60s >> unresponsive_timeout(10s) + reconnect time
    default_command_timeout: Duration::from_secs(60),
    ..Default::default()
  };

  let conn = ConnectionConfig {
    unresponsive_timeout: Duration::from_secs(10),
    max_command_attempts: 3,
    connection_timeout:   Duration::from_secs(5),
    ..Default::default()
  };

  let policy = ReconnectPolicy::new_constant(10, 1000);
  let client = RedisClient::new(config, Some(perf), Some(conn), Some(policy));

  // Subscribe to events for observability
  let _unresponsive_task = client.on_unresponsive(|server| {
    println!("[EVENT][unresponsive] Server went unresponsive: {}", server);
    Ok(())
  });
  let _reconnect_task = client.on_reconnect(|server| {
    println!("[EVENT][reconnect] Reconnected to: {}", server);
    Ok(())
  });
  let _error_task = client.on_error(|err| {
    println!("[EVENT][error] Connection error: {:?}", err);
    Ok(())
  });

  client.connect();
  client.wait_for_connect().await?;
  println!("[OK] Connected to Redis.");

  let _: () = client.set("mykey", "hello", None, None, false).await?;
  println!("[OK] SET mykey = hello");

  // Signal orchestration script to pause Redis, then wait 1s before sending GET
  println!("[READY_FOR_PAUSE]");
  use std::io::Write;
  std::io::stdout().flush().ok();
  tokio::time::sleep(Duration::from_secs(1)).await;

  println!();
  println!("[T=0s] Sending GET mykey (Redis is paused — waiting for watchdog to reconnect)...");
  let t0 = Instant::now();

  match client.get::<Option<String>, _>("mykey").await {
    Ok(val) => println!(
      "[T={}ms] GET succeeded: {:?}  (reconnected + retried transparently!)",
      t0.elapsed().as_millis(),
      val
    ),
    Err(e) => println!(
      "[T={}ms] GET error: {} (kind: {:?})",
      t0.elapsed().as_millis(),
      e,
      e.kind()
    ),
    // Expected with correct config:
    //   [EVENT][unresponsive] fires at ~10-12s
    //   [EVENT][reconnect] fires shortly after (once docker unpaused)
    //   [T=~12000ms+] GET succeeded: Some("hello")
  }

  println!("[DONE] Exiting.");
  client.quit().await?;
  Ok(())
}
