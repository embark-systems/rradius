# 01 Getting to know rradius

> After this chapter you can:
> - Say what rradius does and what it does not do
> - Follow a RADIUS request through the server and know which places it passes
> - Tell the configuration file, the data directory and the dictionary apart, and what each one is for
> - Decide whether the feature you need is in the core edition or needs the enterprise edition

Prerequisites: none. This chapter only covers concepts. You do not need to install anything first.

In this chapter:

- [What rradius is](#what-rradius-is)
- [Supported protocols](#supported-protocols)
- [One program, with a built-in cluster database](#one-program-with-a-built-in-cluster-database)
- [How a request travels](#how-a-request-travels)
- [The configuration file, the data directory and the dictionary](#the-configuration-file-the-data-directory-and-the-dictionary)
- [Command overview](#command-overview)
- [Testing with external tools](#testing-with-external-tools)
- [Core and enterprise editions](#core-and-enterprise-editions)
- [How to read this manual](#how-to-read-this-manual)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## What rradius is

rradius is an AAA server. It answers the question "can this user come in?" for a NAS (network access server, for example a Wi-Fi controller, a switch, a BNG or a VPN gateway), and it records how long each user stayed and how much traffic they used.

It has three features that the later chapters rely on:

1. **Login rules live in the configuration file, in SQL and in Lua scripts.** In simple cases you write one SQL query in the configuration file. When you need rules about time of day, quotas or groups, you write a Lua script (in practice the Luau dialect).
2. **Data lives in a built-in cluster database.** Accounts, accounting and sessions are stored in a database that ships with the program. It runs on a single machine, and for high availability you join several machines into a cluster. You do not need a separate database server.
3. **There is only one executable.** The server and the management commands are both `rradiusd`: `rradiusd serve` starts the service, `rradiusd user add` adds an account, and `rradiusd status` shows the state.

What rradius does **not** do:

- It has no web management interface. All management is done on the command line and in the configuration file.
- It does not decide what your tables look like. You create the tables yourself with migrations (SQL files), and you write the queries. The example scenarios include a set of tables you can use as they are.
- It does not manage certificates. You prepare the server certificate and the CA certificate that EAP needs (chapter 11).
- The management commands work through the cluster's API port and the metrics port. There is no separate management API service.

## Supported protocols

The core edition of `rradiusd` supports:

| Protocol | Used for | Default port | Described in |
| --- | --- | --- | --- |
| RADIUS authentication (RFC 2865) | User login: PAP, CHAP, MS-CHAP, MS-CHAPv2 | UDP 1812 | Chapters 05 and 07 |
| RADIUS accounting (RFC 2866) | Accounting records such as Start, Interim-Update and Stop | UDP 1813 | Chapter 13 |
| Status-Server (RFC 5997) | A monitoring system checks that the server is alive, without a test account | The same two ports | Chapter 05 |
| EAP over RADIUS | 802.1X: EAP-TLS, PEAP, EAP-TTLS | The RADIUS authentication port | Chapter 11 |
| TACACS+ (RFC 8907) | Administrator login to network devices, command authorization, accounting | TCP 49 | Chapter 12 |

There are also two ports that are not AAA protocols but that every node uses:

- The metrics port: it is opened only when `[metrics]` is set. It defaults to `127.0.0.1:9812` and serves `/metrics`, `/livez` and `/readyz` (chapter 17). The `status` command reads this port.
- The cluster ports: the Raft port and the API port. The starting file that `config init` produces uses `127.0.0.1:8100` and `127.0.0.1:8200`. Nodes synchronize data over them, and management commands write data through them (chapter 16).

> **Note:** The metrics port has no authentication, so bind it to an internal address only. The cluster ports are protected only by a shared secret, so do not expose them to the outside either.

## One program, with a built-in cluster database

The rradius database is hiqlite. Each node has its own SQLite database, and the nodes synchronize with each other using the Raft protocol. For you, this means:

- **A single machine is a cluster of one node.** The configuration always has a `[cluster]` section, even with one machine. On a single machine you can ignore leaders and followers.
- **Writes go through the leader.** One node in the cluster is the leader, and every write needs its confirmation. While the cluster cannot elect a leader, requests that need to write get no result.
- **Tables are created by migrations.** `migrations_dir` in `[cluster]` points to a directory of SQL files. The server applies them in order of the number in the file name when it starts, and it does not run files it has already applied. To change the structure, add a file with the next number. Do not edit an old file. A single file cannot be larger than 2,000,000 bytes (the limit for one write to the cluster): put structure and a small amount of initial data in migrations, and do not write bulk data as a migration. If a file is larger, the server fails to start with the message `migration file '3_big.sql' is 3000001 bytes, more than the 2000000 the cluster takes in one write: …`. `check` does not find this, so you only learn about it at startup. It does not matter if several files add up to more than 2,000,000 bytes: the server writes them in batches by itself.
- **Management commands write through the API port.** Commands such as `user add` do not open the database files directly. They connect to the cluster's API port, so the server must be running first for the command to have something to talk to.

Besides the built-in database, you can also have the sql handler query an external PostgreSQL or MySQL (chapter 09), or authenticate with LDAP or Active Directory (chapter 08).

## How a request travels

If you know which checkpoints a request passes inside the server, you know where to look when something goes wrong. The following uses RADIUS as the example and follows the order the code actually uses.

### Authentication requests (Access-Request)

1. **Status-Server is handled first.** If the packet is a Status-Server, the server answers it right away. It runs no script and does not go through retransmit detection.
2. **Match the client.** The server uses the packet's source IP address to find the matching client in `[[radius.clients]]`. A client can be a single address or a whole network (chapter 05). If none is found, the packet is dropped without a reply, and the log records at most one line per second: `RADIUS packet from unknown client … discarded`.
3. **Check the packet.** The server checks the Message-Authenticator with this client's shared secret. If the computed value is wrong (usually because the shared secret is set wrong), the packet is dropped. If the client is required to send a Message-Authenticator and the packet has none, it is also dropped (`require_message_authenticator`, chapter 05).
4. **Retransmit detection.** When a NAS gets no reply, it sends the same packet again (retransmit). The server remembers the requests it handled recently: a retransmit of a request that is still being processed is dropped, and a request that was already answered gets the same reply again.
5. **No reply when there is no leader.** If `cluster.stop_without_leader_ms` is set and the cluster has had no leader for longer than that time, authentication requests get no reply, so the NAS asks another server (chapter 16).
6. **EAP.** If the packet carries an EAP-Message and `[radius.eap]` is set, it goes to EAP handling. The server first completes a TLS handshake with the client, which involves several Access-Challenge round trips (chapter 11). The identity inside EAP is still passed to the next step to decide:
   - EAP-TLS: after the certificate is verified, the server asks the auth chain using the name the certificate proves. If there is no auth chain, it asks the top-level Lua script.
   - PEAP and EAP-TTLS: the password inside the tunnel must be checked by an auth chain. If the client has no auth chain, this kind of login is always rejected.
7. **Auth chain or Lua script, one of the two.** Only one of these is used:
   - If `[auth]` is set, the server uses an auth chain. If the client has `auth = "<name>"`, it uses `[auth.chains.<name>]`. Otherwise it uses the one in `[auth]` itself (named `default`). An auth chain is made of one or more handlers (sql, lua, ldap, netlogon), and `policy` decides the result (chapter 07).
   - If there is no `[auth]`, the server runs the `script` in `[lua]` with `phase` set to `authentication`. The script returns `accept`, `reject` or `challenge` (chapter 10).
8. **No reply when the database is unavailable.** If the database or an external backend cannot be used during authentication, the server does not send an Access-Reject. It sends no reply, and the log says `not answered: the authentication backend could not be used`. The NAS then retransmits or asks another server, so a user is not treated as having a wrong password because a backend failed for a moment.
9. **The reply.** If the result is accept, the server encodes the reply attributes (for example `Session-Timeout` and `Filter-Id`) according to the dictionary and sends an Access-Accept. If the result is reject, it sends an Access-Reject. Every authentication writes one authentication record to the log (`auth` in `[log]`, on by default). The record contains the client, the user, the auth chain, the handler and the result.

> **Note:** Once `[auth]` is set, the top-level Lua script no longer takes part in RADIUS password authentication (except for EAP-TLS without an auth chain). Many people think the script runs once more after the auth chain, but it does not. To add rules inside the auth chain, use a lua handler (chapter 07).

### Accounting requests (Accounting-Request)

Accounting takes a different, shorter path:

1. As with authentication, the server first matches the client, then checks the Request Authenticator with the shared secret, then does retransmit detection.
2. **It always goes to the top-level Lua script**, with `phase` set to `accounting`. Accounting requests do not go through an auth chain.
3. The server sends an Accounting-Response only if the script returns `accept`. If there is no reply, the NAS retransmits, so the writes in your script must be safe to run more than once.
4. If the script fails to write to the database and `[radius.accounting_spool]` is set, the server first writes the request to the accounting spool (a temporary store on disk where requests wait to be replayed) and then replies to the NAS. When the database recovers, the server hands the requests in the spool back to the script by itself (chapter 13).

### TACACS+

TACACS+ runs over TCP, and the flow is similar. The server matches `[[tacacs.clients]]` by source address. A login password goes to an auth chain or a script. Command authorization is decided by the rules in `authorize_groups` or by a script. Accounting goes to a script. The details are in chapter 12.

## The configuration file, the data directory and the dictionary

rradius needs three things to run, and each has a different role:

| What | What it is | Who writes it | Where it is |
| --- | --- | --- | --- |
| Configuration file | A TOML file, plus the files it pulls in with `include`. It holds the listening ports, the clients, the auth chains, the script location and the cluster settings | You | `/etc/rradius/rradius.toml` when there is no `--config` |
| Data directory | `data_dir` in `[cluster]`: the cluster database and the Raft log | The server | `/var/lib/rradius` for the packages and Docker; `data` next to the configuration file for the example scenarios |
| Dictionary | The top-level `dictionary`: the table of attribute names, numbers and types | Do not change it | `/usr/share/rradius/dictionary` for the packages and Docker |

A few things are easy to get wrong:

- **Relative paths in the configuration file are relative to the directory that holds the configuration file.** They are not relative to the directory you ran the command from. So `data_dir = "data"` means the `data` directory next to the configuration file.
- **A data directory can be used by only one node.** It is the server's own state. Do not edit the files inside by hand, and do not let two programs use the same directory at the same time.
- **The dictionary decides which attribute names scripts and the configuration can use.** The server reads the dictionary at startup, and names such as `Filter-Id` and `Session-Timeout` come from it. The dictionary that ships with rradius is an unmodified copy of the dictionary files of the FreeRADIUS project v3.2.x (licensed under CC-BY 4.0; see `NOTICE` in the dictionary directory). To add your own attributes, see chapter 06.

Besides these three, you usually also have:

- A Lua script (`script` in `[lua]`): the configuration must have a `[lua]` section, and the script file must exist.
- A migration directory (`cluster.migrations_dir`): SQL files that create the tables.
- Secret files: the shared secrets and the cluster secrets go in separate files, which you point to with `secret_file`, `secret_raft_file` and `secret_api_file`. Set their permissions to 0600 or 0640 (chapter 04).

## Command overview

Run `rradiusd` with no arguments, or run `rradiusd help`, to print all the commands:

```console
$ rradiusd help
usage: rradiusd serve [--config <path>]
usage: rradiusd check [--config <path>]
usage: rradiusd status [--config <path>] [--watch] [--json] [--cluster]
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
without --config, the configuration is /etc/rradius/rradius.toml
```

| Command | What it does | Described in |
| --- | --- | --- |
| `serve` | Starts the server | Chapter 03 |
| `check` | Checks the configuration file without starting any service | Chapter 04 |
| `config show`/`diff`/`init` | Prints the full configuration with defaults, compares two configurations, produces a starting file | Chapter 04 |
| `schema` | Prints the JSON Schema of the configuration file, with a description and default for every key | Chapter 04 |
| `status` | Reads the metrics port and summarizes it as a one-page status | Chapter 17 |
| `tuning` | Gives performance tuning advice based on this machine | Chapter 19 |
| `user` | Adds, changes the password of, disables, enables, deletes and shows accounts | Chapter 07 |
| `session` | Lists the sessions that are online | Chapter 13 |
| `query` | Runs a named query from the configuration | Chapter 09 |
| `cluster` | Shows and removes cluster members | Chapter 16 |

There is also a separate load-testing program, `rradius-bench`, which can send RADIUS and TACACS+ requests. This manual uses it as the login test tool in chapter 03 and for load testing in chapter 19.

## Testing with external tools

All the login tests in this manual use `rradius-bench` (chapter 03). If you want to use other client tools:

- **RADIUS:** `radtest` and `radclient` from FreeRADIUS (in the `freeradius-utils` package on Debian and Ubuntu; on macOS you can use `freeradius-server` from Homebrew).
- **802.1X:** `eapol_test` from wpa_supplicant (chapter 11).
- **TACACS+:** `tacacs_client` from the Python package `tacacs_plus` (chapter 12).

By default rradius drops Access-Requests that have no Message-Authenticator (`require_message_authenticator`, chapter 05). A client tool that does not send one gets a timeout, and the server log says `discarded … Access-Requests lacking a Message-Authenticator`. In that case use `radclient` and add `Message-Authenticator=0x00` to the input, and it computes the correct value:

```console
$ echo 'User-Name=alice,User-Password=alice-password,Message-Authenticator=0x00' \
    | radclient 127.0.0.1:1812 auth "$(cat localhost.secret)"
```

```console
$ radtest alice alice-password 127.0.0.1 0 "$(cat localhost.secret)"
$ radtest -t chap alice alice-password 127.0.0.1 0 "$(cat localhost.secret)"
$ echo 'User-Name=alice,Acct-Session-Id=s1,Acct-Status-Type=Start' \
    | radclient 127.0.0.1:1813 acct "$(cat localhost.secret)"
$ echo 'Message-Authenticator=0x00' | radclient 127.0.0.1:1812 status "$(cat localhost.secret)"
```

The last line is a Status-Server request. `radius.status_server` is on by default (chapter 05).

> **Not verified on real hardware:** The external tool commands in this section were not run again for this version. The previous version of the manual tested them with `radclient` from FreeRADIUS 3.2.10 (older versions may not add the Message-Authenticator automatically). The fact that `Message-Authenticator` is required by default was checked against the code and the measurements in chapters 05 and 21.

## Core and enterprise editions

rradius comes as two programs:

- **`rradiusd` (the core edition):** the subject of this manual. The source code is public and the license is BSD-2-Clause.
- **`rradius-enterprise` (the enterprise edition):** it includes everything in the core edition and adds some enterprise features. It has its own manual.

Both programs read the same configuration file format. When the core edition meets a setting or command that exists only in the enterprise edition, it **does not ignore it silently**. It refuses it, explains why, and exits with code 2:

```console
$ rradiusd check --config rradius.toml
rradius.toml: proxy: not supported by this program (proxying needs rradius-enterprise)
$ rradiusd dpsk show alice
dpsk: not supported by this program (DPSK needs rradius-enterprise)
```

The table below lists the items that the core edition refuses and that exist only in the enterprise edition. It is based on three lookup tables in the code:

| Enterprise feature | Setting | Command |
| --- | --- | --- |
| RadSec (RADIUS over TLS) | `[radius.tls]` | — |
| Dynamic clients | `[radius.dynamic_clients]`, `[accounts.clients]` | `client` |
| Second factor (TOTP/HOTP) | `[mfa]` | `otp` |
| EAP-AKA', EAP-SIM | `[radius.eap.aka]`, `[accounts.aka]`, EAP method `aka-prime` | `aka`, `aka-seal` |
| Diameter | `[diameter]` | — |
| DPSK (one Wi-Fi password per person) | `[radius.dpsk]`, `[accounts.dpsk]` | `dpsk`, `dpsk-seal` |
| TACACS+ over TLS | `[tacacs.tls]` | — |
| Accounting export | `[accounting_export]` | — |
| Proxying by realm | `[proxy]` | — |
| CoA and Disconnect | `[radius.coa]` | `coa`, `disconnect` |
| Cluster backup | `[cluster.backup]` | — |

Besides the table, `[radius.eap]` and `[[radius.clients]]` also have a few keys that only the enterprise edition can use, for example OCSP revocation checking and session resumption in EAP. The core edition refuses most of these too, with a message that also ends in `needs rradius-enterprise`. A few are accepted but have no effect in the core edition. These keys are listed in [Appendix A](../manual.zh-TW/90-config-reference.md) and marked "not supported in core". The main text of this manual does not describe them.

The rule of thumb is simple: check your configuration with `rradiusd check`. If it passes, the core edition can run it. If you see `needs rradius-enterprise`, that item needs the enterprise edition.

## How to read this manual

Pick what you need for the job:

- **First contact:** read chapters 01 to 03 in order, until the server gives you your first Access-Accept. Then read chapters 04 and 05 to learn to write the configuration file and add a NAS.
- **Connecting an account source:** chapter 07 (the built-in tables and auth chains), chapter 08 (LDAP, AD), chapter 09 (external databases), chapter 10 (Lua scripts).
- **Setting up 802.1X:** chapter 11.
- **Controlling logins to network devices:** chapter 12.
- **Recording accounting and seeing who is online:** chapter 13.
- **Following a ready-made scenario:** chapter 14 (broadband and ISP) and chapter 15 (organization network access). Each scenario matches one directory under `rradius-server/examples/scenarios/`.
- **Going to production:** chapter 16 (clusters), chapter 17 (monitoring), chapter 18 (day-to-day operations), chapter 19 (performance), chapter 20 (security).
- **When something goes wrong:** chapter 21 (troubleshooting).
- **Looking up key names, commands, metrics and Lua functions:** appendices A to E.

The formats used in the book:

- The first line of a configuration example says which file it belongs in, for example `# rradius.toml`.
- Commands start with `$ `, and the actual output follows. Where long output is cut, the cut is marked with `…`.
- `> **Note:** ` marks things that are easy to get wrong. `> **Warning:** ` marks things that affect security or can interrupt service. `> **Not verified on real hardware:** ` means the passage was only checked against the code and the tests, and was not run on real devices or services.
- Shared secrets and cluster secrets always go in files, generated with `openssl rand -hex 32`. The examples do not contain plain-text secrets.

## Common mistakes

**Symptom:** `check` prints `…: not supported by this program (… needs rradius-enterprise)` and exits with code 2.
**Cause:** The configuration file or `conf.d/` uses a setting that exists only in the enterprise edition, or you ran a command that exists only in the enterprise edition.
**Fix:** Remove that section, or switch to `rradius-enterprise`. Use the table above to find out which item it is.

**Symptom:** After you set `[auth]`, the authentication rules in your Lua script have no effect.
**Cause:** With `[auth]`, RADIUS password authentication goes only through the auth chain. The top-level script handles only accounting (and TACACS+ authorization and accounting).
**Fix:** Rewrite the rules as a lua handler in the auth chain (chapter 07), or remove `[auth]` and leave everything to the script (chapter 10).

**Symptom:** Every PEAP or EAP-TTLS login is rejected, and the log says `this client has no chain to check a password with`.
**Cause:** The password inside the tunnel can be checked only by an auth chain, and this client has no auth chain.
**Fix:** Set `[auth]` (chapter 07, chapter 11).

**Symptom:** The NAS says the RADIUS server does not answer, but the server log has no authentication record.
**Cause:** The request was dropped while matching the client or checking the packet, for example because the source address is not in `[[radius.clients]]`, or the shared secrets do not match.
**Fix:** Look in the log for `from unknown client`, and look at the `Discarded: unknown client` and `Discarded: invalid` counters in `status` (chapter 21).

## Related chapters

- [02 Installation](02-install.md)
- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](../manual.zh-TW/07-users-and-chains.md)
- [10 Writing policies in Lua](../manual.zh-TW/10-lua-policies.md)
- [16 Clusters and high availability](../manual.zh-TW/16-cluster.md)
- [Appendix E: Glossary and RFC cross-reference](../manual.zh-TW/94-glossary.md)
