Context

The user wants to understand how the unresponsive_timeout feature works in fred.rs v7.1.2 — a Rust async Redis client. This feature detects when a Redis server stops responding to in-flight commands and triggers reconnection + retry automatically.

What It Is

unresponsive_timeout is a client-side watchdog that detects hung connections — situations where a command was sent to Redis but no response ever arrives (e.g., network partition, server freeze). It is opt-in via a compile-time feature flag in v7.1.2.

Feature Flag

In v7.1.2 the entire feature is gated behind:

toml

# Cargo.toml features = ["check-unresponsive"]

All structs, methods, and globals related to this feature are #[cfg(feature = "check-unresponsive")]. If the flag is absent, zero overhead is incurred.

(In v10, this feature was removed and the behaviour became always-on via UnresponsiveConfig in ConnectionConfig.)

Configuration

File: src/types/config.rs

In v7.1.2, the timeout is a single Duration field directly on ConnectionConfig:

rust

pub struct ConnectionConfig {   // ...   /// The amount of time a command can wait without a response before the   /// corresponding connection is considered unresponsive. This will trigger   /// a reconnection and in-flight commands will be retried.   ///   /// Default: 10 sec   #[cfg(feature = "check-unresponsive")]   pub unresponsive_timeout: Duration, }  impl Default for ConnectionConfig {   fn default() -> Self {     ConnectionConfig {       // ...       #[cfg(feature = "check-unresponsive")]       unresponsive_timeout: Duration::from_millis(10_000), // 10 seconds     }   } }

There is no nested struct (unlike v10's UnresponsiveConfig { max_timeout, interval }).

The check interval (how often the watchdog polls) is a separate global setting:

File: src/modules/globals.rs

rust

// Default: 2 seconds pub unresponsive_interval: Arc<AtomicUsize>,  // default = 2_000 ms  // Public API to read/write it: pub fn get_unresponsive_interval_ms() -> u64 { ... } pub fn set_unresponsive_interval_ms(val: u64) -> u64 { ... }

So in v7.1.2:

SettingLocationDefaultTimeout (per command)ConnectionConfig.unresponsive_timeout10 secondsPoll intervalglobals::unresponsive_interval (process-wide)2 seconds

How It Works — Step by Step

Step 1: Timestamp stamped when command is sent

File: src/protocol/command.rs

Each RedisCommand has a field:

rust

pub network_start: Option<Instant>,

When a command is written to the socket, network_start is set to Instant::now(). This records exactly when the command left the client.

Blocking commands (e.g., BLPOP) are excluded from this check — they intentionally wait for a long time.

Step 2: A dedicated background task watches all connections

File: src/router/types.rs — NetworkTimeout and ConnectionState

When a connection is established, a dedicated Tokio background task is spawned:

rust

pub struct NetworkTimeout {   handle: Arc<RwLock<Option<JoinHandle<()>>>>,   state:  ConnectionState,  // shared view of all connection buffers }  impl NetworkTimeout {   pub fn spawn_task(&self, inner: &Arc<RedisClientInner>) {     let inner = inner.clone();     let state = self.state.clone();     let interval = Duration::from_millis(globals().unresponsive_interval_ms()); // 2s      *self.handle.write() = Some(tokio::spawn(async move {       loop {         let unresponsive = state.unresponsive_connections(&inner);         if unresponsive.len() > 0 {           state.interrupt(&inner, unresponsive);         }         sleep(interval).await;  // wake every 2 seconds       }     }));   } }

The ConnectionState holds a HashMap<Server, SharedBuffer> — a shared reference to each connection's in-flight command queue. It is synced whenever connections are added/removed (centralized, clustered, sentinel, and replica connections all register here).

Step 3: Poll — find unresponsive servers

File: src/router/types.rs — ConnectionState::unresponsive_connections()

Every 2 seconds the background task calls this method:

rust

pub fn unresponsive_connections(&self, inner: &Arc<RedisClientInner>) -> VecDeque<Server> {   let now = Instant::now();   let mut unresponsive = VecDeque::new();    for (server, commands) in self.commands.read().iter() {     let last_command_sent = commands.lock()       .front()                         // look at the oldest in-flight command       .and_then(|cmd| {         if cmd.blocks_connection() {   // skip blocking commands           None         } else {           cmd.network_start.clone()    // get the send timestamp         }       });      if let Some(sent) = last_command_sent {       let elapsed = now.duration_since(sent);       if elapsed > inner.connection.unresponsive_timeout {  // compare vs 10s default         warn!("Server {} unresponsive after {} ms", server, elapsed.as_millis());         unresponsive.push_back(server.clone());       }     }   }    unresponsive }

Logic:





Look at the front (oldest) pending command for each server



Calculate now - network_start



If elapsed > unresponsive_timeout → mark server as unresponsive

Step 4: Interrupt the reader task + broadcast event

File: src/router/types.rs — ConnectionState::interrupt()

rust

pub fn interrupt(&self, inner: &Arc<RedisClientInner>, servers: VecDeque<Server>) {   for server in servers.into_iter() {     inner.notifications.broadcast_unresponsive(server.clone()); // notify listeners      if let Some(tx) = self.interrupts.read().get(&server) {       let _ = tx.send(());  // signal the reader task to stop     }   } }

Two things happen:





Event broadcast — an on_unresponsive event fires, delivering the Server to any registered listeners



Reader interrupt — an unbounded channel sends () to the per-connection reader task, causing it to break out of its receive loop

Step 5: Reconnection + command retry — fully automatic

This entire step is handled internally by fred. The user does not need to do anything.

Here is exactly how it works:

a) defer_reconnect() sends a message to the router task

File: src/router/utils.rs — defer_reconnect()

rust

pub fn defer_reconnect(inner: &Arc<RedisClientInner>) {   if inner.config.server.is_clustered() {     // For cluster: send SyncCluster command to router     let cmd = RouterCommand::SyncCluster { tx };     interfaces::send_to_router(inner, cmd);   } else {     // For centralized/sentinel: send Reconnect command to router     let cmd = RouterCommand::Reconnect { server: None, tx: None, force: false, .. };     interfaces::send_to_router(inner, cmd);   } }

This is a non-blocking fire-and-forget message sent over the router's internal command channel. The interrupt (from Step 4) and this reconnect signal are entirely decoupled from user code.

b) The router's command loop picks up the Reconnect message

File: src/router/commands.rs — process_command()

rust

RouterCommand::Reconnect { server, force, tx, .. } =>   process_reconnect(inner, router, server, force, tx).await

c) reconnect_with_policy() loops with backoff until reconnected

File: src/router/utils.rs

rust

pub async fn reconnect_with_policy(inner: &Arc<RedisClientInner>, router: &mut Router) -> Result<(), RedisError> {   let mut delay = next_reconnection_delay(inner)?;  // respects ReconnectPolicy (backoff)   loop {     if !delay.is_zero() {       inner.wait_with_interrupt(delay).await?;       // sleeps, but interruptible     }     if let Err(e) = reconnect_once(inner, router).await {       delay = next_reconnection_delay(inner)?;       // next backoff step       continue;     } else {       break;                                         // success     }   }   Ok(()) }

The backoff strategy is configured by ReconnectPolicy (constant / linear / exponential with jitter) set by the user when building the client. If all attempts are exhausted, the client gives up and the error propagates.

See the dedicated "ReconnectPolicy" section below for the full configuration reference.

d) In-flight commands are automatically buffered and retried

When disconnect_all() is called during reconnection:

rust

pub async fn disconnect_all(&mut self) {   let commands = self.connections.disconnect_all(&self.inner).await;   self.buffer_commands(commands);   // moves in-flight commands into retry buffer   // ... }

Each RedisCommand has an attempts_remaining counter (set by max_command_attempts in ConnectionConfig, default: 3). After reconnect, retry_buffer() replays them:

rust

pub async fn retry_buffer(&mut self) {   for mut command in commands.drain(..) {     if command.decr_check_attempted().is_err() {       command.finish(&self.inner, Err(too_many_attempts_error));  // give up this command       continue;     }     // write the command again to the new connection   } }

Summary: What is automatic vs. what is user's choice

ActionWho does itDetect unresponsive serverFred (automatic) via background taskBroadcast on_unresponsive eventFred (automatic)Interrupt the reader taskFred (automatic)Close the connectionFred (automatic)Reconnect with backoffFred (automatic), respecting ReconnectPolicyBuffer in-flight commandsFred (automatic)Retry in-flight commandsFred (automatic), up to max_command_attemptsConfigure timeout valueUser — ConnectionConfig.unresponsive_timeoutConfigure backoff strategyUser — ReconnectPolicy passed to client.connect()React to event (optional)User — via client.on_unresponsive(fn) listener (optional)

The on_unresponsive() listener is purely informational/observability — it fires so the user can log or alert, but reconnection happens regardless of whether anyone is listening.

Public Listener API

File: src/interfaces.rs

Users can subscribe to unresponsive events:

rust

#[cfg(feature = "check-unresponsive")] fn on_unresponsive<F>(&self, func: F) -> JoinHandle<RedisResult<()>> where   F: Fn(Server) -> RedisResult<()> + Send + 'static, {   let rx = self.unresponsive_rx();   spawn_event_listener(rx, func) }

Example usage:

rust

client.on_unresponsive(|server| {   println!("Server {} stopped responding!", server);   Ok(()) }); ```  Internally backed by a `tokio::sync::broadcast` channel stored in `Notifications::unresponsive`.  ---  ## Data Flow Diagram ``` Command sent to socket   └─► command.network_start = Instant::now()  Background task (every 2s)   └─► ConnectionState::unresponsive_connections()         └─► for each server: now - network_start > unresponsive_timeout (10s)?               └─► YES → ConnectionState::interrupt()                           ├─► broadcast_unresponsive(server)  →  on_unresponsive() listeners                           └─► send () to reader task interrupt channel                                 └─► reader loop exits                                       └─► reconnect + retry in-flight commands

ReconnectPolicy — What "reconnect_backoff" Actually Means

ReconnectPolicy is the user-supplied policy passed to client.connect(policy). It controls how long fred waits between reconnection attempts and how many times it tries. This is what was referred to as "reconnect backoff time" in the timeout formula.

File: src/types/config.rs

Three Variants

1. Constant — same delay every attempt

rust

ReconnectPolicy::new_constant(max_attempts, delay_ms) // e.g. new_constant(5, 1000) → wait 1s between each attempt, up to 5 attempts

Fields:





delay: fixed wait in ms (+ jitter)



max_attempts: 0 = retry forever



jitter: random noise added (default 50ms)

2. Linear — delay grows linearly

rust

ReconnectPolicy::new_linear(max_attempts, max_delay_ms, delay_ms) // e.g. new_linear(5, 10_000, 1000) // attempt 1 → 1s, attempt 2 → 2s, attempt 3 → 3s, ..., capped at 10s

Formula: delay_ms × attempt_number, capped at max_delay_ms.

3. Exponential — delay grows exponentially

rust

ReconnectPolicy::new_exponential(max_attempts, min_delay_ms, max_delay_ms, multiplier) // e.g. new_exponential(5, 100, 30_000, 2) // attempt 1 → 100ms, attempt 2 → 200ms, attempt 3 → 400ms, ..., capped at 30s

Formula: multiplier^(attempt-1) × min_delay_ms, capped at max_delay_ms.

Default (if none provided):

rust

ReconnectPolicy::Constant {   max_attempts: 0,    // retry forever   delay:        1000, // 1 second between attempts   jitter:       50,   // ±50ms }

Jitter

Every variant adds random jitter (default 50ms) to prevent thundering-herd reconnections when many clients disconnect simultaneously. Configurable via policy.set_jitter(ms).

How next_reconnection_delay() Uses It

File: src/router/utils.rs

rust

pub fn next_reconnection_delay(inner: &Arc<RedisClientInner>) -> Result<Duration, RedisError> {   inner     .policy     .write()     .as_mut()     .and_then(|policy| policy.next_delay())   // increments attempt counter, returns next delay     .map(Duration::from_millis)     .ok_or(RedisError::new(Canceled, "Max reconnection attempts reached.")) } ```  When `next_delay()` returns `None` (max attempts exceeded), `next_reconnection_delay` returns `Err(Canceled)`, which causes `reconnect_with_policy` to return that error, ending the connection task.  ### Total Worst-Case Reconnect Time  The "reconnect backoff time" in the timeout formula is the **sum of all delays across all reconnect attempts** before success:  For `Constant { delay: 1000, max_attempts: 3 }` with `connection_timeout: 5s`: ``` Attempt 1: wait 1s + TCP connect (up to 5s) → up to 6s Attempt 2: wait 1s + TCP connect (up to 5s) → up to 6s Attempt 3: wait 1s + TCP connect (up to 5s) → up to 6s                                                       ───── Total worst case:                                  up to 18s ```  For `Exponential { min_delay: 100, max_delay: 30_000, mult: 2, max_attempts: 3 }`: ``` Attempt 1: wait 100ms + TCP connect Attempt 2: wait 200ms + TCP connect Attempt 3: wait 400ms + TCP connect ```  ### Putting It All Together: The Full Formula ``` safe_default_command_timeout =     unresponsive_timeout          // time until detection   + unresponsive_interval         // detection polling lag (worst case)   + (sum of all reconnect delays) // reconnect backoff   + (max_attempts × connection_timeout) // TCP connect time per attempt   + safety_margin                 // extra buffer ```  **Example with defaults + `check-unresponsive`:** ``` = 10s (unresponsive_timeout) + 2s  (unresponsive_interval) + 3s  (3 attempts × 1s constant delay) + 30s (3 attempts × 10s connection_timeout) + 5s  (safety margin) ───────────────────────────────────────── = 50s minimum recommended default_command_timeout

With the default ReconnectPolicy (max_attempts: 0 = infinite), default_command_timeout is the only guard that prevents indefinite waiting.

All Timeout Values — Configuration Deep Dive

There are 4 distinct timeout settings in v7.1.2. They are completely independent of each other and each guards a different phase of the client lifecycle.

Complete Timeout Map

Timeout SettingLocationDefaultGuardsunresponsive_timeoutConnectionConfig (feature-gated)10 secTime between writing a command and getting any response backdefault_command_timeoutPerformanceConfig0 (disabled)Caller-side await for a command response (wall-clock)connection_timeoutConnectionConfig10 secTCP connect + TLS handshakeinternal_command_timeoutConnectionConfig10 secAUTH, SELECT, CLUSTER SLOTS and other setup commands

default_command_timeout — How It Works

File: src/utils.rs — basic_request_response() (the function called by every public command)

rust

pub async fn basic_request_response<C, F, R>(client: &C, func: F) -> Result<Resp3Frame, RedisError> {   let (tx, rx) = oneshot_channel();   command.response = ResponseKind::Respond(Some(tx));    let timed_out = command.timed_out.clone();   let timeout_dur = prepare_command(client, &mut command); // reads default_command_timeout   client.send_command(command)?;                           // fires the command    apply_timeout(rx, timeout_dur)        // race: response channel vs sleep timer     .map_err(move |error| {       set_bool_atomic(&timed_out, true); // mark command as timed out       error     })     .await }

File: src/utils.rs — apply_timeout()

rust

pub async fn apply_timeout<T, Fut, E>(ft: Fut, timeout: Duration) -> Result<T, RedisError> {   if !timeout.is_zero() {     match select(ft, sleep(timeout)).await {       Either::Left((result, _)) => result,                          // response arrived in time       Either::Right(_) => Err(RedisError::new(Timeout, "Request timed out.")),  // deadline exceeded     }   } else {     ft.await  // no timeout: wait forever   } }

Key: When default_command_timeout fires, the caller's .await returns Err(Timeout) immediately. The command is also marked timed_out = true so that if it eventually gets retried (after reconnect), the router skips re-sending it.

Setting timeout_dur per-command (src/protocol/command.rs — inherit_options()):

rust

pub fn inherit_options(&mut self, inner: &Arc<RedisClientInner>) {   // ...   if self.timeout_dur.is_none() {     let default_dur = inner.default_command_timeout();     if !default_dur.is_zero() {       self.timeout_dur = Some(default_dur);  // inherit global default     }   } } ```  Per-command override via `with_options(&Options { timeout: Some(Duration::from_secs(5)), .. })`.  ---  ### How `unresponsive_timeout` and `default_command_timeout` Interact  These two timeouts guard **completely different things** and operate **concurrently**: ``` T=0ms      Command written to socket  →  network_start = Instant::now()            default_command_timeout clock starts (on caller's await)            unresponsive background task polls every 2s  T=2s       Background task polls: elapsed since network_start = 2s < 10s ✓  T=4s       Background task polls: elapsed = 4s < 10s ✓  T=6s       Background task polls: elapsed = 6s < 10s ✓  T=8s       Background task polls: elapsed = 8s < 10s ✓  T=10s      BOTH timers fire approximately simultaneously:            ├─ unresponsive background task: elapsed = 10s ≥ 10s            │    → disconnect + reconnect + retry            └─ default_command_timeout (if set): apply_timeout fires                 → caller gets Err(Timeout) immediately

The Dangerous Misconfiguration: default_command_timeout < unresponsive_timeout

This is the core of your question. If configured improperly, the command times out from the caller's perspective before the unresponsive detection can trigger a reconnect.

Example — misconfigured (problematic):

rust

PerformanceConfig {   default_command_timeout: Duration::from_secs(5),  // caller gives up at 5s   ..Default::default() } ConnectionConfig {   unresponsive_timeout: Duration::from_secs(10),    // reconnect fires at 10s   ..Default::default() } ```  **What happens:** ``` T=0s    Command sent T=5s    default_command_timeout fires → caller gets Err(Timeout) ← caller gives up T=5s    command.timed_out = true T=10s   unresponsive background task fires → disconnect → reconnect → retry_buffer T=10s   retry_buffer: command.timed_out == true → SKIPPED (not retried)

The caller already got an error at 5s. The command is dropped at retry. The user sees a timeout error, not a reconnection. The client does reconnect in the background, but there's no automatic retry of the timed-out command.

Example — well-configured:

rust

PerformanceConfig {   default_command_timeout: Duration::from_secs(15), // gives unresponsive time to act   ..Default::default() } ConnectionConfig {   unresponsive_timeout: Duration::from_secs(10),    // reconnect fires at 10s   ..Default::default() } ```  **What happens:** ``` T=0s    Command sent T=10s   unresponsive background task fires → disconnect → reconnect T=10s+  retry_buffer: command.timed_out == false → retried on new connection ✓ T=<15s  Response arrives on new connection → caller gets Ok(result)

The caller waits transparently while fred reconnects and retries.

The Second Dangerous Case: Both Disabled or Defaults

Default config (no default_command_timeout, no check-unresponsive feature):

rust

// default_command_timeout = 0 (disabled) → caller waits FOREVER // check-unresponsive feature not compiled in → no background task

If Redis stops responding: the caller hangs indefinitely. No timeout, no reconnect, no error.

Default config (with check-unresponsive feature, default values):

rust

// default_command_timeout = 0 (disabled) → caller waits until reconnect // unresponsive_timeout = 10s → reconnect fires after 10s // unresponsive_interval = 2s → checked every 2s, so fires between 10–12s

With only unresponsive_timeout set, the caller eventually gets the result after reconnect (best case), or an error if max_command_attempts is exhausted.

Recommended Configuration Pattern

rust

let config = RedisConfig {   connection: ConnectionConfig {     connection_timeout:       Duration::from_secs(5),   // TCP connect     internal_command_timeout: Duration::from_secs(5),   // AUTH/SELECT/etc     max_command_attempts:     3,                         // retry 3 times before giving up     #[cfg(feature = "check-unresponsive")]     unresponsive_timeout:     Duration::from_secs(10),  // reconnect if no response in 10s     ..Default::default()   },   performance: PerformanceConfig {     // Set default_command_timeout >> unresponsive_timeout so that reconnect+retry     // has time to complete before the caller gives up.     //     // Rule: default_command_timeout > unresponsive_timeout × max_command_attempts     // e.g. 10s × 3 attempts = 30s minimum; add reconnect time → 45-60s is safe     default_command_timeout: Duration::from_secs(60),     ..Default::default()   },   ..Default::default() }; ```  **The rule:** > `default_command_timeout` > `unresponsive_timeout` × `max_command_attempts` + reconnect backoff time  This ensures the command-level timeout never fires before unresponsive detection has had a chance to reconnect and retry all allowed attempts.  ---  ### Poll Interval's Role in Precision  The unresponsive background task fires every `unresponsive_interval_ms` (default: 2s). This means `unresponsive_timeout` has **±2s precision**. A server that stops responding at T=0 will be detected **between T=10s and T=12s** with default settings — not exactly at T=10s.  This makes the gap between `default_command_timeout` and `unresponsive_timeout` need to account for this jitter: ``` effective_unresponsive_trigger = unresponsive_timeout + unresponsive_interval (worst case)                                 = 10s + 2s = 12s

So default_command_timeout should be at least 12s × max_command_attempts + reconnect_backoff.

All Timeout Values — Summary Table

SettingTypeDefaultWhen to Changeunresponsive_timeoutConnectionConfig10sReduce for faster failure detection; increase for high-latency networksdefault_command_timeoutPerformanceConfig0 (off)Always set this if using check-unresponsive; must be > unresponsive_timeout × max_attemptsconnection_timeoutConnectionConfig10sReduce for fast-fail; increase for slow networksinternal_command_timeoutConnectionConfig10sRarely needs changingunresponsive_interval (global)globals::set_unresponsive_interval_ms()2sReduce for finer detection granularity; increases CPU slightly

How to Reproduce and Test — Step by Step

Scenario A: Reproduce default_command_timeout < unresponsive_timeout (commands time out before reconnect)

Setup

1. Checkout v7.1.2:

bash

git -C ~/github/fred.rs checkout v7.1.2

2. Start a local Redis with Docker:

bash

docker run -d --name redis-test -p 6379:6379 redis:7.2

3. Create a test project that depends on the fred crate at v7.1.2:

toml

# Cargo.toml [package] name = "fred-timeout-test" edition = "2021"  [dependencies] fred = { path = "../fred.rs", features = ["check-unresponsive", "tokio-comp"] } tokio = { version = "1", features = ["full"] }

4. Write the test program (src/main.rs):

rust

use fred::prelude::*; use std::time::Duration;  #[tokio::main] async fn main() -> Result<(), RedisError> {     // --- MISCONFIGURED: default_command_timeout < unresponsive_timeout ---     let config = RedisConfig {         server: ServerConfig::new_centralized("127.0.0.1", 6379),         performance: PerformanceConfig {             // Caller gives up at 5s — BEFORE unresponsive fires at 10s             default_command_timeout: Duration::from_secs(5),             ..Default::default()         },         connection: ConnectionConfig {             unresponsive_timeout: Duration::from_secs(10),  // fires at 10s             max_command_attempts: 3,             ..Default::default()         },         ..Default::default()     };      let policy = ReconnectPolicy::new_constant(5, 1000);     let client = Builder::from_config(config).build()?;      // Subscribe to events for observability     let unresponsive_task = client.on_unresponsive(|server| {         println!("[EVENT] Server went unresponsive: {}", server);         Ok(())     });     let reconnect_task = client.on_reconnect(|server| {         println!("[EVENT] Reconnected to: {}", server);         Ok(())     });      client.connect_with_policy(policy);     client.wait_for_connect().await?;     println!("Connected.");      // Set a value first     client.set::<(), _, _>("key", "value", None, None, false).await?;     println!("Set key. Now pause Redis (run: docker pause redis-test)");     println!("Waiting 3 seconds for you to pause...");     tokio::time::sleep(Duration::from_secs(3)).await;      // This GET will hang — Redis is paused     println!("[T=0] Sending GET command...");     let start = std::time::Instant::now();     match client.get::<Option<String>, _>("key").await {         Ok(val) => println!("[T={}ms] Got: {:?}", start.elapsed().as_millis(), val),         Err(e) => println!("[T={}ms] Error: {:?}", start.elapsed().as_millis(), e),         // Expected: Error at ~5s (Timeout) — before unresponsive reconnect fires     }      tokio::time::sleep(Duration::from_secs(15)).await;     client.quit().await?;     unresponsive_task.abort();     reconnect_task.abort();     Ok(()) }

5. Run:

bash

cargo run # In another terminal, pause Redis when prompted: docker pause redis-test # After a few seconds, unpause: docker unpause redis-test ```  **Expected output (misconfigured):** ``` Connected. Set key. Now pause Redis (run: docker pause redis-test) Waiting 3 seconds for you to pause... [T=0] Sending GET command... [T=5001ms] Error: Timeout("Request timed out.")   ← caller gives up at 5s [EVENT] Server went unresponsive: 127.0.0.1:6379  ← fires ~10s (too late) [EVENT] Reconnected to: 127.0.0.1:6379            ← reconnect happens, but command was already dropped

Fix: Set default_command_timeout > unresponsive_timeout

Change the config:

rust

performance: PerformanceConfig {     default_command_timeout: Duration::from_secs(60),  // give 60s for reconnect+retry     ..Default::default() }, ```  **Expected output (correct config):** ``` Connected. [T=0] Sending GET command... [EVENT] Server went unresponsive: 127.0.0.1:6379  ← fires at ~10–12s [EVENT] Reconnected to: 127.0.0.1:6379            ← reconnects automatically [T=~12000ms] Got: Some("value")                   ← command retried and succeeded!

Scenario B: Reproduce missing check-unresponsive feature — caller hangs forever

toml

# Cargo.toml — compile WITHOUT check-unresponsive [dependencies] fred = { path = "../fred.rs", features = ["tokio-comp"] }  # no check-unresponsive

rust

// No default_command_timeout set (default = 0 = disabled) // No unresponsive detection // → GET will hang indefinitely after Redis is paused

Add default_command_timeout: Duration::from_secs(5) to at least get an error back.

Scenario C: Test using Docker pause/unpause (recommended)

docker pause sends SIGSTOP to the container — the TCP connection stays open but Redis stops reading/responding. This is the cleanest way to simulate an unresponsive server without closing the connection (which would be detected by TCP RST, not by unresponsive_timeout).

bash

# Pause (simulate hung server — no RST, connection stays "open") docker pause redis-test  # Unpause after testing docker unpause redis-test  # Alternative: use iptables to drop packets (Linux only) sudo iptables -A OUTPUT -p tcp --dport 6379 -j DROP   # block outbound to Redis sudo iptables -D OUTPUT -p tcp --dport 6379 -j DROP   # restore

Why docker pause and not just stopping Redis? Stopping Redis sends a TCP RST/FIN — fred detects this via the read loop immediately (not via unresponsive_timeout). The unresponsive_timeout specifically catches cases where the TCP connection stays alive but Redis stops responding (e.g., process hung, kernel-level issue, network black hole).

Scenario D: Verify the poll interval precision

To observe the ±2s detection window, set a very short timeout and watch the timestamps:

rust

fred::globals::set_unresponsive_interval_ms(500); // poll every 500ms for faster detection  ConnectionConfig {     unresponsive_timeout: Duration::from_secs(3),  // should fire between 3–3.5s     ..Default::default() }

Running the Existing Integration Tests at v7.1.2

The repo's integration tests use Docker Compose:

bash

cd ~/github/fred.rs  # Start Redis containers source tests/environ docker compose -f tests/docker/compose/centralized.yml up -d  # Run tests with check-unresponsive enabled cargo test --features check-unresponsive -- --nocapture 2>&1 | grep -i "unresponsive\|timeout\|reconnect"  # Clean up docker compose -f tests/docker/compose/centralized.yml down

Key Characteristics in v7.1.2

AspectDetailFeature flagcheck-unresponsive (compile-time opt-in)Config fieldConnectionConfig.unresponsive_timeout: DurationDefault timeout10 secondsPoll intervalGlobal: globals::unresponsive_interval = 2 secondsDetection methodBackground Tokio task polling network_start timestampsBlocking commandsExcluded from checks (blocks_connection() returns true)Pub/SubNot mentioned in v7 checks (different from v10 which explicitly skips them)Action on detectionInterrupt reader task → reconnect → retryUser notificationon_unresponsive(fn(Server)) broadcast channelScopePer-connection (all connection types: centralized, cluster, sentinel, replicas)

How v7.1.2 Differs from v10.1.0

v7.1.2v10.1.0Feature flagcheck-unresponsive requiredAlways available (no flag)Config shapeConnectionConfig.unresponsive_timeout: DurationConnectionConfig.unresponsive: UnresponsiveConfig { max_timeout, interval }Default timeout10 secondsNone (disabled by default)Poll intervalGlobal singleton (process-wide)Per-client, inside UnresponsiveConfig.intervalDetection approachSeparate background NetworkTimeout task with ConnectionStateInline during poll_connection() using conn.last_write on the Connection structTimestamp locationRedisCommand.network_startConnection.last_writeTriggerPolls SharedBuffer command queue frontFires when poll_next returns Poll::PendingPub/Sub exclusionImplicit (blocking commands excluded)Explicit (!command.kind.is_pubsub())