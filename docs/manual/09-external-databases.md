# 09 External databases and named queries

> After this chapter you can:
> - Connect to an existing PostgreSQL or MySQL database and let the auth chain use the account table in it directly
> - Read and write an external database from a Lua script with `db_query` and `db_execute`
> - Watch the health of an external database with `/readyz`, metrics and `status`
> - Turn a piece of SQL into a Prometheus metric, and run it directly with the `query` command

Prerequisites: you have run the quick-start from [Chapter 03](03-quick-start.md), and you have read the sql handler in [Chapter 07](07-users-and-chains.md). To use an external database from a script, read [Chapter 10](10-lua-policies.md) first.

In this chapter:

- [Cluster database and external databases](#cluster-database-and-external-databases)
- [Set up an external database](#set-up-an-external-database)
- [Let the sql handler use an external database](#let-the-sql-handler-use-an-external-database)
- [Read and write an external database from a script](#read-and-write-an-external-database-from-a-script)
- [When the database is unreachable](#when-the-database-is-unreachable)
- [Monitor health](#monitor-health)
- [Named queries: turn SQL into metrics](#named-queries-turn-sql-into-metrics)
- [After changing the configuration](#after-changing-the-configuration)
- [Common mistakes](#common-mistakes)
- [Accounts in another system: HTTP API](#accounts-in-another-system-http-api)
- [Related chapters](#related-chapters)

## Cluster database and external databases

rradius has a database of its own: the cluster database (the built-in hiqlite, a SQLite that every node holds a copy of and that is replicated with Raft). The quick-start account table and accounting table are in it. An external database is a PostgreSQL or MySQL database that you set up separately and that rradius connects to over the network.

| | Cluster database | External database |
| --- | --- | --- |
| Where | One copy on every node, replicated automatically | On another server; rradius connects with a connection pool |
| Configuration | `[cluster]` | `[databases.<name>]`, and there can be several |
| Creating tables | Migrations in `cluster.migrations_dir` | You create them on the database yourself |
| sql handler | No `database` | `database = "<name>"` |
| Lua | `sql_query`, `sql_execute` | `db_query`, `db_execute` |
| Named queries | Yes | No; `[queries]` only queries the cluster database |
| Read latency | Local read, no network | One network round trip each time |
| Write restrictions | Cannot use functions such as time or random that give a different result on each node | No such restriction |

When to use an external database:

- The accounts are already in another system (for example a membership system, or the account table of an old RADIUS server), and you do not want to move or synchronize them.
- You want to write accounting data to a place that a report or billing system reads directly.

When rradius manages the accounts entirely, use the cluster database: logins read locally and are not affected by the network or by another server. Chapter 07 describes how to move the accounts and password hashes of an old system into the cluster database.

## Set up an external database

Each external database gets one `[databases.<name>]` section. You choose `<name>`; it may only contain ASCII letters, digits and `_`. The sql handler and scripts use this name to refer to the database.

```toml
# rradius.toml
# … (other settings omitted)

[databases.billing]
kind = "postgresql"
host = "db.example.com"
database = "radius"
user = "rradius"
password_file = "billing.secret"
```

Put the password in a file, with permissions that only the account running rradius can read:

```console
$ printf '%s\n' '<database password>' > billing.secret
$ chmod 600 billing.secret
```

The newline at the end of the file is removed. If the file is empty, `check` fails: `databases.billing.password_file: 'billing.secret' is empty`.

The keys (see [Appendix A](90-config-reference.md) for the full description; any change to `[databases]` needs a restart):

| Key | Default | Description |
| --- | --- | --- |
| `kind` | — | `postgresql` or `mysql`; required |
| `host` | — | Host name or address; required |
| `port` | PostgreSQL `5432`, MySQL `3306` | |
| `database` | — | Database name; required |
| `user` | — | Account; required |
| `password`, `password_file` | — | Set at most one; if neither is set, no password is sent. The examples always use `password_file` |
| `tls` | `verify` | `verify` encrypts and verifies the certificate and the host name; `off` does not encrypt |
| `ca_file` | System root certificates | An additional CA (PEM) to trust when `tls = "verify"` |
| `pool_size` | `8` | The maximum number of connections to keep |
| `connect_timeout_ms` | `5000` | Time limit for opening one connection, in milliseconds |
| `required_for_ready` | `false` | Whether `/readyz` answers 503 when the database is unreachable; see [Monitor health](#monitor-health) |

`rradiusd config show` prints the configuration with the defaults filled in, so you can use it to check values you did not write, such as `pool_size`.

### How many connections to plan for

Each node has its own connection pool and connects to the external database on its own. The most connections the database sees is:

```text
pool_size × number of nodes, plus 1 health-check connection per node (only when required_for_ready = true)
```

For example, with three nodes, `pool_size = 8` and `required_for_ready = true`, that is at most 27 connections. Leave this many under the database's connection limit (`max_connections` in PostgreSQL, `max_connections` in MySQL), and leave room for other systems too.

The pool is created at startup and connects the first time it is used, so it does not fill up the moment the server starts; idle connections are kept for reuse. When `pool_size` is too small, requests wait in a queue for a connection at peak times, and if they wait too long, the result is the same as when the database is unreachable (see [When the database is unreachable](#when-the-database-is-unreachable)). Start with the default 8, and tune it by looking at whether `rradius_external_db_connections{state="in_use"}` reaches the limit at peak times.

### The database account for rradius

Give it only the privileges it needs. To only read the account tables, you can create it like this in PostgreSQL:

```sql
-- On the radius database, run as an administrator
CREATE ROLE rradius_ro LOGIN PASSWORD '<database password>';
GRANT SELECT ON radcheck, radreply TO rradius_ro;
```

When a script needs to write an accounting copy, create a separate account that has only `INSERT` and `UPDATE` on that table, and set it up as another `[databases.<name>]`. One account for one purpose means that less can be done if the account leaks.

Tested (PostgreSQL 17): with this account, the sql handler logs users in as usual; a script that runs `DELETE` on `radcheck` gets `permission denied for table radcheck`, and reading a table it was not granted gets `permission denied for table acct_copy`.

### TLS

`tls = "verify"` is the default: the connection is encrypted, the database certificate must be issued by a trusted CA, and the name on the certificate must be the one written in `host`. When the database uses its own CA:

```toml
# rradius.toml (excerpt)
[databases.billing]
# … (kind, host and so on as above)
ca_file = "db-ca.pem"
```

`ca_file` is read by `check` and at startup. If the file does not exist or contains no certificate, it fails right away instead of waiting for the first login.

Tested (PostgreSQL 17, the server certificate is issued by a self-made CA, and the only name on the certificate is `localhost`):

| Setting | Result |
| --- | --- |
| `host = "localhost"`, `ca_file = "db-ca.pem"` | Connects; logins work |
| `host = "127.0.0.1"`, `ca_file = "db-ca.pem"` | Cannot connect: `invalid peer certificate: certificate not valid for name "127.0.0.1"; certificate is only valid for DnsName("localhost")` |
| `host = "localhost"`, no `ca_file` | Cannot connect: `invalid peer certificate: UnknownIssuer` |

A certificate that fails verification is handled like an unreachable database: with `required_for_ready = true`, startup logs `not reachable at startup: …`, and logins get no answer (see [When the database is unreachable](#when-the-database-is-unreachable)).

`tls = "off"` is only suitable when the database is on the same machine (`host` is a loopback address such as `127.0.0.1`). When `host` is not a loopback address, startup and `check` both warn:

```text
… WARN rradius_core::external_db: databases.legacy: tls = "off" sends the password and the data to db.example.com unencrypted
```

> **Warning:** With `tls = "off"`, the database password and the content of every query (including account names and password hashes) travel over the network in clear text.

With MySQL and `tls = "off"`, the database account must use `mysql_native_password`. The `caching_sha2_password` that MySQL 8 gives new accounts by default needs an RSA exchange on an unencrypted connection, which rradius does not include, so the connection fails. Tested (MySQL 8.4, server started with `--mysql-native-password=ON`):

| Authentication method of the account | `tls` | Result |
| --- | --- | --- |
| `mysql_native_password` | `off` | Connects; logins work |
| `caching_sha2_password` | `off` | Cannot connect, logins get no answer: ``error with configuration: RSA auth backend disabled; enable feature `mysql-rsa` (or `rsa` if using sqlx-mysql directly) or use TLS.`` |
| `caching_sha2_password` | `verify` (`ca_file`) | Connects; logins work |
| `mysql_native_password` | `verify` (`ca_file`) | Connects; logins work |

To create an account with `mysql_native_password`: `CREATE USER 'rradius'@'%' IDENTIFIED WITH mysql_native_password BY '<database password>';`. MySQL 8.4 disables this authentication method by default, so the server must be started with `--mysql-native-password=ON` (the tests for this book were started that way). When you can use TLS, `verify` is better than changing the authentication method of the account.

## Let the sql handler use an external database

Add `database` to the sql handler, and its queries go to that external database instead of the cluster database. The example uses the `radcheck` and `radreply` tables of an old system (`rradius-server/examples/scenarios/migrate-users/external-db.toml`):

```toml
# rradius.toml
# … (other settings omitted)

[databases.legacy]
kind = "postgresql"
host = "db.example.com"
database = "radius"
user = "rradius_ro"
password_file = "legacy-db.secret"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
database = "legacy"
user_query = "SELECT password FROM radcheck WHERE username = ?"
reply_query = "SELECT attribute, value FROM radreply WHERE username = ?"
```

- Always write `?` for values in SQL. When sending to PostgreSQL, rradius rewrites them in order to `$1`, `$2` …; when sending to MySQL, they are sent unchanged. Forms such as `?1` and `:name` are not accepted.
- The format of the password column is the same as in the account table of the cluster database (prefixes such as `{crypt}` and `{clear}`; see Chapter 07).
- You only read, so give rradius a read-only account that can read these two tables.
- If the plaintext passwords of the old system need the `{clear}` prefix added, concatenate it in SQL: in PostgreSQL use `SELECT '{clear}' || value FROM radcheck WHERE username = ?`, in MySQL use `SELECT CONCAT('{clear}', value) FROM radcheck WHERE username = ?` (in MySQL `||` is a logical OR by default). For when you need this, see [Chapter 07](07-users-and-chains.md#plaintext-without-a-prefix-mysql-and-concat). These two statements were not run again on a real database.
- Limits on placeholders: rradius scans the SQL of the sql handler for `?`, so PostgreSQL `$$ … $$` strings and the `$1` form are rejected (`only plain ? placeholders are supported, found $$…`, `sql_placeholders` in `rradius-core/src/lua_engine.rs`), and the jsonb `?` operator is treated as a placeholder. To use them, rewrite the SQL (for example with the `jsonb_exists(…)` function).
- If the account table of the old system is still maintained by the old system, you only read it here; to write accounting back to the old system, use `db_execute` in a script (see the next section).
- If `database` names something that is not in `[databases]`, `check` fails: `auth.handlers[0].database: there is no [databases.nosuch]`.

Tested (PostgreSQL 17 and MySQL 8.4, with tables identical to the quick-start accounts): `bob` (bcrypt) and `alice` (plaintext) get `accept 1` with the correct password, and a wrong password gets `reject 1`.

## Read and write an external database from a script

A script uses an external database through two functions. The first argument is the name of the `[databases.<name>]` section:

| Function | What it does | Returns |
| --- | --- | --- |
| `db_query(name, sql, params)` | Read | An array with one table per row, with the column names as keys |
| `db_execute(name, sql, params)` | Write | The number of rows affected |

The parameter rules are the same as for `sql_query`: always pass values with `?` and a parameter array, and never build them into the SQL string. The error messages of each function are in [Appendix D](93-lua-reference.md).

Example: write a copy of the accounting data to the PostgreSQL of a billing system. In the quick-start `accounting.lua`, add this before the final `return 'accept'`:

```lua
-- accounting.lua (excerpt, placed before the final return 'accept')
db_execute('billing', [[
  INSERT INTO acct_copy (nas, session_id, username, updated, session_time)
  VALUES (?, ?, ?, ?, ?)
  ON CONFLICT (nas, session_id) DO UPDATE SET
    updated = GREATEST(acct_copy.updated, excluded.updated),
    session_time = GREATEST(acct_copy.session_time, excluded.session_time)
]], {nas, session, get_attr('User-Name') or '', seen, seconds})
```

`nas`, `session`, `seen` and `seconds` are variables that the quick-start script has already computed earlier. Note two things:

- This write must be safe to run more than once. When the external database is unreachable, the script fails, and afterwards, whether the NAS retransmits or the request is replayed from the accounting spool (a temporary store on disk where requests wait to be sent again), the script runs again from the start, and the record already written to the cluster database is written again. So use an "update if it already exists" form on both sides.
- An external database does not have the cluster replication problem, so functions such as `now()` can be used; but it is still better to use the values the script computed, so the data on both sides agree.

On the PostgreSQL side, create the table first:

```sql
-- Run on the billing database
CREATE TABLE acct_copy (
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT NOT NULL,
  updated BIGINT NOT NULL,
  session_time BIGINT NOT NULL,
  PRIMARY KEY (nas, session_id)
);
```

### Value types

| Database column | What Lua gets |
| --- | --- |
| Integer, floating point | A number |
| Boolean | `1` or `0` |
| Text | A string |
| Binary | A string of bytes |
| PostgreSQL `NUMERIC`, MySQL `DECIMAL` | A decimal string, not rounded (the result of `SUM` and `AVG` is often of this kind) |
| NULL | `nil` |

A type that cannot be read gives an error and a hint to convert it to text in the SQL. Tested: `DATE` columns in both PostgreSQL and MySQL cannot be read: `db_query: database billing: column created has a type that cannot be read; CAST it to text in the SQL`; after changing it to `CAST(created AS TEXT) AS created`, you get `'2026-11-23'`. A MySQL `BIGINT UNSIGNED` that holds a value larger than a signed 64-bit integer also gives an error: `column u_big holds 18446744073709551615, more than a script number can hold; CAST it to text in the SQL`.

Other tested mappings: a PostgreSQL `NUMERIC(8,2)` column with `12.50` gives the string `'12.50'`, and `SUM` also gives `'12.50'`; a `BOOLEAN` `true` gives `1`; `BYTEA` gives a byte string; a MySQL `DECIMAL(8,2)` gives `'12.50'`, and `TINYINT(1)` gives `1`.

> **Note:** Script numbers are double-precision floating point, so integers above 2^53 (9007199254740992) lose precision. Tested: a PostgreSQL `BIGINT` of `9007199254740993` gives `9007199254740992`. Always `CAST` IDs this large to text before reading them.

### Time limit

All database calls in one request share `lua.query_timeout_ms` (default 5000 milliseconds): `sql_query`, `sql_execute`, `db_query` and `db_execute` count together. When an accounting script waits for an external database, it gives up its place in accounting, so it does not block other accounting requests.

Tested (PostgreSQL 17): `db_query('billing', 'SELECT username FROM radcheck WHERE username = ? OR username = ?', {'alice', 'bob'})` returns two rows, which shows that the two `?` were rewritten in order to `$1` and `$2`; a `'?'` inside an SQL string literal is not treated as a parameter. With the accounting excerpt above writing to `acct_copy`: writing `updated = 100` for a session, then an older `90`, still leaves `100`; writing `200` then updates it to `200`, so a replay does not make the data go backwards.

## When the database is unreachable

When an external database is unreachable, rradius starts as usual, and the connection pool only connects the first time it is used. You can try this out in the quick-start without a real database, and it is worth looking at once before going to production.

### Try it in the quick-start

Goal: make the sql handler use an unreachable external database, and see how logins, `/readyz` and the log react.

Setup: in the quick-start `rradius.toml`, add one line `database = "billing"` to the sql handler, and add at the end a database that points to a port nothing listens on (15432 is used here on purpose):

```toml
# rradius.toml (add one line to the sql handler)
[[auth.handlers]]
type = "sql"
database = "billing"
# … (user_query and reply_query unchanged)
```

```toml
# rradius.toml (add at the end)
[databases.billing]
kind = "postgresql"
host = "127.0.0.1"
port = 15432
database = "radius"
user = "rradius"
password_file = "billing.secret"
tls = "off"
required_for_ready = true
```

Any value will do in `billing.secret` (`printf 'unused\n' > billing.secret`). Start the server. The log has:

```text
… WARN rradius_core::external_db: databases.billing: not reachable at startup: error communicating with database: Connection refused (os error 61); /readyz answers 503 until it is
```

This line only appears for a database with `required_for_ready = true`; `host` is a loopback address, so there is no warning about `tls = "off"`.

Check: log in once as `bob`, then look at `/readyz`:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/readyz
database billing unreachable 503
```

Result: the login gets no answer (`timeouts 1`); it is not rejected. After about 5 seconds (the default `connect_timeout_ms`; about 1 second when set to `1000`) the log says:

```text
… ERROR rradius_core::auth_chain: auth sql handler error: database billing: pool timed out while waiting for an open connection
… WARN rradius_core::lua_engine: RADIUS Access-Request from client localhost not answered: the authentication backend could not be used
```

An accounting Start sent at the same time is answered as usual, because the quick-start accounting script writes to the cluster database and does not go through `billing`. When you have finished, remove these two pieces.

### Who is affected

A login that uses the unreachable external database:

| Who uses it | Result |
| --- | --- |
| sql handler | No answer; the log says `auth sql handler error: database billing: pool timed out while waiting for an open connection` and `RADIUS Access-Request from client localhost not answered: the authentication backend could not be used` |
| `db_query` in a `lua` handler or the top-level script | No answer. Even when the script catches the error with `pcall` and returns `'reject'`, there is still no answer |
| `db_execute` in an accounting script | With an accounting spool (`[radius.accounting_spool]`), the request is first written to the spool and an Accounting-Response is sent, and it is replayed when the database recovers; without a spool, there is no answer and the NAS retransmits |
| TACACS+ | Answers ERROR, so the device asks the next server |

Why no answer instead of a reject: an unreachable database is not the user's fault. A reject would get the user kicked out or locked out; with no answer, the NAS retransmits or asks another RADIUS server. So it is best to configure two or more RADIUS servers on the NAS, and make sure they do not depend on the same external database.

Failed calls are recorded by cause in the `reason` label of the metric `rradius_external_db_failures_total`:

| `reason` | Meaning |
| --- | --- |
| `unavailable` | Cannot connect, the pool cannot get a connection, timeout, or the database says it is busy right now (connection limit reached, deadlock and so on). Trying again later may work |
| `mistake` | The SQL or the parameters are wrong, and the database rejects the statement. Trying again gives the same result |
| `unknown` | Anything else |

Only `unavailable` turns a login into no answer; `mistake` is an error in the script or the configuration, and logins are rejected as usual, so you need to fix the SQL.

## Monitor health

`required_for_ready = true` means "if this database is unreachable, this node should not receive traffic":

```toml
# rradius.toml (excerpt)
[databases.billing]
# … (kind, host and so on as above)
required_for_ready = true
```

Once it is set, every 5 seconds rradius runs `SELECT 1` on a dedicated connection that is not in the pool: two failures in a row count as unreachable, and one success counts as recovered. While it is unreachable, `/readyz` answers 503, so a load balancer or Kubernetes stops sending traffic; `/livez` is not affected, so the program is not restarted because of it. Tested:

```console
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/readyz
database billing unreachable 503
$ curl -s -w ' %{http_code}\n' http://127.0.0.1:9812/livez
ok 200
```

Tested (PostgreSQL 17, the database was stopped and started again while running): about 10 seconds after it was stopped, `/readyz` became 503, and the log says:

```text
… WARN rradius_core::external_db: databases.legacy: unreachable after 2 failed checks, the last: error communicating with database: Connection refused (os error 61); /readyz answers 503
```

About 5 seconds after the database started again, it recovered to 200, and the log says `databases.legacy: reachable`.

> **Note:** Set `required_for_ready` only for a database that every node needs in order to work. For a database used only for a secondary purpose such as an accounting copy, setting it makes the whole cluster stop receiving traffic because of it.

Every external database has these metrics in `/metrics` (tested output):

```text
rradius_external_db_failures_total{database="billing",reason="unavailable"} 1
rradius_external_db_failures_total{database="billing",reason="mistake"} 0
rradius_external_db_failures_total{database="billing",reason="unknown"} 0
rradius_external_db_connections{database="billing",state="in_use"} 0
rradius_external_db_connections{database="billing",state="idle"} 0
rradius_external_db_connections_max{database="billing"} 8
rradius_external_db_reachable{database="billing"} 0
```

| Metric | Meaning |
| --- | --- |
| `rradius_external_db_failures_total` | The number of failed calls, by `reason` |
| `rradius_external_db_connections` | The current connections of the pool: `in_use` is in use or being opened, `idle` is open and not in use |
| `rradius_external_db_connections_max` | Equal to `pool_size` |
| `rradius_external_db_reachable` | 1 if the health check can connect, otherwise 0; only present for a database with `required_for_ready = true` |

If `in_use` stays equal to `max`, there are not enough connections and requests are queuing for a connection: first check whether the queries on the database side are too slow, and then consider raising `pool_size`.

`rradiusd status` has an `External databases` section:

```console
$ rradiusd status
…
External databases
  billing connections in use      0
  billing connections idle        0
  billing connections max         8
  billing failures (unavailable)  0.0/s (0)
  billing failures (mistake)      0.0/s (0)
  billing failures (unknown)      0.0/s (0)
  billing reachable               no
```

You can set alerts like this: `rradius_external_db_reachable == 0` for one minute, or `rate(rradius_external_db_failures_total{reason="unavailable"}[5m]) > 0`. The full list of metrics is in [Appendix C](92-metrics-reference.md).

## Named queries: turn SQL into metrics

A named query is read-only SQL that you write in the configuration. Every node runs it periodically, and the result becomes the Prometheus gauge `rradius_query_<name>`. It suits questions such as "how many accounts are disabled now" or "how much is left in the address pool", without writing a separate exporter.

> **Note:** Named queries only query the cluster database. They cannot query an external database.

Add this at the end of the quick-start `rradius.toml`:

```toml
# rradius.toml (add at the end)
[queries.users_by_state]
sql = "SELECT CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS state, count(*) AS n FROM users GROUP BY state"
help = "Accounts in the users table, by state"
labels = ["state"]
value = "n"
interval_ms = 10000

[queries.open_sessions]
sql = "SELECT count(*) AS value FROM accounting WHERE stopped IS NULL"
help = "Sessions without a Stop"
```

The keys (see Appendix A for the full description; any change to `[queries]` needs a restart):

| Key | Default | Description |
| --- | --- | --- |
| `sql` | — | Required. It must begin with `SELECT` or `WITH`, and cannot have parameters |
| `value` | `value` | The column for the sample value; it must be an integer or a real number |
| `labels` | `[]` | The columns to use as labels; the label name is the column name; the value must be text or an integer |
| `help` | A text that gives the query name | The `# HELP` of the metric |
| `interval_ms` | `30000` | The interval between the starts of two runs, in milliseconds, at least 5000 |
| `timeout_ms` | `5000` | The time limit of one run, in milliseconds, from 1 to `interval_ms` |
| `max_rows` | `200` | The maximum number of rows that become samples, from 1 to 10000 |

A query name begins with a lowercase letter and may only contain lowercase letters, digits and `_`. Names that clash with a command or with another metric, such as `list` and `failures_total`, cannot be used.

After a restart, `/metrics` has these extra lines (tested):

```console
$ curl -s http://127.0.0.1:9812/metrics | grep rradius_query_users_by_state
# HELP rradius_query_users_by_state Accounts in the users table, by state
# TYPE rradius_query_users_by_state gauge
rradius_query_users_by_state{state="disabled"} 1
rradius_query_users_by_state{state="enabled"} 2
```

The quick-start has three demo accounts and one of them is disabled, so the values are 1 and 2.

- Every node runs the query on its own and reports on its own, and each result comes from the local copy of the database. When Prometheus scrapes every node, use `max` or take only one node; do not add them up.
- It runs once right after startup, and then once every `interval_ms`. If a run takes longer than the interval, the next one starts after it ends, so runs never overlap.
- `/metrics` gives the result of the last run, not the current value.

Another example: how many sessions each NAS has now. The number of result rows follows the number of NASes, so set `max_rows` higher than the number of NASes:

```toml
# rradius.toml (add at the end)
[queries.sessions_by_nas]
sql = "SELECT nas, count(*) AS value FROM accounting WHERE stopped IS NULL GROUP BY nas"
help = "Open sessions by NAS"
labels = ["nas"]
max_rows = 500
```

Tested: send one accounting Start with `rradius-bench` (the `NAS-IP-Address` it carries is `192.0.2.1`), and after the next run:

```text
rradius_query_sessions_by_nas{nas="192.0.2.1"} 1
```

Use a label only when its values are a limited set, for example the NAS, the address pool or the account state. Do not use an account name or a session ID as a label: every different value becomes a time series in Prometheus, and a large number of them will overwhelm Prometheus.

### Query with the command

The `query` command runs through the API port of the cluster, so the server must be running.

List all named queries:

```console
$ rradiusd query list
name=open_sessions
help=Sessions without a Stop
interval_ms=30000
sql=SELECT count(*) AS value FROM accounting WHERE stopped IS NULL

name=users_by_state
help=Accounts in the users table, by state
interval_ms=10000
sql=SELECT CASE enabled WHEN 1 THEN 'enabled' ELSE 'disabled' END AS state, count(*) AS n FROM users GROUP BY state
```

Run one:

```console
$ rradiusd query users_by_state
state     n
disabled  1
enabled   2
```

The command runs the SQL again right now. It does not read the result in `/metrics`, and `max_rows` does not limit it. Tested: querying right after sending an accounting Start, `query open_sessions` was already `1`, while `rradius_query_open_sessions` in `/metrics` was still the `0` of the previous run, and it only updates at the next run (30 seconds by default).

Add `--json` for programs to read:

```console
$ rradiusd query open_sessions --json
[{"value":1}]
```

A name that is not in the configuration gives exit code 2:

```console
$ rradiusd query nosuch
queries.nosuch is not set in rradius.toml
```

### Failed queries and dropped rows

Every named query also has three shared metrics:

| Metric | Meaning |
| --- | --- |
| `rradius_query_failures_total{query,reason}` | The number of failures. `timeout` exceeded `timeout_ms`; `error` the database rejected the SQL; `shape` the columns of the result are wrong |
| `rradius_query_dropped_rows_total{query,reason}` | Rows of a successful run that did not become samples. `null_value` the value is NULL; `over_limit` more than `max_rows` |
| `rradius_query_last_success_timestamp_seconds{query}` | The Unix time of the last success; 0 if it has never succeeded |

- On failure, the gauge keeps the result of the last success and is not cleared. So to know whether the numbers are fresh, look at whether `last_success_timestamp_seconds` is moving forward.
- A failure logs one WARN line, at most one line a minute for the same reason, for example `query broken: the run failed (shape): the result has no column "n"`.
- The causes of `shape` include: there is no column named in `value` or `labels`, the value is not a number, a label is NULL, or two rows have exactly the same labels.

Example: forgetting `value`. The value column of the query below is called `n`, but `value = "n"` is not set:

```toml
# rradius.toml (add at the end; written wrongly on purpose)
[queries.users_count]
sql = "SELECT count(*) AS n FROM users"
help = "Accounts in the users table"
```

`check` cannot see this, because the columns of the result are only known when the query runs. Right after startup the log has:

```text
… WARN rradius_core::named_queries: query users_count: the run failed (shape): the result has no column "value"
```

`/metrics` has only `# HELP` and `# TYPE` and no samples; the shared metrics show the failure:

```text
rradius_query_failures_total{query="users_count",reason="shape"} 1
rradius_query_last_success_timestamp_seconds{query="users_count"} 0
```

`status` says `users_count last success (seconds ago)     never`. But `rradiusd query users_count` still prints `n` and `3`, because the command only runs the SQL and does not check the columns. So when the command finds data but the metric does not, look at the log for `shape` first. To fix it, add `value = "n"`, or change the SQL to `count(*) AS value`, and then restart.

The `Queries` section of `rradiusd status` (tested):

```console
$ rradiusd status
…
Queries
  open_sessions last success (seconds ago)   1
  open_sessions rows                         1
  open_sessions failed runs                  0.0/s (0)
  users_by_state last success (seconds ago)  1
  users_by_state rows                        2
  users_by_state failed runs                 0.0/s (0)
```

You can set an alert like this: `time() - rradius_query_last_success_timestamp_seconds > 3 * <interval in seconds>`.

## After changing the configuration

Any change to `[databases]` and `[queries]` needs a restart. When you send `SIGHUP` to reload, the other settings take effect as usual, and the log lists the parts that were not applied:

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: [databases]
```

A change to `[queries]` gives `these need a restart to apply: [queries]`. Check with `rradiusd check` first; most errors in `[databases]` and `[queries]` show up there.

## Common mistakes

**Symptom:** `check` says `databases.billing.kind must be "mysql" or "postgresql", not "oracle"`.
**Cause:** Only PostgreSQL and MySQL are supported.
**Fix:** Change it to `postgresql` or `mysql`.

**Symptom:** `check` says `` `url`: unknown field `url` ``.
**Cause:** An external database does not accept a connection string.
**Fix:** Split it into `host`, `port`, `database`, `user` and `password_file`.

**Symptom:** `check` says `databases.bill-ing: a database name may only have ASCII letters, digits and _`.
**Cause:** The name has a `-`.
**Fix:** Change it to `bill_ing`, and change the name in the sql handler and in scripts too.

**Symptom:** `check` says `databases.billing.password: at most one of password and password_file may be set`.
**Cause:** Both are set.
**Fix:** Keep `password_file`.

**Symptom:** `check` says `databases.billing.tls must be "verify" or "off", not "require"`.
**Cause:** `tls` has only two values.
**Fix:** To encrypt, use `verify` (the default), and point `ca_file` to the CA of the database.

**Symptom:** `check` says `databases.billing.ca_file: cannot read 'db-ca.pem': I/O error: No such file or directory (os error 2)` or `databases.billing.ca_file: no certificate in 'db-ca.pem'`.
**Cause:** The path of `ca_file` is wrong, or the file is not a PEM certificate. A relative path is relative to the directory of the configuration file.
**Fix:** Check the path; first convert a DER certificate to PEM with `openssl x509 -inform der -in ca.der -out db-ca.pem`.

**Symptom:** `check` says `databases.billing.ca_file is only used with tls = "verify"`.
**Cause:** `tls = "off"` does not verify certificates, so `ca_file` is of no use.
**Fix:** Choose one: remove `ca_file`, or remove `tls = "off"`.

**Symptom:** Logins get no answer, and the log has `pool timed out while waiting for an open connection`.
**Cause:** The external database is unreachable, or all the connections of the pool are in use and the wait exceeded the time limit.
**Fix:** Look at `rradius_external_db_connections`: if `idle` and `in_use` are both 0, it cannot connect at all, so check the network, the account and TLS; if `in_use` stays equal to `max`, there are not enough connections or the queries are too slow.

**Symptom:** The log has `invalid peer certificate: UnknownIssuer`.
**Cause:** The database certificate was not issued by a CA the system trusts, for example a self-made CA.
**Fix:** Use `ca_file` to point to the certificate (PEM) of that CA.

**Symptom:** The log has `invalid peer certificate: certificate not valid for name "127.0.0.1"`.
**Cause:** The name written in `host` is not on the database certificate, for example the certificate has only `localhost` while `host` is an address.
**Fix:** Change `host` to a name on the certificate, or issue a new certificate that includes this name.

**Symptom:** MySQL cannot connect, and the log has `RSA auth backend disabled`.
**Cause:** `tls = "off"`, and the database account uses the MySQL 8 default `caching_sha2_password`.
**Fix:** Use `tls = "verify"`; when you cannot use TLS, change the account to `mysql_native_password`.

**Symptom:** A script gives `db_query: there is no database 'billing' in [databases]`.
**Cause:** The name is misspelled, or the section does not exist; this error only appears when the script runs, and `check` cannot catch it.
**Fix:** Compare it with the name of `[databases.<name>]`.

**Symptom:** A script gives `column x has a type that cannot be read; CAST it to text in the SQL`.
**Cause:** The script has no value for the type of the column; dates and times are the common case.
**Fix:** Convert it to text in the SQL, for example `CAST(created AS TEXT) AS created`.

**Symptom:** `check` says `queries.ok.sql: must begin with SELECT or WITH: it only reads`.
**Cause:** Named queries can only read.
**Fix:** Change it to a query that begins with `SELECT` or `WITH`.

**Symptom:** `check` says `queries.ok.interval_ms must be at least 5000, not 1000`.
**Cause:** The interval is too short.
**Fix:** At least 5000. `timeout_ms` also cannot be larger than `interval_ms`.

**Symptom:** `check` says `queries.ok.labels: "value" is also the value column`.
**Cause:** The same column cannot be both the value and a label.
**Fix:** Remove it from `labels`, or set `value` to another column.

**Symptom:** The log has `query broken: the run failed (shape): the result has no column "n"`, and the metric never changes.
**Cause:** The column named in `value` is not in the query result. When `value` is not set, the result must have a column called `value`.
**Fix:** Give the column the same name with `AS` in the SQL, for example `count(*) AS n`.

**Symptom:** `rradius_query_dropped_rows_total{reason="over_limit"}` keeps increasing.
**Cause:** The result has more rows than `max_rows`, and the extra rows are dropped.
**Fix:** Aggregate in the SQL first or add `LIMIT`; when you really need it, raise `max_rows` (up to 10000).

## Accounts in another system: HTTP API

When the accounts are in a membership system (the booking or billing system of a hotel or a coworking space) that you can only ask through an HTTP API, use `http_request` in a `lua` handler. For the full configuration and script, see [the external-api section of Chapter 15](15-access-recipes.md#accounts-in-an-external-system-external-api) and [Chapter 10](10-lua-policies.md). This section adds the points those two chapters do not cover.

**When the API only speaks JSON.** Scripts have no function to encode or decode JSON (`rradius-core/src/lua_engine.rs` does not provide one), so the example lets the API answer with a status code and response headers, and the request is form-encoded. If your API only speaks JSON:

- If you build the request string yourself, you must escape quotes and backslashes in the values yourself, which is easy to get wrong and is not recommended; it is better to ask the API side to add an endpoint that accepts form encoding.
- To get a value out of a JSON response, you can only use Lua string matching (`response.body:match(…)`). That is fine for a fixed, simple format, but unreliable for a complex structure.
- The most dependable way is to put a very thin adapter service in front of the membership system, or to open a dedicated endpoint for RADIUS that answers with only a status code and headers.

**Source address and DNS.**

- **Every node calls the API on its own.** Every node of the cluster sends requests from its own address, so the firewall and the rate limit on the API side must accept the address of every node.
- **`allowed_hosts` compares host names; it does not check the resolved address.** If DNS is tampered with, a request may be sent somewhere else; HTTPS certificate verification is the protection in that case, so do not use `http://` (only on the local machine or an isolated network). `script_http.rs` does not check the address of the resolved result, and only limits the number of lookups in progress at the same time.
- **Redirects are not followed.** When the API answers 3xx, the script gets the 3xx, and the HTTP client in the code is set not to redirect (`redirect::Policy::none()`).
- **Limit the source and the rate on the API side.** rradius forwards to the API the account name and password that anyone types on a login page, so traffic that guesses passwords reaches it too.

**Whether accounting should notify the API.** The accounting in the example only writes to a local table. To notify the membership system when a user goes online or offline, you can also call `http_request` in the accounting script, but:

- The accounting script must return `accept` before the NAS gets a reply. When the API does not answer or the script fails, the NAS retransmits and the script runs again, so the API must tolerate receiving the same notification twice (be idempotent).
- Do not let a failure of the API block accounting. A steadier approach is to write accounting to the table as usual and return `accept`, and let another program send the data from the table to the membership system.

**Cache passes.** If every login asks the API once, the speed of the API is the speed of logins. To reduce the load, the script can record a "pass" in an rradius table for a few minutes (`sql_execute`) and look in the table first afterwards. This is a design idea; neither the examples nor this book implement or test it; do not record a plaintext password in it.

**Common problems.**

| What you see | Cause and what to do |
| --- | --- |
| The log has `attempt to call a nil value`, pointing at the `http_request` line | The configuration has no `[lua.http]`, so there is no `http_request` in scripts |
| Every login is rejected, and the log has `… is not in lua.http.allowed_hosts` | The host or port in the script is not exactly the same as in `allowed_hosts` (when only a host name is written, only HTTPS 443 is allowed) |
| The reason in the log is a timeout | The API is too slow or unreachable; try the same URL with `curl` from the node. `timeout_ms` must be shorter than the NAS retransmit time |
| The reason has to do with a certificate | The API certificate is issued by an internal CA; set `lua.http.ca_file`; server certificates are always verified, and there is no option to turn it off |
| `rradius_script_http_requests_total{result="busy"}` increases | More than `max_concurrent` requests to the same host are waiting at the same time: the API is too slow, or the limit is too low |
| PAP works, Wi-Fi does not | With PEAP or MS-CHAPv2 on Wi-Fi, what arrives is not the plaintext password, so the API has nothing to compare; only EAP-TTLS inner PAP works, and the handler must set `verifies_password = true` |
| Startup fails, saying there is no trusted root certificate | At startup it prints `lua.http: no trusted certificates; this system has none, so set lua.http.ca_file`: this host has no system CA certificates (common in minimal containers); install `ca-certificates` or set `ca_file` |

**Not tested:** any real membership or billing system, and HTTPS certificate verification (the scenario test uses a local fake API without TLS).

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
- [Appendix D: Lua API reference](93-lua-reference.md)
