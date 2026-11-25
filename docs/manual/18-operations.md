# 18 Day-to-day operations

> After this chapter you can:
> - Change the configuration with "`check` → `config diff` → reload → read the log and metrics"
> - Tell which settings take effect on reload and which need a restart, and restart and shut down safely
> - Upgrade the program, add migrations, and know which tasks need downtime
> - Know how the core protects your data, and that it has no backup feature
> - Do all of this under systemd, logrotate, Docker Compose, Kubernetes and Synology

Prerequisites: you have done [Chapter 03](03-quick-start.md) and read [Chapter 04](04-configuration.md), so you know the configuration file, `include` and `[shutdown]`. For the cluster parts, read [Chapter 16](16-cluster.md) first.

In this chapter:

- [Standard procedure for changing configuration](#standard-procedure-for-changing-configuration)
- [What reload applies](#what-reload-applies)
- [Shutdown and restart](#shutdown-and-restart)
- [Rolling restart of a cluster](#rolling-restart-of-a-cluster)
- [Upgrading the program](#upgrading-the-program)
- [Adding a migration](#adding-a-migration)
- [Accounts, sessions and queries](#accounts-sessions-and-queries)
- [Changing secrets](#changing-secrets)
- [Purging old data](#purging-old-data)
- [Data directory and data protection](#data-directory-and-data-protection)
- [Logs and logrotate](#logs-and-logrotate)
- [Managing with systemd](#managing-with-systemd)
- [On Docker Compose and Synology](#on-docker-compose-and-synology)
- [On Kubernetes](#on-kubernetes)
- [Monitoring: the metrics to watch first](#monitoring-the-metrics-to-watch-first)
- [Pre-launch checklist](#pre-launch-checklist)
- [What requires downtime](#what-requires-downtime)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

The examples in this chapter use the quick-start scenario ([Chapter 03](03-quick-start.md)), plus an accounting log and `[shutdown]`. All ports are the defaults: RADIUS 1812 and 1813, metrics 9812.

## Standard procedure for changing configuration

Every configuration change follows these four steps:

1. **`check`:** can the new configuration be loaded?
2. **`config diff`:** compared with the running one, what exactly changed?
3. **reload:** send SIGHUP so the server rereads the configuration.
4. **Read the log and metrics:** confirm that the change was applied, or learn which parts need a restart.

### 1. check

Write the new configuration to a separate file, or back up the old one first, then check:

```console
$ cp rradius.toml rradius.toml.orig
$ vi rradius.toml
$ rradiusd check --config rradius.toml
configuration ok
```

`check` reads the configuration file, the included files, the dictionary, the scripts and the secret files. Exit code 0 means it can be loaded and 2 means there is an error. It opens no ports and does not touch the data directory, so you can run it while the server is running.

### 2. config diff

`config diff` fills in the defaults for both configurations and compares them key by key:

```console
$ rradiusd config diff rradius.toml.orig rradius.toml
lua.max_execution_ms: 1000 | 500
radius.auth_port: 1812 | 1822
```

- Each line is `key: old value | new value`; if one side has no value, it says `(absent)`.
- Secrets are shown only as `differs (hidden)`; their content is not printed.
- Exit codes: 1 if there are differences, 0 if there are none (it prints `no differences`), 2 if a configuration cannot be read.
- Differences in `cluster.node_id` and `cluster.listen` are listed at the end under `node-specific:` and do not count as differences. This is handy when you compare the configurations of two cluster nodes ([Chapter 16](16-cluster.md)).
- To see one configuration in full with the defaults filled in, use `config show --config rradius.toml`.

In the differences above, `radius.auth_port` needs a restart to take effect, while `lua.max_execution_ms` takes effect on reload. The table in the next section tells you how to distinguish them.

### 3. reload

Send SIGHUP to the server process:

```console
$ kill -HUP <pid>
```

Under systemd, use `systemctl reload rradius` (`ExecReload` sends SIGHUP).

A reload is all or nothing. The server first builds everything that needs replacing from the new configuration, and swaps it all in only if every step succeeds. If any step fails, nothing is replaced and the server keeps serving as before. Requests being handled finish with the scripts, auth chain and client settings they started with, and are not interrupted.

### 4. Read the log and metrics

Every reload leaves a line in the log. Everything applied:

```text
INFO rradius_core::reload: configuration reloaded from rradius.toml
```

Some changes were not applied and need a restart:

```text
WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port
```

The other changes are already in effect (`lua.max_execution_ms` in the example), and the server is still on the old port. On every later reload, as long as the configuration still differs from the running one, the line is written again, until you restart.

If the configuration has an error:

```text
ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: line 63: `x`: extra `=`, expected nothing
```

The error message names the file and line, but does not quote the line, because it might contain a password. Fix it and send SIGHUP again.

The metrics show the result of reloads:

```text
rradius_reload_total{result="ok"} 1
rradius_reload_total{result="error"} 1
rradius_reload_last_success_timestamp_seconds 1795385561
```

The `Reload` section of `status` shows the same data ([Chapter 17](17-monitoring.md)). Monitor the increase of `rradius_reload_total{result="error"}` to catch failed reloads that nobody noticed.

> **Note:** Each node in a cluster reads its own configuration file. After a change, edit every node and reload every node.

## What reload applies

Settings that are applied and not applied on reload:

| Takes effect on reload | Needs a restart |
| --- | --- |
| The top-level script `lua.script`, and the scripts of the auth chain and handlers | `dictionary` (changing the content of a dictionary file is not reread either) |
| The auth chain and settings in `[auth]` | Any key of `[cluster]`, including migrations |
| The limits in `[lua]` (except `max_memory_mb`) | `lua.max_memory_mb` |
| The RADIUS and TACACS+ client lists and settings, including the content of `secret_file` | `[metrics]`, `[log]` (except `log.trace`), `[runtime]` |
| `log.trace` ([Chapter 17](17-monitoring.md)) | `[databases]`, `[queries]` |
| `[shutdown]` | `radius.bind`, `auth_port`, `acct_port`, `sockets`, `recv_buffer_bytes`, `status_server`, `max_auth_inflight`, `max_acct_inflight` |
| `[accounts]`, `[sessions]` (read each time a command runs) | `[radius.accounting_spool]`; `queue` of `[radius.accounting_log]` |
| `file` of `[radius.accounting_log]` (the file is reopened on reload) | `tacacs.bind`, `port`, `max_connections`, `max_connections_per_source`, `max_body_len`, `idle_timeout_ms` |
| | Adding or removing a whole `[radius]` or `[tacacs]` |

Whether each key can be reloaded is also written in the reload column of [Appendix A: Configuration reference](90-config-reference.md).

## Shutdown and restart

### Shutdown order

After SIGTERM (or Ctrl+C in the foreground, which is SIGINT), the server does the following in order:

1. `/readyz` switches to 503 `stopping`, but requests are still answered as usual for `shutdown.ready_delay_ms`.
2. It stops accepting new requests: RADIUS stops reading the socket, TACACS+ closes its listener, and replay of the accounting spool (the on-disk buffer that holds accounting requests while the database cannot be written) stops (what was already written stays on disk).
3. It waits for the requests being handled to finish, for at most `shutdown.drain_ms`.
4. It closes the cluster node's database and exits with exit code 0.

| Key | Default | Purpose |
| --- | --- | --- |
| `shutdown.ready_delay_ms` | 0 for a single node; 5000 when `cluster.nodes` has more than one | Time for load balancers to see the 503 and move traffic away |
| `shutdown.drain_ms` | 5000 | Upper limit for waiting on requests being handled |

Both keys take effect on reload, using the configuration at the moment the stop signal arrives. See [Chapter 04](04-configuration.md) for details.

Measured: with `ready_delay_ms = 3000` in quick-start, sending SIGTERM and probing every 0.4 seconds:

```text
t=0.0 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=1.4 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=2.7 readyz=[stopping 503] accept 1, reject 0, challenge 0, accounting 0, timeouts 0
t=3.2 readyz=[ 000] accept 0, reject 0, challenge 0, accounting 0, timeouts 1
exited
```

For the first 3 seconds `/readyz` already answers 503 while logins are still accepted. After 3 seconds requests are no longer accepted and the process exits. The log:

```text
INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 3000 ms more, then those being handled are waited for, for at most 5000 ms
INFO rradius_core::cli: stopping: no more requests taken
INFO rradius_core::cli: stopping: every request taken was answered
WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

If requests are still not finished when `drain_ms` runs out, a WARN is written that lists how many RADIUS requests and TACACS+ connections are left, and then they are cut off. If you send SIGTERM or SIGINT again during the stop, the server stops waiting and goes straight to closing the database; closing the database is never skipped. `/livez` still answers 200 during the stop.

### Effect on load balancers

- When a load balancer uses `/readyz` to decide, `ready_delay_ms` must be longer than its check interval times the number of failures, so that traffic moves away before the server stops accepting.
- When a NAS is configured with several RADIUS servers directly, nobody looks at `/readyz`. After the server stops accepting, the NAS requests time out and the NAS asks the next server.
- A single node has no other server to ask, so the default is `ready_delay_ms = 0`.

### How long shutdown takes

In a cluster with more than one node, a node also waits about 9.5 seconds before closing the database, to let the cluster hand over (in measured logs the gap from `every request taken was answered` to `SQL writer is shutting down` is exactly 9.5 seconds). Measured:

| Case | SIGTERM to process exit |
| --- | --- |
| Single node, `ready_delay_ms = 3000` | About 3 seconds |
| Leader of a three-node cluster, default values | About 14.5 seconds |
| Follower of a four-node cluster, default values | About 15 seconds |

So give the process manager a long enough wait: the bundled systemd unit has `TimeoutStopSec=60`, the Compose and Synology examples have `stop_grace_period: 30s`, and the Kubernetes example has `terminationGracePeriodSeconds: 30`. Docker waits only 10 seconds by default, which is not enough.

> **Warning:** If the wait is too short, the process is killed with SIGKILL. A node that was killed has to rebuild its local database from the Raft log at the next start, and the log says `Node did not shut down gracefully - auto-rebuilding State Machine`. No data is lost, but the start is slower.

### Restart

A restart is SIGTERM followed by a start. At start, the node waits for the leader, applies migrations, and catches up with the cluster before it begins serving ([Chapter 16](16-cluster.md)). A single node answers no requests while it restarts.

## Rolling restart of a cluster

When you apply a setting that needs a restart, or upgrade the program, restart one node at a time:

1. Change this node's configuration (or replace its program) and run `check`.
2. Send SIGTERM and wait for the process to exit.
3. Start it, wait for its `/readyz` to return `ok` and for `has caught up with the cluster` to appear in the log.
4. Use `cluster members` to confirm that all three nodes are voters and there is a leader.
5. Then do the next node.

Stopping one node at a time keeps a majority in a three-node cluster, so writes are not interrupted. When the leader is stopped, writes are slower for a while (about 7 to 8 seconds: 6 to 7 seconds of election plus about 1 second for clients to reconnect; measured on 2026-11-23 on a three-node Docker setup on one host, for both the core and the enterprise edition). If you know which node is the leader, leave it for last, so the election happens only once. For measurements of rolling upgrades across sites with higher latency, see [Chapter 16, "Across data centers"](16-cluster.md#across-data-centers).

> **Note:** When you change `secret_raft` or `secret_api` in `[cluster]`, nodes with the old and new secret do not recognize each other. This change cannot be done one node at a time: stop the whole cluster, change it everywhere, and start it together. This is inferred from the program's behavior and was not measured in this manual.

## Upgrading the program

Use `--version` to see the core's version:

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
```

The second line is the configuration file that is read when `--config` is not given. It is fixed at build time.

The upgrade steps:

1. Run `check` with the new program against the current configuration. If the new version rejects a configuration key, `check` points it out, so it does not fail only at start.
2. Replace one node at a time as in [Rolling restart of a cluster](#rolling-restart-of-a-cluster). For a single node, stop it, replace the program and start it.
3. After each node, look at `Version` in `status` and at the log.

> **Not verified on real hardware:** Nodes of different versions working together in one cluster. This manual had only one version to test, and the rolling upgrade steps are written from the program's start-up and join behavior. If a new version changes the format of the cluster data, read the notes for that version.

## Adding a migration

The SQL files in `cluster.migrations_dir` are applied in order at start, before the server begins serving. The rules:

- File names are `<integer>_<name>.sql`, starting at 1, with no gaps or repeats.
- Each file is one transaction. After it is applied, the file name and the SHA-256 of its content are recorded, and later starts skip it.
- **A file that has been applied cannot be edited or renamed.** To change a table, add a file with the next number.
- In a cluster, each file is applied only once, written through the cluster by the first node that starts and gets the leader; the other nodes receive it by replication.
- Each file runs for at most 30 seconds. If it takes longer, this start fails, the file keeps running to completion in the cluster, and the next start sees that it has been applied.
- `migrations_wait_ms` is the time limit for each of the three waits (for the leader, for applying, for catching up), so it must be longer than the slowest file.

### Add a file

```console
$ printf 'ALTER TABLE users ADD COLUMN note TEXT;\n' > migrations/3_user_note.sql
$ rradiusd check --config rradius.toml
configuration ok
```

`check` examines the file names and numbers (for a gap it prints the same message as in [Numbering gap](#numbering-gap) below, with exit code 2), but it does not apply anything and does not compare the content of applied files; migrations are applied only at start. So you must restart after you add a file: reload does not apply it (`[cluster]` needs a restart). After the restart the log has no special line; if applying fails, the start ends with exit code 2 and the message names the file.

In a cluster, put the new file in the `migrations_dir` of every node, then do a rolling restart. The first node to restart applies it. A node that has not yet received the new file will find, when it restarts, that the cluster has applied more files than its own directory has, warns, and serves as usual.

### Editing an applied file

```console
$ printf -- '-- edited\n' >> migrations/1_schema.sql
$ rradiusd serve --config rradius.toml
…
migration file '1_schema.sql' changed after it was applied
$ echo $?
2
```

Not even a comment line may be added. Change the file back to the original to start.

### Numbering gap

```console
$ mv migrations/3_user_note.sql migrations/5_user_note.sql
$ rradiusd serve --config rradius.toml
migration numbering must start at 1 without gaps or repeats: expected number 3, found '5_user_note.sql'
$ echo $?
2
```

File name and number errors are checked before the cluster node is started, so the program exits at once whether or not the cluster is ready; `check` also catches this kind of error. A changed content is found only when the program starts and reads the record of what has been applied.

## Accounts, sessions and queries

The `user`, `session` and `query` commands all talk to the running server through the cluster's API port:

- When the server is not running, they wait 30 seconds and exit with exit code 3.
- They read `[accounts]`, `[sessions]` and `[queries.<name>]` from the configuration file each time they run, so a change needs no reload.
- In a cluster, you can use the configuration of any node; writes are handed to the leader.

For the usage of each command: `user` is in [Chapter 07](07-users-and-chains.md), `session` in [Chapter 13](13-accounting.md), and `query` in [Chapter 09](09-external-databases.md).

## Changing secrets

Each kind of secret is changed in a different way. The key points are which ones take effect on reload, which need a restart, and whose requests fail during the change.

| Secret | How to change it | During the change |
| --- | --- | --- |
| Shared secret of a RADIUS client (`secret_file`) | Edit the file and send SIGHUP. Change it on the NAS too; the closer in time the two changes are, the better | While the two sides differ, requests from this NAS are discarded (`Discarded: invalid`). Each NAS has its own secret, so only one NAS is affected at a time. For the steps and measurements, see [Chapter 05](05-radius-clients.md) |
| Shared secret of a TACACS+ client | Same as above | The client list can be reloaded; open connections are also matched against the new list ([Chapter 12](12-tacacs.md)) |
| LDAP service account (`bind_password_file`) | First change the service account's password in the directory, then edit the file and send SIGHUP | While the directory has changed but rradius has not reloaded yet, LDAP authentication fails ([Chapter 08](08-ldap-and-ad.md)) |
| External database (`password_file` of `[databases.<name>]`) | First change the password in the database, then edit the file and **restart** | `[databases]` cannot be reloaded (see the table above). In a cluster use a rolling restart ([Chapter 09](09-external-databases.md)) |
| `secret_raft`, `secret_api` (or `_file`) | Must be the same on every node; change it for the whole cluster together | `[cluster]` needs a restart. Nodes with different secrets cannot connect, so schedule a maintenance window (inferred from the program's behavior, not verified on real hardware) |
| EAP certificate and private key | Replace the files and send SIGHUP | New conversations use the new ones and ones in progress are not affected; if the new files have an error, the reload fails and the old ones stay in use ([Chapter 11](11-eap.md)) |

To generate a new secret:

```console
$ (umask 077; openssl rand -hex 32 > nas.secret)
```

- Give only the account that runs rradius read permission on secret files (0600 or 0640). When the permissions are too wide, `check`, start and reload warn; see [Chapter 20](20-security.md) for details.
- A shared secret shorter than 16 bytes, or one that contains `change-me` (the public value in the examples), also gives a warning and does not block the start. Some old devices limit the length or the characters of the secret, so make sure the device accepts it before you change it.

## Purging old data

Tables such as `accounting` and `tacacs_log` keep growing, and rradius does not purge them automatically. You can delete periodically from the accounting script: at most once an hour, and only one request in the whole cluster actually does the work.

First use a migration to create a table that records the last run time. The following comes after quick-start's `1_schema.sql` ([Chapter 03](03-quick-start.md)) as the next migration number:

```sql
-- 3_housekeeping.sql
CREATE TABLE housekeeping (name TEXT PRIMARY KEY, last_run INTEGER NOT NULL);
INSERT INTO housekeeping VALUES ('purge-accounting', 0);
```

> **Note:** The number in the file name must follow your existing migrations, with no gap (see "Adding a migration" above). quick-start has `1_schema.sql` and `2_demo_users.sql`, so this one is `3_`; in an environment where the demo file has been deleted, the number depends on your case.

Then add this before `return 'accept'` in the accounting script:

```lua
-- At most once an hour: read the local copy first, so not every request writes to the cluster.
local now = math.floor(now_unix())
local due = sql_query("SELECT 1 FROM housekeeping WHERE name = 'purge-accounting' AND last_run < ?", {now - 3600})
-- Writes go through the cluster; only one request can change last_run, and it does the deletion.
if #due > 0 and sql_execute("UPDATE housekeeping SET last_run = ? WHERE name = 'purge-accounting' AND last_run < ?",
    {now, now - 3600}) == 1 then
  -- Delete at most 10000 rows at a time; leave the rest for the next hour, so one write does not take too long.
  -- ORDER BY makes every node delete the same rows.
  sql_execute([[DELETE FROM accounting WHERE rowid IN
    (SELECT rowid FROM accounting WHERE stopped IS NOT NULL AND stopped < ? ORDER BY rowid LIMIT 10000)]], {now - 180 * 86400})
end
```

How it works:

- `sql_query` reads the local copy. Most requests end at this step and do not write to the cluster.
- `sql_execute` returns the number of affected rows. Within the same hour, many requests see "due" at the same time, but only one can update `last_run` (it returns 1); the others return 0 and do not delete.
- `stopped IS NOT NULL` deletes only sessions that have ended and leaves open ones alone; `stopped < ?` keeps the most recent 180 days.
- `LIMIT 10000` limits the amount at one time, and `ORDER BY rowid` makes every cluster node delete the same rows. With a large backlog it takes several hours to clear.
- The same pattern can purge `tacacs_log` ([Chapter 12](12-tacacs.md)): change the condition to `logged < ?`, and add another `housekeeping` row that records its `name`.

Things to note:

- How long to keep data (180 days in the example) depends on law and company policy. A carrier may have a minimum retention period, so **check before you delete**.
- The deletion runs while one accounting request is being handled, so that request is answered a little later. The amount deleted must finish within `[lua] query_timeout_ms`, or `sql_execute` times out.
- If you need to keep historical data, move it away with an external export process first, then delete; for the accounting log, see [Chapter 13](13-accounting.md).

## Data directory and data protection

### What is in the data directory

Below `cluster.data_dir` everything is managed by hiqlite. What you actually see is:

```text
data/
├── logs/                    Raft log (*.wal, meta.hql, lock.hql)
└── state_machine/
    ├── db/hiqlite.db        the database itself (with -wal, -shm)
    ├── snapshots/           snapshots
    ├── backups/             (not used by the core)
    └── lock                 lock while running
```

- Do not change the files here while the server runs, and do not write to the database with other programs. All writes must go through the cluster, or the data on the nodes becomes inconsistent.
- The server removes `lock` when it exits normally. If the next start finds `lock` still there, it knows the last exit was not normal and rebuilds the database from the Raft log.
- To start from scratch (only for a single-node test environment): stop the server, delete the whole `data_dir`, and the next start creates it again and applies the migrations again.

### The core has no backup feature

The core has no built-in backup: there is no setting that copies the database out on a schedule, and no restore command. The ways the core can protect data, as far as you can see in the code, are these:

| Method | What it protects against | Notes |
| --- | --- | --- |
| Cluster replication | One host failing | Every node has a full copy. A three-node cluster can lose one node without losing data ([Chapter 16](16-cluster.md)) |
| Rebuild from other nodes | A node's disk or data directory failing | Stop it, empty `data_dir`, and start it with the original configuration; it copies the data back from the other nodes. Measured in Chapter 16 |
| Automatic rebuild after an abnormal exit | Power loss, forced kill | The next start rebuilds the local database from the Raft log |
| Accounting spool | Accounting when the database cannot be written | Accounting is first written to local disk and replayed later ([Chapter 13](13-accounting.md)) |
| Accounting log | Keeping every accounting request separately | `[radius.accounting_log]` writes every accounting request as one JSON line; you can rotate it with logrotate and move it elsewhere ([Chapter 13](13-accounting.md)) |
| External database | Keeping data in a database you manage yourself | Authentication and scripts can use PostgreSQL or MySQL in `[databases]` ([Chapter 09](09-external-databases.md)); those databases use their own backup methods |
| Configuration and migrations in version control | Rebuilding the whole environment | The table structure and demo data are all in `migrations/`; with the configuration file you can rebuild an empty cluster |

None of these protects against a wrong write: an account deleted by mistake is replicated to every node at once. If the cluster loses the data of a majority of its nodes at the same time, the data is gone.

> **Enterprise edition:** The enterprise edition has cluster backup and restore.

> **Note:** Copying `data_dir` directly after a stop as a backup has not been verified by the code or the tests, and this manual does not treat it as a backup procedure.

## Logs and logrotate

### The server's logs

The server writes its log to standard error, one entry per line, with time and level. It does not write a log file itself:

- Under systemd, the log goes to the journal: `journalctl -u rradius`.
- In a container, read it with `docker logs` or `kubectl logs`.
- The level, the format (`text` or `json`) and the authentication record are set by `[log]`, and a change needs a restart; to trace one user temporarily use `log.trace`, which takes effect on reload. All of this is explained in [Chapter 17](17-monitoring.md).

### Rotating the accounting log

The file of `[radius.accounting_log]` keeps growing, so rotate it. The server reopens the file when it receives SIGHUP: if the file was moved away, it creates a new one. Measured:

```console
$ mv acct.jsonl acct.jsonl.1
$ kill -HUP <pid>
$ rradius-bench run … --load acct-start,concurrency=1 --requests 1
$ wc -l acct.jsonl*
       1 acct.jsonl
       2 acct.jsonl.1
```

Accounting after the SIGHUP goes to the new `acct.jsonl`, and the old file no longer grows.

The `/etc/logrotate.d/rradius` that comes with the package (`packages/common/rradius.logrotate` in the source) does exactly this:

```text
/var/log/rradius/*.jsonl {
    daily
    rotate 30
    missingok
    notifempty
    nocreate
    compress
    delaycompress
    postrotate
        systemctl kill -s HUP rradius.service >/dev/null 2>&1 || true
    endscript
}
```

- It rotates daily, keeps 30 copies, and compresses old files except the most recent one.
- `nocreate`: logrotate does not create the new file; the server creates it itself.
- It sends SIGHUP after rotation. This also reloads the configuration; when the configuration has not changed, it is just a reload with no change.
- Do not use `copytruncate`: lines written between the copy and the truncation are lost.
- It handles only `*.jsonl` under `/var/log/rradius/`, so put the accounting log there, for example `file = "/var/log/rradius/accounting.jsonl"`.

## Managing with systemd

The key points of the unit that comes with the package (`packages/common/rradius.service` in the source):

| Setting | Value | Meaning |
| --- | --- | --- |
| `ConditionPathExists` | `/etc/rradius/rradius.toml` | Do not start without a configuration file |
| `User`, `Group` | `rradius` | Run as an ordinary user |
| `ExecStart` | `/usr/bin/rradiusd serve` | Reads the default configuration file |
| `ExecReload` | `/bin/kill -HUP $MAINPID` | `systemctl reload rradius` is a reload |
| `Restart`, `RestartSec` | `on-failure`, `2` | When the exit code is not 0, restart after 2 seconds |
| `TimeoutStopSec` | `60` | Time to wait for shutdown |
| `AmbientCapabilities` | `CAP_NET_BIND_SERVICE` | Can listen on ports below 1024, for example TACACS+ port 49 |
| `ProtectSystem`, `ReadWritePaths` | `strict`, `/var/lib/rradius /var/log/rradius` | Can write only these two directories |
| `LimitNOFILE` | `65536` | Upper limit of file descriptors |

So:

- Put `data_dir` and the accounting spool under `/var/lib/rradius` and the accounting log under `/var/log/rradius`, or they cannot be written.
- `Restart=on-failure` catches the cases where the server exits by itself with exit code 1, for example when the Raft core of a cluster node stops ([Chapter 16](16-cluster.md)). Exit code 2 for a configuration error also triggers a restart, but the restart fails the same way; run `check` after you change the configuration.
- Commonly used commands: `systemctl start|stop|restart|reload rradius`, `systemctl status rradius`, `journalctl -u rradius -f`.

> **Not verified on real hardware:** This manual did not run this unit under systemd. The values in the table are copied from the file, and the behavior is inferred from systemd's general rules and from this chapter's measurements.

## On Docker Compose and Synology

The source's `compose.yaml` (single node), `compose.cluster.yaml` (three nodes, [Chapter 16](16-cluster.md)) and `deploy/synology/compose.yaml` (a Container Manager project on Synology) all use the same image:

- The program runs as `ENTRYPOINT ["/usr/local/bin/rradiusd"]` with the default arguments `serve --config /etc/rradius/rradius.toml`, as the `rradius` user; the data is in the volume `/var/lib/rradius`.
- `stop_grace_period: 30s`: gives shutdown enough time. Docker's default of 10 seconds is not enough.
- `restart: unless-stopped`: restarts automatically after an abnormal exit.
- The root file system is read-only, and the configuration is mounted read-only at `/etc/rradius`.

Day-to-day operations:

| Task | Command |
| --- | --- |
| reload | `docker compose kill -s HUP <service>` |
| Restart | `docker compose restart <service>` |
| Read the log | `docker compose logs -f <service>` |
| Check status | `docker compose exec <service> rradiusd status --config <config file>` |
| Add an account | `docker compose exec -T <service> rradiusd user add --config <config file> <account>`, giving the password on standard input |

The project directory on Synology is `/volume1/docker/rradius/`: `compose.yaml`, `config/` (copied from `docker/config/` in the source, edit `rradius.toml`), and `secrets/` (three secret files). The configuration is mounted read-only, so to change it, edit the files in `config/` on the NAS, then send SIGHUP to the container or restart the project. The data is in Docker's named volume `data`, not in a folder next to `config/`; when you delete the project, take care not to delete the volume with it.

Daily work on Synology:

| Task | How |
| --- | --- |
| Read the log | The "Log" tab of the project in Container Manager, or SSH in and run `sudo docker compose -p rradius logs` |
| Change the configuration | Edit the files in `config/`, then restart the project. With a configuration error the container exits at once and the log gives the reason; it is safer to run `rradiusd check` on another computer first |
| Update the version | Build the new image on another computer, import it to the NAS, and restart the project (for building and importing see [Chapter 02](02-install.md#synology-nas)). The data is in the `data` volume and does not disappear because of an update |
| Stop | "Stop" on the project page. `stop_grace_period` is 30 seconds, which gives the database time to close; do not use a forced stop, or the next start has to rebuild the local database |
| Back up | Back up the `config/` and `secrets/` folders. The data is in a volume managed by Docker, named `<project name>_data` (`rradius_data` when the project is called `rradius`): stop the project first, then back up the folder that holds that volume with Hyper Backup, or use `docker run --rm -v rradius_data:/d -v $PWD:/b alpine tar czf /b/data.tgz -C /d .` |

For the backup methods and their limits, see [Data directory and data protection](#data-directory-and-data-protection). The location of volume folders on DSM and the use of Hyper Backup were not verified in this manual.

> **Not verified on real hardware:** This manual did not build the image and did not run anything on Docker or Synology. The commands in the table above are the general use of Docker Compose, matched to the signal behavior measured locally in this chapter.

## On Kubernetes

How to operate the source's `deploy/kubernetes/` example ([Chapter 16](16-cluster.md)):

- **Changing configuration:** The configuration comes from a ConfigMap generated by kustomize. When the content changes, the ConfigMap name changes (`rradius-config-<hash>`), the StatefulSet is updated with it, and the pods restart one by one according to `RollingUpdate`. That is, changing configuration on Kubernetes is a rolling restart, not a reload. Before you change it, run `check` locally against the files in `config/`.
- **Adding a migration:** Besides putting the file in `config/`, add a line to `files` in `kustomization.yaml` and a line to `items` in `statefulset.yaml`, to mount it under `migrations/`. If you miss the latter, the file does not appear in the pod's `migrations/`.
- **Touch one pod at a time:** The PodDisruptionBudget `maxUnavailable: 1` makes operations such as draining a Kubernetes node stop only one pod at a time.
- **Running commands:** `kubectl exec -i rradius-0 -- rradiusd user add --config /etc/rradius/rradius-0.toml alice`.
- **Reading the log:** `kubectl logs rradius-0`.
- **Data:** One PVC per pod (`data-rradius-N`). Rebuilding a node ([Chapter 16](16-cluster.md)) here means deleting that pod's PVC and the pod, and letting the StatefulSet recreate it with an empty volume.
- **Upgrading:** Change the image version and then run `kubectl apply -k deploy/kubernetes`; again one pod at a time. The StatefulSet goes from `rradius-2` down to `rradius-0` and does not choose the leader to be last for you; when the leader is replaced there is a write gap during the election (for the number of seconds see [Chapter 16](16-cluster.md#across-data-centers)), and accounting goes to the spool.
- **Node maintenance:** The PodDisruptionBudget makes voluntary disruptions such as `kubectl drain` stop only one pod at a time. **Do not run `kubectl delete pod --all`:** with all three restarting together, the cluster temporarily loses its majority.
- **It cannot scale automatically, and `replicas` is not a knob for the number of nodes.** The cluster members are written in each pod's configuration file, so the number of pods must equal the number of `[[cluster.nodes]]`, and also the number of `rradius-N.toml` files in `kustomization.yaml` and `items` (a pod without that file cannot read its configuration). So do not set an HPA.
  - `kubectl scale --replicas=1` is the same as stopping two nodes, and the behavior of the remaining one is what [Chapter 16](16-cluster.md) says about stopping two. When you scale back to 3, the returning pods may each restart once before they become ready: a node with `stop_without_leader_ms` set exits with exit code 1 when it cannot find a leader at start (message `cluster was not ready to apply migrations`), Kubernetes restarts it, and the second time it joins.
  - To change the number of nodes, change every configuration, `kustomization.yaml` and `items` together; for how to add and remove nodes in the cluster itself see [Adding a node](16-cluster.md#add-a-node) and [Removing a node](16-cluster.md#remove-a-node). This combination was not tried on Kubernetes in this manual.
- **Limiting who can read the metrics:** The metrics port has no authentication and no TLS, so when it is bound to `0.0.0.0` everyone in the cluster can read it. `networkpolicy.yaml` opens only the RADIUS port to the outside, the cluster port only to other rradius pods, and the metrics port only to the `monitoring` namespace (change it to the namespace where Prometheus runs). To use it, add it to `resources` in `kustomization.yaml`. The kubelet's probes come from the node itself and most network plugins allow them, but **check after you apply it that the pods are still ready.** Pods that use `hostNetwork` are not governed by a NetworkPolicy ([Chapter 16](16-cluster.md#without-a-suitable-load-balancer-hostnetwork)).

> **Not verified on real hardware:** None of the operations above on Kubernetes were actually run. This manual only used `kubectl kustomize` to confirm that the example produces resources, and ran `check` on the three configurations.

## Monitoring: the metrics to watch first

The full metrics list and the alert rules are in [Chapter 17](17-monitoring.md). For day-to-day operations, connect the few below first; each corresponds to a situation where you need to act:

| Metric | Means | When it increases or looks wrong |
| --- | --- | --- |
| Rate of increase of `rradius_radius_responses_total{code="access_reject"}` | Logins are rejected | A sudden increase: someone is trying passwords, the directory is down, or the configuration is wrong. Look at `handler` in the authentication record |
| `rradius_radius_discarded_total{reason="unknown_client"}` | The source is not in the client list | A new device has not been added yet, or someone is scanning |
| `rradius_radius_discarded_total{reason="invalid"}` | Wrong signature or format | Usually the shared secret of one device is wrong |
| `rradius_radius_discarded_total{reason="overload"}` | The number of requests handled at once reached the limit | Investigate if above 0: the database, directory or script has become slow, see [Chapter 21](21-troubleshooting.md) |
| Rate of increase of `rradius_radius_duplicates_total` | The NAS is retransmitting | Handling is slower than the NAS's retransmit interval |
| `rradius_cluster_leader` | Whether this node has a leader to write to | 0 for more than a few seconds: the node is isolated, or the cluster has no majority ([Chapter 16](16-cluster.md)) |
| `rradius_acct_spool_bytes` | Accounting in the spool not yet written to the database | Stays above 0: the database or the cluster has a problem |
| `rradius_acct_spool_rejected_total` | Accounting given up on during replay | Above 0: look at `rejected.log` in the spool directory |
| `rradius_accounting_log_dropped_total`, `rradius_accounting_log_failures_total` | Accounting that was not written to the accounting log | The disk is too slow, full or not writable |
| `rradius_reload_total{result="error"}` | The number of failed reloads | Above 0: read the log, fix it, and send SIGHUP again |
| `rradius_log_dropped_lines_total` | Log lines dropped because they could not be written in time | The destination of the log is too slow ([Chapter 17](17-monitoring.md)) |
| p99 of `rradius_radius_response_seconds`, `rradius_tacacs_response_seconds` | How long responses take | It grows: the database, directory or script has become slow |
| `rradius_tacacs_connections_total{result="refused"}` | TACACS+ connections refused because of the connection limit | It increases: `max_connections` or `max_connections_per_source` was reached ([Chapter 12](12-tacacs.md)) |

- Counters accumulate from start, each node counts its own, and reload does not reset them. Alert on the rate of increase, not on the absolute value.
- In an environment with external databases, named queries or EAP, also add the `rradius_external_db_*`, `rradius_query_*` and `rradius_eap_sessions_total` groups; they are explained in Chapters 09, 11 and 17.
- Without Prometheus, run `rradiusd status --config rradius.toml --watch` on the node to list the same data every second ([Chapter 17](17-monitoring.md)).
- Status-Server is the most accurate test of "alive or not", because it takes the same path as authentication: `rradius-bench run --load status-auth`, or `radclient` with Message-Authenticator ([Chapter 05](05-radius-clients.md#status-server)).
- The metrics port has no authentication and no TLS; bind it only to an internal address.

## Pre-launch checklist

This is the integrated operations checklist. The security items are written one by one in [Chapter 20, "Checklist"](20-security.md#checklist); this list only points there and does not repeat them.

- [ ] `rradiusd check --config …` passes, and gives no warnings (secret file permissions, shared secrets that are too short or contain `change-me`, `unencrypted`, and so on; see Chapter 20).
- [ ] All `*.secret` files are newly generated random values, not the ones that come with the examples (see "Changing secrets" above).
- [ ] Certificates are not the demo ones; someone watches the expiry dates of the server certificate and of the NAS and device certificates.
- [ ] The demo data files in `migrations/` (for example `2_demo_users.sql`) have been deleted or replaced with real data, before the first start, because an applied file cannot be changed.
- [ ] `[[radius.clients]]` and `[[tacacs.clients]]` list only devices that really will send requests, and do not use `0.0.0.0/0`.
- [ ] `[metrics]` is bound only to an internal address; with several nodes, `[cluster] listen` is bound to an internal network or a WireGuard address.
- [ ] The firewall opens only the ports in use and limits their sources: 1812 and 1813 (UDP), 49 (TACACS+), and the cluster ports only between nodes.
- [ ] You have run `rradiusd tuning --config …` and set the receive buffer and the file descriptor limit as it recommends ([Chapter 19](19-performance.md)).
- [ ] The server is managed by systemd, Compose or Kubernetes, which send SIGTERM on stop (not SIGKILL) and wait long enough; `data_dir` and the accounting spool are on persistent local disk (see "How long shutdown takes" and "Managing with systemd" above).
- [ ] Automatic restart is set (systemd `Restart=on-failure`, Compose `restart: unless-stopped`): when a node's database breaks, the server exits with exit code 1 and relies on the restart to recover.
- [ ] After start, look at the log: every port that should be open has a `listening` line: `RADIUS Server listening on …`, `TACACS+ Server listening on …`, `Metrics listening on …`. If a RADIUS or TACACS+ port cannot be bound, the server exits with exit code 1; if only the metrics port cannot be bound, it logs one error line and the process keeps running, so you must see `Metrics listening on …`.
- [ ] Try one login and one accounting request from each kind of real device (not only test tools).
- [ ] The accounting tables have a way to purge old data ("Purging old data" above), and the retention period is confirmed.
- [ ] Monitoring at least covers "alive" (Status-Server or `/livez`) and "the metrics to watch first" above; log rotation is set ("Logs and logrotate").

## What requires downtime

| Change | Single node | Three-node cluster |
| --- | --- | --- |
| Scripts, auth chain, clients, shared secrets | No, reload | No, reload on every node |
| Accounts, sessions (`user` command, tables) | No | No |
| Ports, listen addresses, `[metrics]`, `[log]`, `dictionary` and other settings that need a restart | Yes, no service during the restart | No, rolling restart |
| Adding a migration | Yes, restart | No, rolling restart |
| Upgrading the program | Yes | No, rolling restart (not verified on real hardware) |
| Adding or removing a node | — | No ([Chapter 16](16-cluster.md)) |
| Changing `secret_raft`, `secret_api` | Yes | Yes, the whole cluster together (inferred from the program's behavior) |
| Changing an applied migration | Not allowed, use a new file | Not allowed, use a new file |

## Common mistakes

**Symptom:** After a reload the log says `these need a restart to apply: radius.auth_port`, and the server is still on the old port.
**Cause:** This setting needs a restart to take effect.
**Fix:** Find a time to restart (use a rolling restart for a cluster). The other changes are already in effect.

**Symptom:** The log says `reload failed, keeping the running configuration: …`.
**Cause:** The new configuration has an error, and the server keeps serving as before.
**Fix:** Fix it at the file and line the message names, and send SIGHUP again after `check` passes.

**Symptom:** You changed the configuration but the behavior did not change, and the log has no reload entry.
**Cause:** SIGHUP was not sent; or in a cluster only one node was reloaded. Included files are also reread only on reload.
**Fix:** Send SIGHUP to every node and look at the log of every node.

**Symptom:** At start it prints `migration file '1_schema.sql' changed after it was applied`, exit code 2.
**Cause:** An applied migration file was changed.
**Fix:** Change the file back to the original, and write the change you need as a new file with the next number.

**Symptom:** At start it prints `migration numbering must start at 1 without gaps or repeats: expected number 3, found '5_user_note.sql'`, exit code 2.
**Cause:** The numbers in the file names have a gap or a repeat.
**Fix:** Number the files in order.

**Symptom:** The server exits at start with a message that mentions `migrations_wait_ms`, exit code 1.
**Cause:** Within the time limit it could not find a leader, could not finish applying migrations, or the local copy could not catch up.
**Fix:** Make sure a majority of the nodes are running; if a file itself is slow, increase `migrations_wait_ms`.

**Symptom:** After a stop, the log of the next start says `Node did not shut down gracefully - auto-rebuilding State Machine`.
**Cause:** The last stop was forced, for example SIGKILL because the wait was too short.
**Fix:** Increase the wait of the process manager (`TimeoutStopSec` in systemd, `stop_grace_period` in Compose, `terminationGracePeriodSeconds` in Kubernetes).

**Symptom:** After logrotate rotates, the accounting log stops growing, or the new file does not appear.
**Cause:** SIGHUP was not sent after the rotation, and the server is still writing to the file that was moved.
**Fix:** Send SIGHUP in `postrotate`, as the configuration that comes with the package does; do not use `copytruncate`.

**Symptom:** `user add` waited 30 seconds and printed `the cluster did not answer within 30 s`, exit code 3.
**Cause:** The server is not running, or the cluster has no leader.
**Fix:** Start the server first; for a cluster, first bring back a majority of the nodes.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [13 Accounting and sessions](13-accounting.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [21 Troubleshooting](21-troubleshooting.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix B: Command reference](91-command-reference.md)
