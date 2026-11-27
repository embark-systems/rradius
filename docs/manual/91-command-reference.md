# 91 Appendix B: Command reference

> After this chapter you can:
> - Look up the syntax, flags, output and exit codes of every `rradiusd` subcommand
> - Look up every flag and default of `rradius-bench run` and `calibrate`, and every kind of `--load`
> - Judge the result by exit code in a script
> - Recognize the commands that exist only in the enterprise edition, and know how the core reacts to them
> - Compare this reference with the output of `help` after the program is updated

Prerequisites: you have read [03 First start and first login](03-quick-start.md) and can start the quick-start scenario.

In this chapter:

- [How to read this reference](#how-to-read-this-reference)
- [Environment used for verification](#environment-used-for-verification)
- [Command overview](#command-overview)
- [Exit code summary](#exit-code-summary)
- [serve](#serve)
- [check](#check)
- [help, --version](#help---version)
- [schema](#schema)
- [config](#config)
- [tuning](#tuning)
- [status](#status)
- [user](#user)
- [session](#session)
- [query](#query)
- [cluster](#cluster)
- [Old flag syntax](#old-flag-syntax)
- [Enterprise-only commands](#enterprise-only-commands)
- [rradius-bench run](#rradius-bench-run)
- [--load syntax](#--load-syntax)
- [run output and exit codes](#run-output-and-exit-codes)
- [rradius-bench calibrate](#rradius-bench-calibrate)
- [Comparison method](#comparison-method)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

---

## How to read this reference

The content of this appendix comes from measured output of two executables, checked against the command-line parsing code:

- `rradiusd`: `rradius-core/src/cli.rs`, and the `status.rs`, `config_tools.rs`, `tuning.rs`, `accounts.rs`, `sessions.rs`, `query_command.rs`, `cluster_commands.rs` and `admin.rs` of the subcommands
- `rradius-bench`: `rradius-bench/src/cli.rs`, `main.rs`, `run.rs`, `report.rs`, `calibrate.rs`

Every command in this appendix was actually run. The output in the examples is copied as it was, except that machine-specific paths are shortened to `…`. A few exit-code paths were checked only against the code; they are listed under the [Exit code summary](#exit-code-summary). Numbers such as rates and latencies differ from machine to machine; do not use them as performance references.

Rules that all commands share:

- **`--config <path>`:** when it is not given, the command reads `/etc/rradius/rradius.toml`. The actual default path is decided at build time; the second line of `rradiusd --version` prints it. When `serve` or `status` cannot read the default configuration file, the message has one more line, `(the default configuration; give --config <path> for another)`.
- **Position of flags:** the flags of `status`, `user`, `session` and `cluster` can be placed anywhere after the subcommand; `user show --config a.toml dave` and `user show dave --config a.toml` are the same. The other commands need the order shown in the usage: for example `config --config a.toml show` and `query --json list` are usage errors. Giving the same flag twice is also a usage error.
- **Output:** results go to standard output; errors and usage text go to standard error. Administration commands write only `WARN` and above of the log to standard error, so `check` prints warnings such as secret file permissions.
- **Subcommands have no `--help` of their own.** When the arguments are wrong, the command prints its own usage and exits with code 2; use `rradiusd help` for the full usage list.

## Environment used for verification

The examples were all run in a copy of the quick-start scenario. So that one configuration can exercise EAP, TACACS+ and named queries, the copy makes these changes beyond quick-start:

```toml
# rradius.toml (only the parts that differ from quick-start)
# … (other settings omitted)

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"
listen = "127.0.0.1"

[[cluster.nodes]]
id = 1
raft = "127.0.0.1:8100"
api = "127.0.0.1:8200"

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
status_server = true

[radius.eap]
cert_file = "certs/server.pem"
key_file = "certs/server.key"
client_ca_file = "certs/ca.pem"
methods = ["tls", "peap", "ttls"]

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
eap_methods = ["tls", "peap", "ttls"]

[tacacs]
bind = "127.0.0.1"
port = 4949

[[tacacs.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "tacacs.secret"

[metrics]
bind = "127.0.0.1"
port = 9812

[queries.enabled_users]
help = "Accounts that can log in"
sql = "SELECT enabled, COUNT(*) AS value FROM users GROUP BY enabled"
labels = ["enabled"]
interval_ms = 60000
```

- Ports use the defaults: RADIUS 1812 and 1813, metrics 9812, cluster 8100 and 8200. TACACS+ uses 4949, as in [12 TACACS+ device administration](12-tacacs.md), because the standard port 49 is below 1024 and an ordinary account cannot listen on it.
- `[accounts]` is the same as the one added in [03 First start and first login](03-quick-start.md).
- The certificates were generated into this directory with the `setup.sh` of the wifi-certs scenario: `sh <repo>/rradius-server/examples/scenarios/wifi-certs/setup.sh .`. The name of the server certificate is `radius.corp.example.com`.
- `tacacs.secret`, like the other `.secret` files, was generated with `openssl rand -hex 32` and has permissions 0600.

This configuration passes `check`; for the meaning of each section, see [Appendix A: Configuration reference](90-config-reference.md).

## Command overview

The `rradiusd` commands, in the order of `rradiusd help`:

| Command | What it does | Server must be running | Main chapter |
| --- | --- | --- | --- |
| `serve` | Starts the server and runs until it receives a stop signal | — | [03](03-quick-start.md) |
| `check` | Checks the configuration without opening any port | No | [04](04-configuration.md) |
| `status` | Reads the metrics port and condenses it into one page | Yes (reads `[metrics]`) | [17](17-monitoring.md) |
| `--version` | Prints the version and the default configuration path | No | This chapter |
| `schema` | Prints the JSON Schema of the configuration file | No | [04](04-configuration.md) |
| `config show` | Prints the full configuration with defaults filled in | No | [04](04-configuration.md) |
| `config diff` | Compares two configurations | No | [04](04-configuration.md) |
| `config init` | Generates a configuration draft | No | [04](04-configuration.md) |
| `tuning` | Gives system tuning advice for this machine and configuration | No | [19](19-performance.md) |
| `user add`, `user passwd`, `user enable`, `user disable`, `user delete`, `user show` | Manages accounts with the SQL in `[accounts]` | Yes (cluster API port) | [07](07-users-and-chains.md) |
| `session list`, `session show` | Lists online sessions with the SQL in `[sessions]` | Yes (cluster API port) | [13](13-accounting.md) |
| `accounting find` | Looks up, from the accounting log file, who held a public address and port at a given time | No (reads files only) | [13](13-accounting.md) |
| `query list`, `query <name>` | Lists or runs `[queries.<name>]` | `list` no; running yes | [09](09-external-databases.md) |
| `cluster members`, `remove` | Lists the cluster members, removes a node | Yes (cluster API port) | [16](16-cluster.md) |
| `help` | Prints the usage of all commands | No | This chapter |

The `rradius-bench` commands:

| Command | What it does | Main chapter |
| --- | --- | --- |
| `run` | Sends load to a server and reports rate and latency; with `--requests 1` it also works as a login test | [19](19-performance.md) |
| `calibrate` | Sends load to a responder inside its own process, to measure the limit of this tool on this machine | [19](19-performance.md) |
| `help` | Prints the command list, or the help of one command | This chapter |

## Exit code summary

The convention of `rradiusd` is: 0 for success, 2 for a usage or configuration error. Other values depend on the command:

| Command | 0 | 1 | 2 | 3 |
| --- | --- | --- | --- | --- |
| `serve` | Stopped normally after SIGTERM or SIGINT, including a stop requested during startup | Cannot serve after starting: the RADIUS port cannot be listened on, a cluster error aborted a migration, there is no leader and `stop_without_leader_ms` is set, the Raft core stopped, the node has been removed from the cluster (`has been removed from the cluster`) | Error in the configuration, dictionary or script, a migration file breaks the rules or its SQL fails, the cluster node cannot start (`cannot start cluster node …`), wrong arguments | — |
| `check` | `configuration ok` | — | Error in the configuration | — |
| `help`, `--version`, no arguments | Prints the result | — | Unknown command | — |
| `schema` | Prints the schema | Writing failed, for example the pipe was closed | Wrong arguments | — |
| `config show`, `init` | Prints the result | — | Wrong arguments or error in the configuration | — |
| `config diff` | No differences, or only node-specific differences | There are differences | Wrong arguments or error in a configuration | — |
| `tuning` | Prints the advice, including when the default configuration file cannot be read and when the platform has no advice | — | Wrong arguments, the file given to `--config` cannot be read | — |
| `status` | Prints the result; `--watch` ended by SIGINT or SIGTERM | — | Wrong arguments, no `[metrics]`, the configuration cannot be read, the cluster members cannot be asked | The metrics port cannot be read; with `--cluster`, no member can be read |
| `user` | Success | No matching account, SQL failed | Wrong arguments or error in the configuration, `[accounts]` lacks this action, empty password or the two passwords differ | The cluster cannot be reached, or did not answer within 30 seconds |
| `session` | There is data | There are no sessions | Wrong arguments or error in the configuration, `[sessions]` lacks this action, SQL failed | Same as `user` |
| `query` | Success, including no queries and no rows | — | Wrong arguments or error in the configuration, `queries.<name>` is not set, SQL failed | Same as `user` |
| `accounting find` | A hold was listed | No hold for that address, port and time | Wrong arguments, an address or time that is not valid, a file that cannot be read | — |
| `cluster` | Success | `remove`: the node is not in the cluster, the leader refused, the leader changed twice in a row | Wrong arguments or error in the configuration, wrong API secret, the node to remove still answers, only one or two voters would remain without `--force`, the last voter | No answer within 30 seconds; `remove` got no response after it was sent |
| Enterprise commands | — | — | Always 2 | — |

> **Not verified on real hardware:** these cells of the table were checked only against the code and not reproduced: the 0 of `serve` stopped during startup; the 1 of `serve` for a cluster error aborting a migration, `stop_without_leader_ms` and the Raft core stopping; the 2 of `serve` when the cluster node cannot start; the 0 of `tuning` on a platform without advice; the 2 of `status --cluster` when the members cannot be asked; the 2 of `query` for an SQL failure and the 0 of `query list` when there are no queries; the 2 of `user` for two different passwords; and every cell of `cluster` that needs several nodes (see [cluster](#cluster)). All other cells were tested.

`rradius-bench` has a separate set, see [run output and exit codes](#run-output-and-exit-codes): 0 all Accept, 1 some Reject or Challenge, 2 some timeouts, invalid responses or socket errors, 3 wrong arguments or could not start.

> **Note:** for the same "nothing found" case, the exit code of `session` is 1 but that of `query` is 0 (it prints `no rows`). To tell in a script whether `query` returned data, look at the output, not the exit code.

---

## serve

```text
rradiusd serve [--config <path>]
```

Starts the server: reads the configuration, starts the cluster node, applies migrations, opens the RADIUS, TACACS+ and metrics ports, and then keeps running. For the startup sequence and the log, see [03 First start and first login](03-quick-start.md).

While it runs, you can send these signals:

| Signal | Effect |
| --- | --- |
| SIGHUP | Reloads the configuration. Log: `configuration reloaded from rradius.toml`; keys that take effect only after a restart are listed with it. A SIGHUP received during startup is handled after the server starts serving |
| SIGTERM, SIGINT (Ctrl+C) | Stops: `/readyz` answers 503 first, no new requests are accepted, requests in progress are awaited for at most the time set in `[shutdown]`, then the database is closed |
| Another SIGTERM or SIGINT during the stop | Stops waiting for requests in progress |

```console
$ rradiusd serve --config rradius.toml
…
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
```

Examples of exit codes (all tested):

```console
$ rradiusd serve --config bindfail.toml      # the RADIUS port is held by another process
…
RADIUS server error: cannot listen on 127.0.0.1:1812: Address already in use (os error 48)
$ echo $?
1
$ rradiusd serve --config badmig.toml        # the SQL in a migration file is wrong
migration file '1_bad.sql' could not be applied: Sqlite: near "TABL": syntax error in CREATE TABL oops (x);
 at offset 7
$ echo $?
2
$ rradiusd serve --config rradius.toml extra
usage: rradiusd serve [--config <path>]
$ echo $?
2
```

- When the port is in use, the cluster node has already started, and the server closes the database cleanly before it exits, so the exit code is 1, not 2. The number of the error code depends on the operating system.
- A normal stop has exit code 0 (tested: after sending SIGTERM, `wait` returns 0).
- When the Raft core stops, the log says `its Raft core stopped (…); exiting so that a restart recovers it` and the exit code is 1, leaving systemd or the container runtime to restart it. This case was not reproduced.

## check

```text
rradiusd check [--config <path>]
```

Does the checks that startup does, but opens no port, creates no data directory and connects to nothing: it reads the configuration file and the files named by `include`, the dictionary, compiles the Lua script, checks the file names in the migrations directory, the auth chain, `[lua.http]` and the accounting spool directory, and finally prints warnings such as a shared secret that is too short.

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd check --config ent.toml           # has a [proxy] that the core does not support
ent.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ echo $?
2
```

- On success it prints `configuration ok` (standard output) and exits with 0; on an error it prints the reason (standard error) and exits with 2.
- `check` checks only the **file names** of migration files; it does not run the SQL. The syntax error in `1_bad.sql` in the previous section passes `check`; only `serve` finds it.
- An extra argument after `check`, for example `check --config a.toml extra`, prints `usage: rradiusd check [--config <path>]` and exits with 2.

## help, --version

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
$ rradiusd help
usage: rradiusd serve [--config <path>]
usage: rradiusd check [--config <path>]
usage: rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]
usage: rradiusd --version
usage: rradiusd schema [--lang en|zh-TW]
usage: rradiusd config show [--config <path>]
usage: rradiusd config diff <a.toml> <b.toml>
usage: rradiusd config init [--full] [--lang en|zh-TW]
usage: rradiusd tuning [--config <path>]
usage: rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
  add and passwd read the password from standard input
usage: rradiusd session list [--config <path>] [--json]
usage: rradiusd session show [--config <path>] [--json] <name>
usage: rradiusd query list [--config <path>] [--json]
usage: rradiusd query <name> [--config <path>] [--json]
usage: rradiusd cluster members|remove [--config <path>] [--force] [<node_id>]
  remove takes a node id, and --force to leave one or two voters
usage: rradiusd accounting find --ip <address> --port <port> --at <time> [--file <path>]... [--json]
  --file - or no --file reads standard input; --at is RFC 3339, such as 2026-11-25T10:00:00Z
without --config, the configuration is /etc/rradius/rradius.toml
```

- `help`, `--help`, `-h` and no arguments at all all print this list (standard output) and exit with 0.
- An unknown command, for example `rradiusd bogus`, prints the same list to standard error and exits with 2.
- `--version` can only be used alone.

## schema

```text
rradiusd schema [--lang en|zh-TW]
```

Prints the JSON Schema of the configuration file to standard output; the descriptions use the language given by `--lang`, default `en`. An editor can use it for completion and checking; Appendix A was also compiled from it.

```console
$ rradiusd schema --lang zh-TW > schema.json
$ python3 -c 'import json; print(sorted(json.load(open("schema.json"))["properties"]))'
['accounts', 'auth', 'cluster', 'databases', 'dictionary', 'include', 'log', 'lua', 'metrics', 'queries', 'radius', 'runtime', 'sessions', 'shutdown', 'tacacs']
$ rradiusd schema --lang fr
usage: rradiusd schema [--lang en|zh-TW]
```

- An unknown language or an extra argument: exit code 2.
- When writing fails, it prints `schema: …` and exits with 1. For example `rradiusd schema | head -1` shows `schema: Broken pipe (os error 32)` on standard error; this is caused by `head` closing the pipe early, not by a problem with the schema.

## config

```text
rradiusd config show [--config <path>]
rradiusd config diff <a.toml> <b.toml>
rradiusd config init [--full] [--lang en|zh-TW]
```

Only `init` accepts `--lang`; giving `--lang` to `show` or `diff` is a usage error. None of the three subcommands connects to the server. For details of use, see [04 Configuration file basics](04-configuration.md).

### config show

Prints the configuration with all defaults filled in (including the files named by `include`). Secrets are shown as `<hidden>`; the comment on the first line lists the values computed for this machine.

```console
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
dictionary = "…/share/dictionary"

[lua]
script = "accounting.lua"
max_execution_ms = 1000
max_memory_mb = 256
query_timeout_ms = 5000
…
```

When the configuration has an error, `config show` prints the same error as `check` and exits with 2.

### config diff

Compares two configurations after the defaults are filled in, one key per line, in the format `<key>: <value in a> | <value in b>`:

```console
$ rradiusd config diff rradius.toml b.toml
auth.policy: "first_match" | "all_accept"
radius.auth_port: 1812 | 1822
$ echo $?
1
$ rradiusd config diff n2.toml n3.toml
cluster.secret_api: differs (hidden)
cluster.secret_api_file: "api.secret" | (absent)
node-specific:
  cluster.listen: "127.0.0.1" | "127.0.0.2"
$ rradiusd config diff rradius.toml rradius.toml
no differences
```

- A key present on only one side is written `(absent)`; a secret only says `differs (hidden)` and the value is not printed.
- `cluster.node_id` and `cluster.listen` are meant to differ on every node; they are placed at the end under `node-specific:`, and these two keys do **not** count as differences: when only these two keys differ, the exit code is 0.
- `radius.clients` and `tacacs.clients` are matched by name (by address when there is no name); a different order is not a difference.
- Exit codes: 0 no differences, 1 there are differences, 2 wrong arguments or an error in either configuration.

### config init

Prints a configuration draft to standard output. The cluster secrets and the shared secret of the example client are generated randomly (32 characters). The comments are in English by default; `--lang zh-TW` makes them Traditional Chinese. For the contents of the draft and what to change next, see [03 First start and first login](03-quick-start.md).

```console
$ rradiusd config init --lang zh-TW > rradius.toml
$ head -2 rradius.toml
# 由 `config init` 產生的 rradius 設定檔；`config init --full` 會印出所有設定。
# 叢集的每個節點都要用相同的 secret_raft 與 secret_api；範例用戶端的 secret 也要設在 NAS 上。要自己建立腳本 auth.lua。
```

## tuning

```text
rradiusd tuning [--config <path>]
```

Reads the system settings of this machine and the UDP drop counters, then estimates the socket buffers and file counts needed, and prints suggested values, the reasons and the commands. `--config` is optional: without it, the command reads the configuration file at the default path, and uses all defaults for the estimate if there is none; with it, the command checks against your settings (for example `recv_buffer_bytes`, `sockets`) whether the system limits are enough. The commands only read; they change nothing. Linux, macOS and FreeBSD have advice; other platforms print `rradius tuning: no advice for this platform (Linux, macOS and FreeBSD have it)` and exit with 0. For the meaning of each item, see [19 Performance tuning and load testing](19-performance.md).

```console
$ rradiusd tuning --config rradius.toml
rradius tuning advice for macOS

UDP packets dropped because a receive buffer was full, since the system started: …
  Raise the receive buffers below, then watch whether this count still grows.

Largest socket buffer (kern.ipc.maxsockbuf)
  current:   8388608
  suggested: 33554432
  why:       the receive buffer counts its bookkeeping against this limit too, so twice the buffer; raise it before recvspace
  now:       sysctl -w kern.ipc.maxsockbuf=33554432
  persist:   /etc/sysctl.conf: kern.ipc.maxsockbuf = 33554432
…
These suggestions are unmeasured starting points: apply them, then confirm with the dropped packet counts above.
```

- `tuning` only prints advice; it changes no system setting.
- When `--config` is not given and the default configuration file cannot be read, it prints one line with the reason and `the default settings are used`, gives advice with the defaults, and the exit code is still 0. When `--config` is given and cannot be read, the exit code is 2.

## status

```text
rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]
```

| Flag | Meaning |
| --- | --- |
| (none) | Reads `/metrics` of this node twice, one second apart, prints one page and ends |
| `--watch` | Then reads once more every second and updates the screen until Ctrl+C; on a terminal it clears and redraws the screen |
| `--json` | Prints JSON instead, one object at a time |
| `--cluster` | Asks the cluster for its members and reads the metrics port of each member, one line per node |
| `--tui` | Full-screen interactive view (a table of items plus a line chart of the last 60 seconds; up and down select an item, `[` and `]` select a member or all, `p` pauses); cannot be combined with `--json` or `--watch`, and standard output must be a terminal, otherwise the exit code is 2 |

`status` reads the address in `[metrics]` of the configuration file; when `bind` is `0.0.0.0` or `::`, it connects to a local address instead. For the meaning of each section and line, see [17 Monitoring, status and logs](17-monitoring.md).

```console
$ rradiusd status --config rradius.toml
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
…
$ rradiusd status --config rradius.toml --json
{"address":"127.0.0.1:9812","counter_reset":false,"interval_seconds":1.002444875,"ready":{"status":200,"text":"ok"},"sections":[…]}
$ rradiusd status --config rradius.toml --cluster
rradius cluster status over 1.0 s, members from the cluster

node   role    ready  auth/s  acct/s  discarded/s  in progress  address
1      leader  ok     0.0     0.0     0.0          0            127.0.0.1:9812
total          1/1    0.0     0.0     0.0          0
```

- `--watch --json` prints one line of JSON every second, which suits feeding another program.
- `--cluster` asks the members with the cluster's API secret, so `[cluster]` in the configuration file must agree with the cluster. The metrics port of each member is the host of that member's API address plus the port of `[metrics]` in this configuration.
- Errors:

  | Situation | Output | Exit code |
  | --- | --- | --- |
  | No `[metrics]` | `status needs [metrics]: the command reads this node's /metrics. Add to …`, followed by an example `[metrics]` | 2 |
  | Nobody answers on the metrics port | `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)` | 3 |
  | `--cluster`, no member can be read | The table is printed as usual, members that cannot be read are marked `unreachable`, and the reasons are listed at the end | 3 |
  | Wrong API secret | `127.0.0.1:8200 refused the api secret: [cluster] secret_api differs from the cluster's` | 2 |
  | The same flag twice | Usage | 2 |

  With `--watch`, a metrics port that cannot be read does not end the command; it prints the error once a second and retries (this was checked only against the code).

## user

```text
rradiusd user add|passwd|enable|disable|delete|show [--config <path>] <name>
```

Runs the SQL of the same name in `[accounts]` and writes to or queries the table through the cluster API port, so the server must be running. `<name>` is normalized as at login; for example the `username` setting of the default auth chain removes the realm. For the account table and how to write `[accounts]`, see [07 User accounts and auth chains](07-users-and-chains.md).

| Action | Key used | Output on success |
| --- | --- | --- |
| `add` | `accounts.add` | `added <name>` |
| `passwd` | `accounts.passwd` | `password changed for <name>` |
| `enable` | `accounts.enable` | `enabled <name>` |
| `disable` | `accounts.disable` | `disabled <name>` |
| `delete` | `accounts.delete` | `deleted <name>` |
| `show` | `accounts.show` | One line per column, `column=value`; a blank line between rows |

- `add` and `passwd` read the password from standard input: on a terminal they turn off echo and ask twice (`password: `, `again: `); from a pipe they read only the first line. The password is processed according to `accounts.scheme` before it is stored in the table.
- Write actions judge by whether a row was changed: when no row was changed, they print `no account <name>` and exit with 1.

```console
$ printf 'dave-password\n' | rradiusd user add --config rradius.toml dave
added dave
$ printf 'new-password\n' | rradiusd user passwd --config rradius.toml dave
password changed for dave
$ rradiusd user show --config rradius.toml dave
username=dave
enabled=1
$ rradiusd user delete --config rradius.toml dave
deleted dave
$ rradiusd user delete --config rradius.toml dave
no account dave
$ echo $?
1
```

Examples of errors:

| Situation | Output | Exit code |
| --- | --- | --- |
| `add` when the account exists | `Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")` | 1 |
| The password is empty | `the password is empty; nothing was changed` | 2 |
| The two passwords differ on a terminal | `the two passwords differ; nothing was changed` | 2 |
| The name is missing | `a name is required`, followed by the usage | 2 |
| One argument too many | `unexpected argument 'b'`, followed by the usage | 2 |
| `--config` has no value | `--config needs a value`, followed by the usage | 2 |
| `[accounts]` lacks this action | `accounts.add is not set in rradius.toml` | 2 |
| The server is not running | After 30 seconds, `the cluster did not answer within 30 s`; while waiting, standard error shows `Error looking up DB metrics` | 3 |
| Wrong API secret | Immediately `127.0.0.1:8200 refused the api secret: [cluster] secret_api differs from the cluster's` | 2 |

"The two passwords differ" can happen only on a terminal; it was not tested and is written from the code.

## session

```text
rradiusd session list [--config <path>] [--json]
rradiusd session show [--config <path>] [--json] <name>
```

Runs the `list` or `show` query of `[sessions]` (the `?1` of `show` is the normalized name) and prints the result in the column order of the SELECT. The server itself has no session table; the definition of "online" is entirely in the SQL you write. For setup, see [13 Accounting and sessions](13-accounting.md).

```console
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=11b8000000000000
started=1795384834
updated=1795384834
$ rradiusd session list --config rradius.toml --json
[{"username":"alice","nas":"192.0.2.1","session_id":"11b8000000000000","started":1795384834,"updated":1795384834}]
$ rradiusd session show --config rradius.toml nobody
no sessions for nobody
$ echo $?
1
$ rradiusd session show --config rradius.toml --json nobody
[]
```

- The text format has one line per column, `column=value`, with a blank line between sessions; NULL is printed as an empty string and a BLOB as lowercase hexadecimal. `--json` is an array of objects, and numbers stay numbers.
- With no data: the text format prints `no sessions` or `no sessions for <name>`, `--json` prints `[]`, and the exit code is 1 in both cases.
- When the SQL fails, it prints `the statement failed: …` and the exit code is **2** (for `user`, an SQL failure is 1). Tested: `the statement failed: Sqlite: no such column: nosuchcol in SELECT nosuchcol, …`.
- When `[sessions]` has no `list`, it prints `sessions.list is not set in …` and exits with 2. When the cluster cannot be reached, it is the same as `user`: exit code 3.

## accounting

```text
rradiusd accounting find --ip <address> --port <port> --at <time> [--file <path>]... [--json]
```

Reads the accounting log file (`[radius.accounting_log]`) and lists the `IP-Port-Range` segments (RFC 8045) that held the public address `--ip` and port `--port` at the time `--at`. It needs no configuration file and does not connect to the cluster. `--at` is RFC 3339 and must have a time zone (`Z` or `±HH:MM`, fractional seconds allowed); `--file` can be given several times, and `-` or no `--file` reads standard input; a `--file` whose name ends in `.gz` is decompressed automatically (it may be several gzip members joined together), standard input is not decompressed, so pipe it through `zcat`; a damaged or truncated `.gz` gives exit code 2 and names the file.

```console
$ rradiusd accounting find --ip 198.51.100.7 --port 1500 --at 2026-11-25T10:30:00Z --file accounting.jsonl
198.51.100.7:1024-2047 allocated 2026-11-25T10:00:00.120Z ended 2026-11-25T11:00:00Z ended_by release time_source received identity "Acct-Session-Id=s-1 client=bng1" client=bng1 User-Name=alice Acct-Session-Id=s-1 Framed-IP-Address=100.64.0.5
$ rradiusd accounting find --ip 198.51.100.7 --port 1500 --at 2026-11-25T11:30:00Z --file accounting.jsonl
$ echo $?
1
$ rradiusd accounting find --ip 198.51.100.7 --port 1500 --file accounting.jsonl
--at is required
usage: rradiusd accounting find --ip <address> --port <port> --at <time> [--file <path>]... [--json]
  --file - or no --file reads standard input; --at is RFC 3339, such as 2026-11-25T10:00:00Z
$ echo $?
2
```

- Exit codes: 0 at least one holder was listed, 1 none, 2 wrong arguments, a file, or a line that is not a JSON object (the message names the file and line number).
- `--json` is an array with the keys `ext_ip`, `port_start`, `port_end`, `allocated`, `released` (`null` if not ended yet), `ended_by` (`release`, `stop` or `null`), `time_source`, `identity` and `attributes`; when there is no holder it is `[]`.
- For the holding rules, the time basis and the known limits, see "Reverse lookup of public addresses and ports" in [13 Accounting and sessions](13-accounting.md).

## query

```text
rradiusd query list [--config <path>] [--json]
rradiusd query <name> [--config <path>] [--json]
```

- `query list` only reads the configuration file and lists the name, `help`, `interval_ms` and `sql` of every `[queries.<name>]`; it does not connect to the cluster. When there are no queries it prints nothing and exits with 0.
- `query <name>` runs that SQL once on the cluster and prints it as a table. The name `list` is reserved for listing and cannot be used as a query name.
- For how to write named queries and the metrics they map to, see [09 External databases and named queries](09-external-databases.md).

```console
$ rradiusd query list --config rradius.toml
name=enabled_users
help=Accounts that can log in
interval_ms=60000
sql=SELECT enabled, COUNT(*) AS value FROM users GROUP BY enabled
$ rradiusd query enabled_users --config rradius.toml
enabled  value
0        1
1        2
$ rradiusd query enabled_users --config rradius.toml --json
[{"enabled":0,"value":1},{"enabled":1,"value":2}]
$ rradiusd query nobody_here --config q2.toml
no rows
$ echo $?
0
$ rradiusd query nope --config rradius.toml
queries.nope is not set in rradius.toml
```

- NULL in a table is printed as `NULL`; in `--json` NULL is `null`. `query list --json` is one line with an array of objects.
- Multi-line SQL is written with `\n` for the line breaks in the text format of `query list`, so that one query stays on one line.
- Errors: a name that is not set 2; SQL failed 2 (same message as `session`); cluster cannot be reached 3.

## cluster

```text
rradiusd cluster members [--config <path>]
rradiusd cluster remove [--config <path>] [--force] <node_id>
```

Finds the leader through the API addresses in `[[cluster.nodes]]` of the configuration file, then lists or changes the members. For the full member management procedure, see [16 Clusters and high availability](16-cluster.md).

```console
$ rradiusd cluster members --config rradius.toml
node 1  voter    leader  raft 127.0.0.1:8100  api 127.0.0.1:8200
```

`members` prints one line per member: node number, `voter` or `learner`, whether it is the `leader`, and the Raft and API addresses. A member that is in the cluster but not listed in the configuration file gets `(not in this configuration)` at the end of the line; a node listed in the configuration file but not in the cluster prints `node <id>  not in the cluster (listed in this configuration)`.

`remove` takes a node that has **already stopped** out of the cluster:

| Situation | Output | Exit code |
| --- | --- | --- |
| Success | `removed node <id>` | 0 |
| The node still answers | `node 1 still answers at 127.0.0.1:8200: stop it first, then remove it` | 2 |
| The node is not in the cluster | `no node 2 in the cluster` | 1 |
| The number is not a positive integer | `the node id must be a positive integer, not '0'` | 2 |
| No number given | `a node id is required`, followed by the usage | 2 |
| Only one voter would remain | `removing node <id> leaves one voter, with nothing to take over when it stops; give --force to remove it anyway` | 2 |
| Only two voters would remain | `… leaves two voters, and the cluster stops writing when either stops; give --force to remove it anyway` | 2 |
| The last voter | `node <id> is the last voter; the cluster cannot remove it` | 2 |
| No response within 30 seconds after sending | `the removal of node <id> may have been made, …; check with cluster members` | 3 |

- `--force` is used only with `remove`, so that the "only one or two voters remain" cases above are removed anyway. `members --force` is a usage error.
- With two voters and one of them stopped, the first `remove --force` of the stopped one ends with exit code 3 and leaves it a `learner`; see [Remove a node](16-cluster.md#remove-a-node) for how to finish it.
- Wrong API secret: `… refused the api secret: [cluster] secret_api differs from the cluster's`, exit code 2, and it appears immediately (the same for `user`, `session`, `query`, `client`, `dpsk`, `aka`, and for `cluster` and `status --cluster`). The cluster does not answer at all: `the cluster did not answer within 30 s`, exit code 3.

> **Not verified on real hardware:** the rows "Success", "Only one or two voters would remain" and "No response within 30 seconds after sending" were run on 2026-11-27 with several processes on one machine, not on several hosts; the row "The last voter" was checked only against `cluster_commands.rs` and its tests; the other rows were tested on a single node.

## Old flag syntax

Starting and checking used to be flags and are now subcommands. With the old syntax, `rradiusd` does not start the server; it prints the full usage list with one more hint line at the end and exits with 2:

| Old syntax | Hint on the last line |
| --- | --- |
| `rradiusd --config <path>` | `to start the server: rradiusd serve --config <path>` |
| `--check` anywhere, for example `rradiusd --check --config <path>` | `to check a configuration: rradiusd check [--config <path>]` |

## Enterprise-only commands

The core `rradiusd` knows the names of the enterprise commands but does not run them: before reading the configuration file it prints `<command>: not supported by this program (<feature> needs rradius-enterprise)` and exits with 2. The arguments that follow do not change the result.

| Command | Message printed by the core |
| --- | --- |
| `aka` | `aka: not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| `aka-seal` | `aka-seal: not supported by this program (EAP-AKA' needs rradius-enterprise)` |
| `client` | `client: not supported by this program (dynamic clients need rradius-enterprise)` |
| `dpsk` | `dpsk: not supported by this program (DPSK needs rradius-enterprise)` |
| `dpsk-seal` | `dpsk-seal: not supported by this program (DPSK needs rradius-enterprise)` |
| `disconnect` | `disconnect: not supported by this program (CoA needs rradius-enterprise)` |
| `coa` | `coa: not supported by this program (CoA needs rradius-enterprise)` |
| `otp` | `otp: not supported by this program (two-factor login needs rradius-enterprise)` |

This list is `COMMANDS_NEEDING_ENTERPRISE` in `rradius-core/src/extension.rs`, and all eight were tested. These commands are not in the list of `rradiusd help`.

> **Enterprise edition:** the `help` of the enterprise edition also lists these commands and their usage.

---

## rradius-bench run

> **Note:** `rradius-bench` is not in any package, archive or Docker image; build it from source, see [02 Installation](02-install.md).

```text
rradius-bench run [OPTIONS] --server <SERVER> --secret <SECRET> --load <LOADS>
```

Sends load to a server and prints a report when it ends. `--server`, `--secret` and at least one `--load` are required. All arguments are checked before any request is sent; on an error it exits with code 3 and sends nothing.

Flags and defaults (`rradius-bench run --help` is authoritative):

| Flag | Default | Meaning |
| --- | --- | --- |
| `--server <SERVER>` | Required | Name or address of the server |
| `--secret <SECRET>` | Required | Shared secret; TACACS+ loads also use it as the key. It shows up in the process list, so use only a test secret |
| `--load <LOADS>` | Required | A load, syntax in the next section; can be given several times and runs at the same time |
| `--auth-port` | `1812` | Authentication port |
| `--acct-port` | `1813` | Accounting port |
| `--bind <BIND>` | — | Source address of the sockets |
| `--user` | `bench` | Account name |
| `--password` | `bench` | Password; for the `aka-prime` load write `KI:OPC`, 32 hexadecimal digits each |
| `--users <N>` | — | Uses `<user>0` to `<user>N-1` in turn |
| `--nas-ip` | `192.0.2.1` | `NAS-IP-Address` of every request |
| `--nas-identifier` | `rradius-bench` | `NAS-Identifier` of every request |
| `--attr <TYPE=KIND:VALUE>` | — | An attribute added to every request; `KIND` is `int`, `text`, `hex` or `ip`; `TYPE` is 1 to 255 and cannot be 80; can be given several times |
| `--no-message-authenticator` | Off | Access-Request carries no Message-Authenticator |
| `--timeout` | `5` | Seconds to wait for a response, then resend or give up |
| `--retries` | `0` | Number of resends |
| `--warmup` | `2` | Seconds to send before measuring starts |
| `--duration` | `10` | Seconds to measure |
| `--requests <N>` | — | Each load stops after N requests are measured (`session` counts flows), with no warmup; cannot be used together with `--warmup` or `--duration` |
| `--metrics-url` | — | `http://host:port/metrics` of the server; the report shows how the counters changed during the measurement; only `http://` is accepted |
| `--json` | Off | The report is printed as JSON |
| `--tls-version` | `1.2` | TLS version of EAP loads, `1.2` or `1.3`; `1.3` works only with `eap-tls` |
| `--ca-file` | — | CA used to check the server's EAP certificate and, with `--transport tls`, its RadSec certificate; when not given, any certificate is accepted |
| `--tls-name` | The value of `--server` | With `--ca-file`, the name the server certificate must match |
| `--cert-file`, `--key-file` | — | Certificate and private key for an `eap-tls` login; also needed by `--transport tls` |
| `--eap-fragment` | `1000` | Most bytes of TLS data one EAP-Response carries |
| `--transport` | `udp` | `udp`, or `tls` (RadSec, RFC 6614, one TLS connection per socket) |
| `--tls-port` | `2083` | RadSec port, shared by authentication and accounting |
| `--tacacs-port` | `49` | TACACS+ port for the `tacacs-*` loads |
| `--tacacs-connect` | `keep` | `keep`: one connection per socket for the whole run; `per-request`: a new connection for every request |

These combinations are rejected before anything is sent (exit code 3; all messages were tested):

| Combination | Message |
| --- | --- |
| `--transport dtls` | `--transport dtls: udp or tls` |
| `--transport tls` without a certificate | `--transport tls needs --cert-file and --key-file: RadSec servers ask for a client certificate` |
| `--transport tls --retries 1` | `--retries: nothing is sent again over TLS (RFC 6614), so --transport tls takes 0` |
| A TACACS+ load with `--transport tls` | `--transport tls: TACACS+ loads go over TCP, not RadSec` |
| `--tacacs-connect sometimes` | `--tacacs-connect sometimes: keep or per-request` |
| An EAP load with `--no-message-authenticator` | `--no-message-authenticator: EAP loads always carry Message-Authenticator (RFC 3579)` |
| `peap` with `--tls-version 1.3` | `--tls-version 1.3: only for eap-tls, not peap` |
| `eap-tls` without `--cert-file` | `eap-tls needs --cert-file` |
| `aka-prime` with a wrong `--password` format | `aka-prime needs --password KI:OPC, each 32 hex digits` |
| `--attr 80=hex:00` | `--attr 80=hex:00: type 80 is Message-Authenticator, which the tool handles` |
| `--metrics-url https://…` | `--metrics-url https://127.0.0.1/metrics: must start with http://` |
| `--requests 0` | `--requests must be above 0` |
| `--requests` together with `--duration` | The usage text of clap |
| Name resolution fails | `--server nonexistent.invalid: failed to lookup address information: …` |

> **Not verified on real hardware:** `--transport tls` needs a RadSec server, and the core `rradiusd` has no RadSec, so only the argument checks in the table above were verified; it was never run against a server.

## --load syntax

```text
--load KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]
```

At least one of `concurrency` and `rate` is required, otherwise you get `--load pap: needs concurrency or rate`.

| Key | Default | Meaning |
| --- | --- | --- |
| `concurrency` | 256 when `rate` is given | Number of requests in flight at the same time. Alone it is fixed-concurrency; together with `rate` it is the upper limit. Must be above 0 |
| `rate` | — | How many to send per second, fixed-rate. Must be above 0 |
| `window` | RADIUS 64, TACACS+ 1 | The most requests at the same time on one socket, 1 to 256. For TACACS+ it can only be 1 |
| `threads` | Fixed-concurrency 2, fixed-rate 1 | How many threads this load uses; never more than the number of sockets |
| `alive` | 0 | Used only by `session`: how many Interim-Updates to send between Start and Stop |

The number of sockets is `concurrency ÷ window`, rounded up. For example `pap,concurrency=4096` uses 64 sockets, and `tacacs-pap,concurrency=32` uses 32.

All the kinds of `KIND`. `--help` does not list them; the list comes from `Step::name` in `rradius-bench/src/packet.rs` plus `session`; the column "Tested against the core" is the result of sending each one once to this chapter's verification environment:

| Kind | What it sends | Tested against the core |
| --- | --- | --- |
| `pap` | Access-Request with `User-Password` | accept |
| `chap` | `CHAP-Password` and `CHAP-Challenge` | accept |
| `chap-authenticator` | `CHAP-Password` with the Request Authenticator as the challenge, no `CHAP-Challenge` | accept |
| `mschapv2` | `MS-CHAP-Challenge` and `MS-CHAP2-Response` | accept |
| `acct-start`, `acct-alive`, `acct-stop` | Accounting-Request with `Acct-Status-Type` 1, 3, 2 | accounting |
| `acct-on`, `acct-off` | `Acct-Status-Type` 7, 8 | accounting |
| `status-auth`, `status-acct` | Status-Server, sent to the authentication port or the accounting port | accept, accounting |
| `session` | With one session ID, in order: login (`pap`), Start, `alive` Interim-Updates, Stop; the next step is sent only when the previous one was answered; when the login is rejected, no accounting is sent | Each step reported separately |
| `eap-tls` | An EAP-TLS conversation; needs `--cert-file` and `--key-file` | reject (the verification environment has no `certificate_query`, log `not authorized`) |
| `peap` | A PEAP conversation, inner MS-CHAPv2 | accept |
| `ttls` | An EAP-TTLS conversation, inner PAP | accept |
| `ttls-mschapv2` | An EAP-TTLS conversation, inner MS-CHAPv2 | accept |
| `aka-prime` | An EAP-AKA' conversation, `--password KI:OPC` | reject (the core does not offer this method, log `the peer asked only for methods this server does not offer`) |
| `tacacs-pap` | TACACS+ PAP authentication | accept |
| `tacacs-chap` | TACACS+ CHAP authentication (the server needs an auth chain that can compare the password, otherwise the server answers `RESTART`, which counts as `invalid`) | accept (with such an auth chain) |
| `tacacs-ascii` | TACACS+ ASCII login, the server asks for the account and password step by step (one login measured `rounds` 3.0) | accept |
| `tacacs-author` | TACACS+ authorization | reject (the verification environment has no authorization rules) |
| `tacacs-acct` | TACACS+ accounting | accounting |

For EAP loads, `concurrency`, `rate` and `--requests` count "conversations"; for `tacacs-ascii` they count "logins".

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load peap,concurrency=1 --requests 1 \
    --ca-file certs/ca.pem --tls-name radius.corp.example.com
measured 0.003 s after 0.000 s of warmup

peap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 15%
  peap: 382/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 10.0
    latency us: p50 2578.4, p90 2578.4, p99 2578.4, p99.9 2578.4, max 2578.4
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user alice --password alice-password --load tacacs-ascii,concurrency=1 --requests 1
measured 0.000 s after 0.000 s of warmup

tacacs-ascii (fixed-concurrency): concurrency 1, 1 threads, 1 sockets over tcp, busiest worker 12%
  tacacs-ascii: 2542/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 3.0
    latency us: p50 391.2, p90 391.2, p99 391.2, p99.9 391.2, max 391.2
```

## run output and exit codes

Structure of the text report:

- The first line is `measured <seconds> s after <seconds> s of warmup`.
- One section per load: kind, mode (`fixed-concurrency` or `fixed-rate`), `concurrency`, `rate`, number of threads and sockets; for non-UDP, `over tcp` or `over tls` is added; fixed-concurrency shows `busiest worker`, fixed-rate shows `send lag p99`.
- One line of counts per step: `sent`, `accept`, `reject`, `challenge`, `accounting`, `timeouts`, `invalid`, `late`, `socket errors`, `retransmits`, then one line of latency (microseconds) with p50, p90, p99, p99.9 and max. EAP and `tacacs-ascii` have one more line, `rounds per finished conversation`.
- With `--metrics-url` there is one more section, `server counters: …`, and when there are warnings, one more section, `warnings:`.

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1812 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,rate=50 --warmup 0.5 --duration 1 \
    --metrics-url http://127.0.0.1:9812/metrics
measured 1.000 s after 0.500 s of warmup

pap (fixed-rate): concurrency 256, rate 50/s, 1 threads, 4 sockets, send lag p99 89.0 us
  pap: 50/s, sent 50, accept 50, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 383.7, p90 561.7, p99 604.2, p99.9 604.2, max 604.2

server counters: auth 49, acct 0, duplicates 0, discarded overload 0, unknown_client 0, invalid 0
```

`--json` prints a JSON document instead: the top level has `schema` (currently 1), `warmup_s`, `duration_s`, `trustworthy`, `loads`, `server_metrics` and `warnings`; each load has `kind`, `mode`, `transport`, `concurrency`, `rate`, `threads`, `sockets`, `reconnects`, `send_lag_us`, `worker_busy_max` and `steps`; each step has `step`, `sent`, `responses` (`accept`, `reject`, `challenge`, `accounting`), `timeouts`, `invalid`, `late`, `socket_errors`, `retransmits`, `throughput_per_s`, `rounds` and `latency_us` (`p50`, `p90`, `p99`, `p999`, `max`).

Warning codes:

| Code | When it appears |
| --- | --- |
| `tool_saturated` | A worker thread of a fixed-concurrency load was busy for at least 90% of the measurement: what was measured is the limit of the tool |
| `rate_not_met` | A fixed-rate load sent less than 99% of the schedule, or the p99 of `send_lag_us` is above 1000 |
| `duplicate_path` | With `--retries 0`, the server's duplicate request counter increased |
| `server_discarded_<reason>` | The server's counter of discarded packets increased, for example `server_discarded_overload` |
| `metrics_unavailable` | `--metrics-url` could not be read; the measurement completed as usual |

The three codes `tool_saturated`, `rate_not_met` and `duplicate_path` make `trustworthy` false, and the text report ends with `these numbers do not describe the server alone: see the warnings`. Warnings and `late` do not affect the exit code.

Exit codes:

| Exit code | Meaning |
| --- | --- |
| 0 | All measured requests were Accept, or all accounting requests were answered |
| 1 | There were Reject or Challenge, with no timeouts or errors |
| 2 | There were timeouts, invalid responses or socket errors |
| 3 | Wrong arguments or could not start (for example name resolution failed), no result; an unknown command or a missing required flag also gives 3 |

```console
$ rradius-bench run --server 127.0.0.1 --auth-port 1899 --secret x --load pap,concurrency=1 --requests 1 --timeout 0.5
measured 0.503 s after 0.000 s of warmup

pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 0%
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 1, retransmits 0
    latency us: p50 0.0, p90 0.0, p99 0.0, p99.9 0.0, max 0.0
$ echo $?
2
```

This example deliberately uses port 1899, where nobody is listening. The exit code of `rradius-bench --help`, `-h`, `--version` and `-V` is 0; with no arguments it prints the command list and exits with 3.

## rradius-bench calibrate

```text
rradius-bench calibrate [--duration <seconds>] [--json]
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--duration` | `3` | Seconds to measure each combination; must be above 0 |
| `--json` | Off | Prints JSON instead: `schema` 1 and four `results` |

It sends load to a responder inside the tool's own process that signs its responses correctly, using the same code as `run`. It measures four combinations: 1 thread with window 1 and concurrency 1; and 1, 2 and 4 threads with window 64. The result is how fast this tool can send on this machine; when `run` measures close to this number, the bottleneck may be the tool.

```console
$ rradius-bench calibrate --duration 0.5
what this tool can do here, against responders that do no other work:
  1 threads, window 1, concurrency 1: …/s, p50 … us, p99 … us, busiest worker …%, timeouts 0
  1 threads, window 64, concurrency 64: …
  2 threads, window 64, concurrency 128: …
  4 threads, window 64, concurrency 256: …
```

The numbers differ from machine to machine and are omitted here. The exit code is 0; `--duration 0` prints `rradius-bench: --duration must be above 0` and exits with 3.

---

## Comparison method

After the program is updated, use the following method to confirm that this reference has not missed or added any commands or flags. Result of running it against this appendix on 2026-11-23: every difference was 0.

**1. Commands and flags of `rradiusd`.** The output of `help` already contains the usage of every subcommand; take out the "command subcommand" pairs and the flags, and check one by one that they appear in this appendix:

```console
$ rradiusd help > help.txt
$ grep -oE -- '--[a-z]+(-[a-z]+)*' help.txt | sort -u > flags-help.txt
$ grep -oE -- '--[a-z]+(-[a-z]+)*' 91-command-reference.md | sort -u > flags-book.txt
$ comm -23 flags-help.txt flags-book.txt          # in help, not in the book: must be empty
$ sed -n 's/^usage: rradiusd \([a-z-]*\) \([a-z|]*\).*/\1 \2/p' help.txt \
    | awk '{n=split($2,a,"|"); for(i=1;i<=n;i++) print $1" "a[i]}' \
    | grep -v ' $' \
    | while read c; do grep -qF "\`$c" 91-command-reference.md || echo "missing: $c"; done
```

The second part produces pairs such as `config show`, `user add` and `cluster remove`; it passes only if it prints nothing.

**2. Flags that the book has in excess.** `comm -13 flags-help.txt flags-book.txt` lists the flags that are in the book but not in `rradiusd help`. They should all come from the `--help` of `rradius-bench`, from the old flag `--check`, and from `--help`. Add the flags of `rradius-bench --help`, `rradius-bench run --help` and `rradius-bench calibrate --help` to `flags-help.txt` and compare again; what remains can only be `--check` and `--load-syntax` (not a flag: it comes from this chapter's link to the `--load` syntax).

**3. Flags and defaults of `rradius-bench`.** Every flag of `rradius-bench run --help` must be in the table of [rradius-bench run](#rradius-bench-run), and `[default: …]` must agree with the "Default" column of the table; the same goes for `calibrate --help`.

**4. The kinds of `--load`.** `--help` does not list the kinds; take them from the source:

```console
$ sed -n '/pub fn name/,/^    }/p' rradius-bench/src/packet.rs | grep -oE '"[a-z0-9-]+"' | tr -d '"' > kinds.txt
$ echo session >> kinds.txt
$ while read k; do grep -qF "\`$k\`" 91-command-reference.md || echo "missing: $k"; done < kinds.txt
```

There are 22 kinds at the moment.

**5. Enterprise commands.** Take the names from `COMMANDS_NEEDING_ENTERPRISE` in `extension.rs` and run each against the core; every one must give `not supported by this program` and exit code 2, and must appear in the table of [Enterprise-only commands](#enterprise-only-commands).

Exit codes are not in `help`; compare them against the source of each subcommand (`return`, `exit_code`, the `STATUS_*` constants).

## Common mistakes

**Symptom:** adding `--help` after a subcommand, for example `rradiusd user --help`, prints the usage and exits with 2.
**Cause:** subcommands have no `--help`; `--help` is treated as an unknown argument.
**Fix:** use `rradiusd help` to see all usage; the usage text itself is not an error.

**Symptom:** `rradiusd --config rradius.toml` does not start and prints the whole usage and `to start the server: …`.
**Cause:** old syntax; starting is now a subcommand.
**Fix:** write `rradiusd serve --config rradius.toml`.

**Symptom:** `rradiusd config show --config a.toml --lang zh-TW` prints the usage of `config` and exits with 2.
**Cause:** only `config init` accepts `--lang`.
**Fix:** do not add `--lang` to `config show`.

**Symptom:** a script uses the exit code of `query <name>` to tell whether there is data, and it is always 0.
**Cause:** when there are no rows, `query` prints `no rows` and the exit code is still 0.
**Fix:** judge by the output, or use `--json` and check whether it is `[]`.

**Symptom:** `user`, `session` or `cluster` stops for 30 seconds and then prints `the cluster did not answer within 30 s`, exit code 3.
**Cause:** the server is not running, the API address in `[[cluster.nodes]]` is wrong, or the API secret does not agree with the cluster. When the secret does not agree, the command says `refused the api secret` directly with exit code 2 and does not wait for the timeout; it waits 30 seconds only when it cannot connect or gets no answer.
**Fix:** make sure the server is running and that the command uses the same configuration file as the server.

**Symptom:** every result of `rradius-bench` is `timeouts`, and the server log has `discarded 1 Access-Requests lacking a Message-Authenticator`.
**Cause:** `--no-message-authenticator` was used, and the server by default requires Access-Request to carry Message-Authenticator.
**Fix:** remove this flag; to test a NAS that does not send it, first read `radius.clients[].require_message_authenticator` in [Appendix A: Configuration reference](90-config-reference.md).

**Symptom:** `rradius-bench run … --requests 1 --duration 3` prints the usage of clap and exits with 3.
**Cause:** `--requests` cannot be used together with `--warmup` or `--duration`.
**Fix:** choose one.

**Symptom:** the `eap-tls` load gets `reject 1`, and the server log says `the EAP Identity does not fit the certificate`.
**Cause:** the EAP Identity sent by `--user` does not match the name of the client certificate.
**Fix:** write `--user` as the name on the certificate, for example `laptop-001.corp.example.com` of the wifi-certs scenario.

## Related chapters

- [03 First start and first login](03-quick-start.md): first use of `serve`, `user`, `session` and `status`
- [04 Configuration file basics](04-configuration.md): `check`, `config`, `schema`, reload
- [07 User accounts and auth chains](07-users-and-chains.md): `user` and `[accounts]`
- [09 External databases and named queries](09-external-databases.md): `query` and `[queries]`
- [13 Accounting and sessions](13-accounting.md): `session` and `[sessions]`
- [16 Clusters and high availability](16-cluster.md): `cluster members`, `cluster remove`
- [17 Monitoring, status and logs](17-monitoring.md): `status`
- [19 Performance tuning and load testing](19-performance.md): `tuning`, `rradius-bench`
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix E: Glossary and RFC cross-reference](94-glossary.md)
