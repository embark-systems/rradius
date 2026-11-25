# 20 Security hardening

> After this chapter you can:
> - Use `check` to find every security-related warning in the configuration at once, and fix them one by one
> - Tighten secret files, shared secrets, Message-Authenticator and MS-CHAPv1
> - Set the hash cost limits and the lockout after failed logins
> - Restrict what scripts can do, and encrypt connections to databases and LDAP
> - Open the metrics port and the cluster ports only where they should be open

Prerequisites: you have done [Chapter 03](03-quick-start.md) and have a configuration that passes `check`. Every item in this chapter links to its main chapter; here we only cover how to tighten it and what you see when it is not tightened.

In this chapter:

- [Warnings and errors](#warnings-and-errors)
- [Checklist](#checklist)
- [Find all warnings with check](#find-all-warnings-with-check)
- [Permissions of secret files](#permissions-of-secret-files)
- [Shared secret length and example values](#shared-secret-length-and-example-values)
- [Message-Authenticator and limit_proxy_state](#message-authenticator-and-limit_proxy_state)
- [Turn off MS-CHAPv1](#turn-off-ms-chapv1)
- [Stored passwords: format and hash cost limits](#stored-passwords-format-and-hash-cost-limits)
- [Lockout after failed logins](#lockout-after-failed-logins)
- [Limits on concurrent work](#limits-on-concurrent-work)
- [Scripts: sandbox, time limits and the HTTP allowlist](#scripts-sandbox-time-limits-and-the-http-allowlist)
- [Encrypting connections to databases and LDAP](#encrypting-connections-to-databases-and-ldap)
- [Open the metrics port only on the internal network](#open-the-metrics-port-only-on-the-internal-network)
- [Cluster secrets and the network between nodes](#cluster-secrets-and-the-network-between-nodes)
- [Trace writes personal data](#trace-writes-personal-data)
- [systemd and Kubernetes restrictions](#systemd-and-kubernetes-restrictions)
- [Keys and passwords in memory](#keys-and-passwords-in-memory)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

All output in this chapter was produced by running `rradiusd` 0.1.0 on a macOS machine. The timestamp at the start of each log line is replaced by `…`; where a path appears in a message, it is the relative path written in the configuration file.

## Warnings and errors

rradius reacts to security problems in two ways:

| Reaction | When | Result |
| --- | --- | --- |
| Warning (`WARN`) | The configuration works but is not secure, for example a secret file that others can read | `check` still prints `configuration ok` and exits with code 0; the server starts as usual |
| Error | The configuration itself is not valid, for example a cluster secret that is too short | `check` prints the reason and exits with code 2; the server does not start; on reload the old configuration is kept |

Warnings do not stop you, so they are easy to ignore. Most warnings appear at three points: `check`, server start and every reload. A few appear only when the server is actually running; they are marked below.

> **Note:** Warning messages only name the setting, the file and the permissions. They never write the content of a password or key, or the length of a password. It is safe to hand the log to someone else.

## Checklist

Go through the table item by item. The details of each item are in the sections below.

| Item | How to confirm it is done | Section |
| --- | --- | --- |
| Secret files can be read only by the owner (and its group) | `check` has no `has mode` warning | [Permissions of secret files](#permissions-of-secret-files) |
| Files and directories of `include` cannot be written by others | `check` has no `can be written by its group or by others` | [Permissions of secret files](#permissions-of-secret-files) |
| Shared secrets are randomly generated and at least 24 octets | `check` has no `shorter than 16 octets` or `public shared secret` | [Shared secret length and example values](#shared-secret-length-and-example-values) |
| Message-Authenticator is left at the default `yes` | `require_message_authenticator = "yes"` in `config show` | [Message-Authenticator and limit_proxy_state](#message-authenticator-and-limit_proxy_state) |
| MS-CHAPv1 is left at the default, off (when there is an `[auth]` auth chain) | `[auth]` has no `allow_mschapv1 = true` | [Turn off MS-CHAPv1](#turn-off-ms-chapv1) |
| All passwords in the account table have a scheme prefix | The log has no `no scheme prefix` | [Stored passwords: format and hash cost limits](#stored-passwords-format-and-hash-cost-limits) |
| Lockout after failed logins is on | The configuration has `[auth.lockout]`, and `/metrics` has `rradius_auth_lockouts_total` | [Lockout after failed logins](#lockout-after-failed-logins) |
| Scripts can call only hosts on the allowlist | `allowed_hosts` in `[lua.http]` lists only the hosts you need | [Scripts: sandbox, time limits and the HTTP allowlist](#scripts-sandbox-time-limits-and-the-http-allowlist) |
| Connections to databases and LDAP are encrypted | No `tls = "off"` or `allow_plaintext = true`; the start-up log has no `unencrypted` | [Encrypting connections to databases and LDAP](#encrypting-connections-to-databases-and-ldap) |
| The metrics port is not exposed | `metrics.bind` is `127.0.0.1` or an internal address | [Open the metrics port only on the internal network](#open-the-metrics-port-only-on-the-internal-network) |
| Cluster ports are bound only to an internal address | `check` has no `cluster.listen is not set` | [Cluster secrets and the network between nodes](#cluster-secrets-and-the-network-between-nodes) |
| Trace is turned off after use | The configuration has no leftover `[log.trace]` | [Trace writes personal data](#trace-writes-personal-data) |

## Find all warnings with check

**Goal:** See all security warnings of the current configuration at once.

`check` reads the configuration, the dictionary, the scripts and the secret files. It opens no port, so you can run it on a production machine at any time. Try it on an unmodified quick-start scenario:

```console
$ rradiusd check --config rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: 'raft.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::common::seal: cluster.secret_api_file: 'api.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_api is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::common::seal: secret_file: 'localhost.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
configuration ok
$ echo $?
0
```

The exit code is 0, but there are six warnings: three files have permissions that are too wide, and three secrets are the public values shipped with the examples. The next two sections show how to fix them. When all are fixed, `check` should print only the line `configuration ok`.

To block a configuration that has warnings in a deployment script, treat `WARN` in standard error as a failure:

```console
$ rradiusd check --config rradius.toml 2>&1 | grep WARN && echo "warnings found, fix them first"
```

## Permissions of secret files

**Goal:** Make the files that hold passwords and keys readable only by those who should read them.

rradius checks the permissions of the following files when it reads them. If "others" have any permission, or the "group" can write (any bit of the permission mask `0o027`), it logs a warning, but still reads and uses the file:

| Setting | Main chapter |
| --- | --- |
| `radius.clients[].secret_file`, `tacacs.clients[].secret_file` | [05](05-radius-clients.md), [12](12-tacacs.md) |
| `cluster.secret_raft_file`, `cluster.secret_api_file` | [16](16-cluster.md) |
| `databases.<name>.password_file` | [09](09-external-databases.md) |
| `bind_password_file` of the ldap handler | [08](08-ldap-and-ad.md) |
| `machine_password_file` of the netlogon handler | [08](08-ldap-and-ad.md) |
| `radius.eap.key_file` (private key of the server certificate) | [11](11-eap.md) |

The warning has the form "setting name: 'file name' has mode permissions":

```console
$ chmod 644 localhost.secret
$ rradiusd check --config rradius.toml
… WARN rradius_core::common::seal: secret_file: 'localhost.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
configuration ok
```

Measured results for various permissions:

| Permissions | Warning |
| --- | --- |
| `600`, `640`, `400` | None |
| `644`, `660`, `604` | Yes |

Fix:

```console
$ chmod 600 *.secret
```

When you install from a package and run as the `rradius` account, make the files owned by root, with group `rradius` and permissions `640`. The service account can then read but not change them:

```console
$ sudo chown root:rradius /etc/rradius/*.secret
$ sudo chmod 640 /etc/rradius/*.secret
```

The warnings for the other files look like this (all tested):

```text
… WARN rradius_core::common::seal: radius.eap.key_file: 'server.key' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::common::seal: bind_password_file: 'ldap.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::common::seal: databases.legacy.password_file: 'db.secret' has mode 660, which others can reach or its group can write; make it 0600 or 0640
```

### Files and directories of include

A file read by `include` can add clients and change auth chains, which amounts to deciding who can log in. So if the file itself, or the directory it is in, can be written by its group or by others, there is a warning:

```console
$ chmod 666 conf.d/log.toml
$ rradiusd check --config rradius.toml
… WARN rradius_core::config: included configuration file conf.d/log.toml can be written by its group or by others, which lets them change who may log in
configuration ok
$ chmod 644 conf.d/log.toml; chmod 777 conf.d
$ rradiusd check --config rradius.toml
… WARN rradius_core::config: the directory of included configuration file conf.d/log.toml can be written by its group or by others, which lets them change who may log in
configuration ok
```

The standard here is "others cannot write", so a `644` configuration file has no warning; `664` does.

> **Warning:** When the main configuration file `rradius.toml` or its directory can be written by the group or by others, start-up, every reload and `check` warn `configuration file … can be written by its group or by others, which lets them change who may log in` (for the directory, `the directory of configuration file …`), the same as for included files; read-only access does not warn. The warning does not block start-up. You still have to make the main configuration file and its directory writable only by root yourself.

For how to use `include`, see [Chapter 04](04-configuration.md).

## Shared secret length and example values

**Goal:** Give each NAS a long random shared secret, and do the same for the cluster secrets.

### Too short

A shared secret of a RADIUS or TACACS+ client that is shorter than 16 octets causes a warning. The warning does not write the secret or its length:

```console
$ printf 'short-secret\n' > localhost.secret
$ rradiusd check --config rradius.toml
… WARN rradius_core::reload: RADIUS client 'localhost' has a shared secret shorter than 16 octets; use at least 24 random octets
configuration ok
```

The same holds for a TACACS+ client (tested):

```text
… WARN rradius_core::reload: TACACS+ client 'router1' has a shared secret shorter than 16 octets; use at least 24 random octets
```

The content of a TACACS+ packet is protected only by the shared secret, so a short secret hurts more there.

The cluster's `secret_raft` and `secret_api` are a hard rule: shorter than 16 characters and the server cannot start:

```console
$ printf 'abc\n' > raft.secret
$ rradiusd check --config rradius.toml
rradius.toml: cluster.secret_raft must be at least 16 characters
$ echo $?
2
```

### Public values shipped with the examples

The secrets shipped with the example scenarios all contain `change-me`. When a client's shared secret or a cluster secret contains this string (case-insensitive), there is a warning:

```text
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
```

### Fix

Generate a random value as the warning says. `openssl rand -hex 32` produces 64 hexadecimal characters, far more than 24 octets:

```console
$ openssl rand -hex 32 > nas1.secret
$ chmod 600 nas1.secret
```

```toml
# rradius.toml
[[radius.clients]]
name = "nas1"
address = "192.0.2.10"
secret_file = "nas1.secret"
# … (other settings omitted)
```

- Use `secret_file`; do not write the shared secret directly in `secret` in the configuration file. Set exactly one of them: setting both or neither gives `exactly one of secret or secret_file must be set`.
- Use a different secret for each NAS (or each network). When one secret leaks, you only have to change one client.
- A shared secret can be changed with a reload, without a restart ([Chapter 05](05-radius-clients.md)).

## Message-Authenticator and limit_proxy_state

**Goal:** Accept only Access-Requests that carry a Message-Authenticator.

The Message-Authenticator is a signature computed over the whole packet with the shared secret. The server uses it to confirm that the request comes from a NAS that knows the shared secret and that the content was not altered. There are two related settings. They can be written in `[radius]` or as overrides in an individual `[[radius.clients]]`:

| Setting | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `require_message_authenticator` | `"yes"` | yes | `yes`: an Access-Request without a Message-Authenticator is dropped; `no`: it is accepted; `auto`: decided by the first request from this client after the server starts |
| `limit_proxy_state` | `"yes"` | yes | Takes effect only when a Message-Authenticator is not required. `yes`: an Access-Request that has Proxy-State but no Message-Authenticator is dropped |

Status-Server ignores both settings and always needs a correct Message-Authenticator. For the full description of the keys, see [Appendix A](90-config-reference.md); for how to write clients, see [Chapter 05](05-radius-clients.md).

### The default is the safest

If you set nothing, both are `yes`. Use the `--no-message-authenticator` option of `rradius-bench` to imitate an old NAS that does not send a Message-Authenticator:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --no-message-authenticator \
    --load pap,concurrency=1 --requests 1 --timeout 2
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

The server does not answer, and the log has one line (at most once per second for the same port; the number after `discarded` is the count accumulated in that time):

```text
… WARN rradius_core::radius: RADIUS: discarded 1 Access-Requests lacking a Message-Authenticator, the last from 127.0.0.1; see require_message_authenticator and limit_proxy_state
```

`Discarded: invalid` in `status` goes up.

### When an old NAS does not send Message-Authenticator

Set `"no"` only for that one NAS. Do not change the global value in `[radius]`, and keep `limit_proxy_state` at `yes`:

```toml
# rradius.toml
[[radius.clients]]
name = "old-switch"
address = "192.0.2.20"
secret_file = "old-switch.secret"
require_message_authenticator = "no"
```

### Do not use auto

`auto` lets the server learn from the first Access-Request of this client after start-up: if the first one has a Message-Authenticator, all later ones must have it; if the first has none, later ones are never required to, until a restart. A forged first packet can turn the protection off, so when the server learns "not required" it logs a warning (once per source address):

```text
… WARN rradius_core::lua_engine: RADIUS client 'localhost' (127.0.0.1): the first request had no Message-Authenticator, so require_message_authenticator (auto) became no and stays no until the server restarts; a forged first packet can switch this protection off; set it to "yes" or "no" in the client's settings
```

This was tested: with `auto` set, after one request without a Message-Authenticator was sent first, later requests without it were also accepted.

Fix: do as the message says and write `"yes"` or `"no"` explicitly.

`auto` can also be used only on a client with a single address. On a network, `check` refuses it:

```console
$ rradiusd check --config rradius.toml
rradius.toml: radius clients: client '127.0.0.0/24': require_message_authenticator = "auto" needs a single host, not a network
```

`limit_proxy_state = "auto"` on a network gives the same message, with only the setting name different.

## Turn off MS-CHAPv1

**Goal:** Do not accept MS-CHAPv1 logins.

MS-CHAPv1 is a very old method. When there is an `[auth]` auth chain it is **off by default**, and you do not need to write anything. Setups with no `[auth]`, where a Lua script handles `MS-CHAP-Response` itself, are outside the scope of this switch; the script has to decide. If you want to write it out explicitly in the configuration file:

```toml
# rradius.toml
[auth]
policy = "first_match"
allow_mschapv1 = false
# … (other settings omitted)
```

- The default is `false`; it can be reloaded. Write `allow_mschapv1 = true` only to let a NAS log in that cannot do without it.
- When it is not written or is `false`, an MS-CHAPv1 request is refused before any handler runs. It does not query the database and does not count towards lockout after failed logins.
- PAP, CHAP, MS-CHAPv2 and EAP are not affected. A TACACS+ MS-CHAP login is also refused, with FAIL.
- Write the value as a boolean. Written as a string it gives `` `allow_mschapv1`: invalid type: string, expected a boolean ``.

`rradius-bench` has no MS-CHAPv1 load. Here a small program of our own sent one Access-Request with `MS-CHAP-Challenge` and `MS-CHAP-Response`. The server answered Access-Reject, and the log is:

```text
… INFO rradius_core::auth_chain: MS-CHAPv1 refused: client localhost, protocol radius, user "dave"; [auth] allow_mschapv1 is not true
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="dave" account="" chain="default" handler="mschapv1_refused" decision="reject"
```

`handler="mschapv1_refused"` in the authentication record lets you find in the log which NASes still use MS-CHAPv1.

> **Not verified on real hardware:** MS-CHAPv1 was not sent from a real NAS or TACACS+ device. Only the RADIUS case was run, with the test packet above.

## Stored passwords: format and hash cost limits

**Goal:** Every password in the account table states its format, and nobody can drag the server down with an extremely expensive hash.

### Every password needs a scheme prefix

For a password read by the sql handler, the format is decided by the prefix at the start, for example `{crypt}`, `{argon2}`, `{SHA256}`, `{nthash}`; `{clear}` means plaintext (for the list of formats see [Chapter 07](07-users-and-chains.md)). A value without a prefix logs a warning in two cases:

It looks like a hash (for example `$2y$…`) but has no prefix: login is refused.

```text
… WARN rradius_core::auth_chain: the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself user="eve"
```

It looks like a hexadecimal digest but has no prefix: **it is compared as plaintext**.

```text
… WARN rradius_core::auth_chain: the stored password looks like a digest with no scheme prefix and is compared as plaintext; add its prefix ({nthash}, {MD5}, {SHA}, {SHA256}, ...), or {clear} if it is the password itself user="frank"
```

> **Warning:** The second case was tested: `frank` has a 32-digit hexadecimal MD5 digest stored with no prefix. Logging in with the original password is refused, but **logging in with the digest itself as the password succeeds**. Anyone who gets the account table can log in with the digest directly. When you see this warning, always add the correct prefix.

Each of these two warnings is logged once per account after the server starts.

### Hash cost limits

For formats such as `{PBKDF2…}`, the `$5$`/`$6$`/bcrypt forms of `{crypt}`, and `{argon2}`, the computation cost is written inside the stored value. Someone who can write the account table can put in a value with an enormous iteration count, so that every login uses up CPU or memory. rradius reads the cost before computing, and if it is over the limit, it does not compute and treats it as a wrong password:

| Setting (`[auth.password_limits]`) | Default | What it limits |
| --- | --- | --- |
| `pbkdf2_iterations` | 5000000 | Iteration count of PBKDF2 and X-PBKDF2 |
| `crypt_rounds` | 1000000 | Rounds of `$5$`, `$6$`, `$sha1$` and extended DES (starting with `_`) crypt |
| `bcrypt_cost` | 15 | bcrypt cost; each +1 doubles the work |
| `argon2_memory_kib` | 262144 (256 MiB) | argon2 memory |
| `argon2_iterations` | 10 | argon2 iteration count |
| `argon2_lanes` | 16 | argon2 parallelism |

- Without this section, the default limits still apply. It can be reloaded.
- Every value must be greater than 0. Writing 0 gives `auth.password_limits.bcrypt_cost must be greater than 0` (with the matching key name) and exit code 2.
- The worst case of `argon2_memory_kib` is this value times the number of CPUs, because the number of checks running at once equals the number of CPUs. Lower it on machines with little memory.

Tested: `bob` in the quick-start has a bcrypt with cost 10. Lower the limit to 9:

```toml
# rradius.toml
[auth.password_limits]
bcrypt_cost = 9
```

`bob` is refused even with the correct password, and the log records it once (not again for the same account):

```text
… WARN rradius_core::auth_chain: the stored password asks for more than [auth.password_limits].bcrypt_cost allows and is refused; raise the limit if the hash is meant to be used user="bob" format="crypt" parameter="bcrypt_cost"
```

`rradius_password_checks_total{scheme="crypt",result="failed"}` also goes up. If your accounts really use a higher cost, raise the limit as the message says; otherwise someone changed the account table, and you should investigate.

For the `[accounts] scheme` that `user add` uses to store passwords, see [Chapter 07](07-users-and-chains.md).

## Lockout after failed logins

**Goal:** Lock an account temporarily when the same account keeps guessing wrong passwords from the same device.

By default there is no lockout, and the number of guesses is not limited. Adding `[auth.lockout]` starts the counting:

```toml
# rradius.toml
[auth.lockout]
max_failures = 10
window_seconds = 300
lock_seconds = 300
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `max_failures` | 10 | How many failures within `window_seconds` lock it |
| `window_seconds` | 300 | Counting time, from the first failure |
| `lock_seconds` | 300 | How long it stays locked |
| `max_entries` | 100000 | How many pairs each node remembers at most |
| `account_max_failures` | not set | Upper limit of the total failures of one account regardless of source; must be greater than `max_failures` |
| `max_in_flight` | 5 times `max_failures` | Logins of the same pair waiting for a result at the same time; any more are refused immediately |

- The unit of counting is "account + source device": the source is `Calling-Station-Id` for RADIUS and `rem_addr` for TACACS+; when there is none, it is an empty string.
- While locked, requests are refused immediately and the password is not checked, even if it is correct. A successful login clears the count.
- A login that fails because the database is unavailable is not counted; an account that does not exist is counted all the same, so the lockout behavior does not show whether an account exists.
- The counts are kept in the memory of each node: a reload keeps them, a restart resets them, and nodes of a cluster do not share them.
- It can be reloaded.

Tested: `max_failures = 3`, `lock_seconds = 60`, `alice` types a wrong password three times, then the correct one the fourth time:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_core::auth_chain: auth lockout: client localhost: account "alice" from "" is locked (this origin)
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="lockout" decision="reject"
```

- `from ""` is an empty source: `rradius-bench` does not send `Calling-Station-Id`. A real NAS sends the MAC address of the device.
- On the last line, `handler="lockout"`: the password was not checked.

Metrics:

```console
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_auth_lockout'
rradius_auth_lockout_rejected_total 1
rradius_auth_lockouts_total 1
rradius_auth_lockout_untracked_total 0
rradius_auth_lockout_untracked_total 0
```

When `rradius_auth_lockout_untracked_total` goes up, the table is full and all of it is locked pairs, so new pairs are not tracked. Tested with `max_entries = 1` and `max_failures = 1`: `alice` types one wrong password and is locked, then `bob` types a wrong one:

```text
… WARN rradius_core::auth_lockout: auth lockout: the table of 1 keys is full of locks; failures of new keys are not counted
```

`rradius_auth_lockout_untracked_total` becomes 1. This line is logged at most once a minute. If you see it in production, you are usually under heavy guessing, and you need to block it at the network layer.

> **Warning:** `account_max_failures` makes it useless for a password guesser to switch devices, but it also lets anyone type wrong passwords on purpose to lock one account on all devices. Whether to set it depends on which of the two you fear more.

Examples of wrong settings (the exit code is 2 for all):

```text
rradius.toml: auth.lockout.max_failures must be greater than 0
rradius.toml: auth.lockout.account_max_failures must be greater than max_failures (10), not 5
```

For the full description of auth chains, see [Chapter 07](07-users-and-chains.md).

## Limits on concurrent work

The core has no setting for a rate limit such as "requests per second". The limits come from the number of requests handled at the same time:

| Setting | Default | When exceeded |
| --- | --- | --- |
| `radius.max_auth_inflight`, `max_acct_inflight` | 4096 | Dropped; `Discarded: overload` goes up; the log has `RADIUS authentication overloaded: discarded … packets` |
| `tacacs.max_connections`, `max_connections_per_source` | 1024, 64 | Connections over the limit are closed immediately |
| `tacacs.max_body_len` | 65536 | The connection is closed if the packet body is larger |
| `tacacs.idle_timeout_ms` | 300000 | The connection is closed if no complete packet arrives in this time |
| `auth.lockout.max_in_flight` | see the previous section | Refused immediately |

For how to adjust these values to the machine, see [Chapter 19](19-performance.md).

Retransmit detection (answering with the earlier answer when the same NAS retransmits the same request) exists to avoid processing twice. It is not a security mechanism; it is described in [Chapter 05](05-radius-clients.md).

## Scripts: sandbox, time limits and the HTTP allowlist

**Goal:** When a script fails or is badly written, the effect is limited to that one request.

### Sandbox

Lua scripts run in a restricted environment: there is no `io` for reading and writing files, `os.execute` does not exist, `require`, `loadstring`, `getfenv` and `setfenv` are removed, and the built-in libraries are read-only. Tested: when a script calls `io.open` or `os.execute`, that request is refused and the log is:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to index nil with 'open'\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: api.lua:1: attempt to call a nil value\nstack traceback:\n\tapi.lua:1: in function <api.lua:1>
```

### Time and memory

| Setting | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `lua.max_execution_ms` | 1000 | yes | Time the script itself runs for each request; time waiting for the database is not counted |
| `lua.max_memory_mb` | 256 | restart | Memory limit of each Lua VM |
| `lua.query_timeout_ms` | 5000 | yes | Total time each request waits for the database |

Tested with `max_execution_ms = 100` and the script `while true do end`: the request is refused:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
```

According to the spec, catching this error in the script with `pcall` does not help; the request still counts as failed (this was not tested separately).

### HTTP allowlist

The `http_request` function of a script can call only the hosts listed in `allowed_hosts` in `[lua.http]`. Without a `[lua.http]` section, `http_request` does not exist at all: tested, calling it gives `attempt to call a nil value` and the request is refused.

```toml
# rradius.toml
[lua.http]
allowed_hosts = ["members.example.com", "http://legacy.internal:8080"]
timeout_ms = 1500
```

- A host name alone allows only HTTPS on port 443. `host:port` allows HTTPS on the given port. For plaintext HTTP you must write `http://host:port`, which states clearly that you know it is not encrypted.
- Writing `https://…` is refused: `` lua.http.allowed_hosts: 'https://api.example.com' is not host, host:port or http://host:port ``.
- An empty list is refused: `lua.http.allowed_hosts must list at least one host`.
- HTTPS trusts the system root certificates by default; add an internal CA with `ca_file`.

Tested: a script calls `http://127.0.0.1:8080/login`, which is not on the list:

```text
… WARN rradius_core::lua_engine: script http_request refused: http://127.0.0.1 is not in lua.http.allowed_hosts
```

`rradius_script_http_requests_total{result="denied"}` goes up. The script gets `nil` and an error message from `http_request`, and what to do about it is up to the script.

When you build a URL, always encode values that come from users with `url_encode` first. For how to write scripts see [Chapter 10](10-lua-policies.md), and for function details see [Appendix D](93-lua-reference.md).

## Encrypting connections to databases and LDAP

**Goal:** Passwords and query results do not travel over the network in plaintext.

### External databases

`tls` of `[databases.<name>]`:

| Value | Meaning |
| --- | --- |
| `"verify"` (default) | Encrypted, and the certificate chain and host name are verified |
| `"off"` | Not encrypted |

When it is set to `"off"` and `host` is not local (`localhost` or a loopback address), the server logs a warning at start-up:

```text
… WARN rradius_core::external_db: databases.remote: tls = "off" sends the password and the data to 192.0.2.10 unencrypted
```

> **Note:** This warning appears only when the server starts; `check` does not print it. Tested: with the same configuration, `check` printed only `configuration ok`.

When the database uses a certificate signed by an internal CA, name the CA with `ca_file`; do not change to `"off"`. For details see [Chapter 09](09-external-databases.md).

### LDAP

`urls` of the ldap handler:

- `ldaps://`: a TLS connection.
- `ldap://`: upgraded to an encrypted connection with StartTLS by default.
- `ldap://` with `allow_plaintext = true`: **not encrypted**. When `bind` checks a password, the user's password is sent to the directory server in plaintext.

With `allow_plaintext = true` and `ldap://`, start-up, every reload and `check` warn `ldap handler ldap://… is used without encryption because allow_plaintext is true`, but `check` still passes. Do not turn it on unless the directory server is on the same machine. Name an internal CA with `ca_file`. For details see [Chapter 08](08-ldap-and-ad.md).

> **Not verified on real hardware:** Encrypted connections were not tested against a real LDAP or PostgreSQL/MySQL server. The settings and warnings were confirmed only with `check` and an address that cannot be reached.

## Open the metrics port only on the internal network

The HTTP port opened by `[metrics]` has `/metrics`, `/livez` and `/readyz`, with no authentication at all; anyone who can reach it can read it. The metrics contain client names, database names and various counts.

- `metrics.bind` is `127.0.0.1` by default. To let Prometheus on another machine scrape it, bind to an address on the internal or management network; do not bind `0.0.0.0` on an external interface.
- Changing `[metrics]` needs a restart.
- The `status` command reads this very port, so run `status` where it can reach the port.

For details see [Chapter 17](17-monitoring.md).

## Cluster secrets and the network between nodes

The Raft port and the API port of the cluster are protected only by `secret_raft` and `secret_api`; the connections themselves are not encrypted.

- Both secrets must be at least 16 characters. Keep them in `secret_raft_file` and `secret_api_file`, with the permissions described in [Permissions of secret files](#permissions-of-secret-files).
- On a single machine (no `cluster.nodes`), the cluster ports are bound by default only to `127.0.0.1`.
- With `cluster.nodes` but no `cluster.listen`, the cluster ports are bound on all interfaces, and `check` warns:

```text
… WARN rradius_core::config: cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces, with nothing but their shared secrets to protect them; set listen to this node's private or WireGuard address, or to "0.0.0.0" to keep it and silence this warning
```

Fix: set `listen` to this node's address on the internal network or on an encrypted tunnel such as WireGuard. When nodes must cross an untrusted network, make both the `raft` and `api` addresses in `cluster.nodes` go through an encrypted tunnel. For cluster deployment and networking, see [Chapter 16](16-cluster.md).

> **Not verified on real hardware:** The cluster network and WireGuard between several machines were not tested on real hardware. The warning above was reproduced with `check` on a single machine.

## Trace writes personal data

`[log.trace]` writes the whole request and response of given users or clients into the log. While it is on, server start-up and every reload remind you:

```text
… WARN rradius_core::radius: [log.trace] is on for 5 minutes: requests of these users and clients are written out in full, personal data included
```

In a tested trace line, `User-Password` and `Message-Authenticator` are shown as `<hidden>`, and the other attributes are written as they are:

```text
… INFO rradius_trace: request client="localhost" source=127.0.0.1 user="alice" code=1 attributes="Message-Authenticator = <hidden>, User-Name = alice, User-Password = <hidden>, NAS-IP-Address = 192.0.2.1, NAS-Identifier = rradius-bench"
```

- `minutes` must be from 1 to 1440; out of range gives `log.trace.minutes: 2000 is not from 1 to 1440`. It stops by itself when the time is up.
- When you have finished troubleshooting, remove `[log.trace]` from the configuration, and deal with the logs that were already written.

For how to use trace, see [Chapter 17](17-monitoring.md).

## systemd and Kubernetes restrictions

### systemd

The `packages/common/rradius.service` shipped with the packages already applies these restrictions:

| Setting | Effect |
| --- | --- |
| `User=rradius`, `Group=rradius` | Does not run as root |
| `AmbientCapabilities=CAP_NET_BIND_SERVICE`, `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` | Grants only the right to bind low ports (TACACS+ port 49) |
| `NoNewPrivileges=yes` | Cannot gain more privileges |
| `ProtectSystem=strict`, `ReadWritePaths=/var/lib/rradius /var/log/rradius` | The whole system is read-only; only the data directory and the log directory are writable |
| `ProtectHome=yes`, `PrivateTmp=yes` | Cannot see home directories; has its own `/tmp` |

So secret files owned by `root:rradius` with permissions `640` are the best fit: the service can read them but not change them. When your configuration uses another writable path (for example an accounting log kept elsewhere), add it to `ReadWritePaths`, or put it under `/var/lib/rradius` or `/var/log/rradius`. For installation and the other settings of the unit, see [Chapter 02](02-install.md).

### Kubernetes

`deploy/kubernetes/networkpolicy.yaml` (not in `kustomization.yaml` by default; add it yourself):

- RADIUS 1812 and 1813/UDP: from anywhere.
- Cluster 8100 and 8200: only from other rradius pods.
- Metrics 9812: only from the namespace with the label `kubernetes.io/metadata.name: monitoring`. Change the label to the namespace where your Prometheus runs.

`deploy/kubernetes/secret.example.yaml` only shows the shape of a Secret; the values in it are not real passwords. Follow the comment at the top of the file: generate `raft` and `api` with `openssl rand -hex 32` and create them with `kubectl create secret`. For deploying the cluster on Kubernetes, see [Chapter 16](16-cluster.md).

> **Not verified on real hardware:** The systemd unit and the NetworkPolicy were only compared with the file contents, and were not tested on a Linux host or a Kubernetes cluster. A NetworkPolicy takes effect only when the network plugin supports it.

## Keys and passwords in memory

After use, rradius's own code overwrites these data in memory: the EAP MSK and the key material it is derived from, the password of a PAP request, the NT hash and the values derived from it, and the MPPE keys (after the reply attributes are encoded). The debug output format also does not show passwords.

Data held inside third-party libraries, values handed to scripts, and copies left by the compiler are outside this scope.

> **Not verified on real hardware:** This section is based on the spec and the code; it was not checked with a memory dump.

## Common mistakes

**Symptom:** `check` prints `configuration ok`, but there are several `WARN` lines before it.
**Cause:** Warnings do not affect the exit code.
**Fix:** Handle them one by one as in [Find all warnings with check](#find-all-warnings-with-check). A deployment script can block on them with `grep WARN`.

**Symptom:** After `chmod 600`, a service installed from a package cannot read the secret file and fails to start.
**Cause:** The file is owned by root, and the service account `rradius` cannot read it.
**Fix:** `chown root:rradius` and `chmod 640`. `640` gives no warning.

**Symptom:** `check` fails: `cluster.secret_raft must be at least 16 characters`, exit code 2.
**Cause:** The cluster secret is too short.
**Fix:** `openssl rand -hex 32 > raft.secret`. Every node uses the same one.

**Symptom:** `check` fails: `require_message_authenticator = "auto" needs a single host, not a network`.
**Cause:** `auto` is used on a client that is a network, or `[radius]` sets `auto` and the network client does not override it.
**Fix:** Change it to `"yes"` or `"no"`.

**Symptom:** `check` fails: `` `require_message_authenticator`: unknown variant, expected one of `yes`, `no`, `auto` ``.
**Cause:** The value is wrong, or written as a boolean.
**Fix:** Write the string `"yes"` or `"no"`.

**Symptom:** Requests from an old NAS get no answer, and the log has `discarded … Access-Requests lacking a Message-Authenticator`.
**Cause:** That NAS does not send a Message-Authenticator.
**Fix:** First see whether the NAS can turn it on. If not, set `require_message_authenticator = "no"` only on its client.

**Symptom:** An account with the correct password keeps being refused, and the authentication record shows `handler="lockout"`.
**Cause:** The account failed too many times from this device and is locked.
**Fix:** Wait for `lock_seconds` to pass; if it is urgent, restarting the server clears all counts. Find out who is guessing.

**Symptom:** An account with the correct password is refused, and the log has `asks for more than [auth.password_limits]`.
**Cause:** The cost of the stored hash is over the limit.
**Fix:** Make sure the hash was generated by you, then raise the matching limit; if not, find out who changed the account table.

**Symptom:** `check` fails: `` lua.http.allowed_hosts: '…' is not host, host:port or http://host:port ``.
**Cause:** `https://` or a path was written.
**Fix:** For HTTPS write only the host or `host:port`; for plaintext HTTP write `http://host:port`.

**Symptom:** An HTTP call from a script fails, and the log has `script http_request refused: … is not in lua.http.allowed_hosts`.
**Cause:** The host is not on the allowlist, or its port or protocol differs from what is written on the allowlist.
**Fix:** Add it to `allowed_hosts`. Changing `[lua]` can be reloaded.

## Related chapters

- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [08 LDAP and Active Directory](08-ldap-and-ad.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [21 Troubleshooting](21-troubleshooting.md)
- [Appendix A: Configuration reference](90-config-reference.md)
