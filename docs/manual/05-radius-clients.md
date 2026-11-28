# 05 RADIUS service and NAS clients

> After this chapter you can:
> - Decide which addresses and ports RADIUS listens on
> - Add NAS devices as clients, each with its own shared secret
> - Understand how clients are matched and where packets from unknown sources go
> - Configure the Message-Authenticator and Proxy-State checks
> - Understand Status-Server, retransmits, the concurrency limit, and when the server "does not answer"

Prerequisites: a configuration that starts, built by following [chapter 04](04-configuration.md). The checks in this chapter send requests from the local machine with `rradius-bench`, so you do not need a real NAS.

In this chapter:

- [Concept](#concept)
- [Listen addresses and ports](#listen-addresses-and-ports)
- [Adding NAS clients](#adding-nas-clients)
- [How clients are matched](#how-clients-are-matched)
- [Changing a shared secret](#changing-a-shared-secret)
- [Message-Authenticator and Proxy-State](#message-authenticator-and-proxy-state)
- [Status-Server](#status-server)
- [Retransmits and duplicate requests](#retransmits-and-duplicate-requests)
- [Concurrency limit](#concurrency-limit)
- [When the server does not answer](#when-the-server-does-not-answer)
- [Connecting a new NAS](#connecting-a-new-nas)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Concept

A NAS (Network Access Server) is the device a user actually connects to: a wireless AP, a switch, a VPN gateway, a BRAS. When a user logs in, the NAS wraps the account and password in a RADIUS request and sends it to rradius, then lets the user in or refuses them based on the answer.

- **Client**: a NAS that rradius is willing to answer. Packets from a source that is not on the list are dropped without an answer.
- **Shared secret**: each client and rradius each keep a copy. It encrypts User-Password, signs response packets, and is used to check Message-Authenticator. When the two sides differ, the NAS does not see a reject. It sees no response, or an invalid one.
- **Two ports**: the authentication port receives Access-Request, and the accounting port receives Accounting-Request. Both ports receive Status-Server.

RADIUS runs over UDP. When a NAS gets no answer it retransmits, and after waiting some time without one it moves on to the next server. So when rradius cannot answer correctly, it prefers not to answer and lets the NAS ask another server (see [When the server does not answer](#when-the-server-does-not-answer)).

## Listen addresses and ports

| Key | Default | Reload | Description |
| --- | --- | --- | --- |
| `radius.bind` | `0.0.0.0` | restart | One address, or an array of addresses; every address opens both the authentication and the accounting port |
| `radius.auth_port` | `1812` | restart | Authentication port |
| `radius.acct_port` | `1813` | restart | Accounting port |
| `radius.sockets` | On Linux, the number of worker threads; `1` on other platforms | restart | How many receive sockets to open per address and port |
| `radius.recv_buffer_bytes` | System default | restart | Receive buffer of each socket, 4096 to 1073741824 bytes |

For the full types and limits, see [Appendix A](90-config-reference.md). `sockets` and `recv_buffer_bytes` affect performance under heavy traffic; how to tune them is in [chapter 19](19-performance.md).

Listen on the local IPv4 and IPv6 addresses at the same time:

```toml
# rradius.toml (excerpt)
[radius]
bind = ["127.0.0.1", "::1"]
auth_port = 1812
acct_port = 1813
```

At startup one line is logged for each address:

```text
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::radius: RADIUS Server also listening on ::1
```

Rules for `bind`:

- The array has at least one address, and the same address cannot appear twice.
- `0.0.0.0` means all IPv4 addresses and `::` means all IPv6 addresses. If you write `0.0.0.0` you cannot also write another IPv4 address (the same goes for `::`), but `["0.0.0.0", "::"]` is allowed.
- An IPv4 address written in IPv6 form (`::ffff:10.0.0.1`) is refused when the array has other addresses too. Write `10.0.0.1` instead.

For the error messages, see [Common mistakes](#common-mistakes).

> **Note:** The default `0.0.0.0` listens on every network interface. When your NAS devices only come from a management network, set `bind` to the address of that interface and add a firewall.

## Adding NAS clients

**Goal:** Let an AP with a fixed address and a whole network of switches send requests, each with its own shared secret.

**Configuration:** Each client is one `[[radius.clients]]`. A change to a client (including its shared secret) takes effect on reload.

| Key | Default | Description |
| --- | --- | --- |
| `address` | required | An IP address or a CIDR network, for example `192.0.2.10`, `198.51.100.0/24`, `2001:db8::/32` |
| `name` | the value of `address` | The name used in logs, metrics and `[log.trace]`; it cannot repeat within one list (RADIUS or TACACS+), and when `name` is not set the `address` counts as the name, so a duplicate fails at load time (see "Common mistakes") |
| `secret`, `secret_file` | set exactly one | The shared secret, or the file that holds it ([chapter 04](04-configuration.md)) |
| `auth` | the default auth chain | Which `[auth.chains.<name>]` this client's logins use instead ([chapter 07](07-users-and-chains.md)) |
| `eap_methods` | `radius.eap.methods` | The EAP methods this client may use; needs `[radius.eap]` ([chapter 11](11-eap.md)) |
| `require_message_authenticator` | the value in `[radius]` | Overrides the `[radius]` setting, see the next section |
| `limit_proxy_state` | the value in `[radius]` | Overrides the `[radius]` setting, see the next section |

For the details of each key, see [Appendix A](90-config-reference.md).

```toml
# conf.d/20-nas.toml
[[radius.clients]]
name = "branch-ap"
address = "192.0.2.10"
secret_file = "branch-ap.secret"   # openssl rand -hex 32 > branch-ap.secret

[[radius.clients]]
name = "campus"
address = "198.51.100.0/24"
secret_file = "campus.secret"      # openssl rand -hex 32 > campus.secret
```

**Check:**

```console
$ for f in branch-ap campus; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
$ kill -HUP <pid>
$ curl -s http://127.0.0.1:9812/metrics | grep 'client_responses_total{client="campus"'
rradius_radius_client_responses_total{client="campus",code="access_accept"} 0
rradius_radius_client_responses_total{client="campus",code="access_reject"} 0
rradius_radius_client_responses_total{client="campus",code="access_challenge"} 0
rradius_radius_client_responses_total{client="campus",code="accounting_response"} 0
```

**What it looks like:** Every client has a set of `rradius_radius_client_responses_total` counters, labelled with `name`. Requests that arrive later from the NAS are counted here by response type, so you can see which NAS has traffic and whether rejects suddenly go up.

When you give the same shared secret to the NAS, set rradius's address, the authentication port, the accounting port and this secret on the NAS.

- Use `cat campus.secret` to get the secret and paste it into the NAS's configuration screen. The newline at the end of the file is removed, so you do not need to worry about it.
- A shared secret shorter than 16 bytes, or one that contains the `change-me` from the examples, gets a warning; see [chapter 04](04-configuration.md).
- Some older devices limit the length or characters of the shared secret; `openssl rand -hex` produces only 0-9 and a-f, which most devices accept.

> **Note:** `authorize_group` is only for TACACS+ clients. Writing it on a RADIUS client makes `check` fail.

> **Enterprise edition:** The other client keys (RadSec, CoA, second factor) are enterprise features. In the core edition, writing them is either ignored or makes `check` fail; see [Appendix A](90-config-reference.md).

## How clients are matched

The server finds the client by the packet's **source IP**:

- **Longest prefix wins.** When one source matches several entries, the one with the smallest network is used. So you can give a whole network one client and set up another one for a single host in it.
- **Networks are normalized.** `10.0.0.5/24` equals `10.0.0.0/24`. When two entries are the same after normalization, `check` fails.
- **An IPv4-mapped IPv6 source is matched as IPv4.** On a dual-stack socket, a packet from `::ffff:192.0.2.10` is matched against `192.0.2.10`.
- **The port, NAS-IP-Address and NAS-Identifier are not used.** Only the source address counts. When the NAS is behind NAT, write the address as it is after NAT.

A packet with no matching client is dropped, counted in `rradius_radius_discarded_total{reason="unknown_client"}`, and a warning line is logged (at most one per second, so the log is not flooded):

```text
… WARN rradius_core::radius: RADIUS packet from unknown client ::1 discarded
```

**Try it:** Confirm "longest prefix wins". In a configuration where `127.0.0.1` is already the client `localhost`, add another client that covers it:

```toml
# conf.d/30-net.toml
[[radius.clients]]
name = "loopback-net"
address = "127.0.0.0/8"
secret_file = "net.secret"   # openssl rand -hex 32 > net.secret
```

After a reload, send once with each of the two secrets:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 2109/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat net.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**What it looks like:** A request from `127.0.0.1` matches `localhost`, the `/32`, so only its secret works. When you send with the network's secret, the Message-Authenticator does not match, the request is dropped as invalid (`reason="invalid"` goes up by 1), and the bench only sees a timeout.

> **Note:** When the shared secret is wrong, the NAS usually sees "no response" rather than "reject". When a NAS keeps timing out, first check whether `rradius_radius_discarded_total{reason="invalid"}` is going up with it.

### IPv6 NAS

An IPv6 NAS is written as one client, like an IPv4 one. `address` takes an IPv6 address or prefix, and `radius.bind` must include an IPv6 address (or `::`):

```toml
# conf.d/40-v6.toml
[[radius.clients]]
name = "localhost-v6"
address = "::1"
secret_file = "localhost-v6.secret"   # openssl rand -hex 32 > localhost-v6.secret
```

On a server with `bind = ["127.0.0.1", "::1"]`, requests sent from `127.0.0.1` and from `::1` match `localhost` and `localhost-v6` respectively, and both are accepted. Without `localhost-v6`, a request from `::1` is dropped as an unknown source, and the log shows the `unknown client ::1` line above.

> **Note:** When the same NAS sends requests over both IPv4 and IPv6, write one client for each address (they can use the same `secret_file`). The server does not treat them as the same NAS.

## Changing a shared secret

When a shared secret leaks, or when you rotate it on a schedule, both the server and the NAS must change. A client has only one shared secret at a time, and the server does not accept "both the old and the new one".

**Goal:** Replace the shared secret of `localhost-v6` and confirm the old one no longer works.

**Configuration and check:** Generate a new secret and write it to the same file. Until a reload the server still uses the old secret in memory, so writing the file does not affect the service right away:

```console
$ cp localhost-v6.secret old.secret
$ openssl rand -hex 32 > localhost-v6.secret
$ rradius-bench run --server ::1 --secret "$(cat old.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 --timeout 1 --retries 0
…
  pap: 1804/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ kill -HUP <pid>
$ rradius-bench run --server ::1 --secret "$(cat old.secret)" … --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server ::1 --secret "$(cat localhost-v6.secret)" … --timeout 1 --retries 0
…
  pap: 2120/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

(The second and third commands leave out the parameters they share with the first.)

**What it looks like:** Before the reload the old secret still works. After the reload only the new secret works, and requests with the old one are dropped as invalid.

The order to follow in practice:

1. Write the new secret file first and run `check` once.
2. Change to the new secret on the NAS, then send SIGHUP right away. Between the two steps the NAS's requests time out and the NAS retransmits, so the closer together the two steps are, the better.
3. Check that `rradius_radius_discarded_total{reason="invalid"}` stops going up and `rradius_radius_client_responses_total` keeps going up as usual.
4. Delete the copy of the old secret.

When you change many at once, do not let them share one secret file: one file per client lets you change them one by one, and a mistake affects only one.

## Message-Authenticator and Proxy-State

Message-Authenticator is a signature attribute in an Access-Request. Without it, an attacker between the NAS and the server may be able to change a reject into an accept (the BlastRADIUS attack, which relies on Proxy-State together with an unsigned request). So by default rradius requires every Access-Request to carry it.

| Key | Default | Reload | Value |
| --- | --- | --- | --- |
| `radius.require_message_authenticator` | `yes` | yes | `yes`, `no`, `auto` |
| `radius.limit_proxy_state` | `yes` | yes | `yes`, `no`, `auto` |

Both keys can be overridden on each client, with the same key names. See [Appendix A](90-config-reference.md).

- `require_message_authenticator = "yes"`: every Access-Request without a Message-Authenticator is dropped.
- `require_message_authenticator = "no"`: not required. Only then is `limit_proxy_state` consulted.
- `limit_proxy_state = "yes"`: an Access-Request that carries Proxy-State but no Message-Authenticator is dropped. One without Proxy-State is handled as usual.
- `limit_proxy_state = "no"`: not checked.
- `auto`: decided by the **first** Access-Request from this client after the server starts. If the first one has a Message-Authenticator, the setting is treated as `yes` from then on. If it does not, the setting is treated as `no`, and a warning is logged. What was learned is kept across reloads until the next restart. `auto` works only on a client with a single address, not on a network.

Dropped requests are counted in `rradius_radius_discarded_total{reason="invalid"}`, and a combined warning is logged:

```text
… WARN rradius_core::radius: RADIUS: discarded 1 Access-Requests lacking a Message-Authenticator, the last from 127.0.0.1; see require_message_authenticator and limit_proxy_state
```

**Try it:** Make an older device that does not send Message-Authenticator work. First look at the default behavior:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1 \
    --no-message-authenticator --timeout 1 --retries 0
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
…
```

Relax it for this client only:

```toml
# conf.d/10-clients.toml
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
require_message_authenticator = "no"
```

After a reload, send the same request again and the result is `accept 1`. Add `--attr 33=hex:0102` (which carries one Proxy-State) and send again, and it is back to `timeouts 1`: `limit_proxy_state` is still the default `yes`.

**What it looks like:**

| Client setting | Has Message-Authenticator | None, and no Proxy-State | None, but has Proxy-State |
| --- | --- | --- | --- |
| Default (`yes`) | handled | dropped | dropped |
| `require_message_authenticator = "no"` | handled | handled | dropped |
| plus `limit_proxy_state = "no"` | handled | handled | handled |

Keep the last row only for devices where you are sure there is no proxy in between and which really cannot be updated.

The warning when `auto` learns `no` from the first request:

```text
… WARN rradius_core::lua_engine: RADIUS client 'localhost-v6' (::1): the first request had no Message-Authenticator, so require_message_authenticator (auto) became no and stays no until the server restarts; …
```

> **Warning:** What `auto` learns is "what the first request looked like". If the first request is forged by an attacker, the result is `no`. When you are sure the device supports it, write `yes` directly.

## Status-Server

Status-Server (RFC 5997) is the packet a NAS or a monitoring system uses to ask "is the server still alive".

- Sent to the authentication port, it gets an Access-Accept with no attributes; sent to the accounting port, it gets an Accounting-Response. No script or auth chain runs, and no authentication record is written.
- It must carry a correct Message-Authenticator, whatever `require_message_authenticator` is set to.
- The source must be a known client, with the right shared secret.
- It does not go into the duplicate-request cache. Every one is answered again.
- The number of answers is counted in `rradius_radius_status_server_total`.
- With `radius.status_server = false` (default `true`, needs a restart), every Status-Server is dropped. See [Appendix A](90-config-reference.md).

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --load status-auth,concurrency=1 --requests 1
…
  status-auth: 4144/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --load status-acct,concurrency=1 --requests 1
…
  status-acct: 5369/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

A cluster node that has lost the majority and stopped serving does not answer Status-Server on the authentication port, so the NAS asks another node instead ([chapter 16](16-cluster.md)).

> **Not verified on real hardware:** The sentence above and "a Status-Server without Message-Authenticator is always dropped" come from the code and the specification. Only Status-Server with a Message-Authenticator was tested for this chapter.

## Retransmits and duplicate requests

When a NAS gets no answer within its timeout, it retransmits the same request. rradius recognizes retransmits with a cache:

- A request is identified by "the socket that received it, the source address and port, the Identifier, and the Request Authenticator".
- When a retransmit arrives while the original is still being handled: the retransmit is dropped and the answer is sent when the original finishes.
- When it has already been answered: the same response is sent again, without authenticating or accounting again.
- After answering, the entry is kept for 10 seconds, up to 500000 entries; when the cache is full, new requests are handled as usual, they just do not go into the cache.
- A packet with the same source and Identifier but a different Request Authenticator is a new request and replaces the old entry in the cache.
- Retransmits are counted in `rradius_radius_duplicates_total`.

So a slow login that the NAS retransmits many times still runs authentication once and writes one authentication record.

**Check:** Use a script that takes a few hundred milliseconds (for example a loop run for `User-Name == 'slow'`) and make the bench retransmit after 0.1 seconds:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user slow --password x --load pap,concurrency=1 --requests 1 --timeout 0.1 --retries 3
…
  pap: 6/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 1
…
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_radius_duplicates_total'
rradius_radius_duplicates_total 1
```

**What it looks like:** The bench retransmitted once, the server recognized the retransmit, and the log has only one authentication record for this user.

## Concurrency limit

| Key | Default | Reload |
| --- | --- | --- |
| `radius.max_auth_inflight` | `4096` | restart |
| `radius.max_acct_inflight` | `4096` | restart |

When the number of authentication (or accounting) requests being handled reaches the limit, new requests are dropped before any work is created, and not answered. This way the server does not spend memory and CPU on requests that are bound to time out when traffic surges, and the NAS retransmits or asks another server. See [Appendix A](90-config-reference.md).

- Dropped requests are counted in `rradius_radius_discarded_total{reason="overload"}`.
- The log has at most one line per port per second, saying how many were dropped: `RADIUS authentication overloaded: discarded 1 packets` (`accounting` for accounting). For the exact count, use the metric.
- `rradius_radius_requests_limit{kind="auth"}` is the configured limit, and `rradius_radius_requests_in_progress{kind="auth"}` is the number being handled now.

When you set the limit to 1 and send 8 requests at the same time with a slow script, 1 is answered, 7 time out, and `reason="overload"` is 7. For what value to set and how to measure it, see [chapter 19](19-performance.md).

## When the server does not answer

Besides the drops described above, the server also deliberately does not answer in the cases below, so that the NAS retransmits or switches to another server instead of getting a wrong reject:

| Case | Counter |
| --- | --- |
| The source is not a known client | `reason="unknown_client"` |
| The packet is malformed, or the Message-Authenticator is wrong or missing | `reason="invalid"` |
| The number of requests being handled has reached the limit | `reason="overload"` |
| A backend is temporarily unusable when deciding an Access-Request: the cluster database (timeout, no leader found, write queue full), an external database, the LDAP directory, the domain controllers, or a password verification that timed out | `reason="database"` |
| The cluster node has lost the majority and stopped serving | `reason="no_leader"` |

The counters in the table are all labels of `rradius_radius_discarded_total`. When a backend cannot be used, the log line is `RADIUS Access-Request from client <name> not answered: the authentication backend could not be used`. When the data says "no" (no such account, wrong password, the database was queried fine but a script rejects), an Access-Reject is still returned; only "cannot decide" gets no answer. For how accounting requests are handled when the database cannot be used, see [chapter 13](13-accounting.md).

> **Not verified on real hardware:** The database-unusable and `no_leader` rows come from the code and the specification. This chapter did not create a real database failure to verify them. For clusters, see [chapter 16](16-cluster.md).

Also, when a response packet exceeds the RADIUS limit of 4096 bytes, the server sends an Access-Reject with no attributes instead and logs an error line; see [chapter 06](06-dictionary.md).

## Connecting a new NAS

When a new NAS is connected for the first time, check in this order. Most problems show up in the first three steps:

1. **Did the server receive it?** Look at `rradius_radius_discarded_total`. If `unknown_client` goes up, the source address is not on the list. If nothing changes at all, the packet did not reach the server (the firewall, the address or port set on the NAS is wrong, or `radius.bind` does not listen on that interface).
2. **Are the shared secret and Message-Authenticator right?** When `invalid` goes up, first check whether the log has `lacking a Message-Authenticator`: if it does, the NAS did not send it; if it does not, the shared secret most likely differs.
3. **Did the server answer?** Look at `rradius_radius_client_responses_total{client="<name>"}`. If `access_accept` or `access_reject` goes up, the server answered, and the problem is in the authentication itself or on the NAS side that receives the response.
4. **Why was it rejected?** Every decision has one authentication record line (`decision=` of `rradius_auth`), which says which auth chain and which handler were used. When that is not enough, use `[log.trace]` to temporarily record the full requests and responses of this NAS ([chapter 17](17-monitoring.md)).
5. **Is accounting arriving?** Requests on the accounting port are counted per client too, and `code="accounting_response"` goes up. For how accounting data is handled, see [chapter 13](13-accounting.md).

The settings you usually find on a NAS are named differently by each vendor, but mean roughly this:

| Setting on the NAS | Value to enter |
| --- | --- |
| RADIUS server address | The address of rradius (one of the `radius.bind` addresses, or the address of the load balancer in front of it) |
| Authentication port, accounting port | `radius.auth_port`, `radius.acct_port`, by default 1812, 1813 |
| Shared secret | The contents of this client's `secret_file` |
| Message-Authenticator | On |
| Timeout and retransmits | The NAS defaults; the server recognizes retransmits and does not handle them twice |
| Server status check | Can be turned on when Status-Server is supported; the NAS uses it to decide whether the server can still answer |

## Common mistakes

**Symptom:** The NAS keeps timing out, `rradius_radius_discarded_total{reason="unknown_client"}` is going up, and the log has `RADIUS packet from unknown client 203.0.113.7 discarded`.
**Cause:** The packet's source address is not in the client list. This is common when the NAS is behind NAT or sends from another interface.
**Fix:** Add a `[[radius.clients]]` for the address in the log, or fix the NAS's source interface.

**Symptom:** The NAS keeps timing out, `reason="invalid"` is going up, and the log has `discarded … Access-Requests lacking a Message-Authenticator`.
**Cause:** The NAS does not send Message-Authenticator, and it is required by default.
**Fix:** First turn on Message-Authenticator on the NAS. When the device does not support it, write `require_message_authenticator = "no"` for that client only.

**Symptom:** The NAS keeps timing out, `reason="invalid"` is going up, but there is no warning line like the one above.
**Cause:** The shared secret differs on the two sides.
**Fix:** Copy the secret from `secret_file` to the NAS again; make sure the packet is not being matched by another, more specific client entry ([How clients are matched](#how-clients-are-matched)).

**Symptom:** `rradius.toml: radius client '127.0.0.1': authorize_group applies only to TACACS+ clients`.
**Cause:** `authorize_group`, which exists only for TACACS+, was written on a RADIUS client.
**Fix:** Remove it. To set policy per client, use `auth` to pick the auth chain for RADIUS, or read `client` in a script.

**Symptom:** `rradius.toml: radius client '127.0.0.1': unknown auth chain 'vpn' (no [auth.chains.vpn])`.
**Cause:** `auth` points to an auth chain that does not exist.
**Fix:** Add `[auth.chains.vpn]`, or change it to the right name.

**Symptom:** `rradius.toml: radius client '127.0.0.1': eap_methods needs [radius.eap]`.
**Cause:** `eap_methods` was written without configuring EAP.
**Fix:** Configure `[radius.eap]` first ([chapter 11](11-eap.md)), or remove `eap_methods`.

**Symptom:** `radius clients: duplicate client name 'core'` (the TACACS+ list says `tacacs clients: …`).
**Cause:** Two clients in the same list have the same name. A client without `name` is named by its `address`, so one unnamed `10.0.0.5` and another with `name = "10.0.0.5"` also count as the same name. The name is used to separate metrics and logs, and to find the client when the accounting spool is replayed, so it cannot repeat. Names that differ in case are different names, and the RADIUS and TACACS+ lists may use the same name.
**Fix:** Change the `name` of one of them. After renaming, the metric counters of the two clients are separate. **Check before upgrading:** older versions accepted a configuration with duplicate names; after the upgrade, startup and `check` fail with exit code 2.

**Symptom:** `radius clients: duplicate client network '10.0.0.0/24'`.
**Cause:** Two entries are the same network after normalization, for example `10.0.0.5/24` and `10.0.0.0/24`.
**Fix:** Keep one. To set up one host separately, write a single address (`/32`).

**Symptom:** `radius clients: client '127.0.0.0/8': require_message_authenticator = "auto" needs a single host, not a network`.
**Cause:** `auto` was written on a network client.
**Fix:** Change it to `yes` or `no`, or split the host that needs `auto` out into a single-address client.

**Symptom:** `invalid client address 'nas1.example.com'`.
**Cause:** `address` accepts only an IP address or CIDR, not a host name.
**Fix:** Write the NAS's IP address.

**Symptom:** `exactly one of secret or secret_file must be set`, `secret_file 'empty.secret' is empty`, `cannot read secret_file 'missing.secret': No such file or directory (os error 2)`.
**Cause:** The shared secret is not set, both are set, or the file is empty or does not exist.
**Fix:** Keep only `secret_file`, and make sure the file exists and has content.

**Symptom:** `rradius.toml: radius.bind lists 0.0.0.0, which takes all of its family, beside another of them`.
**Cause:** `bind` has both `0.0.0.0` and another IPv4 address.
**Fix:** Keep only `0.0.0.0`, or list only the addresses you want to listen on. Other `bind` errors: `radius.bind must list at least one address` (an empty array), `'nowhere' is not an address`, `lists 127.0.0.1 twice`, `::ffff:10.0.0.1, an IPv4 address written as IPv6: list it as IPv4`.

**Symptom:** `radius.sockets = 4: only Linux shares UDP packets out over several sockets on one port, so it must be 1 here`.
**Cause:** `sockets` was set above 1 on a platform other than Linux.
**Fix:** Remove the line and use the default.

> **Note:** When a client is written in a file that is read in, the client errors above still start with the name of the main configuration file (for example `rradius.toml:`), not that file, and end with where the entry came from, for example `(in conf.d/extra.toml line 2)`.

## Related chapters

- [04 Configuration file basics](04-configuration.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [11 802.1X and EAP](11-eap.md)
- [13 Accounting and sessions](13-accounting.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [20 Security hardening](20-security.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
