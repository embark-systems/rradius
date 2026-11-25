# 90 Appendix A: Configuration reference

> After this chapter you can:
> - Look up the type, default, whether it is required and the meaning of every `rradiusd` setting key
> - Tell which keys take effect on reload and which need a restart
> - Know what the server does when you leave out a whole section
> - Recognize the keys and sections that the core does not support or that have no effect in the core

Prerequisites: you have read [04 Configuration file basics](04-configuration.md) and know how to use the configuration file, `include` and `check`.

In this chapter:

- [How to read this reference](#how-to-read-this-reference)
- [Overview of top-level sections](#overview-of-top-level-sections)
- [Top-level keys: dictionary, include](#top-level-keys-dictionary-include)
- [lua](#lua)
- [cluster](#cluster)
- [metrics](#metrics)
- [log](#log)
- [runtime](#runtime)
- [shutdown](#shutdown)
- [accounts](#accounts)
- [sessions](#sessions)
- [auth](#auth)
- [databases](#databases)
- [queries](#queries)
- [radius](#radius)
- [tacacs](#tacacs)
- [Settings the core does not support or that have no effect](#settings-the-core-does-not-support-or-that-have-no-effect)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

---

## How to read this reference

This appendix is built from `rradiusd schema --lang zh-TW` (JSON Schema) and the output of `rradiusd config show`, checked against `rradius-core/src/config.rs` and `reload.rs` to add the defaults and conditions that the schema does not state. After you update the program, trust these two commands on your version:

```console
$ rradiusd schema --lang zh-TW > schema.json
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
…
```

`config show` prints the whole configuration with defaults filled in for the keys you did not write, and shows secrets as `<hidden>`. The comment on the first line lists the values computed for this machine.

### How key names are written

The first column of each table is the full path of the key:

| Notation | Meaning | Example |
| --- | --- | --- |
| `a.b` | Key `b` in the table `[a]` | `radius.auth_port` |
| `a[].b` | Key `b` in each item of the table array `[[a]]` | `radius.clients[].secret` |
| `a.<name>.b` | Key `b` in the sub-table `[a.<name>]` whose name you choose | `databases.<name>.host` |
| `auth.handlers[ldap].b` | Key `b` in the kind of `[[auth.handlers]]` that has `type = "ldap"` | `auth.handlers[ldap].urls` |

### Columns

- **Type:** string, integer, boolean, IP address, array, table. "Path" is a string; a relative path is always relative to the directory of this configuration file.
- **Default:** the value when you leave the key out. "—" means there is no default: leaving the key out means "this setting does not exist", and the effect is written in the description.
- **Required:** "yes" means you must write it whenever this section is present; "conditional" means it is required only under the condition given in the description; blank means it can be left out.
- **reload:** "yes" means it takes effect on a `SIGHUP` reload; "restart" means reload does not apply it, the log lists these keys in `these need a restart to apply: …`, and you must restart for it to take effect; "command" means only the management commands read it, each time you run the command, and reload has nothing to do with it.
- Integer units follow the key name: `_ms` is milliseconds, `_seconds` is seconds, `_bytes` is bytes, `_mb` is MiB, `_kib` is KiB.

Under the heading of each section, the main chapter is named; detailed usage and examples are in that chapter.

---

## Overview of top-level sections

| Section | Required | When left out | reload | Main chapter |
| --- | --- | --- | --- | --- |
| `dictionary` | yes | Cannot be left out | restart | [06](06-dictionary.md) |
| `include` | | Only this one file is read | yes | [04](04-configuration.md) |
| `[lua]` | yes | `check` fails: `the [lua] section is required` | yes (`max_memory_mb` needs a restart) | [10](10-lua-policies.md) |
| `[cluster]` | yes | `check` fails: `the [cluster] section is required` | restart | [16](16-cluster.md) |
| `[metrics]` | | No HTTP port: no `/metrics`, `/livez`, `/readyz`, and the `status` command cannot read any data | restart | [17](17-monitoring.md) |
| `[log]` | | Level `info`, format `text`, auth records written | restart (`log.trace` yes) | [17](17-monitoring.md) |
| `[runtime]` | | No CPU pinning | restart | [19](19-performance.md) |
| `[shutdown]` | | A single node does not delay; drain waits at most 5000 ms | yes | [04](04-configuration.md) |
| `[accounts]` | | The `user` command cannot be used | command | [07](07-users-and-chains.md) |
| `[sessions]` | | The `session` command cannot be used | command | [13](13-accounting.md) |
| `[auth]` | | No auth chain; the script in `[lua]` decides every login | yes | [07](07-users-and-chains.md) |
| `[databases.<name>]` | | No external databases | restart | [09](09-external-databases.md) |
| `[queries.<name>]` | | No named queries | restart | [09](09-external-databases.md) |
| `[radius]` | at least one of this and `[tacacs]` | RADIUS is not served | adding or removing the whole section needs a restart | [05](05-radius-clients.md) |
| `[tacacs]` | at least one of this and `[radius]` | TACACS+ is not served | adding or removing the whole section needs a restart | [12](12-tacacs.md) |

If neither `[radius]` nor `[tacacs]` is present, `check` fails: `at least one of [radius] or [tacacs] must be configured`.

No table accepts unknown keys. If you mistype a key, `check` points to the line number and the keys you can use; see [Common mistakes](#common-mistakes).

### The smallest configuration

Write only the required keys, use defaults for everything else, and `check` passes:

```toml
# rradius.toml
dictionary = "share/dictionary"      # main dictionary file, relative to this file

[lua]
script = "auth.lua"                  # the file must exist and compile

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"     # openssl rand -hex 32 > raft.secret
secret_api_file = "api.secret"       # openssl rand -hex 32 > api.secret

[radius]

[[radius.clients]]
address = "127.0.0.1"
secret_file = "localhost.secret"     # openssl rand -hex 32 > localhost.secret
```

This configuration listens on ports 1812 and 1813 on all addresses, has no `[metrics]` (so no `/metrics` and no `status`), and has no `[auth]` (so `auth.lua` decides every login). `rradiusd config init --lang zh-TW` produces a starting file with full comments and random secrets, which is easier than starting from here.

---

## Top-level keys: dictionary, include

Main chapters: `dictionary` is in [06](06-dictionary.md), `include` is in [04](04-configuration.md).

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `dictionary` | path | — | yes | restart | The main file of the RADIUS dictionary, for example `/usr/share/rradius/dictionary` installed by the package. A relative path is relative to the directory of the configuration file. |
| `include` | array (string) | — | | yes | Other configuration files to read together with this one, for example `["conf.d/*.toml"]`. Their content is treated as if it were appended to the main file. |

Rules for `include` (`config.rs`):

- The wildcards `*` and `?` can appear only in the last file name segment, for example `conf.d/*.toml`; directory names cannot contain wildcards.
- One name expands in path order; a wildcard does not match file names that start with `.`; the same file is read only once; the main file itself is skipped.
- At most 1000 files can be included, each at most 1048576 bytes (1 MiB).
- An included file must start with a table (`[section]`) and cannot contain `include` itself.
- If the main file already declares a table, an included file cannot declare it again, or you get `duplicate key`.

---

## lua

Main chapter: [10 Writing policies in Lua](10-lua-policies.md).

### `[lua]`

The top-level script that runs for every request, and the resource limits for scripts. These limits also apply to the `lua` handler in an auth chain.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `lua.script` | path | — | yes | yes | The top-level script that runs for every request. `check` compiles it. |
| `lua.query_timeout_ms` | integer | `5000` | | yes | Total time a script can wait for the database in one request, in milliseconds; must be greater than 0. |
| `lua.max_execution_ms` | integer | `1000` | | yes | How long a script can run in one request, in milliseconds; must be greater than 0. |
| `lua.max_memory_mb` | integer | `256` | | restart | Memory limit for each Lua VM, in MiB; must be greater than 0. |

### `[lua.http]`

Only with this table can scripts call `http_request` to reach HTTP APIs on the allow list. Without this table the function does not exist.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `lua.http.allowed_hosts` | array (string) | — | yes | yes | Hosts that scripts can call, at least one: `host`, `host:port`, or `http://host` to use plain HTTP for that host; all the others use HTTPS. |
| `lua.http.timeout_ms` | integer | `2000` | | yes | Time limit for one call from connecting to reading the whole response, in milliseconds; must be greater than 0. |
| `lua.http.max_response_bytes` | integer | `1048576` | | yes | Upper limit for the response body accepted, in bytes; must be greater than 0. |
| `lua.http.max_concurrent` | integer | `64` | | yes | Number of calls in progress at the same time for each host; must be greater than 0. |
| `lua.http.ca_file` | path | — | | yes | CA certificates (PEM) to trust in addition to the system root certificates. |

---

## cluster

Main chapter: [16 Clusters and high availability](16-cluster.md). Any change to `[cluster]` needs a restart.

### `[cluster]`

Node settings for the embedded database (hiqlite, Raft replication). A single server needs this section too: a single server is a cluster with one node.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `cluster.data_dir` | path | — | yes | restart | Data directory for the database and the Raft log. |
| `cluster.node_id` | integer | `1` | | restart | The id of this node, at least 1; it must appear in `nodes`. |
| `cluster.secret_raft` | string | — | conditional | restart | Shared secret of the Raft port, at least 16 characters. Set exactly one of this and `secret_raft_file`. |
| `cluster.secret_raft_file` | path | — | conditional | restart | File that holds the Raft secret. Set exactly one of this and `secret_raft`. |
| `cluster.secret_api` | string | — | conditional | restart | Shared secret of the API port, at least 16 characters. Set exactly one of this and `secret_api_file`. |
| `cluster.secret_api_file` | path | — | conditional | restart | File that holds the API secret. Set exactly one of this and `secret_api`. |
| `cluster.listen` | IP address | `127.0.0.1` without `nodes`, `0.0.0.0` with `nodes` | | restart | Address on which the two cluster ports listen. If you have `nodes` and do not set it, you get a warning; see [Common mistakes](#common-mistakes). |
| `cluster.nodes` | table array | — | | restart | Every node of the cluster, including this one. The fields are in the next table. |
| `cluster.migrations_dir` | path | — | | restart | Directory of SQL files (migrations) applied in order at startup. |
| `cluster.migrations_wait_ms` | integer | `60000` | | restart | Time limit at startup for waiting for the cluster to be ready, applying migrations and catching up; the three steps are timed separately, in milliseconds; must be greater than 0. |
| `cluster.snapshot_every` | integer | `100000` | | restart | Number of Raft log entries between two snapshots; must be greater than 0. |
| `cluster.read_connections` | integer | twice the number of worker threads, at most 32 (depends on the machine) | | restart | Number of connections that read the local database, 1 to 128. The number of worker threads equals the number of CPUs. |
| `cluster.stop_without_leader_ms` | integer | — (always answers) | | restart | After this long without a leader, stop answering logins, in milliseconds; must be greater than 0. |

### `[[cluster.nodes]]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `cluster.nodes[].id` | integer | — | yes | restart | Node id, at least 1. |
| `cluster.nodes[].raft` | string | — | yes | restart | `host:port` of the Raft port of this node. |
| `cluster.nodes[].api` | string | — | yes | restart | `host:port` of the API port of this node. |

`raft` and `api` are ports used between nodes, not management interfaces.

---

## metrics

Main chapter: [17 Monitoring, status and logs](17-monitoring.md).

### `[metrics]`

The HTTP port that serves `/metrics`, `/livez` and `/readyz` opens only if you write this table (even if it is empty); the `status` command also reads this port.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `metrics.bind` | IP address | `127.0.0.1` | | restart | Address on which the HTTP port listens. |
| `metrics.port` | integer | `9812` | | restart | HTTP port. |

---

## log

Main chapter: [17 Monitoring, status and logs](17-monitoring.md).

### `[log]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `log.level` | string | `info` | | restart | Lowest level that is written: `error`, `warn`, `info` or `debug`. |
| `log.format` | string | `text` | | restart | Log format: `text` or `json`. |
| `log.auth` | boolean | `true` | | restart | Write one auth record for every authentication. |

### `[log.trace]`

For a limited time, write the full requests of some users or clients to the log. It is temporary: you must write `minutes`, and it stops by itself when the time is up. It takes effect on reload, so you can start or end tracing without a restart.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `log.trace.users` | array (string) | `[]` | conditional | yes | `User-Name` values to trace; only exact matches count. `users` and `clients` cannot both be empty. |
| `log.trace.clients` | array (string) | `[]` | conditional | yes | Names of RADIUS clients to trace; each must be a `name` in `[[radius.clients]]`. |
| `log.trace.minutes` | integer | — | yes | yes | How long tracing lasts, 1 to 1440 minutes. |

---

## runtime

Main chapter: [19 Performance tuning and load testing](19-performance.md).

### `[runtime]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `runtime.pin_threads` | boolean | `false` | | restart | Pin each worker thread to its own CPU; works only on Linux. |

---

## shutdown

Main chapter: [04 Configuration file basics](04-configuration.md).

### `[shutdown]`

How the server stops after it receives `SIGTERM` or `SIGINT`. The configuration in effect at the moment stopping starts is the one that counts, so reload applies it.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `shutdown.ready_delay_ms` | integer | `0` for a single node, `5000` when `cluster.nodes` has more than one | | yes | After the stop signal, the time during which `/readyz` reports that the server is stopping while it still answers requests as usual, in milliseconds, at most 60000. It gives the load balancer time to move traffic away. |
| `shutdown.drain_ms` | integer | `5000` | | yes | After the server stops accepting new requests, how long it waits at most for requests in progress, in milliseconds, at most 60000. |

---

## accounts

Main chapter: [07 User accounts and auth chains](07-users-and-chains.md).

### `[accounts]`

The SQL that the `rradiusd user …` command runs. `?1` is the account name and `?2` is the password value to store. If you leave out a statement, the matching sub-command cannot be used.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `accounts.scheme` | string | `bcrypt` | | command | How `user add` and `user passwd` store the password: `bcrypt`, `sha512-crypt`, `nthash` or `clear`. MS-CHAP and PEAP need `nthash`; CHAP needs `clear`. |
| `accounts.add` | string | — | | command | SQL that `user add` runs; `?1` account name, `?2` password value. |
| `accounts.passwd` | string | — | | command | SQL that `user passwd` runs; `?1` account name, `?2` password value. |
| `accounts.enable` | string | — | | command | SQL that `user enable` runs; `?1` account name. |
| `accounts.disable` | string | — | | command | SQL that `user disable` runs; `?1` account name. It has an effect only together with a `user_query` that skips disabled accounts. |
| `accounts.delete` | string | — | | command | SQL that `user delete` runs; `?1` account name. |
| `accounts.show` | string | — | | command | SQL that `user show` runs and prints as it is; `?1` account name. Do not select the password column. |

---

## sessions

Main chapter: [13 Accounting and sessions](13-accounting.md).

### `[sessions]`

SQL that the `rradiusd session …` command runs and prints as it is. Both statements can only read: they must start with `SELECT` or `WITH`.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `sessions.list` | string | — | | command | SQL for `session list`: the sessions that are online now. No parameters. |
| `sessions.show` | string | — | | command | SQL for `session show`: the sessions of one user; `?1` is the user name. |

---

## auth

Main chapter: [07 User accounts and auth chains](07-users-and-chains.md); the `ldap` and `netlogon` handlers are in [08 LDAP and Active Directory](08-ldap-and-ad.md). All of `[auth]` can be reloaded.

When `[auth]` is present, logins go to the auth chain: `[[auth.handlers]]` is the default chain and `[auth.chains.<name>]` is a named chain, which a client selects with `auth`. `[auth]` must have at least one handler or one named chain, or `check` fails.

### `[auth]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.policy` | string | `first_match` | | yes | How the default chain combines the results of its handlers: `first_match` uses the first handler that has an answer; `all_accept` runs every handler, rejects if any one rejects, and needs at least one accept. |
| `auth.handlers` | table array | — | conditional | yes | Handlers of the default chain, tried in order. Required when there are no named chains. The fields depend on `type`; see the tables for each handler below. |
| `auth.allow_mschapv1` | boolean | `false` | | yes | Whether the auth chain handles MS-CHAPv1 logins. When not written or `false`, MS-CHAPv1 is rejected before any handler runs and does not count toward `auth.lockout`; PAP, CHAP, MS-CHAPv2 and EAP are not affected. |

`auth.policy` and `[auth.username]` apply only to the default chain. If you write them without `[[auth.handlers]]`, `check` fails, for example `auth.username is set but the default chain has no [[auth.handlers]] entries`; a named chain writes its own `policy` and `username`.

### `[auth.username]`

Normalization of the account name before the default chain gives it to the handlers. Without this table, the name is used as it was received.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.username.strip_realm` | string | `none` | | yes | Which realm to remove: `none`, `suffix` (the part from the last `@`), `prefix` (the part up to the last backslash, for example `REALM\`) or `both` (prefix first, then suffix). |
| `auth.username.lowercase` | boolean | `false` | | yes | Convert the account name to lower case. |
| `auth.username.allowed` | string | — | | yes | A regular expression that the whole normalized name must match; a name that does not match is rejected. |

### `[auth.lockout]`

Limits login failures. If the same key (account plus source device) fails `max_failures` times within `window_seconds`, requests are rejected directly for `lock_seconds` without checking the password. Without this table, failures are not counted.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.lockout.max_failures` | integer | `10` | | yes | How many failures within the window lock this key; must be greater than 0. |
| `auth.lockout.window_seconds` | integer | `300` | | yes | How long counting lasts from the first failure, in seconds; must be greater than 0. |
| `auth.lockout.lock_seconds` | integer | `300` | | yes | How long the lock lasts, in seconds; must be greater than 0. |
| `auth.lockout.max_entries` | integer | `100000` | | yes | Most keys each node remembers; must be greater than 0. When full, expired ones are dropped first, then the oldest unlocked ones; locked ones are not dropped. |
| `auth.lockout.account_max_failures` | integer | — (no total limit per account) | | yes | Total failures of one account from all sources; when reached, the account is locked for all sources. Must be greater than `max_failures`. |
| `auth.lockout.max_in_flight` | integer | 5 times `max_failures` | | yes | Most logins of the same key that can wait for a decision at the same time; more are rejected directly. Must be greater than 0. The total limit per account scales in proportion to `account_max_failures`. |

> **Warning:** If you set `account_max_failures`, someone guessing passwords can lock other people's accounts for all sources just by getting the password wrong on purpose.

### `[auth.password_limits]`

The most computation cost a stored password hash may demand in one comparison. A hash over the limit is not computed and is treated as a wrong password. Every key has a default, so the limits apply even if you do not write this table. Every value must be greater than 0.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.password_limits.pbkdf2_iterations` | integer | `5000000` | | yes | Upper limit on iterations for PBKDF2 and X-PBKDF2 hashes. |
| `auth.password_limits.crypt_rounds` | integer | `1000000` | | yes | Upper limit on rounds for `$5$`, `$6$`, `$sha1$` and extended DES (starting with `_`) crypt hashes. |
| `auth.password_limits.bcrypt_cost` | integer | `15` | | yes | Upper limit on cost for bcrypt hashes; each +1 doubles the work. |
| `auth.password_limits.argon2_memory_kib` | integer | `262144` (256 MiB) | | yes | Upper limit on memory for argon2 hashes, in KiB. The number of comparisons in progress equals the number of CPUs, so the worst case is about this value times the number of CPUs. |
| `auth.password_limits.argon2_iterations` | integer | `10` | | yes | Upper limit on iterations for argon2 hashes. |
| `auth.password_limits.argon2_lanes` | integer | `16` | | yes | Upper limit on parallelism for argon2 hashes. |

### `[[auth.handlers]]`: sql handler

`type = "sql"`: looks up the account and password in the cluster database or in one of the `[databases.<name>]`.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[sql].type` | string | — | yes | yes | Always `sql`. |
| `auth.handlers[sql].user_query` | string | — | yes | yes | SQL that looks up the user, with exactly one `?` (the account name); the first column is the stored password. |
| `auth.handlers[sql].reply_query` | string | — | | yes | SQL that gets the reply attributes, with exactly one `?`, returning two columns `(attribute, value)`. |
| `auth.handlers[sql].certificate_query` | string | — | | yes | Query used instead after an EAP-TLS certificate passes; if it finds a row, the login is accepted. When not set, `user_query` is used. |
| `auth.handlers[sql].default_user` | string | — | | yes | When the user is not found, check the password against this account instead; anyone who knows this password passes. |
| `auth.handlers[sql].database` | string | — (the cluster database) | | yes | In which `[databases.<name>]` to run the queries; the placeholder is still `?`. |
| `auth.handlers[sql].timeout_ms` | integer | `5000` | | yes | Time limit for one query, in milliseconds; must be greater than 0. |

### `[[auth.handlers]]`: lua handler

`type = "lua"`: another Lua script decides.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[lua].type` | string | — | yes | yes | Always `lua`. |
| `auth.handlers[lua].script` | path | — | yes | yes | The script file for this handler. |
| `auth.handlers[lua].verifies_password` | boolean | `false` | | yes | The script checks the password itself. When `true`, the chain can have only this handler, and its accept also counts as proof of the password inside EAP. |

### `[[auth.handlers]]`: ldap handler

`type = "ldap"`: finds the user in an LDAP directory and checks the password.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[ldap].type` | string | — | yes | yes | Always `ldap`. |
| `auth.handlers[ldap].urls` | array (string) | — | yes | yes | Server addresses (`ldap://` or `ldaps://`), at least one, tried in order. |
| `auth.handlers[ldap].bind_dn` | string | — | yes | yes | The DN to bind as when searching for users. |
| `auth.handlers[ldap].bind_password` | string | — | conditional | yes | Password of `bind_dn`. Set exactly one of this and `bind_password_file`. |
| `auth.handlers[ldap].bind_password_file` | path | — | conditional | yes | File that holds the password of `bind_dn`. Set exactly one of this and `bind_password`. |
| `auth.handlers[ldap].base_dn` | string | — | yes | yes | Where the user search starts. |
| `auth.handlers[ldap].filter` | string | — | yes | yes | Search filter that finds the user; `{user}` is replaced with the escaped account name. |
| `auth.handlers[ldap].certificate_filter` | string | — | | yes | Search filter used instead after an EAP-TLS certificate passes; if it finds an entry, the login is accepted. |
| `auth.handlers[ldap].password_check` | string | `bind` | | yes | `bind` checks the password by binding as the user; `attribute` compares against the value of `password_attribute`. |
| `auth.handlers[ldap].password_attribute` | string | — | conditional | yes | The attribute that holds the password when `password_check = "attribute"`. |
| `auth.handlers[ldap].reply_attributes` | table (string to string) | — | | yes | Key is the reply attribute name; value is the LDAP attribute of the user that provides the attribute value. Added in the order written. |
| `auth.handlers[ldap].ca_file` | path | — | | yes | CA certificates trusted for `ldaps://` and StartTLS. |
| `auth.handlers[ldap].allow_plaintext` | boolean | `false` | | yes | Allow `ldap://` without StartTLS. The password is sent in clear text. |
| `auth.handlers[ldap].pool_size` | integer | `4` | | yes | Number of server connections kept open; must be greater than 0. |
| `auth.handlers[ldap].timeout_ms` | integer | `5000` | | yes | Time limit for one LDAP query, in milliseconds; must be greater than 0. |
| `auth.handlers[ldap].required_groups` | array (string) | — (no group required) | | yes | The user must belong to one of these groups to be accepted. If written, it needs at least one. |
| `auth.handlers[ldap].group_attribute` | string | — | | yes | The user attribute that lists the groups the user belongs to, for example `memberOf`. |
| `auth.handlers[ldap].group_base_dn` | string | — | | yes | Where the group search starts when you use `group_filter`. |
| `auth.handlers[ldap].group_filter` | string | — | | yes | Search filter that finds the groups the user belongs to; `{dn}` is the DN of the user. |
| `auth.handlers[ldap].group_reply` | table array | — | | yes | Reply attributes to add when the user belongs to a group. The fields are in the next two rows. |
| `auth.handlers[ldap].group_reply[].group` | string | — | yes | yes | The group that gets these attributes added. |
| `auth.handlers[ldap].group_reply[].attributes` | table (string to string) | — | | yes | The reply attributes to add, with the attribute name as key. At least one. |

### `[[auth.handlers]]`: netlogon handler

`type = "netlogon"`: checks the login through the Netlogon service of an Active Directory domain controller. See [08](08-ldap-and-ad.md) for which login methods are supported.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.handlers[netlogon].type` | string | — | yes | yes | Always `netlogon`. |
| `auth.handlers[netlogon].domain` | string | — | yes | yes | NetBIOS name of the domain. |
| `auth.handlers[netlogon].servers` | array (string) | — | yes | yes | DNS or NetBIOS names of the domain controllers, at least one, tried in order. IP addresses cannot be used: the logon request must carry the name of the DC. |
| `auth.handlers[netlogon].port` | integer | — (ask the endpoint mapper on port 135 first) | | yes | Fixed Netlogon port of the domain controller; must be greater than 0. |
| `auth.handlers[netlogon].computer_name` | string | — | yes | yes | The computer account of this node in the domain, without the trailing `$`, at most 15 characters. Each node uses its own account: nodes that share an account break each other's secure channel. |
| `auth.handlers[netlogon].machine_password_file` | path | — | yes | yes | File that holds the password of the computer account; a trailing newline is removed. |
| `auth.handlers[netlogon].connections` | integer | `4` | | yes | Most connections kept open to each domain controller; must be greater than 0. |
| `auth.handlers[netlogon].timeout_ms` | integer | `2000` | | yes | Time limit for one domain controller call, in milliseconds; must be greater than 0. |
| `auth.handlers[netlogon].required_groups` | array (string) | — (no group required) | | yes | The user must belong to one of these groups to be accepted; groups are given as SIDs (`S-1-5-21-…`). If written, it needs at least one. |
| `auth.handlers[netlogon].group_reply` | table array | — | | yes | Reply attributes to add when the user belongs to a group. |
| `auth.handlers[netlogon].group_reply[].group` | string | — | yes | yes | The SID of the group. |
| `auth.handlers[netlogon].group_reply[].attributes` | table (string to string) | — | | yes | The reply attributes to add, with the attribute name as key. At least one. |

### `[auth.chains.<name>]`

A named auth chain. You choose `<name>`, and a client selects it with `auth = "<name>"`. The fields are the same as the keys of the same name in `[auth]`; every chain needs at least one handler.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `auth.chains.<name>.policy` | string | `first_match` | | yes | Same as `auth.policy`. |
| `auth.chains.<name>.handlers` | table array | — | yes | yes | Handlers of this chain. The four kinds of handler and every key of theirs (including defaults) are exactly the same as `auth.handlers[sql]`, `auth.handlers[lua]`, `auth.handlers[ldap]` and `auth.handlers[netlogon]`. |
| `auth.chains.<name>.username` | table | — | | yes | Name normalization of this chain itself; the keys are in the next three rows. Without it, names are not normalized, and `[auth.username]` is not inherited. |
| `auth.chains.<name>.username.strip_realm` | string | `none` | | yes | Same as `auth.username.strip_realm`. |
| `auth.chains.<name>.username.lowercase` | boolean | `false` | | yes | Same as `auth.username.lowercase`. |
| `auth.chains.<name>.username.allowed` | string | — | | yes | Same as `auth.username.allowed`. |

---

## databases

Main chapter: [09 External databases and named queries](09-external-databases.md). Any change to `[databases]` (including adding one) needs a restart.

### `[databases.<name>]`

An external MySQL or PostgreSQL. You choose `<name>`; the `database` key of an sql handler and Lua scripts use the database by this name.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `databases.<name>.kind` | string | — | yes | restart | `mysql` or `postgresql`. |
| `databases.<name>.host` | string | — | yes | restart | Host name or address of the database server; with `tls = "verify"`, the certificate must carry this name. |
| `databases.<name>.port` | integer | `3306` for mysql, `5432` for postgresql | | restart | Port of the database server. |
| `databases.<name>.database` | string | — | yes | restart | Name of the database to use. |
| `databases.<name>.user` | string | — | yes | restart | Account used to log in to the database. |
| `databases.<name>.password` | string | — | | restart | Password of `user`; set at most one of this and `password_file`. For MySQL with `tls = "off"`, the account must use `mysql_native_password`. |
| `databases.<name>.password_file` | path | — | | restart | File that holds the password; set at most one of this and `password`. |
| `databases.<name>.tls` | string | `verify` | | restart | `verify`: encrypt and check the certificate chain and the host name; `off`: no encryption, and a warning is logged when `host` is not a loopback address. |
| `databases.<name>.ca_file` | path | — (system root certificates) | | restart | CA certificates (PEM) trusted when `tls = "verify"`. |
| `databases.<name>.pool_size` | integer | `8` | | restart | Most connections kept to this database, at least 1. |
| `databases.<name>.connect_timeout_ms` | integer | `5000` | | restart | Time limit for opening one connection, in milliseconds; must be greater than 0. |
| `databases.<name>.required_for_ready` | boolean | `false` | | restart | Whether `/readyz` returns 503 when this database cannot be reached. Every 5 seconds it runs `SELECT 1` on a dedicated connection; two failures in a row count as unreachable, and one success recovers it. |

---

## queries

Main chapter: [09 External databases and named queries](09-external-databases.md). Any change to `[queries]` needs a restart.

### `[queries.<name>]`

Read-only SQL that every node runs periodically in the background; the result becomes samples of the metric `rradius_query_<name>` (gauge). `rradiusd query <name>` also runs it.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `queries.<name>.sql` | string | — | yes | restart | The SQL to run. Read-only (starts with `SELECT` or `WITH`), with no parameters. |
| `queries.<name>.value` | string | `value` | | restart | The column that holds the sample value, integer or real; rows where this column is NULL are skipped. |
| `queries.<name>.labels` | array (string) | `[]` | | restart | Columns whose values become sample labels; the label name is the column name. They cannot be numeric columns and cannot repeat. |
| `queries.<name>.help` | string | empty string (the output uses a text that contains the query name instead) | | restart | The HELP text of the metric. |
| `queries.<name>.interval_ms` | integer | `30000` | | restart | Interval from the start of one run to the start of the next, in milliseconds, at least 5000. |
| `queries.<name>.timeout_ms` | integer | `5000` | | restart | Longest time for one run, in milliseconds, 1 to `interval_ms`. |
| `queries.<name>.max_rows` | integer | `200` | | restart | Most rows of one run that become samples, 1 to 10000; extra rows are dropped and counted. |

---

## radius

Main chapter: [05 RADIUS service and NAS clients](05-radius-clients.md). Tuning of `sockets`, `recv_buffer_bytes` and `max_*_inflight` is in [19](19-performance.md); the accounting spool and the accounting log are in [13](13-accounting.md); `[radius.eap]` is in [11](11-eap.md).

### `[radius]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `radius.bind` | IP address, or array of IP addresses | `0.0.0.0` | | restart | Address on which the RADIUS ports listen, one or a list; as an array it needs at least one. Every address opens both the authentication and the accounting port. |
| `radius.auth_port` | integer | `1812` | | restart | Authentication port. |
| `radius.acct_port` | integer | `1813` | | restart | Accounting port. |
| `radius.sockets` | integer | equal to the number of worker threads on Linux, `1` on other platforms (depends on the machine) | | restart | Number of receiving sockets for each address and port; must be greater than 0. Only Linux can spread packets over several sockets; on other platforms it can only be 1. |
| `radius.recv_buffer_bytes` | integer | — (system default) | | restart | `SO_RCVBUF` of each RADIUS socket, in bytes, 4096 to 1073741824. |
| `radius.max_auth_inflight` | integer | `4096` | | restart | Number of authentication requests handled at the same time; extra requests are dropped. |
| `radius.max_acct_inflight` | integer | `4096` | | restart | Number of accounting requests handled at the same time; extra requests are dropped. |
| `radius.status_server` | boolean | `true` | | restart | Answer Status-Server requests (RFC 5997). |
| `radius.require_message_authenticator` | string | `yes` | | yes | Whether an Access-Request must have a Message-Authenticator: `yes`, `no`, or `auto` (decided by the first Access-Request from this client after startup). A client can override it. |
| `radius.limit_proxy_state` | string | `yes` | | yes | Drop an Access-Request that has Proxy-State but no Message-Authenticator: `yes`, `no` or `auto`. A client can override it. |
| `radius.clients` | table array | — | | yes | RADIUS clients (NAS) that are allowed to send requests. The fields are in the next section. |
| `radius.coa_timeout_ms` | integer | `1000` | | yes | **No effect in the core**; see [Settings the core does not support or that have no effect](#settings-the-core-does-not-support-or-that-have-no-effect). |
| `radius.coa_attempts` | integer | `3` | | yes | **No effect in the core**; same as above. |

### `[[radius.clients]]`

NAS that are allowed to send requests. One item can be one address or one network range. A change to clients (including the shared secret) takes effect on reload.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `radius.clients[].address` | string | — | yes | yes | IP address or CIDR range of the client, for example `10.0.0.0/24`. |
| `radius.clients[].name` | string | the value of `address` | | yes | Name used in logs and metrics; `log.trace.clients` uses it too. |
| `radius.clients[].secret` | string | — | conditional | yes | Shared secret. Set exactly one of this and `secret_file`. |
| `radius.clients[].secret_file` | path | — | conditional | yes | File that holds the shared secret. Set exactly one of this and `secret`. |
| `radius.clients[].auth` | string | — (the default chain) | | yes | Which `[auth.chains.<name>]` this client's logins use instead. |
| `radius.clients[].require_message_authenticator` | string | the value of `radius.require_message_authenticator` | | yes | Overrides the `[radius]` setting: `yes`, `no` or `auto`. |
| `radius.clients[].limit_proxy_state` | string | the value of `radius.limit_proxy_state` | | yes | Overrides the `[radius]` setting: `yes`, `no` or `auto`. |
| `radius.clients[].eap_methods` | array (string) | `radius.eap.methods` | conditional | yes | EAP methods offered to this client, in order: `tls`, `peap`, `ttls`. Needs `[radius.eap]`. When `radius.eap.methods` has two or more methods, every client must write it; a client that does not use EAP writes `[]`. |
| `radius.clients[].authorize_group` | string | — | | yes | **For TACACS+ only**; `check` fails when it is written on a RADIUS client. |
| `radius.clients[].tls_name` | string | — | | yes | **No effect in the core**. |
| `radius.clients[].coa_port` | integer | `3799` | | yes | **No effect in the core**. |
| `radius.clients[].coa_sender` | boolean | `false` | | — | **Not supported by the core**; `check` fails if you write it. |
| `radius.clients[].max_radsec_connections` | integer | — | | — | **Not supported by the core**; `check` fails if you write it. |
| `radius.clients[].mfa` | string | — | | — | **Not supported by the core**; `check` fails if you write it. |

For the actual behavior and the error messages of the last five keys, see [Settings the core does not support or that have no effect](#settings-the-core-does-not-support-or-that-have-no-effect).

### `[radius.accounting_spool]`

When the database cannot be written, accounting requests are first written to the accounting spool on disk (a temporary store that waits for replay) and replayed in order later. Without this table, when the cluster database is briefly unavailable you can only rely on the NAS to retransmit. A change to the whole table needs a restart.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `radius.accounting_spool.dir` | path | — | yes | restart | Directory of the temporary store. |
| `radius.accounting_spool.max_bytes` | integer | `1073741824` (1 GiB) | | restart | Upper limit on the size of the store, in bytes; must be greater than 0. |
| `radius.accounting_spool.replay_interval_ms` | integer | `5000` | | restart | After a replay fails, the wait before the next replay, in milliseconds; must be greater than 0. |
| `radius.accounting_spool.replay_window` | integer | `1` | | restart | Number of entries replayed at the same time, 1 to 1024. The default is 1, strictly in order, because a script may expect the records of one session to arrive in order. |

### `[radius.accounting_log]`

For every accounting request answered, append one line of JSON to a local file.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `radius.accounting_log.file` | path | — | yes | yes | File to append to; its directory must already exist, and if the file exists it must be appendable. The file is reopened on `SIGHUP`, so it works with log rotation, and you can also reload to switch to another file name. |
| `radius.accounting_log.queue` | integer | `65536` | | restart | Upper limit on entries waiting to be written, 1 to 16777216; extra entries are dropped and counted. |

Adding or removing the whole `[radius.accounting_log]` needs a restart.

### `[radius.eap]`

EAP for 802.1X: the server certificate, the methods offered and the limit on unfinished conversations. The whole table takes effect on reload.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `radius.eap.methods` | array (string) | `["tls"]` | | yes | EAP methods offered; the first is the method proposed at the start: `tls`, `peap`, `ttls`, at least one. With `peap` or `ttls` you need `[auth]`. |
| `radius.eap.cert_file` | path | — | yes | yes | Server certificate chain (PEM). |
| `radius.eap.key_file` | path | — | yes | yes | Private key of the server certificate (PEM). |
| `radius.eap.client_ca_file` | path | — | conditional | yes | The CA that issues client certificates. Required when `methods` has `tls`. |
| `radius.eap.crl_file` | path | — | | yes | CRL (revocation list) for client certificates; used only for `tls`. |
| `radius.eap.match_identity` | boolean | `true` | | yes | EAP-TLS: the certificate must correspond to the user in the EAP identity. |
| `radius.eap.fragment_size` | integer | `1000` | | yes | Upper limit of an EAP fragment, 256 to 1400 bytes. |
| `radius.eap.session_timeout_ms` | integer | `30000` | | yes | How long an unfinished EAP conversation is kept, in milliseconds; must be greater than 0. |
| `radius.eap.max_sessions` | integer | `4096` | | yes | Number of unfinished EAP conversations kept in total. |
| `radius.eap.max_sessions_per_client` | integer | `1024` | | yes | Number of unfinished EAP conversations kept for each client. |
| `radius.eap.max_sessions_per_device` | integer | `4` | | yes | Number of unfinished EAP conversations kept for each device (`Calling-Station-Id`). |
| `radius.eap.resumption_ms` | integer | — | | — | **Not supported by the core**; `check` fails if you write it. |
| `radius.eap.shared_tickets` | boolean | — | | — | **Not supported by the core**; `check` fails if you write it. |
| `radius.eap.ticket_seal_file` | path | — | | — | **Not supported by the core**; `check` fails if you write it. |
| `radius.eap.previous_ticket_seal_file` | path | — | | — | **Not supported by the core**; `check` fails if you write it. |

---

## tacacs

Main chapter: [12 TACACS+ device administration](12-tacacs.md).

### `[tacacs]`

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `tacacs.bind` | IP address | `0.0.0.0` | | restart | Address on which the TACACS+ port listens. |
| `tacacs.port` | integer | `49` | | restart | TACACS+ port (TCP). |
| `tacacs.max_connections` | integer | `1024` | | restart | Number of connections open at the same time; extra connections are closed immediately. |
| `tacacs.max_connections_per_source` | integer | `64` | | restart | Number of connections open at the same time from one source address; extra connections are closed immediately. |
| `tacacs.max_body_len` | integer | `65536` | | restart | Upper limit on the packet body, in bytes; a larger one closes the connection. Must be greater than 0. |
| `tacacs.idle_timeout_ms` | integer | `300000` | | restart | If no complete packet arrives within this time, the connection is closed, in milliseconds. |
| `tacacs.enable` | string | `deny` | | yes | How enable is checked: `deny` always rejects and does not ask for a password; `login` treats it as a login, decided by the auth chain or the script. |
| `tacacs.authorize_groups` | table | `{}` | | yes | Authorize groups: a name maps to a list of `permit`/`deny` rules, used to authorize commands and services. See the next section. |
| `tacacs.clients` | table array | — | | yes | TACACS+ clients (devices) that are allowed to connect. The fields are below. |

### `[tacacs.authorize_groups]`

Each key is the name of an authorize group, and the value is an array of rule strings, for example:

```toml
# conf.d/tacacs.toml
[tacacs.authorize_groups]
ops = ["permit service=shell"]

[[tacacs.clients]]
address = "10.0.0.0/24"
secret_file = "tacacs.secret"   # openssl rand -hex 32 > tacacs.secret
authorize_group = "ops"
```

A TACACS+ client selects a group with `authorize_group = "ops"`. If `[tacacs]` is present there must be at least one `[[tacacs.clients]]`, or `check` fails: `[tacacs] is enabled but has no [[tacacs.clients]] entries`. If you write a group name that does not exist, `check` fails. The syntax of the rules is in [12](12-tacacs.md).

### `[[tacacs.clients]]`

The fields of a TACACS+ client share one definition with `[[radius.clients]]`, so the schema lists the same 14 keys; but on a TACACS+ client only the first six are useful, and writing any of the others makes `check` fail.

| Key | Type | Default | Required | reload | Description |
| --- | --- | --- | --- | --- | --- |
| `tacacs.clients[].address` | string | — | yes | yes | IP address or CIDR range of the device. |
| `tacacs.clients[].name` | string | the value of `address` | | yes | Name used in logs and metrics. |
| `tacacs.clients[].secret` | string | — | conditional | yes | Shared secret. Set exactly one of this and `secret_file`. |
| `tacacs.clients[].secret_file` | path | — | conditional | yes | File that holds the shared secret. Set exactly one of this and `secret`. |
| `tacacs.clients[].auth` | string | — (the default chain) | | yes | Which `[auth.chains.<name>]` this device's logins use instead. |
| `tacacs.clients[].authorize_group` | string | — | | yes | Which group in `tacacs.authorize_groups` supplies the rules that authorize this device. |
| `tacacs.clients[].require_message_authenticator` | string | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].limit_proxy_state` | string | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].eap_methods` | array (string) | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].coa_port` | integer | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].coa_sender` | boolean | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].max_radsec_connections` | integer | — | | — | **RADIUS only**: `check` fails. |
| `tacacs.clients[].tls_name` | string | — | | — | **Not supported by the core**: `check` fails. |
| `tacacs.clients[].mfa` | string | — | | — | **Not supported by the core**: `check` fails. |

---

## Settings the core does not support or that have no effect

`rradiusd` and the enterprise edition share the same configuration reader, so some keys appear in the `schema` of the core but are either rejected or have no effect in the core. The results below were measured one by one with `rradiusd check`.

### Keys that are rejected when written

`check` exits with code 2 and the messages are as follows (`<file>` is the path of the configuration file and `<name>` is the client name):

| Key | Message of `check` |
| --- | --- |
| `radius.clients[].coa_sender` | `<file>: radius client '<name>': coa_sender needs RadSec, which needs rradius-enterprise` |
| `radius.clients[].max_radsec_connections` | `<file>: radius client '<name>': max_radsec_connections applies only to RadSec, which needs rradius-enterprise` |
| `radius.clients[].mfa` | `<file>: radius client '<name>': mfa needs rradius-enterprise` |
| `radius.clients[].authorize_group` | `<file>: radius client '<name>': authorize_group applies only to TACACS+ clients` |
| `radius.eap.resumption_ms` | `<file>: radius.eap.resumption_ms: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.shared_tickets` | `<file>: radius.eap.shared_tickets: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.ticket_seal_file` | `<file>: radius.eap.ticket_seal_file: not supported by this program (session resumption needs rradius-enterprise)` |
| `radius.eap.previous_ticket_seal_file` | `<file>: radius.eap.previous_ticket_seal_file: not supported by this program (session resumption needs rradius-enterprise)` |
| `tacacs.clients[].tls_name` | `<file>: tacacs client '<name>': tls_name needs [tacacs.tls]` (and `[tacacs.tls]` is also rejected in the core, see below) |
| `tacacs.clients[].mfa` | `<file>: tacacs client '<name>': mfa needs rradius-enterprise` |
| `tacacs.clients[].require_message_authenticator`, `limit_proxy_state`, `eap_methods`, `coa_port`, `coa_sender`, `max_radsec_connections` | `<file>: tacacs client '<name>': <key> applies only to RADIUS clients` |

EAP methods are the same. When `radius.eap.methods` contains `aka-prime`, the message is `radius.eap.methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)`; when a client's `eap_methods` contains `aka-prime`, it is `radius client '<name>': eap_methods: aka-prime is not supported by this program (EAP-AKA' needs rradius-enterprise)`. When you write `sim`, both places give `'sim' is not a method this server offers; it has tls, peap and ttls` (the client one has `radius client '<name>': eap_methods: ` in front in the same way).

### Keys that are accepted but have no effect

`check` accepts these keys and gives no warning, but the core program never uses them. The core manual does not use them.

| Key | Description |
| --- | --- |
| `radius.clients[].tls_name` | The name that the enterprise RadSec matches against the client certificate. The core has no RadSec. Written together with `address` it is accepted but not used; written alone without `address`, `check` fails: `radius client '<name>' has no address, so it can only use RadSec, which needs rradius-enterprise`. |
| `radius.clients[].coa_port` | The port the enterprise edition uses when it sends CoA and Disconnect to this client (default 3799). The core does not send CoA. |
| `radius.coa_timeout_ms` | How long the enterprise edition waits for a reply to each CoA or Disconnect (default 1000 ms). |
| `radius.coa_attempts` | How many times the enterprise edition tries a CoA or Disconnect, including the first (default 3). |

### Sections rejected by the core (not in the core schema)

These sections and keys are not in the output of `rradiusd schema`; they belong to the enterprise edition. When the core reads them, `check` exits with code 2, and the message has the form `<section>: not supported by this program (<feature> needs rradius-enterprise)`:

| Section or key | Description in parentheses |
| --- | --- |
| `[proxy]` | `proxying needs rradius-enterprise` |
| `[mfa]` | `two-factor login needs rradius-enterprise` |
| `[diameter]` | `Diameter needs rradius-enterprise` |
| `[accounting_export]` | `accounting export needs rradius-enterprise` |
| `[cluster.backup]` | `backups need rradius-enterprise` |
| `[radius.tls]` | `RadSec needs rradius-enterprise` |
| `[radius.coa]` | `CoA needs rradius-enterprise` |
| `[radius.dynamic_clients]` | `dynamic clients need rradius-enterprise` |
| `[radius.dpsk]` | `DPSK needs rradius-enterprise` |
| `[radius.eap.aka]` | `EAP-AKA' needs rradius-enterprise` |
| `[accounts.aka]` | `EAP-AKA' needs rradius-enterprise` |
| `[accounts.dpsk]` | `DPSK needs rradius-enterprise` |
| `[accounts.clients]` | `dynamic clients need rradius-enterprise` |
| `[tacacs.tls]` | `TACACS+ over TLS needs rradius-enterprise` |
| `radius.eap.ocsp`, `radius.eap.ocsp_url` | `OCSP needs rradius-enterprise` (`methods` must have `tls`; without `tls` you get `radius.eap.ocsp only applies to tls, which methods does not have` first) |

An actual line looks like this:

```console
$ rradiusd check --config rradius.toml
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ echo $?
2
```

---

## Common mistakes

**A mistyped key name.**

- Symptom: `check` exits with code 2, for example (in `[metrics]`, `port` mistyped as `prot`):

  ```console
  $ rradiusd check --config rradius.toml
  rradius.toml: line 15: `prot`: unknown field `prot`, expected `bind` or `port`
  ```

- Cause: every table rejects unknown keys.
- Fix: correct it using the line number and the key names in the message, and compare with the tables in this appendix.

**A value that is not an allowed option.**

- Symptom: for example `auth.policy = "any"`:

  ```console
  $ rradiusd check --config rradius.toml
  rradius.toml: line 31: `policy`: unknown variant, expected `first_match` or `all_accept`
  ```

- Fix: change it to one of the values the message lists.

**An included file declares a table that the main file already has.**

- Symptom: `rradius.toml: line 413: duplicate key`.
- Cause: the file in `include` is treated as appended to the main file, and the same table cannot be declared twice.
- Fix: move the settings into the table already in the main file, or delete that table from the main file and keep it only in the included file.

**A required section or secret is missing.**

- `the [lua] section is required`, `the [cluster] section is required`: add that section; see [lua](#lua) and [cluster](#cluster).
- `at least one of [radius] or [tacacs] must be configured`: you need at least one of them.
- `exactly one of cluster.secret_api or cluster.secret_api_file must be set`: write exactly one of the two keys.
- `radius client '127.0.0.1': exactly one of secret or secret_file must be set`: write exactly one of `secret` and `secret_file` on the client.
- `radius client '': address is required, or tls_name for a client without a fixed address`: add `address`.

**An EAP condition is not met.**

- `radius.eap.cert_file is required`: `[radius.eap]` must have `cert_file` and `key_file`.
- `radius.eap.client_ca_file is required`: when `methods` has `tls`, you need `client_ca_file`.
- `radius.eap.methods has a method whose password only a chain can check, and there is no [auth] section`: with `peap` or `ttls` you need `[auth]`.
- `radius client 'example': eap_methods is required when radius.eap.methods has more than one method; [] for a client without EAP`: write `eap_methods` on every client.
- `radius client 'example': eap_methods needs [radius.eap]`: set up `[radius.eap]` first.

**A warning that the cluster ports listen on all interfaces.**

- Symptom: `check` and `serve` print `WARN rradius_core::config: cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, with nothing but their shared secrets to protect them; …`.
- Cause: when you have `[[cluster.nodes]]` and do not write `cluster.listen`, it listens on `0.0.0.0` by default.
- Fix: set `cluster.listen` to a private address of this node; if you do want all interfaces, write `"0.0.0.0"` and the warning goes away. See [16](16-cluster.md) and [20](20-security.md).

**You changed the configuration and it did not take effect after reload.**

- Symptom: the log has a message such as `configuration reloaded from …; these need a restart to apply: [cluster], radius.auth_port`.
- Cause: the listed keys have "restart" in the reload column of this appendix.
- Fix: schedule a restart. Until then these keys keep the values they had at startup, and every later reload lists them again.

**Enterprise settings put into the core.**

- Symptom: `… not supported by this program (… needs rradius-enterprise)`, exit code 2.
- Fix: see [Settings the core does not support or that have no effect](#settings-the-core-does-not-support-or-that-have-no-effect).

---

## Related chapters

- [04 Configuration file basics](04-configuration.md): structure of the configuration file, `include`, `check`, `config show`, `config diff`, reload
- [05 RADIUS service and NAS clients](05-radius-clients.md): `[radius]` and `[[radius.clients]]`
- [06 Dictionary and attributes](06-dictionary.md): `dictionary`
- [07 User accounts and auth chains](07-users-and-chains.md): `[auth]`, `[accounts]`
- [08 LDAP and Active Directory](08-ldap-and-ad.md): ldap and netlogon handlers
- [09 External databases and named queries](09-external-databases.md): `[databases]`, `[queries]`
- [10 Writing policies in Lua](10-lua-policies.md): `[lua]`, `[lua.http]`
- [11 802.1X and EAP](11-eap.md): `[radius.eap]`
- [12 TACACS+ device administration](12-tacacs.md): `[tacacs]`
- [13 Accounting and sessions](13-accounting.md): accounting spool, accounting log, `[sessions]`
- [16 Clusters and high availability](16-cluster.md): `[cluster]`
- [17 Monitoring, status and logs](17-monitoring.md): `[metrics]`, `[log]`
- [19 Performance tuning and load testing](19-performance.md): `[runtime]`, `radius.sockets` and so on
- [91 Appendix B: Command reference](91-command-reference.md): the `check`, `config`, `schema` and other commands
