# fred-timeout-test — Setup, Tests & Variable Reference

## Overview

This repo demonstrates and tests the `unresponsive_timeout` feature in **fred.rs v7.1.2** — a Rust async Redis client.

### What `unresponsive_timeout` does

When a command is sent to Redis but no response ever arrives (TCP connection stays open, server simply stops replying — e.g. network congestion, kernel hang, process freeze), fred's background watchdog detects the stall and automatically:

1. Interrupts the stuck reader task
2. Broadcasts an `on_unresponsive` event
3. Reconnects with the configured backoff policy
4. Retries the in-flight command on the new connection

**The critical misconfiguration:** If `default_command_timeout < unresponsive_timeout`, the caller's `.await` returns `Err(Timeout)` before the watchdog gets a chance to reconnect and retry. The command is silently dropped even though fred does eventually reconnect in the background.

---

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust toolchain | `cargo build` |
| Docker + Docker Compose | Cluster runs via `docker-compose.yml` |
| `redis-cli` | Cluster init and health checks |
| `../fred.rs` | fred.rs v7.1.2 checked out as a sibling directory |
| `iproute2` in redis-cluster-2 | Auto-installed by `test_tc.sh` on first run (requires `CAP_NET_ADMIN`, set in compose) |

---

## Repository Layout

```
fred-timeout-test/
├── Cargo.toml           # three binaries; fred dep at ../fred.rs with check-unresponsive feature
├── docker-compose.yml   # 3-node Redis cluster; redis-cluster-2 has CAP_NET_ADMIN
├── test.sh              # cluster test: simulates node2 freeze via docker pause
├── test_tc.sh           # cluster test: simulates node2 latency via tc netem (no sudo)
└── src/
    ├── misconfigured.rs # standalone: default_command_timeout < unresponsive_timeout → Err(Timeout)
    ├── correct.rs       # standalone: correct config → transparent recovery
    └── cluster.rs       # 3-node cluster: concurrent GETs while node2 is slow/paused
```

---

## Infrastructure Setup

### 1. Start the cluster

```bash
docker compose up -d
```

`redis-cluster-2` is given `CAP_NET_ADMIN` (see `docker-compose.yml`) so that `tc netem` can be run inside the container via `docker exec` without any macOS `sudo`.

### 2. Initialize the cluster (one-time, or after `docker compose down`)

```bash
redis-cli --cluster create \
  192.168.215.1:7001 \
  192.168.215.1:7002 \
  192.168.215.1:7003 \
  --cluster-replicas 0 \
  --cluster-yes
```

Verify it's healthy:

```bash
redis-cli -p 7001 cluster nodes
redis-cli -p 7001 cluster info | grep cluster_state
```

**Expected slot layout:**

```
node1 :7001  →  slots    0–5460   → key "alpha" (slot 865)
node2 :7002  →  slots 5461–10922  → key "key1"  (slot 9189)  ← target node
node3 :7003  →  slots 10923–16383 → key "beta"  (slot 15419)
```

> **Why `--cluster-announce-ip 192.168.215.1`?**
> Containers live inside OrbStack's Linux VM on a Docker bridge network. The gateway IP `192.168.215.1` is what the containers advertise in `CLUSTER SLOTS` responses. This must match what the Rust binary (running on macOS) can actually reach. If this IP differs on your machine, check it with:
> ```bash
> docker inspect redis-cluster-1 --format '{{.NetworkSettings.Gateway}}'
> ```

### 3. Build

```bash
cargo build
```

All three binaries are written to `./target/debug/`.

---

## Tests

### Standalone tests (single Redis node, `docker pause`)

These use a plain Redis instance on `localhost:6379` and coordinate pause/unpause manually.

```bash
docker run -d --name redis-test -p 6379:6379 redis:7.2
```

**Misconfigured** — caller times out before watchdog fires:

```bash
RUST_LOG=fred=debug ./target/debug/misconfigured
# In another terminal when you see [READY_FOR_PAUSE]:
docker pause redis-test
# When you see [READY_FOR_UNPAUSE]:
docker unpause redis-test
```

Expected: `[T=~5000ms] GET error: Request timed out.` — the 5s `default_command_timeout` fires before the 10s `unresponsive_timeout` watchdog.

