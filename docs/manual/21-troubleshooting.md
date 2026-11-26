# 21 Troubleshooting

> After this chapter you can:
> - Look for a problem in a fixed order: `check`, logs, trace, `status`, metrics, `tuning`
> - Tell "no response", "reject" and a TACACS+ ERROR apart, and know where to look for each
> - Go from a single error message to its cause and fix
> - Deal with the common problem types: client matching fails, shared secret mismatch, database unavailable, cluster has no leader, missing accounting records, overload, EAP stuck

Prerequisites: you have read [Chapter 03](03-quick-start.md), can send test requests with `rradius-bench` and can read the state with `status`.

In this chapter:

- [Troubleshooting order](#troubleshooting-order)
- [No response, reject, ERROR](#no-response-reject-error)
- [Configuration cannot be read](#configuration-cannot-be-read)
- [Reload fails or has no effect](#reload-fails-or-has-no-effect)
- [The server fails to start](#the-server-fails-to-start)
- [Client matching fails](#client-matching-fails)
- [Shared secret mismatch](#shared-secret-mismatch)
- [Always rejected](#always-rejected)
- [Database unavailable](#database-unavailable)
- [Cluster has no leader](#cluster-has-no-leader)
- [Missing accounting records](#missing-accounting-records)
- [Overload and UDP packet loss](#overload-and-udp-packet-loss)
- [EAP is stuck or fails](#eap-is-stuck-or-fails)
- [TACACS+ cannot connect or answers ERROR](#tacacs-cannot-connect-or-answers-error)
- [Script errors](#script-errors)
- [needs rradius-enterprise](#needs-rradius-enterprise)
- [Walkthrough: a new NAS gets no response](#walkthrough-a-new-nas-gets-no-response)
- [What to include when reporting a problem](#what-to-include-when-reporting-a-problem)
- [Common mistakes: message table](#common-mistakes-message-table)
- [Related chapters](#related-chapters)

The messages in this chapter were produced by causing the problems on purpose on a macOS machine and running `rradiusd` 0.1.0. The few that were only checked against the code are marked next to them. The timestamp at the start of a log line is replaced with `…`. The number after `os error` depends on the operating system. In this chapter 48 (address already in use) and 61 (connection refused) are macOS values and may be other numbers on your system.

## Troubleshooting order

Whatever the problem, look in this order. The first three steps usually find it:

| Step | Command or place | What to look at |
| --- | --- | --- |
| 1 | `rradiusd check --config <file>` | Whether the configuration has errors or warnings |
| 2 | The server log | `WARN`, `ERROR`; one `rradius_auth` authentication record line per authentication |
| 3 | `[log.trace]` | The full request and response of a chosen user or client |
| 4 | `rradiusd status --config <file>` | Which kind of request is discarded, and the state of the database and cluster |
| 5 | `curl -s http://127.0.0.1:9812/metrics` | Details that `status` does not list, such as per-client counters |
| 6 | `rradiusd tuning --config <file>` | Whether the operating system drops UDP packets |

### Authentication record

Every authentication writes one line:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

- **This line is there:** the request reached the server, matched a client and passed the Message-Authenticator check. The problem is usually in the auth chain or the account data. Look at `handler` and `decision` (for the exception see the third case in [Shared secret mismatch](#shared-secret-mismatch)).
- **This line is missing:** the request was dropped earlier. Look at the lines starting with `Discarded` in `status`.

Common values of `handler`:

| `handler` | Meaning |
| --- | --- |
| `sql`, `ldap`, `lua`, and so on | This handler made the decision |
| `none` | No handler gave an answer, usually because the account does not exist or is disabled |
| `lockout` | Stopped by the failed-login lockout, the password was not checked |
| `mschapv1_refused` | By default (not written, or `allow_mschapv1 = false`), MS-CHAPv1 is refused |
| `length` | The User-Password length is wrong (not 16 to 128 bytes, or not a multiple of 16) |
| `eap-peap`, `eap-tls`, and so on | The result of an EAP conversation |

The authentication record can be turned off with `[log] auth = false`; keep it on when troubleshooting. The log level is `[log] level` (`error`, `warn`, `info`, `debug`), and a change needs a restart. For details see [Chapter 17](17-monitoring.md).

### trace

The authentication record does not write attributes. To see what the NAS actually sent and what the server answered, turn on trace for that user:

```toml
# rradius.toml
[log.trace]
users = ["alice"]
minutes = 30
```

After a reload (`kill -HUP <pid>`), every request of `alice` gets two more lines:

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Session-Timeout = 3600, Filter-Id = staff"
```

`code=2` is Access-Accept and `code=3` is Access-Reject. trace writes personal data, so remove it when you are done ([Chapter 20](20-security.md)).

> **Note:** trace only records requests that passed the shared secret check. Discarded requests do not appear in trace; look at `status`.

## No response, reject, ERROR

The NAS sees only a few kinds of result, and each has different causes:

| The NAS sees | What the server did | Where to look first |
| --- | --- | --- |
| Access-Reject | Authenticated and decided to reject | The `handler` in the authentication record |
| No response (timeout) | Dropped the request, or could not answer | The `Discarded` lines of `status`, and `not answered` in the log |
| TACACS+ ERROR | Could not answer | `answered ERROR` in the log |

"No response" is deliberate. When the database is unavailable, there is no leader (with `stop_without_leader_ms` set, or when the request needs a write) or the server is overloaded, the server does not send a Reject. It lets the NAS retransmit or ask another server, instead of kicking the user out.

The RADIUS section of `status`:

```text
RADIUS
  …
  Discarded: overload        0.0/s (56)
  Discarded: unknown client  0.0/s (1)
  Discarded: invalid         0.0/s (3)
  Discarded: database        0.0/s (1)
  Discarded: no leader       0.0/s (1)
…
```

| This line grows | Cause | Section |
| --- | --- | --- |
| `Discarded: unknown client` | The source address is not in the client list | [Client matching fails](#client-matching-fails) |
| `Discarded: invalid` | Shared secret mismatch, missing Message-Authenticator, malformed packet | [Shared secret mismatch](#shared-secret-mismatch) |
| `Discarded: database` | The database needed for authentication or accounting is unavailable | [Database unavailable](#database-unavailable) |
| `Discarded: no leader` | `stop_without_leader_ms` is set and the cluster has no leader | [Cluster has no leader](#cluster-has-no-leader) |
| `Discarded: overload` | More requests are being handled at once than the limit | [Overload and UDP packet loss](#overload-and-udp-packet-loss) |

The same counters in `/metrics` are `rradius_radius_discarded_total{reason="…"}`, where `reason` is `overload`, `unknown_client`, `invalid`, `database` or `no_leader`.

When you test with `rradius-bench`, look at its counters and exit code:

| `rradius-bench` result | Exit code |
| --- | --- |
| `accept 1` | 0 |
| `reject 1` | 1 |
| `timeouts 1` (the server did not answer, or the signature of the answer does not match) | 2 |
| `timeouts 1, … socket errors 1` (the server is not running at all) | 2 |

### Separating network and shared secret from the account

When the NAS times out and you cannot tell whether the packet did not arrive, the shared secret is wrong or the account is the problem, send a Status-Server first. It does not look up an account and does not run a script, so whether it gets an answer depends only on the earlier checks:

```console
$ rradius-bench run --server <address> --secret "$(cat <secret file>)" \
    --load status-auth,concurrency=1 --requests 1
```

If you have FreeRADIUS `radclient`, you can use it too (Status-Server must carry a correct Message-Authenticator, so add `Message-Authenticator=0x00` to the input and let `radclient` compute the value):

```console
$ echo 'Message-Authenticator=0x00' | radclient -t 2 -r 1 <address>:1812 status "$(cat <secret file>)"
```

| Result | Meaning | Next step |
| --- | --- | --- |
| An answer | The packet arrives, the source address is in the client list, the shared secret is correct and `radius.status_server` is on | The problem is in the request itself: look at the `handler` in the authentication record ([Always rejected](#always-rejected)) |
| Timeout | One of the earlier checks stopped it: the packet did not arrive, the source is not recognized, the shared secret is wrong, `radius.status_server = false`, or the node has `stop_without_leader_ms` set and no leader (the authentication port does not answer Status-Server) | Look at the `Discarded` lines of `status`, then capture packets in the next step |

Run this test from a host that is in the client list. From any other host the result only says "source not recognized". When an unrecognized source is discarded, `Discarded: unknown client` grows; when a signature does not match, `Discarded: invalid` grows.

To check whether the packet reaches the server, capture on the server:

```console
$ tcpdump -ni any udp port 1812
```

If you see the NAS requests, the network works and the problem is on the server side (source address, shared secret). If you see nothing at all, check the firewall, the routing, the server address and port on the NAS, and whether `radius.bind` listens only on `127.0.0.1`. The accounting port is 1813.

## Configuration cannot be read

When `check` fails, it prints one line with the reason and exits with code 2. The message starts with the file that has the error and includes the line number when there is one. Messages seen in testing:

```console
$ rradiusd check --config nope.toml
nope.toml: cannot read configuration: No such file or directory (os error 2)
$ rradiusd check
/etc/rradius/rradius.toml: cannot read configuration: No such file or directory (os error 2)
```

Without `--config`, it reads `/etc/rradius/rradius.toml`.

TOML mistakes:

```text
rradius.toml: line 40: unclosed table, expected `]`
rradius.toml: line 28: `auth_prot`: unknown field `auth_prot`, expected one of `bind`, `auth_port`, `acct_port`, …
rradius.toml: line 28: `auth_port`: invalid type: string, expected u16
```

- When a key name is misspelled, the message lists all the keys the table accepts.
- A number in quotes becomes a string.

Files pulled in by `include`:

```text
conf.d/dup.toml: line 1: duplicate key
conf.d/bad.toml: line 1: an included file has to begin with a table (`[section]`), not a setting: a setting before its first table would belong to the file before it
```

Errors found only after merging (for example `radius clients: duplicate client name 'core'`) have no line number. A location such as `(in conf.d/10-clients.toml line 3)` is added at the end; use it to find the entry that caused the error.

The most common cause of `duplicate key` is a file in `conf.d/` that declares a table the main configuration file already has, such as `[radius]`. A table can appear only once, so make the change in the original place.

Dictionaries and scripts:

```text
dict/missing: cannot read dictionary: No such file or directory (os error 2)
dictionary:1: ATTRIBUTE needs a name, number and type: ATTRIBUTE	Broken-Attr
cannot compile script 'accounting.lua': syntax error: 2: Expected identifier when parsing variable name, got <eof>
```

- A dictionary error shows the file name, the line number and the content of that line.
- In your own dictionary, you can first bring in the shipped dictionary with `$INCLUDE` and then add your own attributes ([Chapter 06](06-dictionary.md)).

A required part is missing:

```text
rradius.toml: the [cluster] section is required
```

For the message when none of `[lua]`, `[radius]` and `[tacacs]` is present, see Common mistakes in [Chapter 03](03-quick-start.md).

Clients and secret files:

```text
rradius.toml: radius client 'localhost': exactly one of secret or secret_file must be set
rradius.toml: radius client 'localhost': secret_file 'localhost.secret' is empty
rradius.toml: radius client 'localhost': cannot read secret_file 'localhost.secret': No such file or directory (os error 2)
rradius.toml: radius client 'localhost': unknown auth chain 'staff' (no [auth.chains.staff])
rradius.toml: radius clients: duplicate client network '127.0.0.1/32'
```

- Two clients with the same address or the same name are not allowed (a single address is written as `/32`): `check` fails with `duplicate client network` or `duplicate client name`. Names appear in logs and metrics.

EAP certificates:

```text
rradius.toml: radius.eap.key_file 'server.key' does not fit cert_file 'server.pem': keys may not be consistent: KeyMismatch
rradius.toml: radius.eap.key_file 'server.key': I/O error: No such file or directory (os error 2)
rradius.toml: radius.eap.client_ca_file is required
rradius.toml: radius.eap.fragment_size must be from 256 to 1400, not 2000
```

`client_ca_file` is required when `methods` has `tls`.

For range and value errors, the message states the rule directly, for example:

```text
rradius.toml: auth.lockout.max_failures must be greater than 0
rradius.toml: log.trace.minutes: 0 is not from 1 to 1440
rradius.toml: lua.http.allowed_hosts must list at least one host
```

The error messages for security-related settings are collected in Common mistakes in [Chapter 20](20-security.md).

> **Note:** Warnings from `check` do not affect the exit code. Read the `WARN` lines before `configuration ok` too; for what to do about them see [Chapter 20](20-security.md).

## Reload fails or has no effect

`SIGHUP` makes the server read the configuration again. Check the result only in the log:

| Log | Meaning |
| --- | --- |
| `INFO rradius_core::reload: configuration reloaded from rradius.toml` | Everything was applied |
| `WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port` | Everything else was applied; the listed ones take effect only after a restart |
| `ERROR rradius_core::reload: reload failed, keeping the running configuration: rradius.toml: auth.lockout.max_failures must be greater than 0` | The new configuration has an error; nothing was applied and the server keeps the old configuration |

A failed reload does not stop the server, but the next restart will fail with the same error. So run `check` before you reload, and use `config diff` to confirm what changed:

```console
$ rradiusd config diff rradius.toml.orig rradius.toml
radius.clients[localhost].address: "127.0.0.1" | "127.0.0.2"
$ echo $?
1
```

When `config diff` finds differences, the exit code is 1.

Metrics show it too: `rradius_reload_total{result="error"}` grows and `rradius_reload_last_success_timestamp_seconds` does not change. The list of settings that need a restart is in [Appendix A: Configuration reference](90-config-reference.md); the reload procedure is in [Chapter 18](18-operations.md).

## The server fails to start

### The port is already in use

When another program uses the RADIUS or TACACS+ port, the server exits with code 1 (RADIUS prints `RADIUS server error: …`, TACACS+ prints `TACACS+ server error: …`):

```text
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
RADIUS server error: cannot listen on 127.0.0.1:1812: Address already in use (os error 48)
```

When the cluster Raft port is in use, it exits with code 2:

```text
cannot start cluster node 1: Config: Cannot bind Raft listen address '127.0.0.1:8100': Error("Address already in use (os error 48)")
```

Fix: find out who holds the port (on Linux use `ss -lunp` and `ss -ltnp`, on macOS use `lsof -i :<port>`). The most common cause is an old rradius that has not finished stopping, or two configurations running on the same machine.

> **Warning:** When the metrics port is in use, the server does **not** exit. The log only gets one extra line, `ERROR … metrics: cannot listen on <address>: Address already in use (os error 48); no metrics are served, and `status` cannot read them`. RADIUS keeps serving, but this server does not open `/metrics`, `/livez` or `/readyz`. After every start, check that the log has `Metrics listening on …`.

### The previous instance has not finished stopping

A cluster node takes some time to stop. If you start a new one before it has finished, the data directory is locked and the new one exits:

```text
… WARN hiqlite_wal::log_store: LockFile data/logs exists already - this is not a clean start!
thread 'tokio-rt-worker' (…) panicked at …/hiqlite-wal-0.16.0/src/log_store.rs:47:21:
LockFile data/logs is locked and in use by another process
… ERROR rradius_core::cli: cluster node: a panic in its database (…/hiqlite-wal-0.16.0/src/log_store.rs:47); exiting so that a restart recovers it
```

Fix: wait for the old process to exit (`pgrep -f rradiusd`), then start. The systemd unit shipped in the package waits up to `TimeoutStopSec=60` seconds when stopping.

### Does not start without a leader

With `cluster.stop_without_leader_ms` set, if no leader is found at startup (for example only one of three nodes is running), the server waits for a while and then exits with code 1:

```text
… INFO rradius_core::cli: cluster node 1 started
cluster was not ready: no usable leader within 53.980820917s, and stop_without_leader_ms is set
```

Fix: first bring up a majority of the nodes. A node without `stop_without_leader_ms` starts as usual and answers from its own data; see [Cluster has no leader](#cluster-has-no-leader).

## Client matching fails

**Symptom:** The NAS gets no response. The server log has no authentication record, only:

```text
… WARN rradius_core::radius: RADIUS packet from unknown client 127.0.0.1 discarded
```

`Discarded: unknown client` in `status` grows.

**Cause:** The source address of the packet is not in the `address` of any `[[radius.clients]]`. In testing, changing the client to `127.0.0.2` and sending from `127.0.0.1` gives exactly this result.

**Fix:**

- Use the address in the log, not the one you think it is. When the NAS has several interfaces, goes through NAT, or sends from an address other than the management interface, the source differs from what you expect.
- NAS devices in one network can be written as a single client with CIDR, for example `address = "10.0.0.0/24"`.
- Confirm that the reload succeeded after the change (look for `configuration reloaded` in the log).

This warning is logged at most once per second so that a flood of unknown packets does not fill the log. When there was more than one in that time, `(N since the last line)` is added at the end of the line, where N is the total in that period. For client settings see [Chapter 05](05-radius-clients.md).

## Shared secret mismatch

When the NAS and the server have different shared secrets, the result depends on whether the request has a Message-Authenticator. All three cases were tested:

| Request | Client setting | Server | Log |
| --- | --- | --- | --- |
| With Message-Authenticator | Any | The signature does not match, discards it, `Discarded: invalid` grows | **No line at all** |
| Without Message-Authenticator | `require_message_authenticator = "yes"` (default) | Discards it, `Discarded: invalid` grows | `discarded … Access-Requests lacking a Message-Authenticator` |
| Without Message-Authenticator | `"no"` | The decrypted password is garbage, answers Access-Reject; the NAS cannot verify this answer with its own secret and treats it as not received | An authentication record with `decision="reject"` |

The first case is the hardest to notice because the log is completely silent. If the NAS times out, the log has no authentication record and no `unknown client`, check whether `Discarded: invalid` is growing.

Fix:

- Set the shared secret again on both sides. Copy it from a file, do not type it by hand. The server strips the trailing newline when it reads `secret_file`, so do not include a newline when you copy it to the NAS.
- Test once from the server itself with `rradius-bench --secret "$(cat <file>)"`. If that works, the secret on the server side is correct and the problem is on the NAS side.

For handling a missing Message-Authenticator see [Chapter 20](20-security.md). For a wrong TACACS+ shared secret see [TACACS+ cannot connect or answers ERROR](#tacacs-cannot-connect-or-answers-error).

## Always rejected

There is an authentication record with `decision="reject"`. Look at `handler`:

| Authentication record | Cause | Fix |
| --- | --- | --- |
| `handler="sql"` (or `ldap`, and so on) | The handler found the account but the password is wrong | Check the password; check that the method the NAS uses (PAP, CHAP, MS-CHAPv2) is compatible with the format the account stores ([Chapter 07](07-users-and-chains.md)) |
| `handler="none"`, `account` has a value | No handler recognizes this account: it does not exist, is disabled, or the query did not select it | Use `user show` or query the table directly |
| `handler="lockout"` | Locked by failed logins | See [Chapter 20](20-security.md) |
| `handler="mschapv1_refused"` | MS-CHAPv1 is off by default | Make the NAS use another method, or write `allow_mschapv1 = true` once you are sure you need it |
| `handler="length"` | The User-Password length is wrong: the NAS did not pad the password to a multiple of 16, or the password is longer than 128 bytes | Look at the client and length in the WARN line logged at the same time; fix the NAS configuration, see [Chapter 07](07-users-and-chains.md#password-length) |

The following warnings mean the account data itself has a problem (all were tested):

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].bcrypt_cost allows and is refused; raise the limit if the hash is meant to be used user="bob" format="crypt" parameter="bcrypt_cost"
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="eve"
```

The first line: the hash cost is above the limit. The second line: the hash has no format prefix. For the fix see [Chapter 20](20-security.md).

### Correct password, problem with reply attributes

When the name of a reply attribute is not in the dictionary, the whole response cannot be built and the server answers Access-Reject instead:

```text
… ERROR rradius_core::radius: RADIUS response attribute error: unknown attribute 'No-Such-Attr'
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

In testing, a row `No-Such-Attr` was added to the `user_reply` table and `alice` was rejected even with the correct password. Fix: correct the attribute name in the table, or add the attribute to the dictionary ([Chapter 06](06-dictionary.md)).

## Database unavailable

When the database that authentication or accounting uses cannot be reached, the server **does not answer** instead of rejecting.

### External databases

In testing, an sql handler pointed at a PostgreSQL address that nobody listens on:

```text
… ERROR rradius_core::auth_chain: auth sql handler error: database legacy: pool timed out while waiting for an open connection
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

`Discarded: database` in `status` grows and the NAS times out.

When a database with `required_for_ready = true` cannot be reached, `/readyz` answers 503, so a load balancer can steer traffic away:

```text
… WARN rradius_core::external_db: databases.legacy: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

```console
$ curl -s -w ' http=%{http_code}\n' http://127.0.0.1:9812/readyz
database legacy unreachable http=503
$ rradiusd status --config rradius.toml
…
Node
  Ready                     database legacy unreachable
…
External databases
  …
  legacy reachable               no
…
```

- The check runs every 5 seconds; two failures in a row count as unreachable, and one success recovers it.
- `required_for_ready` is `false` by default: when a database without it goes down, `/readyz` still answers 200 and only requests get no response.

Fix: check the database itself, the network and the credentials. `connect_timeout_ms` must be shorter than the NAS retransmit time, so that the NAS has a chance to ask another server. For details see [Chapter 09](09-external-databases.md).

> **Not verified on real hardware:** Only one way of being unreachable was tested: the port is not listening. The messages for a database that is running but has wrong credentials or a TLS certificate that fails verification were not tested.

### Cluster database

Problems with the cluster database (the account tables and the tables the accounting script writes) are usually caused by having no leader; see the next section.

## Cluster has no leader

A write has to be confirmed by the leader, so when there is no leader the database cannot be written. This was simulated with three nodes on one machine: after all three were up, nodes 2 and 3 were stopped and only node 1 was left.

### stop_without_leader_ms not set (default)

Node 1 keeps answering from its own data:

- Login: it only reads data, so `alice` is still `accept`.
- Accounting: the script has to write, fails, and there is no response:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: no leader for 5 s\nstack traceback:\n…\taccounting.lua:44: in function <accounting.lua:1>\n
```

The Node section of `status`:

```text
Node
  Ready                     ok
  Leader                    1
  Has a leader              0
  Caught up                 1
  …
  Seconds without a leader  5
```

`Has a leader 0` with `Seconds without a leader` growing means there is no leader. In testing the `Leader` line still showed 1 at this point, so judge by `Has a leader`.

### stop_without_leader_ms set

Add `stop_without_leader_ms = 10000` to `[cluster]`. After more than 10 seconds without a leader:

```console
$ curl -s -w ' http=%{http_code}\n' http://127.0.0.1:9812/readyz
no leader http=503
$ rradiusd status --config rradius.toml
…
Node
  Ready                     no leader
  …
  Has a leader              0
  …
  Seconds without a leader  16
…
  Discarded: no leader       0.0/s (1)
```

- All RADIUS requests get no response, even read-only logins, so the NAS can ask another node.
- After nodes 2 and 3 come back, `/readyz` returns to `ok` and logins resume (tested).
- This setting also makes a node exit at startup when it finds no leader; see [The server fails to start](#the-server-fails-to-start).

Fix: bring back a majority of the nodes. In a three-node cluster, at least two must be running and able to reach each other. Check the network between the nodes, the addresses in `cluster.nodes`, and whether the two cluster secrets are the same on every node. For how the cluster works and how to manage members see [Chapter 16](16-cluster.md).

> **Not verified on real hardware:** The above is a simulation with three nodes on one machine. A network failure across machines, and nodes with different secrets, were not tested.

## Missing accounting records

A NAS that sends an accounting request counts it as delivered only when it receives an Accounting-Response. If it gets none, it retransmits, and after a few retransmissions it gives up and that record is lost. So "missing accounting records" almost always means "the server did not send an Accounting-Response".

The server answers only after the accounting script succeeds. Two cases of script failure (both tested):

**The database is temporarily unavailable**, for example there is no leader:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: no leader for 6 s\nstack traceback:\n…\taccounting.lua:44: in function <accounting.lua:1>\n
```

**The script or the SQL is wrong**, for example a mistyped table name:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: PrepareStatement: no such table: accountingx\nstack traceback:\n…
```

Neither gets a response. If `Acct requests` in `status` grows and `Accounting-Response` does not grow with it, this is the case.

### Riding out a database outage with the accounting spool

With `[radius.accounting_spool]` set, in the first case (database unreachable or timed out) the request is first written to the spool on disk and then the NAS gets its response; when the database recovers, the request is replayed automatically:

```toml
# rradius.toml
[radius.accounting_spool]
dir = "spool"
```

In testing: after two of three nodes were stopped, one Start was sent. The script still failed, but the NAS got a response (`accounting 1` in `rradius-bench`), and the metrics were:

```text
rradius_acct_spool_written_total 1
rradius_acct_spool_bytes 110
```

After nodes 2 and 3 were started again:

```text
rradius_acct_spool_written_total 1
rradius_acct_spool_replayed_total 1
rradius_acct_spool_bytes 0
```

The Accounting spool section of `status` has the same numbers (`Bytes waiting`, `Replayed`, `Rejected`, `Not spooled`).

- The second case (wrong SQL) does **not** go into the spool, because a replay would fail the same way. In testing `rradius_acct_spool_written_total` stayed at 0. When you see this error, fix the script.
- If `Bytes waiting` never goes down, replay keeps failing; look at the errors in the log. If `Rejected` grows, some records were judged impossible to process and moved to the `rejected` file in the spool directory.
- When the spool is full (`radius.accounting_spool.max_bytes`), there is no response and `Not spooled` grows; when the disk cannot be written, there is no response either.

For the spool settings, the replay rules and how to write the script see [Chapter 13](13-accounting.md).

## Overload and UDP packet loss

### The server discards

When more requests are being handled at once than `max_auth_inflight` (or `max_acct_inflight`), the extra ones are dropped. In testing, `max_auth_inflight` was set to 2 and 30 parallel logins of bcrypt accounts were sent. 28 of them were discarded, and the log had one line:

```text
… WARN rradius_core::radius: RADIUS authentication overloaded: discarded 1 packets
```

Each port writes at most one such line a second. The first discard is written at once; the ones that follow are added to the next line, which is written only when a packet is discarded again a second or more later (here, a second burst brought `discarded 28 packets`). For the exact number, use the count in `status` or the metric.

`Discarded: overload` in `status` grows, and `Auth limit` shows the current limit.

### The operating system discards

Before packets reach rradius, the operating system may drop them because the receive buffer of the socket is full. rradius cannot see this kind of drop; ask the operating system:

```console
$ rradiusd tuning --config rradius.toml
rradius tuning advice for macOS

UDP packets dropped because a receive buffer was full, since the system started: 137296
  Raise the receive buffers below, then watch whether this count still grows.
…
```

This number is the total for the whole machine since boot. Run it again after a while and see whether it has grown.

When the buffer that `radius.recv_buffer_bytes` asks for is above the system limit, a warning is written at startup:

```text
… WARN rradius_core::radius: RADIUS: asked for a receive buffer of 67108864 bytes and each socket got 8388608; raise kern.ipc.maxsockbuf to at least 67108864
```

The name of the system setting in the message is `net.core.rmem_max` on Linux and `kern.ipc.maxsockbuf` on macOS and FreeBSD. `tuning` prints the value to change and the command.

For the fix and for capacity planning see [Chapter 19](19-performance.md).

## EAP is stuck or fails

EAP is a conversation with many rounds, going back and forth between the NAS and the server several times. In the authentication record `handler` is `eap-peap`, `eap-tls` and so on, and a failure adds a line with the reason. Tested (PEAP, with `rradius-bench` as the client):

| Situation | Server log | `rradius-bench` |
| --- | --- | --- |
| Wrong inner password | `EAP conversation with client localhost failed: the chain did not accept the inner name` | `reject 1` |
| The client asks for a method the server does not offer | `EAP conversation with client localhost failed: the peer asked only for methods this server does not offer` | `reject 1` |
| The client does not trust the server certificate | **No line at all** | `TLS: invalid peer certificate: UnknownIssuer`, `invalid 1` |
| The name in the server certificate is wrong | **No line at all** | `certificate not valid for name "wrong.example.net"; …`, `invalid 1` |

In the last two cases the client gave up the conversation by itself, and the server only sees a conversation with no follow-up. The EAP section of `status` shows these unfinished conversations under `Sessions under way`; they are cleaned up after `radius.eap.session_timeout_ms` (30000 milliseconds by default). So:

> **Note:** When a user says "cannot connect" and the log has no EAP failure record, first suspect that the client device does not trust the server certificate: the device does not have your CA installed, or the server name it is configured with does not match the certificate. Look at the messages on the device side.

### Fragmentation and MTU

The TLS messages of EAP are larger than one RADIUS packet and have to be cut into several fragments. `radius.eap.fragment_size` is the limit of each fragment; the default is 1000 and the range is 256 to 1400 bytes. When the path MTU between the NAS and the server is smaller, or a device in the middle drops IP fragments, large RADIUS packets disappear on the way and the EAP conversation gets stuck at the certificate exchange.

Fix: reduce `fragment_size` and try again.

> **Not verified on real hardware:** Packet loss caused by MTU needs real network equipment to reproduce, and was not tested. The fix above follows from what `fragment_size` does.

For EAP settings and certificates see [Chapter 11](11-eap.md).

## TACACS+ cannot connect or answers ERROR

When the shared secrets differ, the server cannot decode the packet body and closes the connection:

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: a body did not decode, so the shared secret may be wrong
```

`Malformed` in the TACACS+ section of `status` grows. On the `rradius-bench` side you see `lengths: the fields of the reply do not add up to its length, so the shared secret may be wrong`.

When the database used for authentication is unavailable, TACACS+ does not stay silent but answers ERROR:

```text
… WARN rradius_core::lua_engine: TACACS+ authentication for client router1 answered ERROR: the authentication backend could not be used
```

What the device does after it receives ERROR (ask the next server, use a local account) depends on its configuration.

A connection that sends no complete packet for longer than `tacacs.idle_timeout_ms` (300000 milliseconds by default) is closed. In testing, the value was set to 2000 and one connection was opened that sent nothing:

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: idle for 2s
```

If a device keeps one connection open for a long time, this value must be longer than the interval at which the device sends packets. For TACACS+ settings see [Chapter 12](12-tacacs.md).

> **Not verified on real hardware:** TACACS+ was never tried with real network equipment, only with the `tacacs-pap` and `tacacs-chap` loads of `rradius-bench`.

### Connection closed: limits

TACACS+ uses the number of connections as a limit. At the limit, a new connection is closed immediately (not left without a response):

| Log | Cause | Fix |
| --- | --- | --- |
| `TACACS+ overloaded: closed N connections` | The connections open at the same time reached `tacacs.max_connections` (default 1024); N is the number of new connections closed since the previous line | If devices open connections and never close them, lower `idle_timeout_ms`; raise `max_connections` only when you really have that many connections. Both need a restart to take effect |
| `TACACS+ connection from <address> closed: it already holds max_connections_per_source = N connections` | The connections open from this source address reached `tacacs.max_connections_per_source` (default 64) | When the address is behind NAT or a jump host and the number of connections is legitimately high, raise `max_connections_per_source`; this needs a restart |
| `TACACS+ connection from unknown client <address> closed` | The source is not in `[[tacacs.clients]]` | Add the address and reload |

These lines are also logged at most once per second, and may end with `(N since the last line)`. `rradius_tacacs_connections_total{result="refused"}` is increased by 1 for every refused connection. For the limits see [Chapter 12](12-tacacs.md).

## Script errors

When a script fails, the request is treated as a reject (for accounting, as no response), and the log has `Lua script error (<protocol>/<phase>)` followed by the reason and the line number:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to index nil with 'open'\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to call a nil value\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
```

- `exceeded the execution limit`: the script ran too long and passed `lua.max_execution_ms`. Find the loop or the slow part; time spent waiting for the database does not count.
- `attempt to index nil`, `attempt to call a nil value`: something that does not exist was used. The sandbox removes `io`, `os.execute`, `require` and so on; without `[lua.http]`, `http_request` does not exist either.

Messages that a script writes with `log(...)` appear in the log with the script name and the client:

```text
… INFO rradius_core::lua_engine: script api.lua (radius/authentication, client localhost): api gave no answer: http://127.0.0.1 is not in lua.http.allowed_hosts
```

For how to write and debug scripts see [Chapter 10](10-lua-policies.md).

## needs rradius-enterprise

When `rradiusd` reads a setting that exists only in the enterprise edition, it refuses clearly and exits with code 2:

```text
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
rradius.toml: radius.tls: not supported by this program (RadSec needs rradius-enterprise)
rradius.toml: radius.eap.resumption_ms: not supported by this program (session resumption needs rradius-enterprise)
rradius.toml: radius client 'localhost': mfa needs rradius-enterprise
rradius.toml: radius client 'localhost': eap_methods: 'sim' is not a method this server offers; it has tls, peap and ttls
```

This is not a mistake in the configuration. This program does not have that feature.

Fix: remove that section or that key.

> **Enterprise edition:** The features above are available in the enterprise edition.

## Walkthrough: a new NAS gets no response

This joins the methods above together. Scenario: a new NAS was installed and user logins keep timing out. The output in every step below is from testing.

**Step 1: Is the configuration fine?**

```console
$ rradiusd check --config rradius.toml
configuration ok
```

The configuration has no errors and no warnings.

**Step 2: Do requests reach the server?** Look in the log for an authentication record of this NAS. There is none, but there is:

```text
… WARN rradius_core::radius: RADIUS packet from unknown client 127.0.0.1 discarded
```

The source address the NAS uses is not in the client list. Add it (or correct `address`), reload, and confirm that the log has `configuration reloaded from rradius.toml`.

**Step 3: Still timing out.** This time the log has nothing: no authentication record and no `unknown client`. Look at `status`:

```console
$ rradiusd status --config rradius.toml
…
  Discarded: unknown client  0.0/s (1)
  Discarded: invalid         0.0/s (3)
…
```

`Discarded: invalid` is growing: the request carried a Message-Authenticator, but the signature does not match, which means the shared secrets differ. Set the shared secret again on both sides.

**Step 4: Now there is a response, but it is a reject.** The log has an authentication record:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
```

`handler="sql"`: the account was found and the password is wrong. Ask the user to check the password. If it still fails, turn on trace for this user, look at which kind of password attribute the NAS sends (`User-Password`, `CHAP-Password` or an MS-CHAP attribute), and confirm that the format stored for the account can verify this method.

The whole process used only `check`, the log and `status`, in the order of the table in [Troubleshooting order](#troubleshooting-order).

## What to include when reporting a problem

When you cannot find the cause yourself and need to ask someone for help, include the following. It saves many round trips:

- The output of `rradiusd --version`.
- The full output and exit code of `rradiusd check --config <file>`.
- `rradiusd config show --config <file>`: the full configuration with defaults filled in, secrets shown as `<hidden>`, so you can paste it as it is. Do not paste the original configuration file, which may contain passwords in plain text.
- The log of the period when the problem happened, including the `WARN` and `ERROR` lines and the authentication records around it.
- The output of `rradiusd status --config <file>`; add `--json` if a program will read it.
- For UDP packet loss problems, also `rradiusd tuning --config <file>`.

The output of trace contains personal data. Before you give it to anyone, mask data such as user names and addresses.

## Common mistakes: message table

You see this message; here are the cause and the fix. All of them are real output from testing; `…` is a part that changes from case to case.

### check and startup

| Message | Cause | Fix |
| --- | --- | --- |
| `cannot read configuration: No such file or directory (os error 2)` | Wrong configuration file path; without `--config` it reads `/etc/rradius/rradius.toml` | Add `--config` or correct the path |
| `` line N: unclosed table, expected `]` `` | TOML syntax error | Look at that line |
| `` `X`: unknown field `X`, expected one of … `` | Misspelled key, or put in the wrong table | Compare with the keys listed in the message and [Appendix A](90-config-reference.md) |
| `` `X`: invalid type: string, expected u16 `` | A number in quotes, or a wrong type | Change to the correct type |
| `line 1: duplicate key` | A file in `conf.d/` declares a table that already exists | Merge into the original table |
| `an included file has to begin with a table` | The first line of the `include` file is not `[table]` | Write the table name first |
| `cannot read dictionary: …` | Wrong dictionary path | Change `dictionary` |
| `dictionary:N: ATTRIBUTE needs a name, number and type: …` | Wrong format in the dictionary file | Fix that line |
| `cannot compile script '…': syntax error: …` | Lua syntax error | Fix the script |
| `the [cluster] section is required` | There is no `[cluster]` | Add it ([Chapter 16](16-cluster.md)) |
| `exactly one of secret or secret_file must be set` | The client has no shared secret, or both are set | Keep only `secret_file` |
| `secret_file '…' is empty` | The secret file is empty | Write the secret into it |
| `cannot read secret_file '…': …` | The secret file does not exist or cannot be read | Check the path and permissions |
| `unknown auth chain 'X' (no [auth.chains.X])` | The `auth` of a client points to a chain that does not exist | Add `[auth.chains.X]` or rename |
| `duplicate client network '…'` | Two clients have the same address | Merge them or delete one |
| `duplicate client name '…'` | Two clients in the same list have the same name (when no name is written, the address is the name) | Change the `name` of one of them ([Chapter 05](05-radius-clients.md)) |
| `cluster.secret_raft must be at least 16 characters` | The cluster secret is too short | `openssl rand -hex 32` |
| `key_file '…' does not fit cert_file '…'` | The private key and the certificate are not a pair | Use files that belong together |
| `radius.eap.client_ca_file is required` | `tls` is used but there is no CA | Add `client_ca_file` |
| `fragment_size must be from 256 to 1400, not N` | Out of range | Change it to a value in range |
| `… not supported by this program (… needs rradius-enterprise)` | A setting that exists only in the enterprise edition | Remove it, or switch to the enterprise edition |
| `RADIUS server error: cannot listen on …: Address already in use` (exit code 1) | The RADIUS port is in use | Stop the program that holds it |
| `Cannot bind Raft listen address '…'` (exit code 2) | The cluster port is in use | Stop the program that holds it |
| `metrics: cannot listen on …: Address already in use` (the server keeps running) | The metrics port is in use | Stop the program that holds it, then restart rradius |
| `LockFile data/logs is locked and in use by another process` | The old process has not finished stopping | Wait for it to exit, then start |
| `cluster was not ready: no usable leader within …, and stop_without_leader_ms is set` (exit code 1) | The cluster has no majority at startup | Bring up the other nodes first |
| `cluster was not ready to apply migrations: …` (exit code 1) | At startup, no leader within `migrations_wait_ms`, so the migrations could not be applied, or the local replica could not catch up; the second half of the message sometimes says "the cluster node gives one migration file 30 s": one file ran for more than 30 seconds | Bring up a majority of the nodes first; when a file is slow by itself, increase `migrations_wait_ms`, then start again ([Chapter 18](18-operations.md#common-mistakes)) |

### While running

| Message | Cause | Fix |
| --- | --- | --- |
| `RADIUS packet from unknown client X discarded` | The source address is not in the client list | Add a client with the address from the log |
| `RADIUS: discarded N Access-Requests lacking a Message-Authenticator, …` | The NAS does not send a Message-Authenticator, or the secret is wrong and none was sent | Turn on Message-Authenticator on the NAS; if that is not possible, set `"no"` for that client only |
| (no log, `Discarded: invalid` grows) | Shared secret mismatch | Set the shared secret again on both sides |
| `RADIUS Access-Request from client X not answered: the authentication backend could not be used` | The database or directory used for authentication is unavailable | Look at the error on the previous line |
| `auth sql handler error: database X: pool timed out while waiting for an open connection` | The external database cannot be reached | Check the database and the network |
| `databases.X: not reachable at startup: …; /readyz answers 503 until it is` | A database with `required_for_ready` cannot be reached | Same as above |
| `Lua script error (radius/accounting): runtime error: no leader for 5 s` | The cluster has no leader, so it cannot write | Bring back a majority of the nodes |
| `/readyz` answers `no leader` (503) | `stop_without_leader_ms` is set and there is no leader | Same as above |
| `Lua script error (radius/accounting): runtime error: PrepareStatement: no such table: …` | The SQL of the accounting script is wrong; no response and not put in the spool | Fix the script |
| `RADIUS authentication overloaded: discarded N packets` | Over `max_auth_inflight` | Check for abnormal traffic; adjust the limit ([Chapter 19](19-performance.md)) |
| `asked for a receive buffer of N bytes and each socket got M; raise … to at least N` | The system limit for socket buffers is too low | Adjust it as `tuning` says |
| `RADIUS response attribute error: unknown attribute 'X'` | The reply attribute is not in the dictionary | Fix the data or the dictionary |
| `the stored password asks for more than [auth.password_limits].X allows and is refused` | The hash cost is above the limit | [Chapter 20](20-security.md) |
| `the stored password looks like a hash with no scheme prefix and is refused` | The hash has no format prefix | Add the prefix |
| `auth lockout: client X: account "Y" from "Z" is locked (…)` | Too many failures | Wait for `lock_seconds`; find out who is guessing |
| `EAP conversation with client X failed: the chain did not accept the inner name` | The inner EAP password is wrong or the account does not exist | Same as a normal reject |
| `EAP conversation with client X failed: the peer asked only for methods this server does not offer` | The EAP method the device wants is not enabled | Adjust `radius.eap.methods` or the device settings |
| `TACACS+ connection from X closed: a body did not decode, so the shared secret may be wrong` | TACACS+ shared secret mismatch | Set the shared secret again on both sides |
| `TACACS+ overloaded: closed N connections` | The number of connections reached `tacacs.max_connections` | [Connection closed: limits](#connection-closed-limits) |
| `TACACS+ connection from X closed: it already holds max_connections_per_source = N connections` | The number of connections from this source address reached the limit | Same as above |
| `TACACS+ authentication for client X answered ERROR: the authentication backend could not be used` | The database used for authentication is unavailable | Check the database |
| `script exceeded the execution limit of N ms` | The script ran too long | Fix the script, or adjust `lua.max_execution_ms` |
| `script http_request refused: … is not in lua.http.allowed_hosts` | The host the script calls is not on the allow list | Add it to `allowed_hosts` |
| `stopping: after N ms, M RADIUS requests were still being handled and K TACACS+ connections open; they are cut off` | At shutdown, the server waited `[shutdown] drain_ms` and some requests were still not finished (script, LDAP or database too slow); those requests get no response and the device retransmits to another server | Find out why it is slow, or increase `drain_ms` ([Chapter 18](18-operations.md#how-long-shutdown-takes)) |
| `reload failed, keeping the running configuration: …` | The new configuration has an error | Run `check` first, fix it, then reload |
| `configuration reloaded from …; these need a restart to apply: …` | Some settings take effect only after a restart | Plan a restart |

### Commands

| Message | Cause | Fix |
| --- | --- | --- |
| `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)` (`status`, exit code 3) | The server is not running, or the `[metrics]` address the server uses differs from the configuration the command reads | Start the server, or point `--config` at the configuration the server uses |
| `the cluster did not answer within 30 s` (commands such as `user`, exit code 3) | The server is not running | Start the server ([Chapter 03](03-quick-start.md)) |
| `timeouts 1, … socket errors 1` of `rradius-bench` (exit code 2) | The server is not running or the port is wrong | Check the address and the port |

A log line that ends with `(N since the last line)` is a message that anyone can trigger (unknown sources, refused connections and so on): each kind is logged at most once per second, and N is the total that happened between this line and the previous one, so a large N does not mean it happened only once.

In the test of the `user` row, the 30 seconds of waiting also produced several lines of `ERROR hiqlite::client::helpers: Error looking up DB metrics: …`.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [18 Day-to-day operations](18-operations.md)
- [19 Performance tuning and load testing](19-performance.md)
- [20 Security hardening](20-security.md)
- [Appendix A: Configuration reference](90-config-reference.md)
