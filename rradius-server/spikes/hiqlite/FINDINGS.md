# hiqlite spike findings

Spike for the `hiqlite-cluster` change. hiqlite 0.15.3 with `default-features = false`,
features `sqlite` and `auto-heal`, together with `mlua` 0.12 (`luajit`, `async`, `send`)
and `tokio` 1.x. Run on macOS (Darwin 27.2), release build, 2026-11-02/03.

```
cargo run --release              # single node
cargo run --release -- forbidden # non-deterministic function on the write path
cargo run --release -- cluster   # 3 nodes in one process
cargo run --release -- processes # 3 nodes as separate processes, SIGKILL the leader
```

All numbers below are single measurements from one run each, not averages.

## 1. Builds together with mlua and tokio — matches the design

`cargo build --release` succeeds; a LuaJIT state evaluates `return 40 + 2` in the same
binary. No dependency conflicts.

## 2. Single node start, execute, query, shutdown — matches, with two caveats

- `start_node` + `wait_until_healthy_db`: **2551 ms**; `shutdown`: **10 ms**.
- `execute` returns the affected row count (inserting 2 rows returned 2).
- **Caveat (differs from design):** `query_raw` returns `Row::Owned`, whose column names
  are not public, so a generic `SELECT *` cannot be turned into Lua tables with it.
  `query_try_map` with a type implementing `TryFrom<&mut Row>` receives `Row::Borrowed`
  (the underlying `rusqlite::Row`) for local reads, which exposes column names and typed
  values (`Integer`, `Real`, `Text`, `Null`, `Blob` all observed). `sql_query` must use
  `query_try_map`, not `query_raw`.
- **Caveat (differs from design):** 2.5 s per node start makes "start a node per test"
  too slow for the ~40 database tests. Tests need one shared node per test process with
  per-test table names.

## 3. enc_keys and listen addresses — matches the design

- `enc_keys` exists only with the `s3` or `dashboard` features; without them the field
  is absent and nothing has to be configured.
- `listen_addr_api` / `listen_addr_raft` are bare hosts (`"127.0.0.1"`, default
  `"0.0.0.0"`); the ports come from this node's `addr_raft` / `addr_api` entries.

## 4. Writes through the read API are rejected — matches the design

`query_raw` and `query_try_map` with an `INSERT` both return
`Sqlite("attempt to write a readonly database")` and the table is unchanged. hiqlite's
read connections are read-only, so `sql_query` can rely on this error and needs no
statement check of its own.

## 5. Non-deterministic functions — differs from the design

- hiqlite replaces these functions on the write connection with a panicking guard,
  whatever their arguments: `date`, `datetime`, `julianday`, `now`, `random`,
  `randomblob`, `strftime`, `time`, `timediff`, `unixepoch`
  (`FORBIDDEN_NON_DET_FNS` in `store/state_machine/sqlite/state_machine.rs`).
- `strftime('%s', 1700000000)` (fixed arguments) **was rejected** by that guard. The
  design's rule only catches the literal `'now'`, so such calls would pass our check.
- The panic is caught: `execute` returned
  `Sqlite("Error code 1: SQL logic error Some(\"unwinding panic\")")` and the **next write
  succeeded**. The node does not crash; the panic is less severe than the design assumed.
- `CURRENT_TIMESTAMP` is **not** guarded by hiqlite (`execute` returned `Ok(1)`), so it
  silently diverges between nodes. Our own check must reject it.

Conclusion: the `sql_execute` check should reject calls to exactly hiqlite's ten function
names plus the keywords `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_TIME`; the `'now'`
literal rule is not needed.

## 6. Three nodes in one process — differs from the design

- 3 nodes started in one process were healthy after **12633 ms**; a write on node 2 took
  **1 ms** and was visible on node 1 immediately.
- After `shutdown()` of the leader, the surviving nodes returned
  `ClientWriteError: raft stopped` for the whole 30 s observation window. Stopping one
  in-process node does not behave like a node failure, so in-process clusters cannot test
  failover.

## 7. Three nodes as separate processes — matches the design

- 3 node processes ready after **8583 ms**; write on node 2 visible on node 1.
- After SIGKILL of the leader process, the first write timed out (500 ms) and writes
  succeeded again **6522 ms** after the kill.
- With 1 of 3 nodes left, a write under a 500 ms timeout failed after **502 ms**, as the
  "Write without a leader" scenario requires.

Conclusion: the three-node test (task 4.2) has to run nodes as separate processes, for
example by starting the `rradius-server` binary three times and checking the result
through RADIUS requests.

## Design items to update before continuing

1. Non-deterministic check: hiqlite's ten function names (as calls) plus the three
   `CURRENT_*` keywords, instead of the `'now'` literal (affects task 2.2 and the
   `sql-backend` scenarios).
2. `sql_query` reads through `query_try_map` with `Row::Borrowed`, not `query_raw`
   (affects task 4.1).
3. Tests share one hiqlite node per test process instead of starting one per test
   (affects task 4.1).
4. The three-node test uses separate processes (affects task 4.2).