---

**Correctly configured** — watchdog fires first, command retried transparently:

```bash
RUST_LOG=fred=debug ./target/debug/correct
# pause/unpause redis-test at the signals as above
```

Expected: `[T=~12000ms] GET succeeded: Some("hello")` — fred reconnects and retries without the caller seeing an error.

---

### Cluster tests

Both scripts run the same `cluster` binary. They differ in *how* node2 is made unresponsive:

| Script | Mechanism | Node2 state | Real latency? |
|---|---|---|---|
| `test.sh` | `docker pause` / `docker unpause` | Frozen (SIGSTOP) | No — binary on/off |
| `test_tc.sh` | `tc netem delay Xms` via `docker exec` | **Running normally** | **Yes — gradual delay** |

#### `test.sh` — docker pause (binary freeze)

```bash
bash test.sh
```

Sends SIGSTOP to `redis-cluster-2`. The TCP socket stays open; Redis stops reading. This is the simplest way to reproduce `unresponsive_timeout` but doesn't reflect real network conditions.

#### `test_tc.sh` — tc netem (real latency, node stays up)

```bash
bash test_tc.sh

# Custom delay (must satisfy constraints — see Variables section):
DELAY_MS=4000 bash test_tc.sh
```

Applies `tc qdisc add dev eth0 root netem delay Xms` to node2's network interface. Redis accepts new TCP connections and processes commands normally, but every packet is delayed. This accurately simulates real-world scenarios: network congestion, slow disks causing Redis to lag, overloaded kernel.

**To demonstrate the misconfigured scenario** with tc netem, change `default_command_timeout` in `src/cluster.rs` to a value smaller than `DELAY_MS * 2` (e.g. `Duration::from_secs(3)` with default `DELAY_MS=3000`), rebuild, and rerun.

---

## Variables and Rationale

### Variables in `src/cluster.rs`

#### `unresponsive_timeout` — `ConnectionConfig`
**What it guards:** How long a sent command can wait without any response before the connection is declared unresponsive.

**Default in this repo:** `2s`

**What happens when it fires:** Fred interrupts the reader task for that node only, broadcasts `on_unresponsive`, then enters the reconnect loop. The other two cluster nodes keep serving normally.

**Constraint with tc netem:**
```
unresponsive_timeout < DELAY_MS
```
If `DELAY_MS ≤ unresponsive_timeout`, the delayed response arrives before the watchdog checks — the watchdog never fires and the command just hangs until `default_command_timeout`.

---

#### `default_command_timeout` — `PerformanceConfig`
**What it guards:** The wall-clock deadline on the caller's `.await`. When this fires, the caller gets `Err(Timeout)` and the command is marked `timed_out = true`.

**The danger:** If this fires before the watchdog reconnects and retries, the command is dropped permanently. The `timed_out` flag causes fred to skip retrying it even after a successful reconnect.

**Well-configured value:** Must be significantly larger than the total reconnect time:
```
default_command_timeout > unresponsive_timeout + unresponsive_interval + reconnect_backoff + connection_time
```
In this repo: `60s` (well-configured) vs `3s` (misconfigured demo).

**Misconfigured value:** Anything smaller than `unresponsive_timeout` (or smaller than `DELAY_MS * 2` with tc netem) will cause `Err(Timeout)` before recovery.

---

#### `connection_timeout` — `ConnectionConfig`
**What it guards:** How long fred waits for a TCP connection (+ TLS handshake if applicable) to complete when establishing a new connection.

**Default in this repo:** `15s`

**Why 15s with tc netem:**
`tc netem` delays every packet one-way. A TCP handshake requires a round trip (SYN → SYN-ACK), so the full RTT is `DELAY_MS * 2`. With `DELAY_MS=3000ms`, the RTT is 6s. `connection_timeout` must exceed this:
```
connection_timeout > DELAY_MS * 2
```
If this is too small (e.g. the old `5s` value with `DELAY_MS=3000`), fred cannot establish new connections to node2 during the reconnect loop → cluster sync fails.

---

