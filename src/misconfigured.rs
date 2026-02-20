// Scenario A: MISCONFIGURED
// default_command_timeout (5s) < unresponsive_timeout (10s)
// The caller gives up at 5s — BEFORE the unresponsive watchdog fires at ~10-12s.
// Result: caller gets Err(Timeout); command is NOT retried after reconnect.

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
    // DANGER: caller gives up at 5s, but unresponsive fires at 10s
    default_command_timeout: Duration::from_secs(5),
    ..Default::default()
  };

  let conn = ConnectionConfig {
    unresponsive_timeout: Duration::from_secs(10),
    max_command_attempts: 3,
    ..Default::default()
  };

  let policy = ReconnectPolicy::new_constant(5, 1000);
  let client = RedisClient::new(config, Some(perf), Some(conn), Some(policy));

  // Subscribe to unresponsive and reconnect events for observability
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

  // Write a key so we have something to GET
  let _: () = client.set("mykey", "hello", None, None, false).await?;
  println!("[OK] SET mykey = hello");

  // Signal to the orchestration script that we're ready to be paused.
  println!("[READY_FOR_PAUSE]");
  use std::io::Write;
  std::io::stdout().flush().ok();
  // Give the external script 1s to pause Redis before we send the GET
  tokio::time::sleep(Duration::from_secs(1)).await;

  println!();
  println!("[T=0s] Sending GET mykey (Redis should be paused now)...");
  let t0 = Instant::now();

  match client.get::<Option<String>, _>("mykey").await {
    Ok(val) => println!("[T={}ms] GET succeeded: {:?}", t0.elapsed().as_millis(), val),
    Err(e) => println!(
      "[T={}ms] GET error: {} (kind: {:?})",
      t0.elapsed().as_millis(),
      e,
      e.kind()
    ),
    // Expected with misconfiguration:
    //   [T=~5000ms] GET error: Request timed out.  (Timeout)
    //   — caller gave up at 5s, BEFORE unresponsive watchdog fires at ~10-12s
    //   — unresponsive event fires later (around T=10s), reconnect happens, but command is SKIPPED
  }

  println!();
  println!("Waiting 15 more seconds to observe background events (unresponsive + reconnect)...");
  println!("[READY_FOR_UNPAUSE]");
  std::io::stdout().flush().ok();
  tokio::time::sleep(Duration::from_secs(15)).await;

  println!("[DONE] Exiting.");
  client.quit().await?;
  Ok(())
}
