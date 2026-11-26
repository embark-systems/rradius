# 03 First start and first login

> After this chapter you can:
> - Start rradius in ten minutes with the quick-start scenario
> - Send a login with `rradius-bench` and get your first Access-Accept
> - Add an account with `user add` and log in with it right away
> - Send an accounting request and see it written to the table with `session list`
> - Check the server with `status`, then stop it cleanly

Prerequisites: build `rradiusd` and `rradius-bench` as described in "Build from source" in [chapter 02](02-install.md). Everything in this chapter runs on one machine. You do not need a real NAS.

In this chapter:

- [Prepare a working directory](#prepare-a-working-directory)
- [Read the quick-start files](#read-the-quick-start-files)
- [Replace the example secrets](#replace-the-example-secrets)
- [Make the user commands work](#make-the-user-commands-work)
- [Check the configuration](#check-the-configuration)
- [Start the server](#start-the-server)
- [First login](#first-login)
- [Add an account](#add-an-account)
- [Send an accounting request](#send-an-accounting-request)
- [Check server status](#check-server-status)
- [Stop the server](#stop-the-server)
- [Another starting point: config init](#another-starting-point-config-init)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Prepare a working directory

quick-start is a single-node scenario. Accounts live in a table in the cluster database, the sql handler checks passwords, and a Lua script writes accounting to a table. It is in the source tree at `rradius-server/examples/scenarios/quick-start/`.

Do not run it inside the source tree: the server creates a `data/` directory next to the configuration file, and the steps below edit the configuration file. Copy it to a working directory first. Run these commands from the root of the source repository:

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/quick-start ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/quick-start/rradius.toml
$ cd ~/rradius-lab/quick-start
```

- The first line makes the `rradiusd` and `rradius-bench` commands in this chapter available directly.
- The scenario's `dictionary = "../../../../share/dictionary"` is relative to its place in the repository. After you copy it, that relative path no longer works, so `sed` changes it to the absolute path of the dictionary.
- If you installed from a deb or rpm package, the example is in `/usr/share/doc/rradius/examples/scenarios/quick-start/` and the dictionary is `/usr/share/rradius/dictionary`. The packages do not include `rradius-bench`, so for the login tests you still need the one built from source.

All commands from here on run in `~/rradius-lab/quick-start`.

## Read the quick-start files

```console
$ ls -1
accounting.lua
api.secret
localhost.secret
migrations
raft.secret
rradius.toml
rradius.toml.bak
```

| File | Contents |
| --- | --- |
| `rradius.toml` | The configuration file (`rradius.toml.bak` is the original that `sed` left behind) |
| `accounting.lua` | The accounting script: one row per session; Start inserts, Interim-Update and Stop update |
| `migrations/1_schema.sql` | Creates the `users`, `user_reply` and `accounting` tables |
| `migrations/2_demo_users.sql` | Demo accounts and reply attributes |
| `raft.secret`, `api.secret` | The cluster's Raft secret and API secret |
| `localhost.secret` | The shared secret of the client `localhost` |

The contents of the configuration file (comments removed):

```toml
# rradius.toml
dictionary = "…/share/dictionary"

[lua]
script = "accounting.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[radius]
bind = "127.0.0.1"
status_server = true

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"

[metrics]

[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
show = "SELECT username, nas, session_id, started, updated FROM accounting WHERE username = ?1 AND stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
```

Section by section (for defaults and whether each setting can be reloaded, see the full list in [Appendix A](90-config-reference.md)):

| Setting | Value here | Default | reload | Meaning |
| --- | --- | --- | --- | --- |
| `dictionary` | Dictionary path | The default dictionary path | Restart needed | Where attribute names come from (chapter 06) |
| `lua.script` | `accounting.lua` | Required | Yes | The top-level script. This scenario has `[auth]`, so the script handles only accounting |
| `cluster.data_dir` | `data` | Required | Restart needed | Directory of the cluster database, relative to the configuration file |
| `cluster.secret_raft_file`, `secret_api_file` | File names | None | Restart needed | Which files hold the cluster secrets |
| `cluster.migrations_dir` | `migrations` | None | Restart needed | SQL files applied in order at startup |
| `auth.policy` | `first_match` | `first_match` | Yes | How the auth chain decides (chapter 07) |
| `auth.handlers` | One sql handler | None | Yes | `user_query` looks up the password, `reply_query` looks up the reply attributes |
| `radius.bind` | `127.0.0.1` | `0.0.0.0` | Restart needed | The address RADIUS listens on |
| `radius.auth_port`, `acct_port` | Not set | `1812`, `1813` | Restart needed | Authentication port, accounting port |
| `radius.status_server` | `true` | `true` | Restart needed | Answer Status-Server |
| `radius.clients` | `127.0.0.1` | None | Yes | The clients allowed to send requests (chapter 05) |
| `metrics.bind`, `port` | Not set (an empty `[metrics]` table turns the port on) | `127.0.0.1`, `9812` | Restart needed | The metrics port, which `status` reads |
| `sessions.list`, `show` | Two SQL queries | None | Only the commands read it, no reload needed | The queries that `session list` and `session show` run (chapter 13) |

The demo accounts are in `migrations/2_demo_users.sql`:

| Account | Password | State | Reply attributes |
| --- | --- | --- | --- |
| `alice` | `alice-password` (stored in clear text) | Enabled | `Session-Timeout = 3600`, `Filter-Id = staff` |
| `bob` | `bob-password` (stored as a bcrypt hash) | Enabled | `Reply-Message = Welcome, bob` |
| `carol` | `carol-password` | Disabled (`enabled = 0`) | None |

`carol` is disabled. `user_query` has `AND enabled = 1`, so it finds no row for her, which is the same as an account that does not exist.

> **Warning:** The accounts and passwords in `2_demo_users.sql` are public demo values. If you turn this scenario into a production setup, delete this file and use your own data.

## Replace the example secrets

The three `.secret` files that come with the scenario are public demo values, and their permissions are too open. See what `check` says first:

```console
$ rradiusd check --config rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: 'raft.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
… WARN rradius_core::config: cluster.secret_raft is the public value shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
…
… WARN rradius_core::reload: RADIUS client 'localhost' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32`
configuration ok
```

These are only warnings. The configuration itself is fine. Replace the secrets as the warnings say:

```console
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

## Make the user commands work

Commands such as `user add` need to know which SQL to run against the tables. That SQL is written in `[accounts]`. quick-start does not have this section, so running `user add` now gives `accounts.add is not set in rradius.toml`.

The quick-start configuration also has no `include`, so you cannot put this in `conf.d/`. Add it directly at the end of `rradius.toml`:

```toml
# rradius.toml (add at the end of the file)
[accounts]
scheme = "bcrypt"
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

- `?1` is the account name. `?2` is the password value to store in the table.
- `scheme` is how `user add` and `user passwd` store passwords. The default is already `bcrypt`; it is written out here only so you can see it. For the other choices and their trade-offs, see [chapter 07](07-users-and-chains.md).
- Do not put the password column in `show`.

Only the `user` commands read `[accounts]`. They read it once each time you run a command, so you do not need to reload the server after changing it.

## Check the configuration

```console
$ rradiusd check --config rradius.toml
configuration ok
```

`check` reads the configuration file, the dictionary, the scripts and the secret files, then exits. It opens no ports and creates no data directory. On success the exit code is 0. On an error it prints the reason and the exit code is 2.

## Start the server

Open a terminal and start the server in the foreground:

```console
$ rradiusd serve --config rradius.toml
… WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
… WARN hiqlite::store::state_machine::sqlite::writer: No metadata exists inside the DB yet
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::radius: RADIUS Server listening on 127.0.0.1:1812 and 127.0.0.1:1813
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

The order of the last two lines can differ from run to run. During startup this happened:

1. The server created the `data/` directory and started cluster node 1. On the first start the database is empty, so hiqlite prints two `No metadata exists inside the DB yet` lines. This is normal.
2. The server applied the SQL files in `migrations/` in file name order, which created the tables and inserted the demo accounts. On later starts, files that were already applied do not run again.
3. The server opened the metrics port and the two RADIUS ports. Once you see `Servers started`, you can send requests.

Leave the server running in this terminal. **Open another terminal**, run `cd ~/rradius-lab/quick-start`, and do the next steps there. The new terminal needs `PATH` set again. You are not in the repository now, so use the absolute path of the repository: `export PATH="<path to the repo>/target/release:$PATH"`.

Check that the server is ready:

```console
$ curl -s http://127.0.0.1:9812/readyz
ok
```

`/readyz` answering `ok` (HTTP 200) means the server can handle requests. `/livez` also answers `ok`. For the difference between them, see [chapter 17](17-monitoring.md).

## First login

Use `rradius-bench` as the NAS and send an Access-Request with PAP:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
measured 0.001 s after 0.000 s of warmup

pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 13%
  pap: 1092/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 802.8, p90 802.8, p99 802.8, p99.9 802.8, max 802.8
```

Look at `accept 1`: that is your first Access-Accept.

The command's options:

- `--server`, `--secret`: the server address and the shared secret. The shared secret shows up in the process list, so use it only in test environments.
- `--user`, `--password`: the account and password to log in with.
- `--load pap,concurrency=1`: send PAP logins with only one request in flight at a time. `--load` must always have `concurrency=` or `rate=`.
- `--requests 1`: stop after sending one.

`rradius-bench` is a load-testing tool (chapter 19), so it prints rates and latencies. When you use it as a login test, look only at `accept`, `reject` and `timeouts`. You can also use its exit code directly:

| Exit code | Meaning |
| --- | --- |
| 0 | All requests were accepted (or all accounting requests got a response) |
| 1 | There was a Reject or a Challenge |
| 2 | There was a timeout, an invalid response or a socket error |
| 3 | A bad argument; no request was sent |

Try a few more cases:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password wrong --load pap,concurrency=1 --requests 1
…
  pap: 3401/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user carol --password carol-password --load pap,concurrency=1 --requests 1
…
  pap: 4508/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 20/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

- Wrong password: `reject 1`.
- `carol` is disabled: `reject 1`.
- `bob` is stored as a bcrypt hash, which takes longer to compare, so the rate is much lower. The result is still `accept 1`.

Back in the terminal that runs the server, every login has an authentication record line:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="accept"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="alice" account="alice" chain="default" handler="sql" decision="reject"
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="carol" account="carol" chain="default" handler="none" decision="reject"
```

- `client` is the name of the client that matched. `chain="default"` is the auth chain from `[auth]`.
- `handler="sql"` means the sql handler made the decision. `carol` was not found, so no handler gave an answer and the record says `handler="none"`.

> **Note:** `rradius-bench` does not print reply attributes. To confirm that `alice` got `Session-Timeout` and `Filter-Id`, use `[log.trace]` to record the full request and response (chapter 17).

## Add an account

`user add` reads the password from standard input. When you run it directly in a terminal, it turns off echo and asks twice. When you feed it through a pipe, it reads only the first line:

```console
$ printf 'dave-password\n' | rradiusd user add --config rradius.toml dave
added dave
$ rradiusd user show --config rradius.toml dave
username=dave
enabled=1
```

The account can log in right away. You do not need to reload the server:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load pap,concurrency=1 --requests 1
…
  pap: 25/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

Disable it, then enable it again:

```console
$ rradiusd user disable --config rradius.toml dave
disabled dave
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load pap,concurrency=1 --requests 1
…
  pap: 4187/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd user enable --config rradius.toml dave
enabled dave
```

The `user` commands write to the tables through the cluster's API port, so the server must be running. Exit codes: 0 is success; 1 is no matching account or a failed SQL statement; 2 is an argument or configuration error; 3 is that the cluster cannot be reached. The full description of the `user` commands is in [chapter 07](07-users-and-chains.md).

## Send an accounting request

Use `acct-start` to send an Accounting-Request (Start):

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1
measured 0.001 s after 0.000 s of warmup

acct-start (fixed-concurrency): concurrency 1, 1 threads, 1 sockets, busiest worker 3%
  acct-start: 1598/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 610.8, p90 610.8, p99 610.8, p99.9 610.8, max 610.8
```

`accounting 1` means an Accounting-Response came back: `accounting.lua` wrote the request to the `accounting` table and returned `accept`, and only then did the server respond.

Use `session list` to see the online sessions:

```console
$ rradiusd session list --config rradius.toml
username=alice
nas=192.0.2.1
session_id=2110000000000000
started=1795365534
updated=1795365534
```

- `nas` is the `NAS-IP-Address` that the NAS reported in the request. `rradius-bench` uses `192.0.2.1` by default.
- `session_id` is the Acct-Session-Id that `rradius-bench` generated. Every run of `rradius-bench run` generates a new session ID.
- `started` and `updated` are Unix times.
- To see only one user: `session show --config rradius.toml alice`. For JSON output, add `--json`.

The SQL in `[sessions]` decides what "online" means. This scenario counts a session as online if it has no Stop yet and was updated within the last 15 minutes. So this Start disappears from the list by itself after 15 minutes, even if no Stop arrives.

To see the whole life of a session, use the `session` load. It sends a login, a Start and a Stop in order, all with the same session ID:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user dave --password dave-password --load session,concurrency=1 --requests 1
…
  pap: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
  acct-start: 26/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
  acct-stop: 26/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd session show --config rradius.toml dave
no sessions for dave
```

After the Stop, the session is no longer in the list, and the exit code of `session show` is 1.

> **Note:** If you run `acct-start` and `acct-stop` as two separate runs, the Stop carries a different session ID and cannot end the session that the earlier Start created. To test Start to Stop, use the `session` load.

How to design the accounting tables, how to write the script, and the accounting spool used when the database fails are all in [chapter 13](13-accounting.md).

## Check server status

`status` reads the metrics port and summarizes it on one page:

```console
$ rradiusd status --config rradius.toml
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
  Has a leader              1
  Caught up                 1
  Applied index             11
  Seconds without a leader  0
  Version                   0.1.0
  Seconds running           12

…

RADIUS
  Auth requests              0.0/s (7)
  Acct requests              0.0/s (3)
  Access-Accept              0.0/s (4)
  Access-Reject              0.0/s (3)
  Access-Challenge           0.0/s (0)
  Accounting-Response        0.0/s (3)
  Duplicates                 0.0/s (0)
  Discarded: overload        0.0/s (0)
  Discarded: unknown client  0.0/s (0)
  Discarded: invalid         0.0/s (0)
  Discarded: database        0.0/s (0)
  Discarded: no leader       0.0/s (0)
…
```

- In the `Node` section, `Ready ok` means the server is ready. On a single node, `Leader` is the node itself (node 1).
- In the `RADIUS` section, the number in parentheses is the total since startup, and the number before it is the rate over the last second.
- The lines that start with `Discarded` count requests that got no response. When you troubleshoot, look here first. For example, if the shared secrets do not match, `Discarded: invalid` goes up.

`status` also has `--watch` (keep updating) and `--json`. For the meaning of every line, see [chapter 17](17-monitoring.md).

## Stop the server

In the terminal that runs the server, press Ctrl+C, or send SIGTERM from another terminal. The server's log:

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 0 ms more, then those being handled are waited for, for at most 5000 ms
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
… WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

The stop sequence: `/readyz` changes to 503 first; the server stops taking new requests; it waits for the requests being handled to finish, for at most 5000 milliseconds; then it closes the database. The waiting times are set by `[shutdown]`; see [chapter 04](04-configuration.md). If you press Ctrl+C again during the stop, the server does not wait for the requests being handled.

> **Note:** Let the server close the database itself before it exits. A node that you force to exit with `kill -9` may have to rebuild its local database at the next start.

Start it again and the data is still there: `migrations/` is not applied again, and `dave` can still log in.

To start over from scratch, stop the server and delete `data/`:

```console
$ rm -rf data
```

At the next start, the database is created again, the migrations are applied again, and the accounts go back to the three demo accounts.

## Another starting point: config init

quick-start is a ready-made scenario. If you want to start from a blank file, use `config init` to generate a starting configuration:

```console
$ mkdir ~/rradius-lab/blank && cd ~/rradius-lab/blank
$ rradiusd config init > rradius.toml
```

What this starting configuration contains:

- The cluster secret and the shared secret of one example client (`127.0.0.1`), both generated randomly.
- `dictionary` is the default dictionary location of this version (for a package install, `/usr/share/rradius/dictionary`). If the dictionary is somewhere else, change it.
- `include = ["conf.d/*.toml"]`: the `.toml` files in the `conf.d/` directory next to it are read as well.
- About 30 lines in total, with no `[[cluster.nodes]]` (a single node). For every section with explanations and commented-out settings that show the defaults, use `rradiusd config init --full`.

It has no migrations, no `[auth]` and no `[accounts]`, and `[lua]` points to an `auth.lua` that does not exist yet. So running `check` on it fails right away. First:

1. Create `auth.lua`.
2. If the dictionary is not at the default location, change `dictionary` to the real location.

To grow this starting configuration into a full one, see [chapter 04](04-configuration.md) and [chapter 07](07-users-and-chains.md).

## Common mistakes

**Symptom:** `rradius-bench` prints `rradius-bench: --load pap: needs concurrency or rate`, exit code 3.
**Cause:** `--load` has only the kind.
**Fix:** Write `--load pap,concurrency=1`.

**Symptom:** The `rradius-bench` result is `timeouts 1`, and the server log has no authentication record.
**Cause:** The server dropped the request. Most often `--secret` does not match the client's shared secret. It can also be that the server is not running, or the address or port is wrong.
**Fix:** Make sure `--secret "$(cat localhost.secret)"` reads the same file. When the shared secrets do not match, `Discarded: invalid` in `status` goes up.

**Symptom:** `user add` prints `accounts.add is not set in rradius.toml`, exit code 2.
**Cause:** The configuration file has no `[accounts]`, or `[accounts]` has no `add`.
**Fix:** Add `[accounts]` as described in [Make the user commands work](#make-the-user-commands-work).

**Symptom:** `user add` prints `Connect: Error code 1555: constraint failed Some("UNIQUE constraint failed: users.username")`, exit code 1.
**Cause:** The account already exists.
**Fix:** To change the password, use `user passwd`.

**Symptom:** `user show` prints `no account nobody`, exit code 1.
**Cause:** There is no such account.
**Fix:** Check the name. For `session show`, the matching message is `no sessions for <name>`.

**Symptom:** `user add` hangs for 30 seconds and finally prints `the cluster did not answer within 30 s`, exit code 3. While it waits, there are several `Error looking up DB metrics` lines.
**Cause:** The server is not running, so the command cannot reach the cluster's API port.
**Fix:** Start the server first.

**Symptom:** `status` prints `cannot reach the metrics port at 127.0.0.1:9812: Connection refused (os error 61)`, exit code 3.
**Cause:** The server is not running, or the configuration has no `[metrics]`. The error number depends on the operating system.
**Fix:** Start the server first, and make sure `[metrics]` exists.

**Symptom:** `check` prints `cannot read dictionary: No such file or directory (os error 2)`.
**Cause:** You did not change `dictionary` after copying the scenario, so the relative path points to a place that does not exist.
**Fix:** Change it to the absolute path of the dictionary as described in [Prepare a working directory](#prepare-a-working-directory).

**Symptom:** With the `config init` starting configuration, `check` prints `cannot read script 'auth.lua': No such file or directory (os error 2)`.
**Cause:** The script that `[lua]` in the starting configuration points to has not been created.
**Fix:** Create `auth.lua`. The configuration must have `[lua]`; without it you get `the [lua] section is required`.

**Symptom:** `check` prints `at least one of [radius] or [tacacs] must be configured`.
**Cause:** The configuration file has neither `[radius]` nor `[tacacs]`.
**Fix:** Add at least one (chapter 05, chapter 12).

## Related chapters

- [02 Installation](02-install.md)
- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [13 Accounting and sessions](13-accounting.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [19 Performance tuning and load testing](19-performance.md)
- [Appendix A: Configuration reference](90-config-reference.md)
