# 13 Accounting and sessions

> After this chapter you can:
> - Receive accounting requests from a NAS and reply only after the data is in the table
> - Design the accounting table and the accounting script so they survive retransmitted and late packets
> - Turn on the accounting spool so no accounting is lost while the cluster database cannot take writes
> - Write every accounting request to a local JSON log file
> - Query online sessions with the `session` command

Prerequisites: you have done [chapter 03](03-quick-start.md) and have `~/rradius-lab/quick-start`; you can write a simple Lua script ([chapter 10](10-lua-policies.md)). Run the commands in this chapter in `~/rradius-lab/quick-start`, with the server running.

In this chapter:

- [How accounting requests are handled](#how-accounting-requests-are-handled)
- [When the server does not reply](#when-the-server-does-not-reply)
- [Reading the accounting script](#reading-the-accounting-script)
- [Designing the accounting table](#designing-the-accounting-table)
- [Sending accounting to test](#sending-accounting-to-test)
- [What happens when the script fails](#what-happens-when-the-script-fails)
- [Querying online sessions](#querying-online-sessions)
- [The accounting log](#the-accounting-log)
- [The accounting spool](#the-accounting-spool)
- [How a script knows it is a replay: replayed](#how-a-script-knows-it-is-a-replay-replayed)
- [TACACS+ accounting](#tacacs-accounting)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## How accounting requests are handled

A NAS sends an Accounting-Request (RFC 2866) when a session starts, during the session and when it ends. It says who is online, for how long and how much data they moved. When rradius receives one:

1. It matches the source IP against `[[radius.clients]]`, then checks the request's Authenticator with that client's shared secret. If there is no match or the check fails, it drops the request without a reply (chapter 05).
2. Retransmit detection: if the same request arrives while it is still being handled, it is dropped. If it was already answered, a copy arriving within 10 seconds gets the original reply again and the script does not run again (chapter 05).
3. It hands the request to the top-level script (`[lua] script`) with `phase` set to `accounting`. The auth chain (`[auth]`) does not handle accounting.
4. Only when the script returns `accept` does it send an Accounting-Response.

The last point is the core of the whole accounting design: **a reply means "I have recorded it"**. If the NAS gets no reply, it retransmits; the NAS decides how many times and how far apart. So rradius replies only once the data is safely written, and stays silent when it cannot write, so the NAS sends it again.

The accounting script can see every attribute in the request, named as in the dictionary (chapter 06). Compare enumerated attributes by number, for example `Acct-Status-Type`:

| Value | Meaning |
| --- | --- |
| `1` | Start |
| `2` | Stop |
| `3` | Interim-Update |
| `7` | Accounting-On (the NAS boots) |
| `8` | Accounting-Off (the NAS shuts down) |

The script can also see `client` (the client's name), `protocol` (`"radius"`), `phase`, and `replayed`, which is true only when the request is replayed from the accounting spool.

## When the server does not reply

| Situation | Reply | Log |
| --- | --- | --- |
| The script returns `accept` | Accounting-Response | None |
| The script returns `reject` or returns nothing | No reply | `Lua script did not accept accounting (radius, client <name>)` |
| The script fails or exceeds `max_execution_ms` | No reply | `Lua script error (radius/accounting): …` |
| A database call fails (timeout, no leader) and the accounting spool is on | Reply after writing to the spool | The script's error |
| A database call fails and there is no accounting spool | No reply (unless the script catches the error and still returns `accept`) | The script's error |
| The accounting spool is full or cannot be written to disk | No reply | `accounting spool: request from client <name> not written: …` |

A request that gets no reply is not counted in any `rradius_radius_discarded_total`. To see how many accounting requests went unanswered, compare `rradius_radius_packets_total{kind="acct"}` with `rradius_radius_responses_total{code="accounting_response"}`.

## Reading the accounting script

The quick-start `accounting.lua` is an accounting script you can use as it is. It inserts or updates a row in the `accounting` table, one row per session. Here it is piece by piece:

```lua
-- accounting.lua (excerpt)
if phase ~= 'accounting' then
  return 'reject'
end
```

quick-start has `[auth]`, so authentication never reaches this script. If the configuration changes later, this script still does not let anyone through.

```lua
local nas = get_attr('NAS-IP-Address') or get_attr('NAS-IPv6-Address') or get_attr('NAS-Identifier')
if not nas or nas == '' then
  nas = client
end
```

A client can be a whole network, so the script tells NASes apart by the address or name the NAS reports itself. The NAS says these values about itself, but they are only compared within one client. NASes in the same client share one secret, so they are equally trusted anyway.

```lua
local status = get_attr('Acct-Status-Type')
if status == 7 or status == 8 then
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
  return 'accept'
end
if status ~= 1 and status ~= 2 and status ~= 3 then
  return 'accept'
end
```

A NAS that reboots sends Accounting-On or Off, which means all of its earlier sessions are over. Here the script marks them all as ended. Any other type it does not recognize gets `accept` anyway: staying silent would only make the NAS retransmit forever.

```lua
local session = get_attr('Acct-Session-Id')
if not session or session == '' then
  log('accounting without Acct-Session-Id from', client)
  return 'accept'
end
local seen = math.floor(now_unix()) - math.floor(get_attr('Acct-Delay-Time') or 0)
local seconds = math.floor(get_attr('Acct-Session-Time') or 0)
local function octets(name)
  return math.floor(get_attr(name .. '-Octets') or 0) + math.floor(get_attr(name .. '-Gigawords') or 0) * 4294967296
end
```

- A request without Acct-Session-Id cannot be recorded, and a retransmit will not help, so the script writes one log line and returns `accept`.
- `Acct-Delay-Time` is the NAS saying "this happened that many seconds ago". Subtracting it from the current time gives the time the event really happened. When a request is replayed from the accounting spool, rradius adds the time it waited in the spool to this value (see [The accounting spool](#the-accounting-spool)), so the same line of code handles replays too.
- For more than 4 GiB of traffic, the NAS puts the carry in `Acct-Input-Gigawords` and `Acct-Output-Gigawords`.

```lua
sql_execute([[
  INSERT INTO accounting (client, nas, session_id, username, started, updated, session_time, input_octets, output_octets)
  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (client, nas, session_id) DO UPDATE SET
    updated = CASE WHEN stopped IS NULL THEN max(updated, excluded.updated) ELSE updated END,
    session_time = max(session_time, excluded.session_time),
    input_octets = max(input_octets, excluded.input_octets),
    output_octets = max(output_octets, excluded.output_octets)
]], {client, nas, session, get_attr('User-Name') or '', seen - seconds, seen, seconds, octets('Acct-Input'), octets('Acct-Output')})

if status == 2 then
  sql_execute("UPDATE accounting SET stopped = ?, terminate_cause = ? WHERE client = ? AND nas = ? AND session_id = ? AND stopped IS NULL",
    {seen, math.floor(get_attr('Acct-Terminate-Cause') or 0), client, nas, session})
end
return 'accept'
```

This part is the most important design in the script: **writing the same packet twice gives the same result as writing it once**.

- It uses `INSERT … ON CONFLICT … DO UPDATE`: whichever of Start, Interim-Update and Stop arrives first, it inserts or updates the same row.
- Counters use `max()`: an old Interim-Update that arrives late does not make the traffic smaller.
- Packets that arrive after the session ended (a late Interim-Update, a retransmitted Stop) no longer change `updated`.
- The Stop `UPDATE` has `AND stopped IS NULL`: a retransmitted Stop does not overwrite the end time recorded the first time.

Why it has to be written this way: a NAS that gets no reply retransmits; a script that fails halfway runs again on the next retransmit; and replay from the accounting spool is "at least once", so a power cut can run the same request again. In all three cases the same packet reaches the script more than once.

> **Note:** When a script has several `sql_execute` calls and an earlier one succeeds but a later one fails, the NAS retransmits the whole packet and the earlier statement runs again. Every statement must be safe to run again.

## Designing the accounting table

The quick-start accounting table (`migrations/1_schema.sql`):

```sql
-- migrations/1_schema.sql (excerpt)
CREATE TABLE accounting (
  client TEXT NOT NULL,
  nas TEXT NOT NULL,
  session_id TEXT NOT NULL,
  username TEXT,
  started INTEGER NOT NULL,
  updated INTEGER NOT NULL,
  stopped INTEGER,
  session_time INTEGER NOT NULL DEFAULT 0,
  input_octets INTEGER NOT NULL DEFAULT 0,
  output_octets INTEGER NOT NULL DEFAULT 0,
  terminate_cause INTEGER,
  PRIMARY KEY (client, nas, session_id)
);
CREATE INDEX accounting_user ON accounting (username);
```

Key points of the design:

- **The primary key is `(client, nas, session_id)`.** Acct-Session-Id is unique only within one NAS, and two NASes can produce the same ID, so the key has to include which NAS it is.
- **Times are stored as Unix seconds.** `started`, `updated` and `stopped` are integers, which makes comparing and calculating easy.
- **`stopped IS NULL` means still online.** But a NAS can drop off without sending Stop, so "online" also needs a "recently updated" condition; see `[sessions]` in the next section.
- **Add indexes for the queries you run.** quick-start looks rows up by `username`; the usage-control scenario sums one person's traffic over a period, so its index is `(username, started)`.

Once a migration has been applied it cannot change. To change the table, add a file with the next number (chapter 10).

Not every scenario needs an accounting table. In the ip-pool and usage-control scenarios, "who is online" is recorded in the address pool table or the connection-slot table. The accounting script updates that table, and `[sessions]` queries it directly (chapter 14).

## Sending accounting to test

`rradius-bench` has these accounting loads:

| `--load` | Request sent |
| --- | --- |
| `acct-start` | Start |
| `acct-alive` | Interim-Update, with `Acct-Session-Time` and traffic |
| `acct-stop` | Stop, with `Acct-Session-Time` and traffic |
| `acct-on`, `acct-off` | Accounting-On, Accounting-Off |
| `session` | PAP login, Start, Stop, all with the same session ID |

Each `rradius-bench run` generates new session IDs, so an `acct-start` and an `acct-stop` sent separately are two different sessions. To test a complete session, use `session` (chapter 03).

Send two Starts, then an Accounting-On:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1
…
  acct-start: 1997/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --load acct-start,concurrency=1 --requests 1
…
$ rradiusd session list --config rradius.toml
username=bob
nas=192.0.2.1
session_id=1138000000000000
started=1795384997
updated=1795384997

username=alice
nas=192.0.2.1
session_id=dc80000000000000
started=1795384997
updated=1795384997
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-on,concurrency=1 --requests 1
…
  acct-on: 2954/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd session list --config rradius.toml
no sessions
```

The NAS address of `rradius-bench` is always `192.0.2.1`, so the Accounting-On ended every session of that "NAS".

To test `Acct-Delay-Time`, add the attribute with `--attr` (41 is the number of `Acct-Delay-Time`):

```console
$ date +%s
1795385623
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user grace --attr 41=int:600 --load acct-start,concurrency=1 --requests 1
…
$ rradiusd session show --config rradius.toml grace
username=grace
nas=192.0.2.1
session_id=2868000000000000
started=1795385023
updated=1795385023
```

`started` is 600 seconds earlier than now, which is exactly the delay the NAS reported.

## What happens when the script fails

Replace the script with one that fails, to see what the NAS sees. Back it up first, then write a script that always fails on accounting:

```console
$ cp accounting.lua accounting.lua.keep
$ printf "if phase == 'accounting' then error('no table today') end\nreturn 'reject'\n" > accounting.lua
$ kill -HUP <server PID>
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --load acct-start,concurrency=1 --requests 1 --timeout 2
…
  acct-start: 0/s, sent 1, accept 0, reject 0, challenge 0, accounting 0, timeouts 1, invalid 0, late 0, socket errors 0, retransmits 0
```

`timeouts 1`: there was no reply. The server log:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/accounting): runtime error: accounting.lua:1: no table today\nstack traceback:…
```

When the script returns `reject` instead, there is also no reply, and the log says:

```text
… WARN rradius_core::lua_engine: Lua script did not accept accounting (radius, client localhost)
```

When you are done, restore the script with `mv accounting.lua.keep accounting.lua` and send `SIGHUP` again. Scripts can be reloaded; if a broken script does not compile, the reload fails and the server keeps using the previous script.

> **Note:** When accounting gets no reply, the NAS keeps retransmitting until it gives up, and some NASes then discard the record. Fix script errors quickly. For short periods when the database cannot take writes, use the accounting spool below.

## Querying online sessions

rradius does not keep "who is online" itself. The `session` command runs the SQL you wrote in `[sessions]` and prints the result as it is, so what "online" means is decided by your table and your SQL.

```toml
# rradius.toml (the quick-start [sessions])
[sessions]
list = "SELECT username, nas, session_id, started, updated FROM accounting WHERE stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
show = "SELECT username, nas, session_id, started, updated FROM accounting WHERE username = ?1 AND stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
```

| Key | Default | reload | Meaning |
| --- | --- | --- | --- |
| `sessions.list` | none | read by the command only | The SQL that `session list` runs, with no parameters |
| `sessions.show` | none | read by the command only | The SQL that `session show <name>` runs; `?1` is the user name |

(See [Appendix A](../manual.zh-TW/90-config-reference.md) for the full description.)

- Both statements may only read: after any whitespace they must start with `SELECT` or `WITH`, in any letter case.
- `show` must use `?1`, and only `?1`.
- Only the command reads this section; the running server does not look at it, so a change needs no reload and no restart. Startup, reload and `check` do validate it, though, so a mistake still fails there.
- quick-start counts a session as online when it has no Stop and was updated in the last 15 minutes. If your NAS sends Interim-Updates less often than every 15 minutes, widen this condition.

How to use the command:

```console
$ rradiusd session show --config rradius.toml --json bob
[{"username":"bob","nas":"192.0.2.1","session_id":"1138000000000000","started":1795384997,"updated":1795384997}]
$ rradiusd session show --config rradius.toml nobody
no sessions for nobody
$ rradiusd session show --config rradius.toml --json nobody
[]
```

- Without `--json`, each row is a block of `field=value` lines in SELECT column order. NULL is an empty value, and rows are separated by a blank line.
- `--json` prints a JSON array: NULL is `null`, integers and reals are numbers, text is a string, and a BLOB is a lower-case hexadecimal string.
- The name given to `show` is normalized by `[auth.username]` of the default chain, the same as the `user` command (chapter 07).
- The command reads through the cluster's API port. It does not start a node and does not create a data directory, but the server must be running. The read is a consistent read: it returns data the cluster has already confirmed.

Exit codes:

| Exit code | Meaning |
| --- | --- |
| 0 | At least one row was printed |
| 1 | No matching row: `list` prints `no sessions`, `show` prints `no sessions for <name>`; with `--json` it prints `[]` |
| 2 | The command or configuration is wrong and nothing ran; or the SQL failed (`the statement failed: …`) |
| 3 | Cannot reach the cluster, or no answer within 30 seconds: `the cluster did not answer within 30 s` |

Messages for a wrong `[sessions]` (exit code 2):

| What you wrote | Message |
| --- | --- |
| `list = "DELETE FROM accounting"` | `rradius.toml: sessions.list: must begin with SELECT or WITH: it only reads` |
| `show = "… WHERE username = ?"` | `rradius.toml: sessions.show: use ?1 for the user name, not ?` |
| `show = "… WHERE username = ?2"` | `rradius.toml: sessions.show: only ?1 (the user name) can be used` |
| No `[sessions]`, or no such key | `sessions.show is not set in rradius.toml` |

The `session` command only queries. To turn the number of people online into a Prometheus metric, use a named query ([chapter 09](09-external-databases.md)).

## The accounting log

`[radius.accounting_log]` makes rradius append one JSON line to a local file for every accounting request it answers. This is separate from the accounting table: the table is data your script writes for you to query, while the log is the raw record the server writes, kept as it was received. It suits collection by a log system or keeping for later checks.

```toml
# rradius.toml (added at the end of the file)
[radius.accounting_log]
file = "log/accounting.log"
```

| Key | Default | reload | Meaning |
| --- | --- | --- | --- |
| `radius.accounting_log.file` | none, required | yes | The file to append to, relative to the configuration file's directory |
| `radius.accounting_log.queue` | `65536` | needs restart | The most records that can wait to be written, 1 to 16777216 |

Adding or removing the whole `[radius.accounting_log]` table needs a restart. (See [Appendix A](../manual.zh-TW/90-config-reference.md) for the full description.)

The directory of the file must exist first, or `check` fails:

```console
$ rradiusd check --config rradius.toml
rradius.toml: radius.accounting_log.file: directory log does not exist
$ mkdir log
$ rradiusd check --config rradius.toml
configuration ok
```

After you restart the server and send one `session`, the log has two more lines (each line is wrapped into two here to make it readable):

```text
{"time":"2026-11-22T22:03:10.581Z","client":"localhost","source":"127.0.0.1","transport":"udp","handling":"script",
 "attributes":{"User-Name":"alice","NAS-IP-Address":"192.0.2.1","NAS-Identifier":"rradius-bench","Acct-Status-Type":"Start","Acct-Session-Id":"98a0000000000000"}}
{"time":"2026-11-22T22:03:10.582Z","client":"localhost","source":"127.0.0.1","transport":"udp","handling":"script",
 "attributes":{"User-Name":"alice","NAS-IP-Address":"192.0.2.1","NAS-Identifier":"rradius-bench","Acct-Status-Type":"Stop","Acct-Session-Id":"98a0000000000000","Acct-Session-Time":"60","Acct-Input-Octets":"20000","Acct-Output-Octets":"30000"}}
```

The fields of each line, in a fixed order:

| Field | Content |
| --- | --- |
| `time` | The time of the reply, RFC 3339, UTC, to the millisecond |
| `client` | The client's name |
| `source` | The source address |
| `transport` | Always `udp` in the core |
| `handling` | `script`: the script accepted it; `spool`: it was written to the accounting spool |
| `attributes` | The request's attributes, keyed by dictionary name. Enumerated values are written as names (`Start`), not numbers. An attribute that appears several times is an array |

When a request carries the RFC 8045 `IP-Port-Range` (an operator's NAS or CGN uses it to report the public address and ports allocated to or released from a subscriber), `attributes` gets one more key at the end, `IP-Port-Range`. It is an array with one object per `IP-Port-Range`, whose keys are the dictionary names of the sub-attributes, in packet order; a single one is still an array. The original flat keys stay as before, and repeated names are still arrays, so existing readers are not affected. The key exists because with several ranges in one packet, flat arrays can only be matched up by position, and a range that lacks one sub-attribute shifts everything after it; the grouping keeps the fields of each range together.

What is written and what is not:

- Only requests that **got a reply** are written. A request the script did not accept, and so did not reply to, is not written.
- A retransmit answered directly by retransmit detection is not written again.
- Only RADIUS accounting. TACACS+ accounting is not written here.
- The server creates the file with mode 0640. Measured: `-rw-r-----`.

**Writing does not slow down replies.** After the reply, the record goes into a queue, and another thread writes it in batches. If `queue` records are already waiting, the new one is dropped and counted. When a write fails, the failure is counted, and the same error is warned about at most once a minute; the server keeps running.

**Log rotation.** On `SIGHUP`, the server finishes writing what is in the queue, closes the file, and opens `file` again (creating it if it does not exist). So to rotate, rename first, then send `SIGHUP`:

```console
$ mv log/accounting.log log/accounting.log.1
$ kill -HUP <server PID>
$ ls -l log
-rw-r-----  1 …   368 … accounting.log
-rw-r-----  1 …  1187 … accounting.log.1
```

Records after the `SIGHUP` go to the newly created `accounting.log`. Records between the rename and the `SIGHUP` still go to the old file (the one that was renamed) and are not lost. If reopening the file fails during a reload, one failure with `stage="open"` is counted and warned about, and the server keeps writing to the original file. If the directory of a new `file` does not exist, the reload fails and the configuration does not change.

With logrotate, send `SIGHUP` in `postrotate` and do not use `copytruncate` (it loses lines). The `/etc/logrotate.d/rradius` shipped in the deb and rpm packages is written this way. It rotates `/var/log/rradius/*.jsonl`, so if you put `file` in that directory with the extension `.jsonl` when you install the package, it is rotated with the rest (chapter 02).

**On shutdown.** On SIGTERM or SIGINT, the server first stops accepting requests and waits for requests in progress to finish, then writes what is left in the queue, waiting at most 5 seconds.

**Metrics and status.** These metrics exist only when `[radius.accounting_log]` is set, and all start at 0:

| Metric | Meaning |
| --- | --- |
| `rradius_accounting_log_written_total` | Records written to the log file |
| `rradius_accounting_log_dropped_total` | Records dropped because the queue was full |
| `rradius_accounting_log_failures_total{stage="open"}` | Failures to open the file |
| `rradius_accounting_log_failures_total{stage="write"}` | Failures to write |

`status` adds a section after the accounting spool section:

```console
$ rradiusd status --config rradius.toml
…
Accounting log
  Written        0.0/s (5)
  Dropped        0.0/s (0)
  Failed opens   0.0/s (0)
  Failed writes  0.0/s (0)
…
```

With `--json`, the key for this section is `acct_log`. If `Dropped` or `Failed` keeps growing, the log is missing records: check disk space and file permissions.

### Reverse lookup of public addresses and ports

When an operator uses CGNAT (many subscribers sharing one public address), the question "who had this public address and port at that moment?" is answered from the log file. `rradiusd accounting find` only reads log files. It does not contact the cluster and needs no configuration file, so you can run it on backed-up files:

```console
$ rradiusd accounting find --ip 198.51.100.7 --port 1500 --at 2026-11-25T10:30:00Z --file accounting.jsonl
198.51.100.7:1024-2047 allocated 2026-11-25T10:00:00.120Z ended 2026-11-25T11:00:00Z ended_by release time_source received identity "Acct-Session-Id=s-1 client=bng1" client=bng1 User-Name=alice Acct-Session-Id=s-1 Framed-IP-Address=100.64.0.5
$ rradiusd accounting find --ip 198.51.100.7 --port 4096 --at 2026-11-25T10:30:00Z --file accounting.jsonl --json
[{"allocated":"2026-11-25T10:00:00.120Z","attributes":{"Acct-Session-Id":"s-1","Framed-IP-Address":"100.64.0.5","User-Name":"alice","client":"bng1"},"ended_by":"release","ext_ip":"198.51.100.7","identity":"Acct-Session-Id=s-1 client=bng1","port_end":4096,"port_start":4096,"released":"2026-11-25T11:00:00Z","time_source":"received"}]
$ rradiusd accounting find --ip 198.51.100.7 --port 1500 --at 2026-11-25T11:30:00Z --file accounting.jsonl
$ echo $?
1
```

- **Times need a time zone**, for example `2026-11-25T10:00:00Z` or `2026-11-25T18:00:00+08:00`. You can give `--file` more than once; if you give none, or give `-`, it reads standard input. A `--file` whose name ends in `.gz` is decompressed automatically (give an `accounting.jsonl.2.gz` rotated by logrotate straight to `--file`; several gzip files joined together also work). Standard input is not decompressed, so use `zcat accounting.jsonl.2.gz | rradiusd accounting find --ip … --at …`. If a compressed file is damaged or truncated, the command exits with code 2 and names the file.
- **Who holds it:** a range whose `IP-Port-Range-Alloc` is `Allocation` starts the hold, and `Deallocation` ends it. A release without start and end ports ends all the earlier allocations of the same holder. A `Stop` with the same client and `Acct-Session-Id` also ends its hold. The RFC does not say this; it is the command's decision, and the output marks it `ended_by` `stop` so you can tell. If there is neither a release nor a Stop, the output says `ended ... still held` (`released` is `null` in JSON).
- **Time basis:** the event time is `Event-Timestamp` first; if there is none, the received time minus `Acct-Delay-Time`; if there is none of that, the received time. `time_source` in the output says which one was used. A NAS retransmit or queueing can make the received time later than the event, so near a boundary, judge by `time_source`.
- **No assumption that there is a user name:** the output lists only the identifying attributes that line has (`User-Name`, `Acct-Session-Id`, `Framed-IP-Address`, `NAS-Identifier`, `NAS-IP-Address`, `client`).
- **Exit codes:** 0 found, 1 not found, 2 a bad argument or file (the message names the file and line number).

**Retention.** The command stores no data. How long log files are kept is decided by the rotation tool (logrotate, newsyslog) and by the enterprise export's `keep_days`. Legal retention periods for operator logs differ from place to place; you decide, and rradius sets no default and deletes no files for you.

> **Not verified on real hardware:** This feature has not been tried with a real CGN or BNG. The test packets were built by hand from the RFC 8045 attribute definitions, so you need to confirm on your own equipment what it really sends (for example whether it has `Ext-IPv4-Addr`, whether a release covers only some of the ports, and whether a `Stop` comes with a separate release).

**Known limits:**

- A release ends the whole entry if it overlaps the held ports at all; it does not split. If a device releases only some of the ports, the remaining ports are treated as released at query time, so the lookup under-reports.
- A range with no `Ext-IPv4-Addr`, no start and end ports, or no identity cannot be matched. The command prints the number of such ranges on standard error.
- A standalone CGN that sends only the inside address, which then has to be matched to the BNG's session, is not supported in this version; IPv6 public addresses are not supported.
- Events are sorted by time and loaded into memory as a whole: memory grows with the number of relevant lines (including `IP-Port-Range` and `Stop`), so pick the files by date before you query.

> **Note:** The log contains personal data such as `User-Name`. Handle the retention period and access rights according to your own rules.

## The accounting spool

A write to the cluster database has to be confirmed by a majority of nodes (chapter 16). While the cluster has lost its majority, or the database is too busy and times out, the accounting script cannot write, so the NAS gets no reply. After a few retransmits the NAS gives up, and those records are lost.

The accounting spool is a local on-disk store of requests waiting to be replayed. With `[radius.accounting_spool]` set, when a database call fails while the script runs because it **cannot connect or times out** (whether or not the script catches the error, and whatever it finally returns), rradius first writes the whole request to disk and sends the Accounting-Response only once it is safely written. Later, in the background, it replays requests in order: it hands each request to the script again, until the script accepts it.

```toml
# rradius.toml (added at the end of the file)
[radius.accounting_spool]
dir = "spool"
```

| Key | Default | reload | Meaning |
| --- | --- | --- | --- |
| `radius.accounting_spool.dir` | none, required | needs restart | The spool directory, relative to the configuration file's directory |
| `radius.accounting_spool.max_bytes` | `1073741824` (1 GiB) | needs restart | The size limit of the spool, in bytes, must be greater than 0 |
| `radius.accounting_spool.replay_interval_ms` | `5000` | needs restart | How long to wait before trying again after a replay fails, in milliseconds, must be greater than 0 |
| `radius.accounting_spool.replay_window` | `1` | needs restart | How many requests to replay at the same time, 1 to 1024 |

A change anywhere in the table needs a restart; on reload, the log says `these need a restart to apply: [radius.accounting_spool]`. (See [Appendix A](../manual.zh-TW/90-config-reference.md) for the full description.)

**What goes into the spool and what does not:**

- Goes in: failures that mean "the database cannot be used right now", such as a database call timeout, no leader, or the write queue refusing the request.
- Does not go in: wrong SQL (a syntax error, a missing table, a wrong number of parameters), a Lua error in the script itself, and a script that returns `reject` without calling the database. Writing these down would not fix them, so they still get no reply.
- When the spool is full (writing more would exceed `max_bytes`) or a disk write fails, there is no reply, `rradius_acct_spool_full_total` is incremented once, and a log line is written. Later requests are still tried as usual.

**Replay rules:**

- It starts from the oldest, takes at most `replay_window` requests at a time, and hands them to the **current** script with `phase` set to `accounting` and `replayed` set to true.
- `Acct-Delay-Time` gets the number of seconds the request waited in the spool added to it; if the request did not have this attribute, one is added.
- If the script accepts and there was no database failure along the way, the replay succeeded and is counted in `rradius_acct_spool_replayed_total`.
- If the database is still unusable, there is a Lua error, the execution time limit is exceeded, or the script caught a database error but still returned `accept`: it waits `replay_interval_ms` and tries the same request again, and everything behind it waits.
- These cases, where trying again will not succeed, are moved to `rejected.log` and counted in `rradius_acct_spool_rejected_total`, and the replay goes on to the next request: the client is no longer in the configuration, what was stored is not a RADIUS packet, the script did not accept without a database failure, or the script ended because of a SQL or parameter error (including a violated unique or foreign key constraint).
- Within a batch, results are settled in write order and stop at the first request that must be retried. The requests after it stay in the spool and run again later, even if the script accepted them.

The default `replay_window = 1` replays strictly one by one in the order received, because a script may expect the Start of a session to come before its Stop. A larger value catches up faster, but the order may interleave, and when a batch stops partway, the requests after the stop run one extra time. If you write the script to be safe to run again, as in [Reading the accounting script](#reading-the-accounting-script), neither order nor repeats cause problems.

Replay is "at least once": if the server is stopped between a replay and recording its progress, after the restart those few requests run again. Progress is written to disk at least every 64 requests and whenever the replay moves to a new file. After a restart, replay continues from the recorded progress.

**Files in the directory:**

| File | Content |
| --- | --- |
| `segment-<20 digits>.log` | Requests waiting to be replayed, stored in order; a file holds up to 16 MiB before the next one starts |
| `position` | How far the replay has got |
| `rejected.log` | Requests that were moved out and will not be replayed again |
| `truncated.log` | If the last record was found unfinished at startup, it is saved here before it is cut off |
| `*.corrupt` | A file with damage in the middle, renamed and left untouched; the replay skips to the next file |

The server creates the directory with mode 0700 and the files with 0600. The files are in a binary format and contain personal data such as `User-Name`; if the disk is lost, the data is lost with it. `check` verifies that `dir` is usable, but it does not create it and does not change any file in it.

**Measured: when the cluster loses its majority.** On one machine, start three nodes (the three configurations of the ha-cluster scenario, each with its own ports, `data_dir` and `dir`; for the setup see chapter 16). Send one accounting request to node 1 to confirm it works, then stop the other two nodes. The commands below leave out node 1's port:

- For the first request after the stop, the script's write waited about 30 seconds before it failed, and then the request was written to the spool and answered. The NAS's timeout is shorter than that, so the NAS retransmits first; the retransmit in progress is dropped, and a retransmit that arrives after the request was written to the spool is answered by retransmit detection.
- Once this node has had no leader for more than 5 seconds, writes fail immediately, and new accounting is written to the spool and answered within a few milliseconds:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat nas.secret)" \
    --user dave --load acct-start,concurrency=1 --requests 1 --timeout 10
…
  acct-start: 166/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradiusd status --config node1.toml
…
Accounting spool
  Bytes waiting  223
  Replayed       0.0/s (0)
  Rejected       0.0/s (0)
  Not spooled    0.0/s (0)
…
```

The log also has the script's error `Lua script error (radius/accounting): runtime error: no leader for 48 s`. That is expected: the script really could not write, but the request is safely on disk.

After you start the other two nodes, the replay finishes within a dozen or so seconds: `Bytes waiting` goes back to 0, `rradius_acct_spool_replayed_total` is 2, the accounting table has both records, and `Acct-Delay-Time` is larger than before by the seconds they waited in the spool.

If you change the script to always `reject` on replay and try again, that request is moved to `rejected.log`:

```text
… WARN rradius_core::lua_engine: Lua script did not accept accounting (radius, client nas-network)
… ERROR rradius_core::lua_engine: accounting spool: request from client nas-network moved to rejected.log: the script did not accept it
```

> **Note:** The seconds added to `Acct-Delay-Time` are counted from the moment the request was **written to the spool**, not from when it was received. For the first request after stopping the nodes above, the script waited about 30 seconds before failing. Those 30 seconds are not counted, so that request's time is about 30 seconds late.

> **Not verified on real hardware:** A full spool, a failed disk write, and the handling of damaged files (`truncated.log`, `*.corrupt`) were checked only against the code and the tests, not measured.

**Metrics and status.** The accounting spool metrics appear whether or not `[radius.accounting_spool]` is set:

| Metric | Meaning |
| --- | --- |
| `rradius_acct_spool_written_total` | Requests written to the spool |
| `rradius_acct_spool_replayed_total` | Requests replayed successfully (the script accepted) |
| `rradius_acct_spool_rejected_total` | Requests moved to `rejected.log` |
| `rradius_acct_spool_full_total` | Requests not written because the spool was full or the disk failed |
| `rradius_acct_spool_corrupt_total` | Damaged files that were renamed and kept |
| `rradius_acct_spool_bytes` | Bytes waiting to be replayed |

In the Accounting spool section of `status`, `Bytes waiting` is `rradius_acct_spool_bytes` and `Not spooled` is `rradius_acct_spool_full_total`. Worth alerting on: `rradius_acct_spool_bytes` that stays above 0 for a long time (it is not catching up), and `full_total` or `rejected_total` going up. For example alert rules, see [chapter 17](17-monitoring.md).

> **Warning:** The spool lives on each node's own disk and is not replicated to other nodes. While a node's spool still has data, do not delete that node's `dir`, and do not throw away its disk after removing the node from the cluster: wait until `Bytes waiting` is back to 0 first.

## How a script knows it is a replay: replayed

When a request is replayed from the spool, the global variable `replayed` is true; otherwise it is false. Most scripts do not need to tell the difference; just write them to be safe to run again. You need it for things that "have side effects and should happen only once, at the time", for example:

- Calling an external HTTP API to notify some system.
- Deciding from an Interim-Update whether to disconnect a user. A replayed Interim-Update is old news from several minutes ago.

```lua
-- accounting.lua (excerpt)
if replayed then
  log('replayed', get_attr('Acct-Session-Id'), 'delay', get_attr('Acct-Delay-Time'))
end
```

During a measured replay, this line printed:

```text
… INFO rradius_core::lua_engine: script accounting.lua (radius/accounting, client nas-network): replayed 57b0000000000000 delay 14
```

## TACACS+ accounting

A TACACS+ accounting request also goes to the top-level script, with `phase` set to `accounting` and `protocol` set to `"tacacs"`, but it differs from RADIUS in three ways:

- When the script does not accept, the reply is `ERROR`, not silence.
- There is no accounting spool.
- It is not written to `[radius.accounting_log]`.

For attributes, table design and examples, see [chapter 12](12-tacacs.md).

## Common mistakes

**Symptom:** The NAS keeps retransmitting accounting requests, `rradius-bench` shows `timeouts 1`, and the server log has `Lua script did not accept accounting` or `Lua script error (radius/accounting)`.
**Cause:** The accounting script did not return `accept`. Common causes are an `if phase ~= 'accounting'` or `protocol` test at the top of the script written the wrong way round, or wrong SQL.
**Fix:** Correct the script as the log says, then send `SIGHUP`.

**Symptom:** `rradius-bench` shows `timeouts 1` and the server log has nothing.
**Cause:** The request was dropped before the script. The most common reasons are a shared secret that does not match (`Discarded: invalid` in `status` goes up) or a source that is not a client in the configuration (`Discarded: unknown client`).
**Fix:** See [chapter 05](05-radius-clients.md).

**Symptom:** The same session has two rows in the accounting table, or its traffic is counted twice.
**Cause:** The script is not safe to run again: it uses a plain `INSERT`, or it adds the traffic up.
**Fix:** Change it to `INSERT … ON CONFLICT … DO UPDATE` and `max()` as in [Reading the accounting script](#reading-the-accounting-script).

**Symptom:** `session list` keeps listing people who went offline long ago.
**Cause:** The NAS did not send Stop (power loss, link loss), and the `[sessions]` SQL looks only at `stopped IS NULL`.
**Fix:** Add a "recently updated" condition, such as the quick-start `updated > strftime('%s', 'now') - 900`, and make the NAS send Interim-Updates.

**Symptom:** `session list` prints `sessions.list is not set in rradius.toml`, exit code 2.
**Cause:** The configuration file has no `[sessions]`, or no `list`.
**Fix:** Add `[sessions]`. No reload is needed after the change.

**Symptom:** `session list` waits 30 seconds, prints `the cluster did not answer within 30 s`, exit code 3.
**Cause:** The server is not running, or the cluster's API port cannot be reached.
**Fix:** Start the server first.

**Symptom:** `check` prints `radius.accounting_log.file: directory log does not exist`.
**Cause:** The directory of the log file does not exist.
**Fix:** Create the directory first, and make sure the account that runs the server can write to it.

**Symptom:** `check` prints `radius.accounting_log.queue must be from 1 to 16777216`.
**Cause:** `queue` is out of range, for example 0.
**Fix:** Change it to a value in range, or delete it to use the default.

**Symptom:** `check` prints `radius.accounting_spool.dir: cannot use 'afile': Not a directory (os error 20)`.
**Cause:** `dir` points to a file, or to a directory that cannot be written to or listed.
**Fix:** Change it to a directory you can write to. If the directory does not exist but its parent can be written to, `check` passes and the server creates it at startup.

**Symptom:** `check` prints `radius.accounting_spool.replay_window must be from 1 to 1024`, `radius.accounting_spool.max_bytes must be greater than 0` or `radius.accounting_spool.replay_interval_ms must be greater than 0`.
**Cause:** A value is out of range.
**Fix:** Change it to a value in range.

**Symptom:** `check` prints ``conf.d/snippet.toml: line 1: missing field `dir` ``, starting with the file and line where `[radius.accounting_spool]` is.
**Cause:** `[radius.accounting_spool]` has no `dir`.
**Fix:** Add `dir`.

**Symptom:** The cluster has recovered, but `rradius_acct_spool_bytes` never goes back to 0, and the same script error appears in the log every 5 seconds.
**Cause:** The request being replayed fails every time for a reason that "may go away next time", such as a Lua error in the script or an execution timeout. Everything behind it is waiting.
**Fix:** Correct the script as the log says and reload; the replay continues from that request.

**Symptom:** `rradius_acct_spool_rejected_total` goes up, and the log has `moved to rejected.log`.
**Cause:** On replay the script rejected the request, or there was a SQL error, or the client has been removed from the configuration.
**Fix:** Fix the cause given in the log. Requests in `rejected.log` are not replayed automatically again.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md): clients, retransmit detection, overload protection
- [06 Dictionary and attributes](06-dictionary.md): attribute names and enumerated values
- [09 External databases and named queries](09-external-databases.md): turning accounting data into metrics
- [10 Writing policies in Lua](10-lua-policies.md): `sql_execute`, `log`, migrations
- [12 TACACS+ device administration](12-tacacs.md): TACACS+ accounting
- [14 Scenarios: broadband and ISP](14-isp-recipes.md): accounting for address pools and traffic control
- [16 Clusters and high availability](16-cluster.md): why a write waits for the leader to confirm
- [17 Monitoring, status and logs](17-monitoring.md)
- [Appendix A: Configuration reference](../manual.zh-TW/90-config-reference.md)
- [Appendix C: Metrics reference](../manual.zh-TW/92-metrics-reference.md)
