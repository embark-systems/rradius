# 14 Scenarios: broadband and ISP

> After this chapter you can:
> - Allocate IPv4 addresses and delegated IPv6 prefixes from address pools in database tables, and confirm and release them with accounting
> - Authenticate IPoE subscribers that have no password, using the DHCP Option 82 line identity
> - Return fixed addresses, routed subnets and fixed prefixes, and reject the login instead of silently falling back to a dynamic address when the data is wrong
> - Limit concurrent connections and traffic over a period, per plan
> - Return different rate attributes for the same plan depending on the BNG vendor

Prerequisites: you have read [chapter 03](03-quick-start.md), and you can copy a scenario, start the server and log in with `rradius-bench`. You know what the auth chain in [chapter 07](07-users-and-chains.md) and the Lua scripts in [chapter 10](10-lua-policies.md) do. Everything in this chapter runs on one machine. You do not need a real BNG.

In this chapter:

- [What these scenarios have in common](#what-these-scenarios-have-in-common)
- [Prepare a lab directory](#prepare-a-lab-directory)
- [IPv4 address pool: ip-pool](#ipv4-address-pool-ip-pool)
- [IPv6 prefix pool: prefix-pool](#ipv6-prefix-pool-prefix-pool)
- [Authenticate by line identity: line-identity](#authenticate-by-line-identity-line-identity)
- [Dual-stack lines: dual-stack-lines](#dual-stack-lines-dual-stack-lines)
- [Fixed addresses and prefixes: static-ip](#fixed-addresses-and-prefixes-static-ip)
- [Plans and usage control: usage-control](#plans-and-usage-control-usage-control)
- [Return rates by BNG: rate-limit](#return-rates-by-bng-rate-limit)
- [Single-file examples](#single-file-examples)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## What these scenarios have in common

The seven scenarios in this chapter are all under `rradius-server/examples/scenarios/` in the source tree. They are all single-node, and their data lives in the cluster database. They use the same set of techniques. Read these points first, and each section below only covers what is different.

**The auth chain checks the password first, then runs the policy.** Scenarios with a password use `policy = "all_accept"` in `[auth]`. The first handler is an sql handler that checks the password, and the second is a lua handler that allocates or checks something. `all_accept` runs the handlers in order and stops at the first reject, so a user with a wrong password never reaches the lua handler and never takes an address or a connection slot. The reply attributes that the lua handler sets go into the Access-Accept.

**The accounting script confirms and releases.** When `[auth]` is present, the auth chain handles authentication, and the `script` in `[lua]` handles only accounting. The server sends an Accounting-Response only when the accounting script returns `accept`. For any other result it does not answer, and the NAS retransmits. So every write in the accounting script is written so that processing the same packet twice gives the same result as processing it once.

**Three states: reserved, in use, released.** Addresses, prefixes and connection slots all have a state column:

| State | When it is entered | When it is left |
| --- | --- | --- |
| `reserved` | When authentication succeeds | An Accounting-Start turns it into `active`; if none arrives within 120 seconds, it is released |
| `active` | Accounting-Start or Interim-Update | Accounting-Stop, an Accounting-On or Accounting-Off from the NAS, or no message for 3 Interim intervals |
| Released | Any of the endings above | The next user takes it |

The script returns `Acct-Interim-Interval = 300` in the Access-Accept, which asks the NAS to send an Interim-Update every 300 seconds. When a NAS crashes without sending a Stop, its session expires after 3 Interim intervals (900 seconds) with no message, and the next reclaim releases it.

**Claim resources with compare-and-update.** The script first reads a few candidates from its local copy of the database. Then it tries to claim one with a conditional `UPDATE` (for example `WHERE state = 'free'`). `sql_execute` returns the number of rows affected, and the claim succeeded only if it is 1. All writes go through the cluster leader one at a time, so two simultaneous requests cannot claim the same row. The local copy may be slightly stale, and the worst case is one extra attempt. This works the same way on several nodes (see [chapter 16](16-cluster.md)).

**Reclaim triggered by requests.** Expired reservations and sessions that stopped sending Interim updates do not disappear by themselves. Someone has to release them. These scenarios have no scheduler, so requests do it on the side. The `housekeeping` table has one row per job, with the time of the last run. When the script sees that more than 60 seconds have passed, it tries to claim the job with `UPDATE housekeeping … WHERE last_run < ?`. Only the request that wins the claim does the reclaim, at most 1000 rows at a time. So even with several nodes, the job runs once per period.

**Write the reason for a reject to the log.** An Access-Reject does not carry the reply attributes that handlers set, so the NAS only knows it was rejected. The script writes the reason into the server log with `log()`, in this form:

```text
… INFO rradius_core::lua_engine: script allocate.lua (radius/authentication, client localhost): ip pool business is exhausted: no address for hank
```

**Monitor with named queries.** Each scenario with a pool has `[queries.<name>]`. Every node queries its own copy every 30 seconds (`interval_ms` defaults to `30000`), and the result becomes the metric `rradius_query_<name>`. `rradiusd query <name>` runs the query once, right now, and prints a table. A named query takes effect only after a restart. See [chapter 09](09-external-databases.md) for details.

The configuration keys used by the scenarios in this chapter (see [Appendix A](90-config-reference.md) for the full description):

| Setting | Default | reload | Use in this chapter |
| --- | --- | --- | --- |
| `auth.policy` | `first_match` | Yes | `all_accept` in every scenario with a password |
| `auth.handlers` | none | Yes | An sql handler plus a lua handler, or a lua handler alone |
| `lua.script` | required | Yes | With `[auth]`, handles accounting only |
| `sessions.list`, `show` | none | Command | Query the pool or slot table directly |
| `queries.<name>.sql`, `labels`, `value` | none, `[]`, `value` | Restart | Pool usage as metrics |
| `queries.<name>.interval_ms` | `30000` | Restart | The scenarios use the default |
| `log.trace.clients`, `users`, `minutes` | `[]`, `[]`, required | Yes | See the reply attributes during experiments |

> **Note:** The demo data is in `migrations/2_demo_*.sql`. The accounts, passwords and pools are public demo values. When you turn a scenario into a production setup, delete this file and use your own data. A migration file that has been applied cannot be edited. To change the structure, add a file with the next number.

**Run the examples from a release archive, and run one at a time.** After you unpack a release archive `rradius-<version>-<platform>.tar.gz`, the directory layout is the same as the source repository (executables in `bin/`, examples in `rradius-server/examples/`). In chapter 03, `export PATH="$PWD/target/release:$PATH"` becomes `export PATH="$PWD/bin:$PATH"` in the unpacked root directory. All other commands and paths stay the same. The scenarios all use the standard ports (1812, 1813, and 49 for TACACS+), so two of them started on the same machine conflict. Run one at a time, or change the ports. Every example has an automatic test (`rradius-server/tests/scenarios_test.rs` starts each scenario the way these chapters do, sends requests and compares the results), so a configuration file copied as shown starts correctly. The `.secret` files hold demo values such as `change-me`. Replace them in production (see "Replace the example secrets" in chapter 03 and "Changing secrets" in chapter 18).

## Prepare a lab directory

Copy each scenario out the way chapter 03 shows, and run it from the copy. Do not start it in the source directory. Taking ip-pool as the example, run this in the root of the source repository:

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/ip-pool ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/ip-pool/rradius.toml
$ cd ~/rradius-lab/ip-pool
$ for f in *.secret; do openssl rand -hex 32 > $f; done; chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

The `for` loop before the last line replaces the public secrets that come with the scenario with random values. If you skip it, `check` prints a warning about permissions and public values for every `.secret` file, but the configuration itself is still fine.

`rradius-bench` does not print reply attributes. To see what the server replied, add tracing at the end of `rradius.toml`, using the client name of this scenario:

```toml
# rradius.toml (add at the end of the file)
[log.trace]
clients = ["localhost"]
minutes = 60
```

`[log.trace]` can be reloaded and stops by itself after 60 minutes. Once it is on, every request and response adds one `rradius_trace` line to the log, with passwords shown as `<hidden>`:

```text
… INFO rradius_trace: response client="localhost" source=127.0.0.1 user="alice" code=2 attributes="Message-Authenticator = <hidden>, Framed-IP-Address = 10.77.0.4, Acct-Interim-Interval = 300"
```

`code=2` is an Access-Accept, `3` is an Access-Reject, and `5` is an Accounting-Response. For the full description of tracing, see [chapter 17](17-monitoring.md).

Then start the server in one terminal (`rradiusd serve --config rradius.toml`) and run the tests of each section in another terminal. This chapter uses three loads of `rradius-bench`:

- `pap`: sends one login.
- `acct-start`: sends one Accounting-Start, with a different session ID on every run.
- `session`: sends a login, a Start and a Stop in order, all with the same session ID. It also copies the `Class` and `Framed-IP-Address` from the Access-Accept into the two accounting requests, as a real NAS does.

To add attributes, use `--attr type-number=type:value`, for example `--attr 8=ip:10.77.0.4`, `--attr 31=text:00-11-22-33-44-55` or `--attr 52=int:2`. `--attr` is added to every request that is sent, authentication and accounting alike.

## IPv4 address pool: ip-pool

### Goal

After a user passes password authentication, the user gets an IPv4 address from the pool they belong to, in `Framed-IP-Address` of the Access-Accept. The same person logging in again gets the same address back. After logging out, the same user gets their last address back first for a day (stickiness). When the pool is used up, the login is rejected and the log says why.

### Should the authentication server allocate addresses

Many BNGs and NASes have address pools of their own. In that case the authentication server only returns a pool name in `Framed-Pool` and the NAS issues addresses from its own pool. You do not need this scenario then. Return `Framed-Pool` from the `reply_query` of the sql handler (see [chapter 03](03-quick-start.md) and [chapter 07](07-users-and-chains.md)). The authentication server should allocate in these cases:

| Case | Why |
| --- | --- |
| The NAS has no address pool, or the pool is too small or lacks features | Only the authentication server can provide the addresses |
| One set of addresses is shared by several NASes | A central pool prevents each NAS from issuing its own addresses and causing conflicts |
| You need to ask "who is using this address now", or audit or limit usage | The ownership of every address is in a table |

### Tables

`migrations/1_schema.sql` creates three tables:

| Table | Contents |
| --- | --- |
| `subscribers` | Account, password, `pool` (which pool the address comes from), `enabled` |
| `ip_pool` | One row per address. The primary key is `(pool, ip)`, so different pools can have overlapping addresses (for example in different VRFs) |
| `housekeeping` | The time of the last run of each reclaim job |

The columns of `ip_pool`:

- `state`: `free`, `reserved` or `active`, enforced by a `CHECK`.
- `username`, `session_id`, `nas`: the user now holding the address, the session confirmed by accounting, and which NAS.
- `expires`: the expiry time. 120 seconds for a reservation, and 3 Interim intervals for an address in use.
- `last_user`, `released`: when an address is released, the previous user and the time are recorded, for stickiness.
- `n`: the sequence number of the address in the pool. Numbers in one pool run consecutively from 1, and new addresses continue the numbering.

When `n` has gaps, the random starting points for choosing an address concentrate on some addresses. Four indexes (`ip_pool_user`, `ip_pool_state`, `ip_pool_last_user`, `ip_pool_expiry`) cover the condition of every query. When SQLite has no statistics and a condition uses only the first few columns of an index, it may choose to scan the whole pool by primary key instead. So when you add columns or change queries, change the indexes with them.

Demo data: the `residential` pool is `10.77.0.2` to `10.77.0.9`, 8 addresses. The `business` pool has only 3 addresses (`10.88.0.2` to `10.88.0.4`) but four users, `dave`, `frank`, `gina` and `hank`, so you can see the pool run out. `erin` is disabled.

### Configuration highlights

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "accounting.lua"

[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM subscribers WHERE username = ? AND enabled = 1"

[[auth.handlers]]
type = "lua"
script = "allocate.lua"

[sessions]
list = "SELECT username, nas, session_id, ip, updated FROM ip_pool WHERE state = 'active' AND expires > strftime('%s', 'now') ORDER BY ip"
show = "SELECT username, nas, session_id, ip, updated FROM ip_pool WHERE username = ?1 AND state = 'active' AND expires > strftime('%s', 'now') ORDER BY ip"

[queries.pool_usage]
sql = "SELECT pool, state, count(*) AS addresses FROM ip_pool GROUP BY pool, state"
help = "Addresses of each pool by state: free, reserved or active."
labels = ["pool", "state"]
value = "addresses"
```

- `[sessions]` queries the pool directly. The address pool itself records who is using which address, so no separate accounting table is needed. Only the `session` command reads `[sessions]`, so changing it needs no reload.
- `pool_usage` becomes the metric `rradius_query_pool_usage{pool,state}`. The query uses the `ip_pool_state` index, so it does not scan the whole table however large the pool is.

### Script highlights

`allocate.lua` (the lua handler) does four things in order:

1. If the user already has a `reserved` or `active` address, it reuses it and does not take another.
2. Otherwise, it first takes the address this user used last, if it is free now and was released less than `HOLD_SECONDS` (86400 seconds) ago.
3. Then it picks `CANDIDATES` (8) free addresses and tries to claim them one by one. The starting point is random within the range of `n` among free addresses, so that simultaneous requests do not all go for the same candidates. It reads at most `ATTEMPTS` (4) rounds of candidates.
4. If it cannot claim any, it writes a line `ip pool … is exhausted` and rejects.

`accounting.lua` handles accounting:

- Start and Interim-Update confirm this user's address as `active`, with the expiry set to 3 Interim intervals from now. If the reservation has already expired and been released while the address is still free, it takes the address back.
- Stop releases only the address of this session, or a reservation that was never confirmed. So a Stop from an old session that arrives late cannot touch a new binding.
- Accounting-On and Accounting-Off mean the NAS rebooted, and release all `active` addresses of this NAS. Which NAS it is comes from `NAS-IP-Address`, `NAS-IPv6-Address` and `NAS-Identifier`, in that order. When none is present, the client name is used, because one client can be a whole network range.

`RESERVE_SECONDS`, `HOLD_SECONDS` and `INTERIM_SECONDS` exist once in each of the two scripts. When you change them, change both.

### Testing and notes

Verified. `alice` logs in twice and `bob` once:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 639/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd query pool_usage --config rradius.toml
pool         state     addresses
business     free      3
residential  free      6
residential  reserved  2
```

Both trace lines of `alice` show `Framed-IP-Address = 10.77.0.4, Acct-Interim-Interval = 300`, so she took only one address. The other `reserved` address is `bob`'s. Addresses are picked at random, so your result may differ.

Log in `dave`, `frank`, `gina` and `hank` in turn. The first three succeed. `hank` gets `reject 1`, and the log says:

```text
… script allocate.lua (radius/authentication, client localhost): ip pool business is exhausted: no address for hank
```

Send a Start, and the address changes from `reserved` to `active` and shows up in `session list`. `acct-start` does not carry an address by itself, so add the one you just got with `--attr`:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1 --attr 8=ip:10.77.0.4
…
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=e370000000000000
ip=10.77.0.4
updated=1795384802
```

The `session` load runs a whole session, and the address is released after the Stop:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load session,concurrency=1 --requests 1
…
  acct-stop: 776/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

When `carol` logs in again, she gets the same address back (stickiness).

Notes:

- A reservation takes 120 seconds to expire. To repeat the pool-exhaustion test, wait two minutes, or stop the server and delete `data/` to start over.
- `rradius_query_pool_usage` is updated only every 30 seconds, so a change you just made shows up in the next round. To see it right away, use `rradiusd query pool_usage`.
- A wrong password, a disabled account and an account that does not exist are all `reject`, and none of them takes an address.

### Operations: look up addresses, add and remove addresses

`ip_pool` is an ordinary table, and there is no dedicated management command. The columns in the SQL below are the columns of `migrations/1_schema.sql`.

Check the usage of each pool, and "who is using this address now":

```sql
SELECT pool, state, count(*) FROM ip_pool GROUP BY pool, state;
SELECT pool, ip, state, username, nas, session_id, expires FROM ip_pool WHERE ip = '10.77.0.2';
```

Check for leaks: rows that have expired but have not been released. These rows are released at the next reclaim. If they stay, no request is arriving to trigger the reclaim:

```sql
SELECT ip, username, expires FROM ip_pool WHERE state != 'free' AND expires < strftime('%s', 'now');
```

Adding addresses means adding rows. `n` continues from the largest value in the pool so that it stays consecutive (the reason is in "Tables" above):

```sql
INSERT INTO ip_pool (pool, ip, n)
VALUES ('residential', '10.77.0.10', (SELECT coalesce(max(n), 0) + 1 FROM ip_pool WHERE pool = 'residential'));
```

When there are many addresses, generate these statements with a program, write them as a migration file with a new number, and put it on every node and restart the nodes one by one (see [chapter 18](18-operations.md#adding-a-migration)). An applied file cannot be edited. To shrink a pool, first check that the addresses you delete are `free`, then delete the rows, starting from the largest `n` so that `n` does not get gaps:

```sql
DELETE FROM ip_pool WHERE pool = 'residential' AND ip = '10.77.0.10' AND state = 'free';
```

### Monitoring and alerts

The metric of `pool_usage` is `rradius_query_pool_usage{pool,state}`. Every node outputs the same numbers, so in a cluster use `max()`, not `sum()`. An alert for a pool that is nearly used up:

```promql
max by (pool) (rradius_query_pool_usage{state="free"}) < 100
```

How much is used:

```promql
1 - (max by (pool) (rradius_query_pool_usage{state="free"})
     / sum by (pool) (max by (pool, state) (rradius_query_pool_usage)))
```

- A lot of `reserved` and few `active` means the NAS is not sending Accounting-Start. A reservation takes 120 seconds to be reclaimed.
- A query with no matching rows produces no sample. When a pool is completely used up (no `free` row at all), the sample with `state="free"` disappears, and the first alert expression above does not fire. To catch this case, use `max by (pool) (rradius_query_pool_usage{state="active"}) unless on (pool) max by (pool) (rradius_query_pool_usage{state="free"})`, or, when you know the pool name, `absent(rradius_query_pool_usage{pool="business",state="free"})`.
- When a query fails, the metric keeps its last value. Check that `rradius_query_last_success_timestamp_seconds{query="pool_usage"}` is advancing (see [chapter 09](09-external-databases.md)).
- Cost: this query uses the covering index `ip_pool_state` (`SCAN ip_pool USING COVERING INDEX ip_pool_state`). On 2026-11-16, on an in-memory SQLite 3.54.1 database with 110,000 addresses, the median was about 3.7 ms. Rewritten as `WHERE state = 'free' GROUP BY pool`, it uses `ip_pool_expiry`, needs a table lookup for every row, and takes about 9.5 ms. So fetching every state of every pool in one query is better. Not measured again.

### Scale

Login storm measurement of 2026-11-16 (change ip-pool-scale; one 14-CPU Docker machine, 7 CPUs for the server and 7 CPUs for the measuring program, 64 concurrent PAP logins, every user logging in for the first time; not measured again since):

| Pool | Logins | Per second | Result |
| --- | --- | --- | --- |
| 22,000 addresses, 20,000 users | 5,000 | about 7,500 | No duplicates, no rejects |
| 110,000 addresses, 100,000 users | 20,000 | about 6,600 | No duplicates, no rejects |
| 110,000 addresses, 100,000 users, all reconnecting at once | 100,000 | about 6,500 (about 15 seconds in all) | 100,000 users each got a different address, no rejects |

This speed depends on two things: choosing candidates uses the `(pool, state, n)` index, and every query at login has a condition covered by the first columns of some index. The `WHERE nas = ? AND state = 'active'` of the accounting script has no index, but it runs only when a NAS reboots and sends Accounting-On or Accounting-Off, not at every login. When SQLite has no statistics, an index covers only part of a condition, and the query may scan the whole pool by primary key. The "reuse the binding" query of an earlier version did exactly that (it had `ORDER BY ip`, which the primary key already satisfies), and with 110,000 addresses it managed only 64 logins per second. When you change a query, use `EXPLAIN QUERY PLAN` to see which index it uses.

The measurement covers authentication only. The accounting writes (Start, Interim, Stop) are counted separately, and the limits are in [chapter 19](19-performance.md). If the reconnect takes longer than 120 seconds, reservations that received no Start are released first, and the "reserved" count is lower than the number of logins.

### Known limitations

- **One address per user.** Two simultaneous sessions of the same account (for example from two devices) get the same address, because an existing binding is reused. To limit concurrent connections, do it separately; see "Plans and usage control".
- **This scenario does not handle DHCPv6-PD.** IPv6 prefixes need aligned allocation; see "IPv6 prefix pool".
- **There are no built-in pool functions and no dedicated management command.** Usage comes from `pool_usage` and `rradiusd query`. Picking and releasing addresses is done in the script.
- **One address at a time.** There is no "allocate a whole range".
- **After a NAS crash, an address is kept for up to 15 minutes** (3 Interim intervals, 900 seconds) before it goes to someone else. When the NAS comes back and sends Accounting-On, the addresses are released at once.
- **When the pool is exhausted, the NAS only sees a Reject.** The reason is in the log. Monitor the log for `is exhausted`.

## IPv6 prefix pool: prefix-pool

### Goal

Delegate prefixes to subscribers with DHCPv6-PD. After the password check succeeds, the subscriber gets a prefix from the pool, in `Delegated-IPv6-Prefix`. The WAN-side /64 is not allocated here. The scenario returns the pool name in `Framed-IPv6-Pool`, and the BNG's own pool decides. After the subscriber goes offline, the prefix is kept for them for a day, and when they come back they get the same one.

### Tables

An IPv6 pool is too large to store one row per prefix. This scenario stores only three constants per pool, and the table holds only the prefixes that have been allocated:

| Table | Contents |
| --- | --- |
| `prefix_pools` | One row per pool: `base` (the high 64 bits of the prefix, as an integer), `plen` (the length of the pool), `dlen` (the length each user gets, at most 64) |
| `prefix_leases` | Only allocated prefixes. A prefix without a row is free. The primary key is `(pool, n)` |
| `subscribers` | Account, password, `pool`. The pool name is also the `Framed-IPv6-Pool` returned to the BNG |

The high 64 bits of prefix number `n` (`n` starts at 0) are `base | (n << (64 - dlen))`, so it is always aligned. A pool has 2 to the power `dlen - plen` prefixes, and a `CHECK` limits this to at most 2 to the power 30. `base` must also be aligned to `plen`, and a value that is not aligned cannot be written. `2001:0db8:0100:0000` is written as `0x20010db801000000`. An address whose top bit is 1 (for example `fd00::/8`) is a negative number in SQLite, which does not matter.

`prefix_leases.state` has one more value, `released`: the subscriber went offline, and the prefix is kept for them for the time being, until `expires` (the release time plus `HOLD_SECONDS`) passes and the row is deleted. Others take the oldest `released` row only when they cannot find a free `n`.

The `prefix` column stores the text of the prefix. It must have exactly the form the server produces when it decodes the `Delegated-IPv6-Prefix` reported by the NAS: compressed and lower case, for example `2001:db8:100::/56`. Accounting confirmation uses this string to find the row.

The demo data has three pools: `residential` is `2001:db8:100::/48` with one /56 per user, `business` is `2001:db8:200::/52` with one /60 per user, and `ula` is `fd12:3456:789a::/48` with one /56 per user, 256 prefixes each.

### Configuration highlights

The configuration is built like ip-pool: `all_accept` with an sql handler first and then the lua handler `allocate.lua`, the `accounting.lua` of accounting in `[lua]`, and `[sessions]` querying `prefix_leases`. There are three named queries:

| Query | Metric | Contents |
| --- | --- | --- |
| `prefix_leases` | `rradius_query_prefix_leases{pool,state}` | Number of prefixes in each state of each pool |
| `prefix_capacity` | `rradius_query_prefix_capacity{pool}` | The capacity of each pool, `1 << (dlen - plen)` |
| `prefix_hold_age` | `rradius_query_prefix_hold_age{pool}` | Seconds from the oldest `released` row to now, which is the hold time you are actually guaranteed right now |

When you compute usage, count only `reserved` and `active`. `released` is kept for offline users and can be taken when needed.

### Script highlights

`allocate.lua`:

1. If the user already has a prefix, it reuses it. A `released` one goes back to `reserved`, waiting for their Accounting-Start.
2. Otherwise, a hash of the account name (FNV-1a) decides the starting `n`. In the next `WINDOW` (1024) values of `n` it looks for a free one and claims it with `INSERT OR IGNORE`: 1 affected row means it won, and 0 means someone just took it (the primary key blocked it). The same person always starts from the same point, so they get the same prefix when that position is free, but this is not a guarantee.
3. If the window has no free `n`, it picks one at random from the oldest few `released` rows and claims it.
4. Only when there is no `released` row to take does it look for a free one in the whole pool. Only when the whole pool is full does it reject, and the log says `prefix pool … is exhausted`.

The text of the prefix is computed in SQL. Luau has no 64-bit integers, so the `CLAIM` SQL builds the compressed form with bit operations and `printf('%x:…')`. When it loses a claim and looks again, the number of candidates doubles with every loss (up to 64), and it tries at most `ATTEMPTS` (16) times.

`accounting.lua`:

- Start and Interim-Update confirm the prefix as `active`.
- Stop changes the prefix of this session to `released` and keeps it for `HOLD_SECONDS`.
- Accounting-On and Accounting-Off change all `active` prefixes of this NAS to `released`.
- If the packet carries `Delegated-IPv6-Prefix`, it must match. If it does not, only the account is used, because one user has only one prefix in one pool.
- A Start that arrives after the reservation expired and was deleted is not put back, because `n` cannot be derived from the text of a prefix. It only writes a line `is not in prefix_leases` to the log.

### Testing and notes

Verified. Four users log in, and each gets a prefix from their own pool:

```text
… user="alice" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = 2001:db8:100:e700::/56, Framed-IPv6-Pool = residential, Acct-Interim-Interval = 300"
… user="carol" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = 2001:db8:200:120::/60, Framed-IPv6-Pool = business, Acct-Interim-Interval = 300"
… user="dave" code=2 attributes="Message-Authenticator = <hidden>, Delegated-IPv6-Prefix = fd12:3456:789a:df00::/56, Framed-IPv6-Pool = ula, Acct-Interim-Interval = 300"
```

The starting point is decided by the account name, so with the same demo data you should get the same prefixes as shown here. After `alice` runs through one round with the `session` load:

```console
$ rradiusd query prefix_leases --config rradius.toml
pool         state     prefixes
business     reserved  1
residential  active    1
residential  released  1
ula          reserved  1
```

The `released` row of `residential` is `alice`'s prefix (the `active` one is `bob`, confirmed separately with `acct-start`). When she logs in again, she gets `2001:db8:100:e700::/56` back, and that row goes back to `reserved`.

Notes:

- The `session` load of `rradius-bench` copies only `Class` and `Framed-IP-Address` into accounting, not `Delegated-IPv6-Prefix`, so the test takes the "account only" path. A real BNG sends the prefix.
- The fuller the pool, the longer the run of consecutive used `n` that has to be walked. So the script looks at only one window, and takes a `released` row when the window has no free one. When `prefix_hold_age` gets smaller, the time that offline users keep their prefix is being squeezed, and it is a signal to expand the pool.

### When the pool is nearly full

When a pool is nearly full, two things matter: whether logins get slow or rejected, and whether a user who goes offline and comes back still gets the same prefix. The first is measured on a real server. The second is mostly measured by simulation. They are described separately. **All the numbers in this section come from chapter 19 of the old manual and from the change `ipv6-prefix-pool` (2026-11-16 to 17, recorded in `docs/spectra/changes/archive/2026-11-16-ipv6-prefix-pool/`), and were not measured again. They all measure the current design, which stores only allocated prefixes and has the `released` hold**, not the earlier design with one pre-generated row per prefix.

#### Login speed: Docker, single node

Capacity 65,536 (`2001:db8::/40` cut into /56), a single node in Docker (7 CPUs for the server and another 7 for `rradius-bench`), new users logging in for the first time with 64 concurrent requests. The pool is first filled to the stated usage with `allocate.lua`, in two ways: "reserved" is just filled, with no `released` row (the worst case: no free slot in the window and no `released` row to take, so the whole pool is searched for a free slot), and "`released`" is the normal state after users go offline.

| Usage | Prefixes in the pool | New users per second | p50 | p99 | Rejected (pool still has free slots) |
| --- | --- | --- | --- | --- | --- |
| 90% | reserved | 4,125 | 11 ms | 71 ms | 0 |
| 95% | reserved | 1,643 | 24 ms | 155 ms | 0 |
| 99% | reserved | 334 | 161 ms | 457 ms | 0 |
| 90% | `released` | 5,114 | 10 ms | 40 ms | 0 |
| 99% | `released` | 3,383 | 11 ms | 67 ms | 0 |
| 100% | `released` | 1,465 | 30 ms | 34 ms | 0 |

- **It is faster when there are `released` rows.** The window covers only the next `WINDOW` (1,024) values of `n` after the starting point. When the window has no free one, the script takes the oldest `released` row directly and does not scan the pool. When the pool is filled with reservations there is no `released` row to take, and the higher the usage the slower it gets. This is also why monitoring recommends an alert at more than 85% usage.
- **No duplicates.** After every measurement, the number of rows in the table equals the number of distinct prefixes (the unique index guards this).

**Where randomized relaxed retry comes from.** The first version looked for the "next" free `n` after losing a claim. Measured on a server, at 90% usage about 2.5% of logins were rejected (76 of 3,000 new users), although the pool still had more than six thousand free slots. Users that arrive at the same time and start in the same stretch all find the same free `n`. Only one wins each round, and the losers all go for the same one again in the next round. The fuller the pool, the shorter the free stretches. So now (`free_near`, `steal` and `width` in `allocate.lua`), after losing a claim, the script picks one at random from the free stretch that follows. The number of candidates doubles with each loss (up to 64), and claiming a `released` row works the same way (starting from the oldest 8 rows and doubling with each loss). It tries at most `ATTEMPTS` (16) times. A simulation that reads once cannot show this problem, because it has no concurrency.

#### Login speed: larger pools, three-node cluster

Another machine and another measurement (Apple M4 Max, 14 CPUs, macOS, server and `rradius-bench` on the same machine, PAP logins, new users logging in for the first time; the old chapter gives the date 2026-11-18). **These numbers can be compared only with each other in this section, not with the Docker table above.** The pool is first filled to the stated usage with real logins (`allocate.lua`). "`released`" means all the filled prefixes were changed to `released`. A pool filled by writing directly to the table with prefixes scattered at random is much faster (about 7,000 per second at 65,536 and 99% reserved), because the prefixes and free slots filled by real logins are in runs. So measure with pools filled by real logins.

Single node, 64 concurrent:

| Capacity | Usage | Prefixes in the pool | New users | Per second | p50 | p99 | Rejected |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 65,536 | 90% | reserved | 2,000 | 10,700 | 4.7 ms | 20 ms | 0 |
| 65,536 | 95% | reserved | 2,000 | 3,737 | 7.4 ms | 108 ms | 0 |
| 65,536 | 99% | reserved | 500 | 549 | 93 ms | 328 ms | 0 |
| 65,536 | 90% | `released` | 2,000 | 10,751 | 5.0 ms | 15 ms | 0 |
| 65,536 | 99% | `released` | 500 | 6,430 | 7.6 ms | 32 ms | 0 |
| 65,536 | 100% | `released` | 500 | 6,054 | 8.9 ms | 27 ms | 0 |
| 262,144 | 90% | reserved | 2,000 | 10,334 | 4.8 ms | 22 ms | 0 |
| 262,144 | 99% | reserved | 500 | 1,879 | 23 ms | 118 ms | 0 |
| 262,144 | 99% | `released` | 500 | 6,421 | 8.9 ms | 25 ms | 0 |
| 262,144 | 100% | `released` | 500 | 6,062 | 9.4 ms | 24 ms | 0 |

- **With 4 times the capacity, the speed at the same usage is the same or better.** 99% reserved at 262,144 is more than three times faster than at 65,536, because 1% free slots is four times as many in absolute terms (2,621 against 655).
- **Still expand before 90%.** At 99% with the pool full of reservations and no `released` row to take, you get only a few hundred to a bit over a thousand per second.
- **The number of table rows equals the number of distinct prefixes.** The 100% group at 262,144 had 262,143 rows, one fewer than expected, and the cause was not found.

Three-node cluster (three `rradiusd` processes on the same machine, capacity 65,536, three measuring programs each aimed at one node, 63 concurrent in total):

| Usage | Prefixes in the pool | New users | Total per second, three nodes | p99 per node | Rejected |
| --- | --- | --- | --- | --- | --- |
| 90% | reserved | 2,100 | 6,654 | 25–28 ms | 0 |
| 99% | `released` | 600 | 5,448 | 27–34 ms | 0 |
| 100% | `released` | 600 | 4,696 | 26–32 ms | 0 |
| 99% | reserved | 450 | 787 | 250–270 ms | 0 |

- At 90% and in the `released` cases it is slower than a single node, at about sixty to eighty percent (6,654 against 10,700, 5,448 against 6,430, 4,696 against 6,054). 99% reserved is faster than a single node (787 against 549), because in that group the bottleneck is finding a free slot, not writing. Writes go through the leader.
- Three nodes claiming `released` rows at the same time produced no rejects and no duplicates: the number of table rows equals the number of distinct prefixes.
- **These are three processes on one machine**, so there is no network latency. Latency between hosts or sites makes each write slower, and this was not measured.

### Does the user get the same prefix when they come back

**Measured on a server (the old chapter gives the date 2026-11-18):** capacity 16,384, single node. Users log in in order and send Start, the NAS sends Accounting-On, the users come back in random order, and the test checks whether each gets the original prefix. With the hold (`HOLD_SECONDS` 24 hours), at 70%, 90% and 95% usage 100% got the same prefix back, with no rejects. With only the hash starting point (`HOLD_SECONDS` set to 0, so all holds expire and are deleted), it was about 55% at 70%, about 41% at 90% and about 37% at 95%. Conclusion: **to be stable across a reboot, rely on the `released` hold, not on the hash starting point.**

**Simulation:** capacity 65,536, the online time of users follows an exponential distribution (mean one day), and the offline time follows an exponential distribution, or 80% are short disconnects with a mean of 3 minutes and 20% are long absences. After a warm-up, the test measures whether each re-login gets the prefix it had last time. Two independently written simulations were checked against each other. This is not a server measurement, and it does not simulate concurrency or clusters. The main points:

- **The hash starting point alone (no `released`) is not stable.** With real churn of users going on and offline, at 90% usage about 39–87% get the same one back (depending on the online ratio and the distribution of offline time), at 95% about 62–68%, and at 99% about 39–53%. At 50–70% usage it is still only about 76–93%. Raising the capacity to 1 million gives the same result.
- **A reboot of the NAS or BNG is the worst case.** A group of online users disconnect at the same time and come back in random order. With the hash starting point alone: about 51% get the same one back at 70% usage, about 28% at 90%, and about 18% at 95%.
- **With the `released` hold (24 hours):** the same reboot case gives 100% back. With normal churn it is about 93–98% at 90% usage (depending on the hold time and the offline distribution), about 89% at 95% and about 82% at 99%. Short disconnects (coming back within the hold period) almost all get the same one back. Users who stay away longer than the hold period, or whose prefix was taken, get a different one.
- The server measurements at 90% and 95% (about 41% and 37%) are more stable than the simulated 28% and 18%. The cause was not investigated. It may be that the server picks a free slot at random inside the window, which is not exactly the algorithm of the simulation.

#### Capacity requirement

**The upper limit of stability depends on the ratio of the total number of users to the pool capacity:** the more users are offline and the fewer free slots there are, the more prefixes get taken. When the total number of users does not exceed the pool capacity, the hold can come close to 100%. When the total number of users far exceeds the capacity, some users must change prefix. For stable prefixes, **the pool capacity must be larger than the total number of users**, not only the usage low.

#### Alternatives that were rejected

These were all tried in the research phase and are not used (from the simulations in the change record):

- **Double-hash probing:** only 1 to 10 percentage points more stability.
- **Splitting into blocks:** equal or worse.
- **Rejecting new users above a usage threshold:** with demand at 95% and the threshold at 90%, 35% of logins are rejected, which is worse than changing prefixes. So it only alerts and does not reject.
- **Claiming forward one by one with `INSERT OR IGNORE`:** at 90% usage p99 has to walk 400 to 1,100 values of `n`, and each is a write through the leader. Limiting it with `ATTEMPTS` would wrongly declare the pool exhausted while free slots remain. So the script now reads once to find free `n` and then claims.
- **One pre-generated row per prefix (generated by a migration):** a large pool (for example a /32 cut into /56 has 16 million prefixes) means generating a lot of data, and it was not certain that a migration of 1 million rows could be applied through the cluster. With only allocated prefixes stored, the pool size does not affect the table size. The price is that when the pool is very full, a longer run of consecutive used slots has to be walked (hence the window and taking `released` rows).
- **Looking for a free slot from a random starting point:** it has no stability (the same user starts from a different point every time), so it is not used.

### Monitoring and alerts

The metrics of the named queries are in "Configuration highlights". The usage of each pool **counts only `reserved` and `active`** (in a cluster use `max()`, not `sum()`: every node queries the same data). `released` is kept for offline users and can be taken when needed. If you counted it, usage would stay close to 100% in steady state.

```promql
sum by (pool) (max by (pool, state) (rradius_query_prefix_leases{state=~"reserved|active"}))
  / max by (pool) (rradius_query_prefix_capacity)
```

**This expression has not been run against a real Prometheus** (the metric names and labels come from the named queries in the example `rradius.toml`). Before going live, run it once in your Prometheus and check that the labels match.

- **Alert when usage is above 85%** (`> 0.85`). The fuller the pool, the slower the logins and the less it can keep the prefixes of offline users (see the numbers above).
- **`rradius_query_prefix_hold_age` is the hold time currently guaranteed, in seconds.** A pool with no `released` rows has no sample. If there are some but the oldest is only a few minutes old, the pool is so full that a prefix is taken as soon as it is released, the hold does not reach `HOLD_SECONDS`, and stability drops. Expand the pool when it is much smaller than `HOLD_SECONDS` (86,400).
- **A query with no matching rows produces no sample**, so a completely empty pool has no `prefix_leases` sample. Fill the gap in alerts with `absent()` (for example, `absent(rradius_query_prefix_hold_age{pool="residential"})` only means there is no `released` row now, which is not necessarily a problem), or treat pools with no sample as 0.
- None of these queries scans the whole pool: the `GROUP BY pool, state` of `prefix_leases` uses the covering index `prefix_leases_pool_state`, taking the oldest `released` row uses the partial index `prefix_leases_released`, `prefix_hold_age` uses `prefix_leases_expiry`, and `prefix_pools` has only a few rows. The table is only as large as the users that are online or recently offline, so the cost does not depend on the pool capacity.

### Use your own pool

The pool constant `base` is the integer of the high 64 bits of the prefix. Compute it with Python (the result is `0x20010db801000000`):

```bash
python3 -c "print(hex(int.from_bytes(__import__('ipaddress').ip_network('2001:db8:100::/48').network_address.packed[:8], 'big')))"
```

Set `plen` and `dlen` according to your address plan. **Decide the pool constants before the first start in production.** An applied migration file cannot be edited, so to add a pool, add a file with the next number. Leave room in the pool size: expand before usage passes 90% (add a pool, or switch to a longer `plen`).

### Combine with the address pool for dual stack (username and password identity)

When users authenticate with a username and password and need both an IPv4 address and a delegated prefix, the server has only **one top-level script** for accounting, so the two examples cannot simply run together. The procedure is below. **This combined version has no example and has not been run** (the combined version with scripts and tests is "Dual-stack lines", but that one uses the line identity):

1. **Authentication:** list `[[auth.handlers]]` in this order: `sql` (checks the password), the `allocate.lua` of ip-pool, and the `allocate.lua` of this scenario. `all_accept` runs them one by one and stops at the first reject. Give the two scripts different file names (for example `ip-allocate.lua` and `prefix-allocate.lua`).
2. **Tables:** the migration file numbers of the two must not collide. Both have `subscribers` and `housekeeping`, so merge them into one table each. `subscribers` needs the IPv4 `pool` and the name of the prefix pool (two columns, or two pools with the same name), and `housekeeping` needs both rows `reap-ip-pool` and `reap-prefix-pool`.
3. **Accounting:** merge the two `accounting.lua` into one. One packet can carry both `Framed-IP-Address` and `Delegated-IPv6-Prefix`: handle the address part as ip-pool does and the prefix part as this section does. Accounting-On and Accounting-Off must release on both sides.
4. **When one side is exhausted:** by default the whole login is rejected (`all_accept`). To allow "IPv4 without a prefix", change the `return 'reject'` of the prefix script for exhaustion to `return 'accept'` without setting attributes. Note that this does not release the IPv4 address that was just reserved. To release it, use a single script like the one in "Dual-stack lines".

Users with a line identity (Option 82) have no usable account, and handlers do not pass values to each other, so they cannot be chained this way. See "Dual-stack lines".

### Known limitations

- **Prefixes are not guaranteed to be stable.** When the starting point is taken by someone else, you get a different one. The fuller the pool, the less stable (see above). No history table records "which one was used last time".
- **It is slow when the pool is very full.** When the pool is full of reservations and has no `released` rows, a single Docker node manages about 1,600 new users per second at 95% and about 330 at 99%. With `released` rows, 99% still gives more than 3,000 per second (numbers above).
- **Reclaim handles at most 1,000 rows at a time, once a minute** (one batch each for turning expired `active` rows into `released` and for deleting expired reservations and `released` rows). When many leases expire together (for example a whole BNG crashes), the rows that have expired but are not yet deleted still hold their prefixes, and clearing them takes several minutes. When the pool is very full, it may wrongly look exhausted during this time.
- **An out-of-order Start can keep the prefix of an offline user for a while longer.** When a Stop arrives first and a Start after it, the Start changes `released` back to `active`, and only when it expires without an Interim does it turn into `released` again (about 900 seconds longer, and the hold is counted again). When a packet carries no prefix and the row is found by user name, a late Start or Interim from an old session can also confirm a new reservation, and the Start of the new session then overwrites it.
- **A reject on exhaustion scans the whole pool.** When there is no `released` row and the window has no free slot, the whole pool is searched to be sure it is exhausted, and the cost is proportional to the number of used rows. When a pool stays exhausted, rejected logins are expensive, so avoid this with the usage alert.
- **A late Start cannot restore an expired reservation** (`n` cannot be derived from the text of a prefix). It is only logged.
- **There is no WAN /64 pool.** `Framed-IPv6-Pool` is only a pool name, and the BNG's pool decides the address of the connection itself.
- **It has not been verified against a real BNG.** The attribute is `Delegated-IPv6-Prefix` of RFC 4818, and accounting packets must carry it.
- **Cluster:** other than the measurement with three processes on one machine, a multi-host cluster has not been measured (see above).

### Common problems

| Symptom | Cause |
| --- | --- |
| Always Reject, and the log has `is exhausted` | The pool really has no free slot. Check `rradius_query_prefix_leases` and the capacity, and expand the pool |
| Always Reject, and the log has `is not in prefix_pools` | `subscribers.pool` has no matching pool in `prefix_pools` |
| Always Reject, and the log has `tries: others took the free ones as fast` | Too many users arrive at once and the pool is too full, so all 16 attempts lost. This is usually also a signal to expand the pool |
| Login succeeds, but the row stays `reserved`, is deleted after 120 seconds, and the prefix goes to someone else | The Start did not arrive, or the `Delegated-IPv6-Prefix` of the Start differs from the text in the table (compression, letter case). Look at the prefix in the log line with `is not in prefix_leases` |
| Cannot write to the pool: `CHECK constraint failed` | `base` is not aligned to `plen`, or `dlen` is smaller than `plen` or larger than 64, or the capacity exceeds 2 to the power 30 |
| The user gets a different prefix at every login | The pool usage is too high and the starting point was taken by someone else; or the user was offline longer than `HOLD_SECONDS`; or the position was taken by someone else after the reservation expired (see "When the pool is nearly full") |

## Authenticate by line identity: line-identity

### Goal

IPoE subscribers on fixed broadband have no account and password. "Which line" is the identity. The BNG puts the circuit-id and remote-id of DHCP Option 82 into the RADIUS request (RFC 4679). For a known line, the scenario returns an address pool name and a profile name. It rejects unknown and disabled lines. If you want, it can also bind a line to the first MAC address it sees.

### How Option 82 reaches RADIUS

This scenario needs the BNG to put the line identity into the Access-Request. `auth.lua` recognizes these attributes:

| Attribute | Use |
| --- | --- |
| `ADSL-Agent-Circuit-Id` (RFC 4679, sub-attribute 1 of vendor 3561) | circuit-id |
| `ADSL-Agent-Remote-Id` (RFC 4679, sub-attribute 2 of vendor 3561) | remote-id; an empty string when absent |
| `NAS-Port-Id` | Used as the circuit-id when there is no `ADSL-Agent-Circuit-Id` |
| `Calling-Station-Id` | The MAC address of the subscriber's device, needed by `lock_mac`; only its 12 hexadecimal characters are used, so colons and hyphens do not matter |

BNGs from different vendors do not agree on which attribute carries Option 82 or what format it has. Some use the set above, some use `NAS-Port-Id`, and some join the two values into one string. **First check what your BNG really sends.** Log in once with an unknown line, and the log shows the values the script saw (`line identity: unknown line <remote-id> <circuit-id>`). Fill `lines` according to it. You can also turn on `[log.trace]` to see the whole request (see [chapter 17](17-monitoring.md)). For how to configure the BNG, see its documentation.

This scenario does not look at the user name or the password at all. A BNG usually sends the line string or the MAC as the name and a fixed value as the password. The script looks only at the attributes above.

### Tables

| Table | Contents |
| --- | --- |
| `plans` | Each plan: the address pool name `pool` returned to the BNG, the profile name `profile` (the rate and so on are decided by the profile of the same name on the BNG), and `interim` |
| `lines` | One row per line. The primary key is `(remote_id, circuit_id)` |

A circuit-id is usually unique only within one access device, and the remote-id is often the name of that device, so the two together form the primary key. When the BNG sends no remote-id, an empty string is used. Both ids are stored in normalized form:

- Leading and trailing whitespace removed and converted to lower case, for example `eth 1/1/3:100` and `olt-a`.
- A value with non-printable characters (binary) is written as `0x` plus lower-case hexadecimal, with nothing else done to it, for example `0x0004000a0001`.

The other columns of `lines`:

- `lock_mac`: `0` only records the last MAC seen (`last_mac`, `last_seen`). `1` binds the MAC into `bound_mac` at the first login, and rejects any other MAC after that.
- `bound_mac`: 12 lower-case hexadecimal characters, with no colons or hyphens. When the subscriber changes device, clear it to `NULL`.

### Configuration highlights

This scenario has no `[auth]`. Both authentication and accounting are handled by the top-level script `auth.lua`. The clients list only the BNG:

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "auth.lua"

[[radius.clients]]
name = "bng"
address = "10.60.0.0/24"
secret_file = "bng.secret"
```

> **Warning:** A line identity is not a secret. Anyone who can send requests from a source other than the BNG can impersonate any line by writing any Option 82 into the request. List only the BNG in the client list, not the whole management network. For an extra layer of protection, use `lock_mac`.

### Script highlights

`auth.lua` first blocks protocols other than RADIUS, and always returns `accept` for accounting. It is the top-level script. If the same server also has `[tacacs]` enabled, accepting everything would authorize every command, so it must block first. For authentication:

1. The line is taken from `ADSL-Agent-Circuit-Id`, or from `NAS-Port-Id` if that is absent. The remote-id is taken from `ADSL-Agent-Remote-Id`, or is an empty string if that is absent. Both are normalized before the lookup in `lines`.
2. If the line is not found, is disabled, or its plan does not exist, it rejects and writes a log line.
3. With `lock_mac = 1`, it compares `Calling-Station-Id`. The first time, it binds with `UPDATE … WHERE bound_mac IS NULL`. When two requests arrive at once, only one wins, and the other rereads and compares.
4. It updates `last_mac` and `last_seen`, and returns `Framed-Pool`, `Filter-Id`, `Acct-Interim-Interval` and `Class` (whose value is the user name). The BNG sends `Class` back in accounting packets, so the accounting records know whose line it is.

### Testing and notes

Verified. `rradius-bench` sends requests from `127.0.0.1`, so for the experiment first change the `address` of `bng` in the lab directory to `"127.0.0.1"`.

`ADSL-Agent-Circuit-Id` and `ADSL-Agent-Remote-Id` are vendor attributes (sub-attributes 1 and 2 of vendor 3561), so `--attr` must be written as attribute 26 in hexadecimal: the 4-byte vendor number, a 1-byte sub-attribute number, a 1-byte length (the length of the value plus 2), then the value. You can build it in the shell. The `vsa` below is a shell function for demonstration, **not a tool provided by rradius**, and it works in bash, zsh and sh:

```console
$ vsa() { printf '26=hex:00000de9%02x%02x%s' "$1" $(( ${#2} + 2 )) "$(printf '%s' "$2" | od -An -tx1 | tr -d ' \n')"; }
$ rradius-bench run --server 127.0.0.1 --secret "$(cat bng.secret)" \
    --user line --password x --load pap,concurrency=1 --requests 1 \
    --attr "$(vsa 1 'eth 1/1/3:100')" --attr "$(vsa 2 'OLT-A')"
…
  pap: 1030/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`--user` and `--password` can be anything, because the script does not look at them. The remote-id was sent as upper-case `OLT-A`, and after normalization it matches `olt-a`. The trace lines in the log:

```text
… rradius_trace: request client="bng" source=127.0.0.1 user="line" code=1 attributes="…, ADSL-Agent-Circuit-Id = eth 1/1/3:100, ADSL-Agent-Remote-Id = OLT-A"
… rradius_trace: response client="bng" source=127.0.0.1 user="line" code=2 attributes="Message-Authenticator = <hidden>, Framed-Pool = residential, Filter-Id = fiber-100m, Acct-Interim-Interval = 300, Class = alice"
```

Results in other cases:

| Request | Result | Log |
| --- | --- | --- |
| The line of `bob` (`eth 1/1/4:100`), with `--attr 31=text:00-11-22-33-44-55` | accept, MAC bound | |
| The same line, changed to `00-11-22-33-44-66` | reject | `line olt-a eth 1/1/4:100 is locked to 001122334455 and the request has 001122334466` |
| The line of `dave` (`olt-b`, `eth 2/0/8:200`) | reject | `line olt-b eth 2/0/8:200 of dave is disabled` |
| A line that does not exist | reject | `unknown line olt-a eth 9/9/9:1` |
| Only `--attr 87=hex:0004000a0001` (`NAS-Port-Id`, binary) | accept, `Class = erin` | |

Notes:

- Every login writes `last_mac` and `last_seen` once. If you do not need this record, remove the final `UPDATE` in `auth.lua` to save one write per login.
- "Accept" returns only a pool name and a profile name, and the BNG's pool decides the address. To have rradius allocate addresses, see the next section.

### Maintain line data

`lines` and `plans` are ordinary tables, and there is no dedicated management command. There are two ways to change the data:

- **A migration file.** Write the SQL below as a migration file with a new number, put it on every node and restart the nodes one by one (see [chapter 18](18-operations.md#adding-a-migration)). This suits lines that rarely change, when you want a change record. It adds one file for every device change, so it is not practical when devices change often.
- **Keep the line data elsewhere.** When there are many lines and they change often (activation, deactivation, device changes), let the script query an external MySQL or PostgreSQL with `db_query` ([chapter 09](09-external-databases.md)), or ask the billing system with `http_request` ([chapter 10](10-lua-policies.md)). `lines` then keeps only the columns that rradius writes itself, such as `bound_mac` and `last_mac`. This scenario does not demonstrate it, and you change `auth.lua` yourself.

Whichever way you choose, this is how the contents of the tables are changed:

```sql
-- Add a plan and a line (both ids in normalized form: lower case, no leading or trailing whitespace)
INSERT INTO plans (plan, pool, profile, interim) VALUES ('fiber-500', 'residential', 'fiber-500m', 300);
INSERT INTO lines (remote_id, circuit_id, subscriber, plan, lock_mac) VALUES ('olt-a', 'eth 1/1/9:100', 'frank', 'fiber-500', 1);

-- Disable, enable
UPDATE lines SET enabled = 0 WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';
UPDATE lines SET enabled = 1 WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- The subscriber changed device: clear the binding, and the next device to log in is bound
UPDATE lines SET bound_mac = NULL WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';

-- The subscriber moves to another line: disable the old line and add a new one, or just change the subscriber of this line
UPDATE lines SET subscriber = 'grace' WHERE remote_id = 'olt-a' AND circuit_id = 'eth 1/1/9:100';
```

Write `bound_mac` as 12 lower-case hexadecimal characters (`001122aabbcc`) with no colons or hyphens. This is how the script compares it.

### Known limitations

- **A line identity is not a secret.** Anyone who can make the BNG send any Option 82, or who can send requests with a forged BNG address, can impersonate any line. List only the BNG in the client list, and keep the network between the BNG and rradius trusted. For more protection, add `lock_mac`, or switch to RADIUS over TLS (RadSec, enterprise edition).
- **`lock_mac` only stops a change of device and cannot stop forgery.** A MAC address is in clear text, and anyone on the same layer 2 network can copy it.
- **Every login writes to the database once (`last_mac`, `last_seen`).** Writes go through the cluster leader. When the cluster has had no leader for more than 5 seconds, the write fails, and even lines that need no MAC lock cannot log in, and the login gets no response (see [chapter 10](10-lua-policies.md)). When you do not need the "last seen" information, remove the final `UPDATE` in `auth.lua` and the dependency is gone. When a request has no MAC, only `last_seen` is updated, and the previous `last_mac` is not overwritten.
- **The first binding can wrongly reject.** The comparison after binding reads the local copy. When a follower node has not yet applied the binding that was just written, the next login of the same device may be rejected for a moment, and a retry passes.
- **Text and binary ids can collide.** A binary value is written as lower-case hexadecimal starting with `0x`. So if the BNG forwards a text id that the user controls, the user sending `0X0004000A0001` (`0x0004000a0001` after lower-casing) is the same as sending that binary value, and text ending with a tab or newline character is also treated as binary. Do not use this scenario if the circuit-id is a string that the user can influence.
- **Only the RFC 4679 attributes and `NAS-Port-Id` are recognized.** For other formats, change the lines of `auth.lua` that read the values.
- **It checks only at login.** Disabling a line does not end a connection that is already online.

### Common problems

| Symptom | Cause |
| --- | --- |
| Always Reject, and the log has `unknown line` | The values in the table differ from what the BNG sends. Look at the remote-id and circuit-id the script saw in the log (they are the normalized values). When the BNG sends no remote-id, register `remote_id` as an empty string |
| The log has `no circuit-id and no NAS-Port-Id in the request` | The BNG did not put Option 82 into the request. Check the BNG configuration, or confirm that it uses another attribute |
| The log has `is locked to` | `lock_mac = 1` and the MAC differs from the bound one. When the device changes, clear `bound_mac` |
| The log has `is locked to a MAC and the request has none` | `lock_mac = 1`, but the request has no usable `Calling-Station-Id` (not 12 hexadecimal characters) |
| The log has `the plan of line … does not exist` | `lines.plan` has no matching plan in `plans` |
| Login succeeds but there is no `Framed-Pool` | This script is not the top-level script (`script` in `[lua]`), or the BNG ignores this attribute |

## Dual-stack lines: dual-stack-lines

### Goal

This puts the previous scenarios together: authenticate by line identity. If the line has a fixed address, return the fixed one. Otherwise take one from the IPv4 address pool and one from the IPv6 prefix pool. Accounting confirms and releases both. When the prefix pool is exhausted, the IPv4 address just reserved for this request is released, so no unused reservation is left behind.

### Tables

`migrations/1_schema.sql` is a combination of the tables of the three previous scenarios:

- `plans` gains `pool` (the IPv4 pool) and `pool6` (the prefix pool).
- `lines` gains the columns for fixed data, `ipv4`, `netmask`, `routed_v4`, `wan_v6` and `pd_v6`, which mean the same as in static-ip in the next section.
- `ip_pool`, `prefix_pools`, `prefix_leases` and `housekeeping` are the same as in ip-pool and prefix-pool, and use `lines.subscriber` as the account name, so `subscriber` has a unique index: one subscriber can have only one line.

IPv4 and IPv6 are decided separately. A line with only a fixed IPv4 still gets its IPv6 prefix from the pool. If either `wan_v6` or `pd_v6` is set, the IPv6 of that line does not come from the pool, and only the one that is set is returned.

Demo data: `alice` and `bob` are fully dynamic. `carol` has a MAC lock. `dave` has fixed IPv4 and IPv6. `frank` has only a fixed IPv4. The prefix pool `tiny6` of the `metro` plan has only two /64, so the third user (`max`) runs into prefix exhaustion.

The three plans and the pools they use (`migrations/2_demo_data.sql`):

| Plan | IPv4 pool | Prefix pool | Description |
| --- | --- | --- | --- |
| `residential` | `res4` (8 addresses, `10.77.0.2` to `10.77.0.9`) | `res6` (`2001:db8:100::/48` cut into /56, 256 prefixes) | Residential |
| `business` | `biz4` (3 addresses, `10.88.0.2` to `10.88.0.4`) | `biz6` (`2001:db8:200::/52` cut into /60, 256 prefixes) | Business |
| `metro` | `res4` | `tiny6` (`2001:db8:300::/63` cut into /64, 2 prefixes) | The prefix pool is very small, to see prefix exhaustion |

Thirteen lines (the remote-id is always `olt-a`, and the circuit-id is `eth 1/1/1:<number>`): `:100` alice and `:101` bob (residential, fully dynamic); `:102` carol (MAC lock); `:103` dave (business, fixed IPv4 and IPv6); `:104` erin (disabled); `:105` frank (fixed IPv4 only); `:106` to `:109` (business, fully dynamic; `biz4` has only 3 addresses, so the fourth runs into IPv4 exhaustion); `:110` to `:112` (metro, the third runs into prefix exhaustion).

### Configuration highlights

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "accounting.lua"

[auth]

[[auth.handlers]]
type = "lua"
script = "line.lua"
```

There is no password, so the auth chain has only one lua handler. Why not "one handler for IPv4 and one for IPv6": handlers in an auth chain do not pass values to each other, and a reply attribute set by the previous one cannot be read by the next. The two pool scripts would each have to work out again "which user is this". Also, only a single script can revoke the IPv4 address just reserved when the prefix runs out.

The named queries `pool_usage` and `prefix_usage` watch the two kinds of pools.

### Script highlights

`line.lua` does these in order:

1. The line identity, as in line-identity.
2. A check of the fixed data: a value that is not valid is rejected and logged, and is not silently changed to dynamic (the reason is in the next section).
3. IPv4: with a fixed `ipv4`, it returns `Framed-IP-Address` and `Framed-IP-Netmask` (`255.255.255.255` when not set), and with `routed_v4` it also returns `Framed-Route`. Without one, it takes an address from the IPv4 pool of the plan.
4. IPv6: with fixed values, it returns `Framed-IPv6-Prefix` and `Delegated-IPv6-Prefix`. Without them, it takes a prefix from the prefix pool and returns `Framed-IPv6-Pool`.
5. When the prefix runs out, it releases the IPv4 address just reserved and then rejects. If you change `ALLOW_V4_ONLY` at the top of the script to `true`, it instead keeps the IPv4 address and returns only the IPv4 attributes: the subscriber has no IPv6 but can reach the network.

Finally it returns `Filter-Id` (the profile name of the plan), `Acct-Interim-Interval` and `Class`.

`accounting.lua` is the two accounting scripts of ip-pool and prefix-pool merged. The accounting packets of IPoE have no reliable `User-Name`, so it finds the line through `Class`. When a packet has no `Class`, it still answers the NAS, does not process the packet, and writes one log line. A line with fixed addresses has no row in the pools, so the corresponding `UPDATE` changes no row, and that is not an error.

### Testing and notes

Verified. As in line-identity, first change the `address` of `bng` to `"127.0.0.1"`, and use the `vsa` function of the previous section to carry the line identity (the remote-id is always `olt-a`). The replies for the lines:

```text
:100 alice  Framed-IP-Address = 10.77.0.9, Delegated-IPv6-Prefix = 2001:db8:100:e700::/56, Framed-IPv6-Pool = res6, Filter-Id = home-100m, Acct-Interim-Interval = 300, Class = alice
:103 dave   Framed-IP-Address = 203.0.113.10, Framed-IP-Netmask = 255.255.255.255, Framed-Route = 203.0.113.8/29 0.0.0.0 1, Framed-IPv6-Prefix = 2001:db8:1:1::/64, Delegated-IPv6-Prefix = 2001:db8:f00::/56, Filter-Id = biz-500m, Acct-Interim-Interval = 300, Class = dave
:105 frank  Framed-IP-Address = 203.0.113.20, Framed-IP-Netmask = 255.255.255.255, Delegated-IPv6-Prefix = 2001:db8:200:c30::/60, Framed-IPv6-Pool = biz6, Filter-Id = biz-500m, Acct-Interim-Interval = 300, Class = frank
```

(Compiled from trace lines; the IPv4 address is picked at random.) `:110` and `:111` take the two /64 of `tiny6`, and `:112` is rejected:

```text
… script line.lua (radius/authentication, client bng): prefix pool tiny6 is exhausted: no prefix for max
```

At this point `res4 reserved` in `rradiusd query pool_usage` is still 3 (`alice`, `kim`, `lee`), and the IPv4 address that `max` had just reserved has been released.

Test accounting with the `session` load: it copies the `Class` and `Framed-IP-Address` of the Access-Accept into the Start and the Stop, as a BNG does. After the Stop, the IPv4 address of that line goes back to `free`, and the prefix becomes `released`.

Notes:

- In the tests `User-Name` is `line`, and the accounting script does not look at it at all, only at `Class`. If a real BNG does not send `Class` back, the accounting of that line is not processed. Check the BNG configuration first.
- The three constants (`RESERVE_SECONDS`, `HOLD_SECONDS`, `REAP_SECONDS`) exist once in `line.lua` and once in `accounting.lua`, and must be the same.

### Maintain the data

```sql
-- Add a dynamic line
INSERT INTO lines (remote_id, circuit_id, subscriber, plan) VALUES ('olt-a', 'eth 1/1/2:100', 'zoe', 'residential');
-- Give a line a fixed IPv4 address and prefix (a NULL column means not fixed)
UPDATE lines SET ipv4 = '203.0.113.30', pd_v6 = '2001:db8:f10::/56' WHERE subscriber = 'zoe';
-- Disable
UPDATE lines SET enabled = 0 WHERE subscriber = 'zoe';
```

Write data changes as a migration file with a new number, put it on every node and restart the nodes one by one (see [chapter 18](18-operations.md#adding-a-migration)). Write `remote_id` and `circuit_id` in normalized form (see "Authenticate by line identity"). Always write IPv6 prefixes in lower case and compressed form. The fixed `ipv4`, `wan_v6` and `pd_v6` each have a unique index (`lines_ipv4`, `lines_wan_v6`, `lines_pd_v6`), so two lines cannot register the same value.

**When a line switches between dynamic and fixed, its existing rows in the pools do not disappear at once.** The address or prefix is released by the request-triggered reclaim only after accounting stops and the entry expires. Until then it still holds one place in the pool.

### Cluster

Writes of both authentication and accounting go through the cluster leader. Every login writes `last_mac` and `last_seen` of `lines`, and a dynamic line also reserves an address and a prefix. When the cluster has no leader (see [chapter 16](16-cluster.md)), logins fail, even for lines with fully fixed data. When you do not need the "last seen" information, remove the last two statements `UPDATE lines SET last_mac …` and `SET last_seen …` of `line.lua`, and fixed lines no longer depend on the leader. A dynamic line must write to reserve its address and prefix, and that cannot be removed.

### Scale

Below is the **extra cost per login after the merge**, taken from chapter 22 of the old manual and **not measured again** (I did not find the raw files for this group in the change records, only the record in the old chapter). Conditions: Apple M4 Max (14 CPUs, macOS), server and `rradius-bench` on the same machine, single node. 64 concurrent PAP logins, with 2,000 different users each logging in for the first time. The IPv4 pool and the prefix pool each have 16,384 entries, with 43% taken beforehand in a scattered way and about 55% at the end of the measurement. Each ran twice.

| Example | Logins per second | p50 | p99 |
| --- | --- | --- | --- |
| Address pool (ip-pool) | about 14,500 (14,473, 14,674) | about 3.6 ms | about 21 ms |
| Prefix pool (prefix-pool) | about 11,200 (11,075, 11,288) | about 4.8 ms | about 22 ms |
| Dynamic lines of dual-stack-lines (IPv4 plus prefix) | about 6,100 (6,102, 6,172) | about 9.4 ms | about 34 ms |

A dynamic line costs about 163 microseconds per login. This is about the sum of the two separate pools (about 69 plus 90 microseconds), plus the line lookup and the last-seen write. There were no rejects and no duplicate allocations. The throughput is not below half of the slower of the two separate examples (the prefix pool, about 5,600). These are relative numbers on one machine, not the capacity of a BNG site. The measurement used `User-Name` as the line name (`rradius-bench` rotates it), not the Option 82 attributes, and the only difference is one more attribute to read. Fixed lines do not touch the pools, and a login is one lookup plus one write, which is cheaper than a dynamic line.

### Known limitations

- **It has not been verified against a real BNG.** The attributes are the standard `Framed-*` and the `Delegated-IPv6-Prefix` of RFC 4818. Whether the BNG accepts them, how `Framed-Route` must be written, and whether it sends `Class` back in accounting packets are in your BNG documentation.
- **Fixed values are not checked against the pools.** If a fixed `ipv4` falls inside the range of `ip_pool`, or a fixed `pd_v6` falls inside a prefix pool, the pool may hand the same value to someone else. Put fixed addresses in a range outside the pools (see "Fixed addresses and prefixes").
- **A line is checked only at login.** Disabling a line does not end a connection that is already online (ending it needs Disconnect, an enterprise feature).
- **Prefixes are not guaranteed to be stable.** When the starting point is taken by someone else, you get a different one, and the fuller the pool the less stable (see "IPv6 prefix pool").
- **One line per subscriber.** The pools and accounting use `lines.subscriber` as the key (the unique index `lines_subscriber`). For one subscriber with several lines, the pool key must be changed.
- **Accounting recognizes only `Delegated-IPv6-Prefix`.** A packet with only `Framed-IPv6-Prefix` (the WAN prefix) does not confirm the prefix.
- **There is no WAN /64 pool.** `Framed-IPv6-Pool` is only a pool name, and the BNG's pool decides the address of the connection itself.
- **Only the RFC 4679 attributes and `NAS-Port-Id` are recognized.** The security limits of the MAC lock and the line identity are the same as in "Authenticate by line identity": a line identity is not a secret, so list only the BNG in the client list.

### Common problems

| Symptom | Cause |
| --- | --- |
| Always Reject, and the log has `dual stack: unknown line` | The values in the table differ from what the BNG sends. Handle it as in "Authenticate by line identity" |
| The log has `has routed_v4 or netmask but no ipv4` | The line registers `routed_v4` or `netmask` but has no `ipv4`. Add `ipv4` or clear them |
| The log has `dual stack: bad … for` | A fixed value is not valid (address, subnet, mask, prefix). The log shows the user, the column and the value, and it does not silently fall back to dynamic |
| The log has `prefix pool … is exhausted` | The prefix pool has no free slot. Expand the pool, or change `ALLOW_V4_ONLY` to `true` |
| The log has `ip pool … is exhausted` | The IPv4 pool has no free slot. It always rejects, whatever `ALLOW_V4_ONLY` says |
| Accounting packets do not change the pools | The log has `no Class`: the BNG does not send `Class` back in accounting packets |
| Login succeeds but there is no `Delegated-IPv6-Prefix` | The `pd_v6` of the line is NULL and `wan_v6` is set (only the one with a value is returned), or `ALLOW_V4_ONLY` is `true` and the prefix pool is full |

## Fixed addresses and prefixes: static-ip

### Goal

Business subscribers or certain plans need a fixed IPv4 address, a routed subnet and a fixed IPv6 prefix. A subscriber with no fixed data still gets a pool name as usual, and the BNG allocates.

### Tables

`subscribers` has `pool` (the pool name returned when there is no fixed data). `static_assignments` has a row only for subscribers with fixed data. Every column can be `NULL`, and a `NULL` column does not return that attribute:

| Column | Returned as | Example |
| --- | --- | --- |
| `ipv4` | `Framed-IP-Address`, plus `Framed-IP-Netmask` | `203.0.113.10` |
| `netmask` | `Framed-IP-Netmask`, `255.255.255.255` when not set | `255.255.255.252` |
| `routed_v4` | `Framed-Route`: `<subnet> 0.0.0.0 1` | `198.51.100.8/29` |
| `wan_v6` | `Framed-IPv6-Prefix`, the prefix of the connection itself, usually a /64 | `2001:db8:1:1::/64` |
| `pd_v6` | `Delegated-IPv6-Prefix`, the prefix delegated to the subscriber's internal network | `2001:db8:100::/56` |

`ipv4`, `wan_v6` and `pd_v6` each have a unique index, so two subscribers cannot register the same text.

> **Note:** The unique index compares text. Always write IPv6 prefixes in lower case and compressed form (`2001:db8:1:1::/64`). `2001:0db8:1:1::/64` and `2001:DB8:1:1::/64` are different texts, and the index does not catch them. It also cannot catch overlapping subnets (one subscriber's /29 containing another's address). Check this in the process that creates the data.

### What the unique indexes stop and what they do not

The three unique indexes (`static_assignments_ipv4`, `static_assignments_wan_v6`, `static_assignments_pd_v6`) stop this: when two subscribers register the same text, the `INSERT` or `UPDATE` written later fails with `UNIQUE constraint failed`. `NULL` does not count, so many subscribers can all have no `wan_v6`. What they cannot stop, besides the prefix notation and the overlapping subnets in the note above:

- **A fixed address inside a dynamic pool.** The pool of the ip-pool scenario does not know which addresses are fixed, and hands the same address to someone else. Always keep fixed addresses outside the range of the pools. When you use both scenarios together, you can check for collisions (this matches only identical addresses):

  ```sql
  SELECT s.username, s.ipv4 FROM static_assignments s JOIN ip_pool p ON p.ip = s.ipv4;
  ```

  This is a maintenance query to run now and then. `ip_pool` has no index that starts with `ip`, so with a large pool it scans the whole table. Different pools (for example different VRFs) can have the same address, so look at the result again to see whether it is the same pool.

### Configuration highlights

`all_accept`: the sql handler checks the password, and the lua handler `static.lua` returns the address attributes. The `accounting.lua` in `[lua]` only blocks protocols other than RADIUS, and always answers accounting without recording it.

### Script highlights

The rule of `static.lua` is "reject when the data is wrong". If a subscriber who pays for a fixed address silently gets one that changes because of a typo in the data, nobody notices. So:

- It sets attributes with `pcall(set_attr, …)`. `set_attr` checks the format of the value against the dictionary and raises an error when it is not valid. The script catches it, writes the user, the column and the value to the log, and rejects.
- `Framed-Route` is a string in the dictionary and `Framed-IP-Netmask` is an address, so `set_attr` cannot stop values with wrong content (for example `garbage` or `255.0.255.0`). The script checks them itself: a subnet must have the form `a.b.c.d/n` with the host bits at 0, and the 1 bits of a mask must be consecutive at the front.
- When there is no row, or the row has no address, subnet or prefix (having only `netmask` also counts as none), it is the same as having no fixed data, and `Framed-Pool` is returned.

### Testing and notes

Verified. The replies for the five demo accounts (compiled from trace lines):

| Account | Reply |
| --- | --- |
| `alice` | `Framed-Pool = residential` |
| `bob` | `Framed-IP-Address = 203.0.113.10, Framed-IP-Netmask = 255.255.255.255, Acct-Interim-Interval = 300` |
| `carol` | `Framed-IP-Address = 203.0.113.20, Framed-IP-Netmask = 255.255.255.252, Framed-Route = 198.51.100.8/29 0.0.0.0 1, Framed-IPv6-Prefix = 2001:db8:1:1::/64, Delegated-IPv6-Prefix = 2001:db8:100::/56, Acct-Interim-Interval = 300` |
| `dave` | `Framed-IPv6-Prefix = 2001:db8:1:2::/64, Delegated-IPv6-Prefix = 2001:db8:200::/56, Acct-Interim-Interval = 300` |
| `erin` | reject (disabled) |

To see the behavior with wrong data, add one more migration before starting, for example `migrations/3_bad.sql`:

```sql
UPDATE static_assignments SET netmask = '255.0.255.0' WHERE username = 'bob';
UPDATE static_assignments SET pd_v6 = '2001:db8:200::/200' WHERE username = 'dave';
```

Both `bob` and `dave` become reject, and the log says:

```text
… script static.lua (radius/authentication, client localhost): static ip: bad netmask for bob : 255.0.255.0
… script static.lua (radius/authentication, client localhost): static ip: bad pd_v6 for dave : 2001:db8:200::/200 - runtime error: '2001:db8:200::/200' is not a valid ipv6prefix for 'Delegated-IPv6-Prefix' …
```

This migration is only for the experiment. After the test, stop the server and delete `data/` and this file.

### Maintain fixed data

`static_assignments` has no dedicated management command. Write data changes as a migration file with a new number, put it on every node and restart the nodes one by one (see [chapter 18](18-operations.md#adding-a-migration)). When there are many subscribers and they change often, keep the data in an external database or the billing system, and let the script query it with `db_query` or `http_request` ([chapter 09](09-external-databases.md), [chapter 10](10-lua-policies.md)). The SQL looks like this:

```sql
-- Add a subscriber and fixed addresses
INSERT INTO subscribers (username, password, pool, enabled) VALUES ('gina', 'gina-password', 'business', 1);
INSERT INTO static_assignments (username, ipv4, netmask, routed_v4, wan_v6, pd_v6)
VALUES ('gina', '203.0.113.30', '255.255.255.252', '198.51.100.16/29', '2001:db8:1:3::/64', '2001:db8:300::/56');

-- Change back to dynamic: delete the row
DELETE FROM static_assignments WHERE username = 'gina';
```

The BNG must accept these attributes. Which devices use `Framed-Route` and `Delegated-IPv6-Prefix` depends on the vendor and the configuration, so see the BNG documentation. When the BNG does not route the `/29`, check this first.

### Known limitations

- **It does not prevent overlapping subnets, and does not check whether a fixed address is in a dynamic pool** (see above).
- **Two simultaneous sessions of the same user get the same fixed address.** To limit concurrent connections, put the `limits.lua` of "Plans and usage control" in front: the order of `[[auth.handlers]]` is the sql handler, `limits.lua`, `static.lua`. Connection slots must be confirmed and released by the accounting script, so the `script` in `[lua]` must be replaced with the `accounting.lua` of usage-control, and the tables it uses (`plans`, `session_slots`, `accounting`, `housekeeping`) must be added to the migrations. The `accounting.lua` of static-ip only answers and does not touch connection slots, so a slot is released after its 120-second reservation, and the limit has no effect.
- **There are no dynamic IPv6 prefixes.** This scenario returns only the fixed prefixes written in the table. For aligned DHCPv6-PD allocation, see "IPv6 prefix pool".
- **It does not give fixed addresses by line identity.** To do so, add columns to `lines` ("Authenticate by line identity"), or change the primary key of `static_assignments` to the line.
- **It is decided only at login.** A data change does not affect connections that are already online, and takes effect at their next login.
- **There is no built-in check tool and no metric.** The validity of addresses and prefixes is checked only at login.

### Common problems

| Symptom | Cause |
| --- | --- |
| A fixed subscriber always gets Reject, and the log has `static ip: bad` | The value of that column is not a valid address or prefix. The log shows the user, the column and the value |
| Login succeeds but there is no fixed address | `static_assignments` has no row for this user, or the column is `NULL` (having only `netmask` also counts as none). The user name must match `subscribers` |
| The BNG does not route the `/29` | The BNG does not accept this format of `Framed-Route`, or needs extra configuration |
| Two subscribers get the same address | A fixed address is inside a dynamic pool, or the subnets overlap. Use the check query above |
| `INSERT` fails with `UNIQUE constraint failed` | That address or prefix is already registered to someone else |

## Plans and usage control: usage-control

### Goal

Each subscriber has a plan, and the plan limits two things: how many connections at the same time, and the traffic over the most recent period. After the quota is exceeded, the plan either slows the subscriber down (the login succeeds, but a different `Filter-Id` is returned) or rejects.

### Tables

| Table | Contents |
| --- | --- |
| `plans` | `max_sessions`, `quota_octets` (bytes in plus out, `0` means unlimited), `period_days` (the last N days, a rolling window, not a calendar month), `over_quota` (`throttle` or `reject`), `normal_filter`, `throttle_filter` |
| `subscribers` | Account, password, `plan`, `enabled` |
| `session_slots` | `max_sessions` connection slots per subscriber, primary key `(username, slot)`. The number of simultaneous connections is the number of slots that are taken |
| `accounting` | One row per session, written as in the quick start. The primary key is `(client, nas, session_id)`, because a session ID is unique only within one NAS |

Demo plans: `basic` has 1 simultaneous connection and unlimited traffic. `family` has 2 simultaneous connections and 100 GiB per 30 days, and throttles to `family-slow` when exceeded. `strict` has 1 simultaneous connection and 5 GiB per 30 days, and rejects when exceeded.

Connection slots are filled in with `INSERT OR IGNORE` at the first login of a subscriber, so you do not need to create them beforehand.

### Configuration highlights

`all_accept`: the sql handler checks the password, and the lua handler `limits.lua` checks the plan. `[sessions]` queries the rows of `session_slots` that are `active` and not expired. The named query `slot_usage` becomes the metric `rradius_query_slot_usage{state}`.

### Script highlights

`limits.lua`:

1. It looks up the plan of the subscriber, and rejects a subscriber that is disabled or does not exist.
2. It checks traffic first: it adds up `input_octets + output_octets` of this subscriber over the last `period_days` days, including the amount accumulated so far by sessions that are still open. If the total is over the quota, it throttles or rejects according to `over_quota`. Checking traffic first means a rejected user does not take a connection slot.
3. Then it tries to claim a free connection slot. When it gets one, it reserves it for 120 seconds and returns `Filter-Id` and `Acct-Interim-Interval`. When it cannot, it rejects. After a plan is made smaller, slots above `max_sessions` are no longer allocated.

`accounting.lua`:

- Every accounting packet is written to `accounting`. Traffic is taken as `max(old value, new value)`, so a retransmitted or late packet does not make the number go backwards.
- For traffic above 4 GiB, the NAS puts the carry in `Acct-Input-Gigawords` and `Acct-Output-Gigawords`. The script multiplies them by 4294967296 and adds them back.
- It restores the time of the event with `Acct-Delay-Time`, so a packet replayed from the accounting spool or sent late by the NAS still has the right time.
- Start confirms the reserved connection slot as `active`. Interim-Update extends it. When no slot is found, it takes a reserved or free one (the case where the reservation has expired and the packet arrives late). Stop releases it. Accounting-On and Accounting-Off release all slots of this NAS.

### Why connection slots, and not counting open sessions in the accounting table at login

Counting "how many sessions this user still has open" at login is simpler, but it has two holes: logins that arrive at the same time cannot see each other, and a reservation cannot be counted before its Accounting-Start arrives. The limit can be exceeded. A connection slot turns "take a place" into one conditional write (`UPDATE … WHERE state = 'free'`, and 1 affected row means it won). All writes go through the cluster leader, so two simultaneous logins cannot take the same slot.

### Numbers to tune

| Number | Where | Default | Description |
| --- | --- | --- | --- |
| `RESERVE_SECONDS` | `limits.lua` | 120 | How long to reserve. A NAS usually sends the Start within a few seconds after login |
| `INTERIM_SECONDS` | `limits.lua`, `accounting.lua` | 300 | How often the NAS is asked to send an Interim-Update. A slot expires after three times this value |
| `REAP_SECONDS` | `accounting.lua` | 60 | The most often the reclaim runs (`limits.lua` has 60 written in) |

The two scripts are separate files and cannot share constants, so `INTERIM_SECONDS` must be the same in both.

### Testing and notes

Verified. Simultaneous connections:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 819/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 3516/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

The second one is rejected, and the log says `usage control: session limit of plan basic ( 1 ) reached by alice - rejected`. `bob` on the `family` plan can log in twice, and the third is rejected.

> **Note:** Although there is no Accounting-Start after the first login yet, the connection slot is already reserved for 120 seconds. When you log in twice in a row in a test, the second is always rejected. This is not an error. The reservation is blocking it. To free the slot, send a Stop, or wait for the reservation to expire.

Traffic: use the `session` load with `--attr 52=int:N` (`Acct-Input-Gigawords`), which sends N × 4 GiB of usage at once:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load session,concurrency=1 --requests 1 --attr 52=int:2
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load pap,concurrency=1 --requests 1
…
  pap: 3180/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`carol` used 8 GiB, more than the 5 GiB of `strict`, and the log says `quota of plan strict is used up by carol - rejected`. After `dave` (`family`) sends `--attr 52=int:25` (100 GiB), his next login still succeeds, but `Filter-Id` changes from `family` to `family-slow`.

Notes:

- Traffic is checked at login. When a subscriber goes over the quota while online, this scenario does not disconnect them. The throttle or reject comes at their next login.
- The period is a rolling window of the last `period_days` days, counted from the start time of the session. To reset every month instead, change that `SELECT` in `limits.lua`.

> **Enterprise edition:** The enterprise edition can send a CoA or Disconnect-Request to a subscriber who is over the quota at accounting time, so it does not have to wait for the next login.

### Operations: look up slots and traffic, change plans

```sql
-- The connection slots of a subscriber
SELECT slot, state, session_id, nas, expires FROM session_slots WHERE username = 'bob' ORDER BY slot;
-- His traffic over the last 30 days (bytes)
SELECT sum(input_octets + output_octets) FROM accounting WHERE username = 'bob' AND started >= strftime('%s', 'now') - 30 * 86400;
-- Change the plan: the next login uses the new plan
UPDATE subscribers SET plan = 'family' WHERE username = 'bob';
-- Change the limit of a plan: family to 200 GiB (200 × 1073741824)
UPDATE plans SET quota_octets = 214748364800 WHERE plan = 'family';
```

When `max_sessions` is made smaller, slots numbered above the new limit are no longer allocated. Sessions that are online are not affected, and after they end that slot is no longer used. Change the data of `plans` and `subscribers` with a migration file with a new number, or keep it in an external database (see "Maintain line data" in "Authenticate by line identity").

### Monitoring

The metric of `slot_usage` is `rradius_query_slot_usage{state}`, counted by every node every 30 seconds:

```toml
# rradius.toml (excerpt, other settings omitted)
[queries.slot_usage]
sql = "SELECT state, count(*) AS slots FROM session_slots GROUP BY state"
help = "Session slots by state: free, reserved or active."
labels = ["state"]
value = "slots"
```

```console
$ rradiusd query slot_usage --config rradius.toml
```

- How many connections are online now: `max(rradius_query_slot_usage{state="active"})`. `reserved` means logged in but the NAS has not sent an Accounting-Start yet. Every node outputs the same numbers, so in a cluster use `max()`, not `sum()`.
- Connection slots are created only at the first login of a subscriber, so the `free` number is "slots not used by subscribers who have logged in", not the capacity for all subscribers.
- Cost: this query uses the covering index of `session_slots_expiry` (`SCAN session_slots USING COVERING INDEX session_slots_expiry`). On 2026-11-16, on an in-memory SQLite 3.54.1 database with 200,000 rows, the median was about 4.3 ms. Not measured again.
- How much traffic has been used is a different matter. The sum over `accounting` gets slower as the number of sessions of a subscriber grows, so do not put it in a metric that runs every 30 seconds.

### Known limitations

- **It checks only at login.** The traffic a session uses while online is checked only at the subscriber's next login, and a connection that is online is not interrupted. To enforce it midway (throttle or disconnect above the limit), you need CoA and Disconnect-Request, which are enterprise features.
- **The metering period is a rolling last N days,** not a calendar month. There is no billing day and no time zone.
- **The traffic sum gets slower with the number of sessions of a subscriber.** There is the `accounting_user` index and only one subscriber is looked at, but with many subscribers and many sessions, measure first. If needed, keep a separate table with a running total per subscriber.
- **When the quota is used up, the NAS only sees a Reject.** The reason is in the log. Monitor the log for `session limit` and `quota`.
- **There is no prepaid balance and no online charging,** and no built-in function. Use of connection slots comes from `slot_usage` above.
- **When a NAS crashes without sending a Stop,** its connection slot stays taken until it expires (3 Interim intervals, 900 seconds). When the NAS comes back and sends Accounting-On, the slots are released at once.
- **Case of account names.** Connection slots and traffic go by the name used at login. To make them case-insensitive, set `[auth.username]` in the auth chain (see [chapter 07](07-users-and-chains.md)).

### Common problems

| Symptom | Cause |
| --- | --- |
| Login succeeds but there is no `Filter-Id` | There is no lua handler in the configuration, or `policy` is not `all_accept`, or the `normal_filter` of the plan is an empty string |
| Always Reject, and the log has `session limit` | Connection slots are taken. Look at `session_slots` for rows that have expired but are not yet reclaimed (nothing is reclaimed when no request comes in) |
| Always Reject, and the log has `quota` | The plan is `reject` and the traffic is used up. Check the sum over `accounting` |
| Traffic does not increase | The NAS does not send Interim-Update or Stop. `input_octets` in `accounting` is a cumulative value |
| A subscriber over the limit is not throttled | Throttling takes effect only at the next login. `Filter-Id` must be mapped to a bandwidth by the NAS |

## Return rates by BNG: rate-limit

### Goal

For the same plan, BNGs from different vendors want different rate attributes: some accept only a profile name, and some accept numbers. Each BNG is one client, and a table records which "style" it uses.

### Tables

| Table | Contents |
| --- | --- |
| `plans` | `down_kbps` (toward the subscriber) and `up_kbps` (from the subscriber toward the network), in kbit/s. `profile` is the name of the profile of the same name on the BNG |
| `subscribers` | Account, password, `plan`, `enabled` |
| `nas_styles` | `client` is the `name` of a `[[radius.clients]]` entry, and `style` is the style. A client with no row uses `profile` |

### Configuration highlights

One client per BNG, with a `name` that matches `nas_styles.client`:

```toml
# rradius.toml (excerpt, other settings omitted)
[[radius.clients]]
name = "bng-default"
address = "127.0.0.1"
secret_file = "localhost.secret"

[[radius.clients]]
name = "bng-nokia"
address = "10.70.0.1"
secret_file = "nokia.secret"
```

The scenario has three more, `bng-mikrotik`, `bng-wispr` and `bng-erx`. The auth chain is again `all_accept`: the sql handler checks the password, and the lua handler `rate.lua` returns the rate attributes.

### Script highlights

Each entry in the `styles` table of `rate.lua` is a function. To support another BNG, add an entry:

| Style | Attributes returned | Description |
| --- | --- | --- |
| `profile` | `Filter-Id = <profile>` | A standard attribute. The rate is in the profile of the same name on the BNG |
| `alc_sla` | `Alc-SLA-Prof-Str = <profile>` | A vendor attribute, the name of the SLA profile |
| `erx` | `ERX-Ingress-Policy-Name = <profile>-up`, `ERX-Egress-Policy-Name = <profile>-down` | Ingress is the subscriber's upload, and egress is the subscriber's download |
| `mikrotik` | `Mikrotik-Rate-Limit = <up>k/<down>k` | Seen from the router: rx is the subscriber's upload and tx is the subscriber's download. `k` is kbit/s |
| `wispr` | `WISPr-Bandwidth-Max-Down`, `WISPr-Bandwidth-Max-Up`, in bit/s | 32-bit integers, at most 4294967295 bit/s. A faster plan cannot be sent, so it is rejected and logged |

For the styles that send only a name, the unit and direction of the rate are decided by the profile on the BNG. For the styles that send numbers, the unit and direction come from the definition of the attribute. Before you connect for real, check the documentation of your BNG.

Take the demo plan `fiber-500` (`down_kbps` 500000, `up_kbps` 250000, `profile` is `fiber-500`) as an example. The values each style actually sends:

| Style | Attributes and values |
| --- | --- |
| `profile` (default) | `Filter-Id = fiber-500` |
| `alc_sla` | `Alc-SLA-Prof-Str = fiber-500` |
| `erx` | `ERX-Ingress-Policy-Name = fiber-500-up`, `ERX-Egress-Policy-Name = fiber-500-down` |
| `wispr` | `WISPr-Bandwidth-Max-Down = 500000000`, `WISPr-Bandwidth-Max-Up = 250000000` |
| `mikrotik` | `Mikrotik-Rate-Limit = 250000k/500000k` |

The policy names of `erx` are `profile` plus `-up` and `-down`, and policies with these two names must exist on the BNG first. The profiles of `profile` and `alc_sla` must also exist on the BNG first.

**The units and directions of the styles that send numbers (`wispr`, `mikrotik`) are written according to the definitions of the attributes and were not verified on real hardware** (the tests of this manual and of the examples check only the values of the attributes, and never connected a real BNG). Before going live, measure the rate once on a real device with a test subscriber. A rate that is off by a factor of 1000 is usually a unit mismatch (see "Common problems" below).

The style is decided by looking up `nas_styles` with the Lua variable `client` (the name of the matching client). When `nas_styles` holds a style the script does not know, or the plan of the subscriber does not exist, the login is rejected and a log line is written.

### Testing and notes

Verified. `bng-default` is `127.0.0.1`. `alice` (`fiber-100`) logs in and gets `Filter-Id = fiber-100`. The plan `gone` of `dave` does not exist, so he is rejected, and the log says `rate limit: the plan of dave does not exist`.

For the other styles, the request has to come from the address of that BNG. In the lab directory, change the address of `bng-default` to something else (for example `10.70.0.9`), change the one you want to test to `127.0.0.1`, and then reload:

```console
$ kill -HUP <pid of the server>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat nokia.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
```

The client list can be reloaded, and the log says `configuration reloaded from rradius.toml`. Switching through the four in turn, the replies `alice` gets:

```text
bng-nokia     Alc-SLA-Prof-Str = fiber-100
bng-mikrotik  Mikrotik-Rate-Limit = 50000k/100000k
bng-wispr     WISPr-Bandwidth-Max-Down = 100000000, WISPr-Bandwidth-Max-Up = 50000000
bng-erx       ERX-Ingress-Policy-Name = fiber-100-up, ERX-Egress-Policy-Name = fiber-100-down
```

Each one is followed by `Acct-Interim-Interval = 300`.

Notes:

- The client name is the mapping key. When you change the `name` of a `[[radius.clients]]` entry, change `nas_styles` with it. Otherwise that BNG falls back to the `profile` style and gets only `Filter-Id`.
- One client can be a whole network range, and the BNGs in the same range must use the same style. Put BNGs of different vendors in different clients.

### Add a style

Each entry in the `styles` table at the top of `rate.lua` is a function. It receives the plan (`plan.plan`, `plan.down_kbps`, `plan.up_kbps`, `plan.profile`) and returns attributes with `set_attr`. For example, a vendor that puts the rate in a single string attribute (`My-Vendor-Rate` is a made-up name, and it needs a dictionary definition first):

```lua
  myvendor = function(plan)
    set_attr('My-Vendor-Rate', string.format('%d/%d', plan.down_kbps, plan.up_kbps))
  end,
```

A function that returns `false` means this plan cannot be sent (this is how `wispr` rejects a plan that is too fast). No return value means it was sent. `set_attr` checks the attribute name and the type of the value against the dictionary. When the name is not in the dictionary or the value is not of that type, the login is rejected because of a script error, and the log says `Lua script error`. For a vendor that is not included, first add its attributes to the dictionary (see [chapter 06](06-dictionary.md)).

### Add a BNG

1. Add a `[[radius.clients]]` to `rradius.toml` (name, address, `secret_file`).
2. Add a row to `nas_styles`, written as a migration file with a new number (for example `3_bng_new.sql`), put it on every node and restart the nodes one by one (see [chapter 18](18-operations.md#adding-a-migration)). `client` must be the same as the `name` in the configuration file:

```sql
INSERT INTO nas_styles (client, style) VALUES ('bng-new', 'wispr');
```

A client not listed in `nas_styles` uses `profile` and gets only `Filter-Id`.

### Maintain the data

`plans`, `subscribers` and `nas_styles` are ordinary tables, and there is no dedicated management command. Write data changes as a migration file with a new number. When there are many subscribers and they change often, keep the data in an external database or the billing system, and let the script query it with `db_query` or `http_request` ([chapter 09](09-external-databases.md), [chapter 10](10-lua-policies.md)). The SQL looks like this:

```sql
INSERT INTO plans (plan, down_kbps, up_kbps, profile) VALUES ('fiber-2g', 2000000, 2000000, 'fiber-2g');
UPDATE subscribers SET plan = 'fiber-500' WHERE username = 'alice';
```

### Known limitations

- **No style has been verified with a real BNG.** For the styles that send numbers, check the unit and direction against the documentation yourself.
- **The `wispr` style cannot send a plan above 4294967295 bit/s (about 4.29 Gbit/s).** The attribute has only 32 bits. For a faster plan, the script rejects the login and writes `is faster than WISPr-Bandwidth can say`, and does not send a truncated value. For such a plan, switch this BNG to a style that sends only a name.
- **A plan has only one download value and one upload value.** There is no burst, no time of day, and no throttling after usage runs out. For throttling, see "Plans and usage control".
- **It is decided only at login.** Changing a plan does not affect connections that are already online, and it takes effect at their next login. Changing the rate of a live connection needs CoA (an enterprise feature, not in the core).
- **One client uses only one style.** For a BNG that needs attributes of two styles at the same time, build them in the script yourself.
- **The dictionary must know the attributes.** Otherwise `set_attr` fails. See "Add a style" above.

### Common problems

| Symptom | Cause |
| --- | --- |
| Always Reject, and the log has `unknown or disabled user` | The subscriber is not in `subscribers`, or `enabled = 0` |
| Always Reject, and the log has `the plan of … does not exist` | `subscribers.plan` has no matching plan in `plans` |
| Always Reject, and the log has `which this script does not know` | The name in `nas_styles.style` is not in the `styles` table of `rate.lua` |
| Always Reject, and the log has `is faster than WISPr-Bandwidth can say` | The plan rate is above the limit of the `wispr` style (4294967295 bit/s) |
| Always Reject, and the log has `Lua script error` | A style function used an attribute that is not in the dictionary, or a value of the wrong type |
| Login succeeds but the BNG does not limit the rate | The BNG has no profile or policy of that name; or, for a style that sends numbers, the unit or direction differs from the BNG's; or this client is not listed in `nas_styles` and `Filter-Id` was returned |
| The rate is off by a factor of 1000 | A unit mismatch: `wispr` is bit/s, and the `k` in the `mikrotik` string is kbit/s. Check the BNG documentation |

## Single-file examples

`rradius-server/examples/` has three single-file scripts related to this chapter. They are simpler than the scenarios above, and the table structure is written in a comment at the top of the file, which you create yourself. All three check policy only and do not check passwords, and they are used the same way:

- As the `script` of `[lua]`, so accounting always arrives here.
- Also as a lua handler in the auth chain, after an sql or ldap handler, with `policy = "all_accept"`, so a login must pass both.

```toml
# rradius.toml (excerpt, other settings omitted)
[lua]
script = "simultaneous-use.lua"

[auth]
policy = "all_accept"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

[[auth.handlers]]
type = "lua"
script = "simultaneous-use.lua"
```

All three were tested in the lab directory made from the quick start, with the tables created from the `CREATE TABLE` at the top of the file and `rradius-bench`. Verified.

### ip-pool.lua: the smallest address pool

One table `ip_pool (ip, holder, expires)`. At login it picks 8 addresses at random whose `expires` has passed and claims one with `UPDATE … WHERE expires <= ?`. Start and Interim-Update extend the lease by `LEASE` (900 seconds), and Stop releases. It is a different design from the ip-pool scenario:

| | `ip-pool.lua` | ip-pool scenario |
| --- | --- | --- |
| Pools | One | Several, by subscriber |
| Login again | Does not recognize the address the user is already using, and takes another | Reuses the existing address |
| Stickiness | None | The last address is returned within a day |
| Reservation | None, a login leases for 900 seconds | Reserved for 120 seconds, confirmed only by the Start |
| Deciding on a Stop | Only whether `holder` is this subscriber | Looks at the session ID |

Tested: with only one address in the pool, `alice` logs in the first time and gets `10.0.0.1`. Logging in again before the Stop is rejected, and the log says `no address free for alice`. After an `acct-stop` with `--attr 8=ip:10.0.0.1`, `bob` gets it.

> **Note:** The comment in the file explains its limits: after the same subscriber gets the same address back, an old Stop that is retransmitted still releases it, and so does a Stop replayed from the accounting spool. For production, start from the ip-pool scenario.

### simultaneous-use.lua: simultaneous connections

One table `active_sessions`, with primary key `(client, session_id)`. Start and Interim-Update write to it (`INSERT OR REPLACE`), and Stop deletes. At login it counts the sessions of this subscriber with news within `STALE_AFTER` (900 seconds), and rejects when the count reaches `MAX_SESSIONS` (2).

The difference from usage-control: a session counts only from its Accounting-Start, so two logins at the same instant can both pass, and the copy on each node may be slightly stale, so the limit is not exact. Tested: after `alice` sends two `acct-start` requests, her login is rejected, and `bob` can still log in. The interval at which the NAS sends Interim-Update must be shorter than `STALE_AFTER`, or sessions that are still online stop being counted.

### quota-and-hours.lua: total time and time of day

Two tables: `quota_users` (`quota_seconds`, `from_hour`, `to_hour`, with time in UTC) and `quota_sessions` (one row per Stop). At login it checks whether now is within the allowed hours (they can span midnight, for example 22 to 6), then computes the remaining seconds and returns them to the NAS in `Session-Timeout`.

Tested: `alice` has 100 seconds and gets `Session-Timeout = 100` the first time. The Stop of `rradius-bench` carries `Acct-Session-Time = 60`, so the next login gets `40`, and the one after that is rejected. An account with both `from_hour` and `to_hour` equal to 0 is always rejected.

Time is counted only at the Stop, so two simultaneous sessions each get the remaining time: two logins of 100 seconds can use up 200 seconds. For a strict limit, use it with `simultaneous-use.lua` and set the limit to 1. Deleting the rows of a subscriber from `quota_sessions` makes them start counting from the beginning, for example once a month.

## Common mistakes

**Symptom:** A login is rejected, the log has no reason written by a script, and the authentication record shows `handler="no_password_check"`.
**Cause:** The account does not exist in `subscribers` or is disabled, and the sql handler gave no answer. The `all_accept` chain has a handler that checks the password but none of them accepted, so the server rejects, and does not let the following lua handler act for someone whose password was not checked.
**Fix:** Make sure the account exists and has `enabled = 1`. This is expected behavior, not a configuration error.

**Symptom:** The pool still has addresses, but a login gets `reject`, and the log says `ip pool … is exhausted`.
**Cause:** All addresses are `reserved`: authentication succeeded, but the NAS did not send an Accounting-Start (or sent it to another server). A reservation takes 120 seconds to expire, and the reclaim runs at most every 60 seconds.
**Fix:** Look at the number in each state with `rradiusd query pool_usage`. Make sure the NAS sends accounting to this server and the shared secret is correct.

**Symptom:** Accounting is always answered, but the address stays `reserved`, and the log has `address … reported by … for … is held by someone else`.
**Cause:** The `Framed-IP-Address` the NAS reports is not the one rradius gave, or `User-Name` differs from the one at authentication.
**Fix:** Turn on `[log.trace]` and compare the address and the account in the authentication reply with those in the accounting packet.

**Symptom:** The accounting of IPoE is answered, but the state in the pools does not change.
**Cause:** The BNG does not send `Class` back in accounting packets, so `accounting.lua` cannot recognize the line. The log has `dual stack accounting: no Class in the packet of session …`.
**Fix:** Turn on `[log.trace]` to see whether the accounting request has `Class`. Configure the BNG to copy the `Class` of the Access-Accept into accounting.

**Symptom:** When you test line-identity, dual-stack-lines or rate-limit with `rradius-bench`, the result is `timeouts 1`.
**Cause:** The client address in these scenarios is the network range of the BNG, not `127.0.0.1`, and the server drops the request as coming from an unknown client. `Discarded: unknown client` in `status` goes up.
**Fix:** Only in the lab directory, change the `address` of that client to `"127.0.0.1"`.

**Symptom:** A subscriber with a fixed address suddenly cannot log in, and the log says `static ip: bad … for …`.
**Cause:** A value in `static_assignments` is not valid, and the script rejects on purpose and does not change to dynamic.
**Fix:** Correct the data in the column the log names. Write IPv6 prefixes in lower case and compressed form.

**Symptom:** The ip-pool login succeeds, but the Access-Accept has no `Framed-IP-Address`.
**Cause:** The auth chain has no lua handler `allocate.lua`, or `policy` is not `all_accept` (the first handler accepted and the chain stopped).
**Fix:** Check `[auth]` against "Configuration highlights" in "IPv4 address pool: ip-pool".

**Symptom:** An address never goes back to `free`.
**Cause:** The NAS sent no Stop and no Interim-Update, so it waits until the entry expires (3 Interim intervals, 900 seconds). Or no request is coming in, and the reclaim is triggered by requests.
**Fix:** Use the leak query in "Operations: look up addresses, add and remove addresses" to see which rows have expired. After a request comes in, the next 60-second period releases them.

**Symptom:** The same address was given to two subscribers.
**Cause:** This should not happen. Common causes are two pools that deliberately share the same addresses (for example different VRFs) but are sent into the same routing table, the primary key `(pool, ip)` of `ip_pool` having been changed, or a fixed address inside a pool (see "Fixed addresses and prefixes").
**Fix:** Check the pool ranges and the primary key. Use the query in "What the unique indexes stop and what they do not" to find fixed addresses that collide.

**Symptom:** A subscriber's simultaneous connections exceed the limit of the plan, or connection slots stay `reserved`.
**Cause:** The NAS does not send Accounting-Start to this server, or the `[lua]` script of this server is not the `accounting.lua` of usage-control, so nobody confirms the slots.
**Fix:** Look at the `state` in `session_slots`. Make sure accounting is sent here and the `script` in `[lua]` is `accounting.lua`.

**Symptom:** You changed `[queries.<name>]` and reloaded, and the metric did not change.
**Cause:** Named queries take effect only after a restart.
**Fix:** Restart the server.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [13 Accounting and sessions](13-accounting.md)
- [15 Scenarios: network access for organizations](15-access-recipes.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix D: Lua API reference](93-lua-reference.md)