#### `internal_command_timeout` — `ConnectionConfig`
**What it guards:** Timeout for setup commands sent immediately after a new connection is established: `AUTH`, `SELECT`, `CLIENT ID`, `CLUSTER INFO`, `CLUSTER SLOTS`.

**Default in this repo:** `15s`

**Why this matters for tc netem — the `Invalid or missing cluster state` error:**

During cluster reconnect, fred's `sync()` function:
1. Sends `CLUSTER SLOTS` via a backchannel (succeeds — uses node1 or node3)
2. Builds a list of nodes to connect to, including node2
3. For each node, calls `transport.setup()` which runs `CLUSTER INFO` to verify the node is healthy

`setup()` wraps the entire sequence in `apply_timeout(internal_command_timeout)`. With tc netem active, the `CLUSTER INFO` round-trip to node2 takes `DELAY_MS * 2 = 6s`. If `internal_command_timeout = 5s`, this times out, `try_join_all()` fails, and the entire sync aborts with:
```
Protocol Error: Invalid or missing cluster state.
```

**Constraint:**
```
internal_command_timeout > DELAY_MS * 2
```

---

#### `max_command_attempts` — `ConnectionConfig`
**What it guards:** How many times fred will attempt to send a command before giving up with an error. Each reconnect cycle counts as one attempt.

**Default in this repo:** `3`

Each attempt decrements the counter. If all attempts are exhausted before a successful response, the caller gets `Err(TooManyAttempts)`.

---

### Variable in `test_tc.sh`

#### `DELAY_MS` — environment variable
**What it controls:** The per-packet one-way delay `tc netem` applies to all traffic on node2's `eth0`. This affects **all** traffic in and out of the container — including new TCP connection handshakes during fred's reconnect loop, not just in-flight Redis command responses.

**Default:** `3000` (3 seconds)

**The constraint triangle:**

```
unresponsive_timeout (2s)
    < DELAY_MS (3s)                →  watchdog fires before response arrives  ✓
    DELAY_MS × 2 (6s RTT)
        < connection_timeout (15s)          →  new TCP handshake completes     ✓
        < internal_command_timeout (15s)    →  CLUSTER INFO completes          ✓
            ≪ default_command_timeout (60s) →  caller waits for full recovery  ✓
```

**If you change `DELAY_MS`, update `cluster.rs` accordingly** to keep all constraints satisfied.

---

### Fred global: `unresponsive_interval`
**What it controls:** How often the background watchdog task polls all in-flight commands to check for stalls.

**Default:** `2000ms` (2 seconds, set in fred's `globals.rs`)

**Effect on detection timing:** The watchdog fires between `unresponsive_timeout` and `unresponsive_timeout + unresponsive_interval`. With defaults, detection happens between 2s and 4s after the command was sent. The `[WARN] Server unresponsive after Xms` log line shows the actual elapsed time.

Can be changed at runtime:
```rust
fred::globals::set_unresponsive_interval_ms(500); // poll every 500ms
```

---

## Environment Notes (OrbStack on macOS Apple M1)

- Docker containers run inside OrbStack's Linux VM, bridged via `bridge104` / `vmenet4`
- `tc` classful qdiscs (`prio`, `htb`, `drr`) are **not available** in OrbStack's stripped kernel — only `netem` works
- Because `prio` is unavailable, port-specific filtering (delay only packets from port 7002, not new TCP connections) is not possible — the entire `eth0` is shaped uniformly
- `CAP_NET_ADMIN` is a Linux container capability, not a macOS privilege — no `sudo` password required at test time
- `pfctl`/`dnctl` on the macOS host could also inject latency at the `bridge104` interface level, but require `sudo`

---

## Resetting State

```bash
# Remove any leftover tc rules from a failed or interrupted test
docker exec redis-cluster-2 tc qdisc del dev eth0 root 2>/dev/null || true

# Unfreeze node2 if docker pause was used
docker unpause redis-cluster-2 2>/dev/null || true

# Full cluster teardown and restart
docker compose down
docker compose up -d
# Then re-run cluster-init:
redis-cli --cluster create \
  192.168.215.1:7001 192.168.215.1:7002 192.168.215.1:7003 \
  --cluster-replicas 0 --cluster-yes
```
