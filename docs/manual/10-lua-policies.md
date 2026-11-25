# 10 Writing policies in Lua

> After this chapter you can:
> - Tell when a script runs and what its return value means
> - Read the attributes of a request and add attributes to the reply
> - Read and write the cluster database with `sql_query` and `sql_execute`, with writes that come out the same on every node
> - Write an accounting script that is safe to run again on a retransmission
> - Call an allow-listed HTTP API from a script, and stay within the limits on execution time, memory and database wait

Prerequisites: you have run the quick-start in [chapter 03](03-quick-start.md), and you have read [chapter 07](07-users-and-chains.md) so you know auth chains and handlers. This chapter covers usage only. The full parameters, return values and error messages of every function are in [Appendix D](93-lua-reference.md).

In this chapter:

- [Where scripts run](#where-scripts-run)
- [Prepare a practice environment](#prepare-a-practice-environment)
- [Your first script](#your-first-script)
- [Return values](#return-values)
- [Read the attributes of a request](#read-the-attributes-of-a-request)
- [Add reply attributes](#add-reply-attributes)
- [Read and write the cluster database](#read-and-write-the-cluster-database)
- [Create tables with migrations](#create-tables-with-migrations)
- [Use a script as a lua handler in the auth chain](#use-a-script-as-a-lua-handler-in-the-auth-chain)
- [Two-step login: reply with Access-Challenge](#two-step-login-reply-with-access-challenge)
- [Write an accounting script](#write-an-accounting-script)
- [Call an HTTP API](#call-an-http-api)
- [Other helper functions](#other-helper-functions)
- [Variables for EAP and certificates](#variables-for-eap-and-certificates)
- [Limits on execution time, memory and database wait](#limits-on-execution-time-memory-and-database-wait)
- [After you change a script](#after-you-change-a-script)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Where scripts run

rradius has two kinds of script:

| Kind | Setting | When it runs |
| --- | --- | --- |
| Top-level script | `script` in `[lua]` | Every accounting request (RADIUS and TACACS+); logins when there is no auth chain; TACACS+ authorization (when the client has no `authorize_group`) |
| `lua` handler | `type = "lua"` in `[[auth.handlers]]` | When the auth chain reaches it, for logins only |

So as soon as the configuration has `[auth]`, logins go to the auth chain, and the top-level script only sees accounting and TACACS+ authorization. The quick-start works this way: `accounting.lua` starts with "reject if this is not accounting".

> **Note:** The auth chain is chosen per client. When `[auth]` has no default chain (no `[[auth.handlers]]`) and the client does not pick a named chain with `auth`, logins from that client also go to the top-level script.

Each time a script runs for a request, it sees a global environment created new for that request:

- Global variables the script sets are not visible to the next request. To remember something, write it to the database.
- The built-in Lua libraries such as `string`, `table` and `math` are available, but read-only. Changing them is an error.
- There is no `io`, `require` or `loadstring`, and `os` keeps only the time-related functions. A script cannot read or write files or run programs.
- The variables and functions rradius adds, such as `get_attr`, `sql_query` and `phase`, are listed in full in Appendix D.

Scripts use Luau (an extension of Lua 5.1). The server compiles them once at startup and runs them directly for each request.

## Prepare a practice environment

This chapter uses a configuration changed from the quick-start so that logins go to the top-level script. That makes it easier to see what the script does.

```console
$ cp -R ~/rradius-lab/quick-start ~/rradius-lab/lua
$ cd ~/rradius-lab/lua
$ rm -rf data
```

Open `rradius.toml` in an editor and do two things:

1. Delete the `[auth]` section and the `[[auth.handlers]]` under it, from the `[auth]` line up to the line before `[radius]`.
2. Change `[lua]` to point to the new script:

```toml
# rradius.toml (only the changed part)
[lua]
script = "policy.lua"
# ... (other settings omitted)
```

The default value of each `[lua]` key and whether it can be reloaded are in [Appendix A](90-config-reference.md):

| Key | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `lua.script` | required | yes | The top-level script, relative to the configuration file |
| `lua.max_execution_ms` | `1000` | yes | How long a script may run in one request (milliseconds) |
| `lua.query_timeout_ms` | `5000` | yes | The total time a script may wait for the database in one request (milliseconds) |
| `lua.max_memory_mb` | `256` | needs restart | Memory limit of each Lua VM (MiB) |

## Your first script

Goal: do with a script what the sql handler does. Look up the `users` table, compare the password, and on accept add two reply attributes.

```lua
-- policy.lua
-- Accounting: the practice environment does not record it. Answer at once so the NAS does not keep retransmitting.
if phase == 'accounting' then
  return 'accept'
end
-- This script only handles RADIUS logins.
if protocol ~= 'radius' then
  return 'reject'
end

local user = get_attr('User-Name')
if not user then
  return 'reject'
end
local rows = sql_query('SELECT password FROM users WHERE username = ? AND enabled = 1', {user})
local account = rows[1]
if not account or not password_verify(account.password, get_attr('User-Password')) then
  log('refused', user)
  return 'reject'
end

set_attr('Filter-Id', 'staff')
set_attr('Session-Timeout', 3600)
return 'accept'
```

Save it as `policy.lua`, check the configuration, then start the server (same as chapter 03):

```console
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd serve --config rradius.toml
```

`check` compiles the script, so you see syntax errors here. Log in from another terminal:

```console
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password bob-password --load pap,concurrency=1 --requests 1
…
  pap: 24/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user bob --password wrong --load pap,concurrency=1 --requests 1
…
  pap: 25/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

The server log:

```text
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="script" handler="script" decision="accept"
… INFO rradius_core::lua_engine: script policy.lua (radius/authentication, client localhost): refused bob
… INFO rradius_auth: authentication protocol="radius" client="localhost" source=127.0.0.1 user="bob" account="bob" chain="script" handler="script" decision="reject"
```

- `chain="script" handler="script"`: the top-level script made this decision, not an auth chain.
- A line written by `log()` starts with the script, the protocol and phase, and the client, so you can tell which request wrote it.
- `bob`'s password is stored as bcrypt. `password_verify` compares the same way the sql handler does, so both storage formats are recognized.

What this walk-through used:

| Name | Kind | Use here |
| --- | --- | --- |
| `phase` | variable | `"authentication"`, `"authorization"` or `"accounting"` |
| `protocol` | variable | `"radius"` or `"tacacs"` |
| `client` | variable | The name of the matched client; its `address` when it has no `name` |
| `get_attr(name)` | function | Read one attribute of the request |
| `sql_query(sql, params)` | function | Query the cluster database |
| `password_verify(stored, given)` | function | Compare a stored password |
| `set_attr(name, value)` | function | Add one attribute to the reply |
| `log(...)` | function | Write one log line |

## Return values

The string the script finally returns decides the result:

| Return | Top-level script | `lua` handler |
| --- | --- | --- |
| `'accept'` | Accept: Access-Accept, TACACS+ PASS, Accounting-Response | Accept (with `first_match` this ends the chain) |
| `'reject'` | Reject | Reject, and the whole chain ends here |
| `'ignore'` | Treated as reject | No opinion, pass to the next handler |
| `'challenge'` | Valid only for RADIUS logins: Access-Challenge | Same |
| Any other value, nothing returned, an error | Reject | Reject |

- Only the exact lower-case string counts. In a test, returning `'ACCEPT'` was a reject.
- For an accounting request, "reject" means no reply: the NAS gets no Accounting-Response and retransmits. The log says `Lua script did not accept accounting (radius, client localhost)`.
- When a script fails, the log gets one ERROR line, for example `Lua script error (radius/authentication): runtime error: policy.lua:12: …`. After the colon come the script file name and line number.
- There is one exception: when a failure comes from a database that cannot be reached or that times out, a login does not get a reject. It gets no reply. See [When the database is unreachable](#when-the-database-is-unreachable).

## Read the attributes of a request

`get_attr(name)` returns the first attribute of the request with that name, or `nil` if there is none. The name is the name in the dictionary (chapter 06), and the case must match.

The type of the value depends on the type in the dictionary:

| Dictionary type | What Lua gets | Example |
| --- | --- | --- |
| `string`, `octets` | A string (raw bytes) | `get_attr('User-Name')` → `"alice"` |
| `integer`, `date`, `byte`, `short`, `signed`, `integer64` | A number | `get_attr('NAS-Port')` → `7` |
| Addresses such as `ipaddr`, `ipv6addr`, `ipv6prefix` | A string in text form | `get_attr('NAS-IP-Address')` → `"192.0.2.1"` |

For an enumerated value you get the number, not the name in the dictionary. For example Start of `Acct-Status-Type` is `1`, so write `get_attr('Acct-Status-Type') == 1`, not `== 'Start'`.

`User-Password` is the decrypted plain text, and exists only for PAP. A CHAP or MS-CHAP request has no plain-text password, so a script cannot compare it. Leave those to the sql or ldap handler of an auth chain.

An attribute can appear several times, for example `Class`. `get_attrs(name)` returns all the values, in packet order:

```lua
-- policy.lua (excerpt)
local classes = get_attrs('Class')   -- an empty table {} when there are none
for i, value in ipairs(classes) do
  log('Class', i, value)
end
```

Tested: when the request carries two `Class` attributes (`a`, `b`), `get_attrs('Class')` is `{"a", "b"}` and `get_attr('Class')` is `"a"`.

A TACACS+ request has no dictionary. Attribute names are the packet field names and AV pair names, for example `user`, `rem_addr`, `service`, `cmd`. The list is in Appendix D, and the usage is in [chapter 12](12-tacacs.md).

> **Warning:** Attribute values come from the NAS or the user. Do not put them straight into SQL or a URL. Always use parameters in SQL (next section), and `url_encode` in URLs.

## Add reply attributes

`set_attr(name, value)` adds one attribute to the reply, in the order of the calls. Calling it twice with the same name gives two attributes. The value can be a string or a number:

```lua
-- policy.lua (excerpt)
set_attr('Filter-Id', 'staff')
set_attr('Service-Type', 'Framed-User')   -- an enumeration can be written with its dictionary name
set_attr('Session-Timeout', 3600)
```

The reply from a test (recorded with `[log.trace]`, chapter 17): `Filter-Id = staff, Service-Type = 2, Session-Timeout = 3600`.

- The name and value are checked at the moment of the call. A name that is not in the dictionary, a value that is not in the enumeration, or a value of the wrong type makes `set_attr` fail, and the request is rejected. For example `set_attr('Service-Type', 'Nonsense')` gives `'Nonsense' is not a value of 'Service-Type'`.
- Write a tagged attribute as `name:tag`, for example `Tunnel-Type:1`.
- Only "accept" and "challenge" send these attributes. A reject does not.
- The Accounting-Response of an accounting request never carries attributes. Anything the script sets is not sent.
- In TACACS+ authorization, `set_attr` adds AV pairs to the reply, for example `set_attr('priv-lvl', 15)`. The name `server_msg` is a special case: it becomes the message shown to the user, not an AV pair.

## Read and write the cluster database

Scripts use the cluster database (the built-in hiqlite: one copy of SQLite on each node, replicated with Raft) through two functions:

| Function | What it does | Returns |
| --- | --- | --- |
| `sql_query(sql, params)` | Reads the local copy of the database | An array with one table per row, with column names as keys |
| `sql_execute(sql, params)` | Writes: sent to the cluster leader, and returns after it is replicated to every node | The number of rows affected |

### Use parameters, do not build strings

Write `?` for every value in the SQL, and put the values in an array as the second argument, matched in order:

```lua
-- policy.lua (excerpt)
local rows = sql_query('SELECT password FROM users WHERE username = ? AND enabled = 1', {user})
```

- A parameter is never treated as SQL. In a test, querying `x' OR '1'='1` returned an empty array.
- The number of `?` must equal the number of values. When it does not, the call fails before it is sent: `sql_query: 1 placeholder, 0 values`.
- Only `?` is allowed. Forms such as `?1` and `:name` fail: `only plain ? placeholders are supported, found ?1`.
- A value can be a string, a number or a boolean (`true` is stored as 1). `nil` is not allowed: a `nil` in the middle of the array gives `parameters must be an array without holes`. To store NULL, write `NULL` in the SQL itself.

The query result:

```lua
local rows = sql_query('SELECT username, enabled FROM users WHERE username = ?', {'alice'})
-- #rows is 1; rows[1].username is "alice"; rows[1].enabled is 1
```

- When nothing is found you get an empty array, and `rows[1]` is `nil`.
- INTEGER and REAL become numbers, TEXT becomes a string, and NULL becomes `nil` (so that key does not exist).
- `sql_query` is read-only. Writing with it fails: `attempt to write a readonly database`.

### Writes: every node must compute the same result

The SQL of `sql_execute` runs unchanged once on every node. So a write must not contain anything that each node would compute differently, such as the current time or a random number. rradius stops these before sending:

- Functions: `date`, `datetime`, `julianday`, `now`, `random`, `randomblob`, `strftime`, `time`, `timediff`, `unixepoch`
- Keywords: `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_TIME`

Compute the time in Lua and pass it in as a parameter:

```lua
-- policy.lua (excerpt)
sql_execute('INSERT INTO last_login (username, at, client) VALUES (?, ?, ?) ' ..
  'ON CONFLICT (username) DO UPDATE SET at = excluded.at, client = excluded.client',
  {user, math.floor(now_unix()), client})
```

When written as `strftime('%s', 'now')`, a test gave `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead ``. It does not matter if these words appear in a string value, in a comment, or as part of a column name (for example `update_date`).

`sql_query` reads only locally, so this limit does not apply. This is why the queries of `[sessions]` can use `strftime`.

> **Note:** `sql_query` reads the local copy. With several nodes in the cluster, data just written on another node may be readable here only a moment later. When you need to "read your own write", decide from the return value of `sql_execute` in the same request, for example `DELETE … WHERE …` returning 1 means you got it.

### When the database is unreachable

A write waits for the leader to confirm. When the cluster has had no leader for more than 5 seconds, a new write fails at once. The first write after the majority is lost has already been handed to the cluster, and waits 30 seconds before it fails (see [chapter 16](16-cluster.md)). When the total time one request spends waiting for the database goes past `lua.query_timeout_ms`, the call fails too, for example `sql_query timed out: the database calls of one request have 5000 ms together`.

This kind of failure means "cannot be asked right now". It does not mean this person may not log in, so the server handles it differently from an ordinary error:

- Login: no reply, so the NAS retransmits or asks another server. The log says `RADIUS Access-Request from client localhost not answered: the authentication backend could not be used`, and `Discarded: database` in `status` goes up by one. The result is the same even if the script catches the error with `pcall` and then returns `'reject'`.
- Accounting: with an accounting spool the request is first written to the spool and replayed later (chapter 13); without one there is no reply.

Tested: with `query_timeout_ms = 200`, a query that ran for a long time made `rradius-bench` show `timeouts 1`, and `Discarded: database` went from 0 to 1.

All database calls in one request (including `db_query` and `db_execute` from chapter 09) share the same `query_timeout_ms`: the second call can only use the time the first left over. The use of external databases is in chapter 09, and the parameters and errors of each function are in Appendix D.

## Create tables with migrations

Create the tables a script uses with `migrations_dir` of `[cluster]`. When the server starts, before it begins to serve, it applies the SQL files in that directory in numeric order, each file only once.

Rules:

- A file name is `<number>_<name>.sql`, numbered from `1` with no gaps, for example `1_schema.sql`, `2_demo_users.sql`.
- Each file is one transaction, written through the cluster, so the tables are the same on every node.
- For each file applied, the name and the SHA-256 of the content are recorded. If you later change a file that was already applied, or a number is missing, the server fails to start with exit code 2.
- To change a table, add a new file, for example `3_last_login.sql`. Do not change old files.

Add a table to the practice environment for the example in the previous section:

```sql
-- migrations/3_last_login.sql
CREATE TABLE last_login (
  username TEXT PRIMARY KEY,
  at INTEGER NOT NULL,
  client TEXT
);
```

Migrations are applied only at startup, so restart the server after you add a new file. How this behaves with several nodes in the cluster (waiting for the leader, catching up) is in [chapter 16](16-cluster.md).

## Use a script as a lua handler in the auth chain

A `lua` handler in an auth chain is good for rules other than the password: leave the password to the sql or ldap handler, and let the script decide only whether this account may go online right now.

Go back to the quick-start configuration (the one with `[auth]`) and add a `lua` handler in front of the sql handler:

```toml
# rradius.toml (replaces the original [auth] part)
[auth]
policy = "first_match"

[auth.username]
lowercase = true

[[auth.handlers]]
type = "lua"
script = "policy.lua"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ? AND enabled = 1"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"
```

```lua
-- policy.lua (lua handler)
-- auth_user is the account name after the auth chain has cleaned it up; User-Name is what the request carried.
log('policy', get_attr('User-Name'), auth_user)
if auth_user == 'carol' then
  return 'reject'      -- stop here: the whole chain ends
end
return 'ignore'        -- no opinion: pass to the next handler (sql) to compare the password
```

Tested results:

| Login | `User-Name` and `auth_user` the script saw | Result | `handler` in the authentication record |
| --- | --- | --- | --- |
| `Alice` with the right password | `Alice`, `alice` | Accept | `sql` |
| `CAROL` | `CAROL`, `carol` | Reject | `lua` |
| `nobody` | `nobody`, `nobody` | Reject | `none` |

- `auth_user` has a value only inside an auth chain. It is the name after `[auth.username]` has cleaned it up. Use it when you query tables.
- A `lua` handler runs only for logins. Accounting always goes to the top-level script.

> **Warning:** In a `first_match` chain, when a `lua` handler returns `'accept'` the login is accepted at once, and the handlers after it do not compare the password. Tested: with a script that returned `'accept'` for `guest`, any password logged in. A script that only applies rules should return `'ignore'` when it lets the login pass. To require both checks, use `policy = "all_accept"` (chapter 07).

When the script really compares the password itself (for example with `password_verify` or by calling an API), add `verifies_password = true` to the handler, so the auth chain treats its accept as "password checked". The inner login of PEAP and TTLS needs this. The details are in chapter 07 and [chapter 11](11-eap.md).

## Two-step login: reply with Access-Challenge

When a script returns `'challenge'`, the server sends an Access-Challenge, and the NAS asks the user once more (for example for a one-time password). The second Access-Request brings back the same `State`.

```lua
-- policy.lua (excerpt)
set_attr('State', random_token())          -- 32 hexadecimal characters
set_attr('Reply-Message', 'Enter your code')
return 'challenge'
```

- Only a RADIUS login can return a challenge. For TACACS+, accounting and authorization, returning `'challenge'` is treated as a reject.
- An Access-Challenge can carry only `Reply-Message`, `State`, `Idle-Timeout`, `Session-Timeout` and vendor attributes, and at most one `State`. If it carries any other attribute, the server sends a reject instead, and the log says `RADIUS Access-Challenge for client localhost not sent: 'Filter-Id' is not allowed in an Access-Challenge`.
- The script must remember whom it gave a challenge to: write the `State` to a table, and look it up in the second request.

The full approach (password, TOTP, a limit on the number of failures, a code usable only once) is in the example `rradius-server/examples/two-step-totp.lua`, explained step by step in [chapter 15](15-access-recipes.md).

## Write an accounting script

Accounting requests always go to the top-level script. `accounting.lua` of the quick-start is a complete example. It shows the main points of writing an accounting script:

1. **Reply only by returning `'accept'`.** Without a reply, the NAS retransmits the same packet.
2. **Make every write safe to repeat.** When the NAS retransmits, the script runs again, so use `INSERT … ON CONFLICT … DO UPDATE`: writing the same packet twice gives the same result as writing it once.
3. **When the request cannot be recorded and a retransmission would not help, still return `'accept'`**, for example for a packet without `Acct-Session-Id`. Leave one line with `log()` first.
4. **Use `now_unix()` minus `Acct-Delay-Time` for the time**: that is when the event happened. For a request replayed from the accounting spool, the server adds the time it waited in the spool to `Acct-Delay-Time`.
5. **When `replayed` is `true`, the request was replayed from the accounting spool** and may be about something long ago. Do not use it for real-time actions.

```lua
-- accounting.lua (excerpt from the quick-start)
local status = get_attr('Acct-Status-Type')
if status == 7 or status == 8 then    -- Accounting-On/Off: this NAS restarted
  sql_execute("UPDATE accounting SET stopped = updated, terminate_cause = 11 WHERE client = ? AND nas = ? AND stopped IS NULL",
    {client, nas})
  return 'accept'
end
```

The number of accounting scripts that run at the same time is limited: the number of Lua VMs minus 2, at least 1 (the number of VMs equals the number of CPUs the program can use). Requests over the limit wait in a queue and are not rejected. A script gives up its place while it waits for the database or an HTTP reply. This keeps CPU available for logins when a lot of accounting arrives. Login and authorization scripts have no such limit.

How to design accounting tables, the spool, and the `session` command are all in [chapter 13](13-accounting.md).

## Call an HTTP API

When accounts live in another system (a membership system, a billing system), a script can call its HTTP API. You first list the allowed hosts in the configuration:

```toml
# rradius.toml (add after [lua])
[lua.http]
allowed_hosts = ["members.example.com"]   # host only: HTTPS only, port 443
timeout_ms = 1500
max_concurrent = 64
```

Without `[lua.http]`, `http_request` and `url_encode` do not exist in the script: they are `nil`. The default of each key is in [Appendix A](90-config-reference.md):

| Key | Default | Meaning |
| --- | --- | --- |
| `lua.http.allowed_hosts` | required | `host`, `host:port`, or `http://host:port` (plain HTTP for this host) |
| `lua.http.timeout_ms` | `2000` | Time limit of one call, from connecting to reading the whole response |
| `lua.http.max_response_bytes` | `1048576` | Upper limit of the response body |
| `lua.http.max_concurrent` | `64` | Calls in progress at the same time for each host; logins and accounting are counted separately |
| `lua.http.ca_file` | none | A CA to trust in addition to the system root certificates |

All these keys can be reloaded. `allowed_hosts` cannot be written as `https://…` or with a path, and `check` points it out: `lua.http.allowed_hosts: 'https://api.example.com' is not host, host:port or http://host:port`.

The `member-api.lua` of the example scenario `external-api` is a complete example. Its core is:

```lua
-- member-api.lua (excerpt from the external-api scenario)
local response, problem = http_request{
  url = API,
  method = 'POST',
  headers = {
    Authorization = 'Bearer ' .. API_KEY,
    ['Content-Type'] = 'application/x-www-form-urlencoded',
  },
  -- The name and password come from the user, so always encode them first.
  body = 'username=' .. url_encode(user) .. '&password=' .. url_encode(password),
}
if not response then
  log('member API gave no answer:', problem)
  return 'reject'
end
if response.status ~= 200 then
  return 'reject'
end
local plan = response.headers['x-plan']
if plan and plan:match('^[%w_-]+$') then
  set_attr('Filter-Id', plan)
end
```

The result of `http_request`:

- When an HTTP response was received, whatever the status code, it returns a table: `status` (a number), `body` (a string) and `headers` (header names always in lower case). 500 and 302 are also responses, and redirects are not followed automatically.
- When there was no response, it returns `nil` and a reason. For example a timeout `timed out after 1500ms`, a response that is too large `the response is over 1048576 octets`, too many at once `too many requests at once`, or not in the allow list `https://evil.example is not in lua.http.allowed_hosts`.
- In these cases the script keeps running, and the script decides what to do. Misuse of the parameters (not a table, no `url`, a method other than GET, POST, PUT, DELETE or PATCH, setting a header such as `Host`) is a Lua error.

This example calls the API from a `lua` handler with `verifies_password = true`, because the API really compares the password.

Tested (with a test HTTP server on the local machine, `allowed_hosts = ["http://127.0.0.1:<port>"]`): with the right password the API answered 200 and `X-Plan: gold`, the login was accepted, and the reply carried `Filter-Id = gold`. With a wrong password the API answered 401 and the login was rejected.

Every call is recorded in the `result` label of the metric `rradius_script_http_requests_total`: `ok`, `timeout`, `error`, `denied` (not in the allow list, or the URL was refused), `too_large`, `busy` (too many at once). On a failure the log also gets one WARN line, for example `script http_request to members.example.com: timed out after 1500ms`. It writes only the host, not the rest of the URL.

> **Note:** `timeout_ms` should be shorter than the NAS retransmit interval. When the API does not answer, the script should reject quickly, instead of making the NAS wait until it gives up.

> **Warning:** Do not put the API key in a script that other people can read. Set the permissions of the script file so that only the account that runs rradius can read it.

## Other helper functions

| Function | Use |
| --- | --- |
| `now_unix()` | The current Unix time (seconds, an integer value). Always use it for times written to the database |
| `random_token()` | 32 hexadecimal random characters, for example as a `State` |
| `totp_verify(secret, code)` | Check a TOTP code (Base32 key, 6 digits, 30-second steps, one step before and after allowed). Returns the step number when correct, `nil` when wrong |
| `password_verify(stored, given)` | Compare a stored password with the same rules as the auth chain, returns `true` or `false` |
| `wpa_pmk(passphrase, ssid)` | Compute the WPA2 PMK (64 hexadecimal characters) |
| `url_encode(text)` | Encode a piece of text as one part of a URL (exists only with `[lua.http]`) |
| `log(...)` | Write one INFO log line; the arguments are joined with spaces, newlines become spaces, at most 4096 bytes |

`totp_verify` returns the step number so that a script can record the step it used, and the same code cannot be used a second time (the way `two-step-totp.lua` does it).

`password_verify` does not count failures and is not protected by `[auth.lockout]`. When a script compares passwords itself, you must limit the number of guesses yourself. Or leave the password to a handler of the auth chain and let `[auth.lockout]` count.

> **Note:** `print()` exists, but it writes to the standard output of the program, not to the log. When you run under systemd or in a container you usually cannot see it. Always use `log()`.

> **Enterprise edition:** `send_disconnect` and `send_coa` can send Disconnect and CoA from a script. The core has these two names too, but calling them always fails: `send_coa: not supported by this program (CoA needs rradius-enterprise)`.

## Variables for EAP and certificates

For an EAP login, the auth chain or the top-level script sees a few more variables:

| Variable | When it has a value | Example |
| --- | --- | --- |
| `eap_method` | After an EAP-TLS certificate is accepted, and for the inner login of PEAP and TTLS | `"tls"`, `"peap"`, `"ttls"` |
| `tls_subject` | After an EAP-TLS certificate is accepted | `"CN=laptop-001"` |
| `tls_subject_attrs` | Same | `{ CN = { "laptop-001" } }`, one array for each name |

Tested (wifi-certs scenario, with a `lua` handler in front of the sql handler that logs these variables): when logging in with the certificate of `laptop-001`, the script saw `eap_method = "tls"`, `tls_subject = "CN=laptop-001"` and `tls_subject_attrs.CN[1] = "laptop-001"`.

For a request that is not EAP, these variables are all `nil`. The usage (for example giving a different VLAN by the OU of the certificate) is in [chapter 11](11-eap.md).

## Limits on execution time, memory and database wait

Scripts run inside the server, and a badly written script must not bring the server down. There are three limits:

| Limit | Setting | When exceeded |
| --- | --- | --- |
| Execution time | `lua.max_execution_ms` (default 1000) | `script exceeded the execution limit of 1000 ms`, the request fails |
| Memory | `lua.max_memory_mb` (default 256, per VM) | `memory error: not enough memory`, the request fails |
| Database wait | `lua.query_timeout_ms` (default 5000, the sum of all calls in one request) | The call fails; a login gets no reply |

Execution time counts only the time the script itself is running. Time spent waiting for the database, waiting for an HTTP reply, or waiting in the queue for an accounting place does not count. Each request is timed separately.

Tested (`max_execution_ms = 100`):

```lua
-- policy.lua
while true do end
```

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: script exceeded the execution limit of 100 ms
```

- After the time limit is passed, catching the error with `pcall` does not help: every following step raises the same error, and in the end even returning `'accept'` counts as a failure. In a test, `pcall(function() while true do end end) return 'accept'` was rejected.
- A long-running loop does not block other requests: about every 10 milliseconds of running, the script lets other requests go first.

The memory limit applies to each Lua VM. The number of VMs equals the number of CPUs the program can use, and all scripts share them. When one script needs too much memory, the error is its own. Other scripts that happen to be on the same VM at the same time may get the same error. In a test with `max_memory_mb = 16`, `string.rep('x', 64 * 1024 * 1024)` made that request fail, and the next request was accepted as usual.

## After you change a script

Script files, like the configuration file, are replaced by new ones when you send `SIGHUP` to reload. You do not need to restart:

```console
$ kill -HUP <server PID>
```

```text
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

- When the new script has a syntax error, the reload fails and the server keeps using the old one: `reload failed, keeping the running configuration: cannot compile script 'policy.lua': syntax error: 1: Malformed string; did you forget to finish it?`
- Changing `lua.max_memory_mb` needs a restart. On reload the log says `these need a restart to apply: lua.max_memory_mb`.
- Run `rradiusd check` first. It compiles all scripts (the top-level one and every `lua` handler), so you see syntax errors there.

## Common mistakes

**Symptom:** You changed `accounting.lua` to handle logins, but the script is never called for logins.
**Cause:** The configuration has `[auth]`, so logins go to the auth chain, and the top-level script gets only accounting.
**Fix:** Write the rule as a `lua` handler in the auth chain, or remove `[auth]`.

**Symptom:** A script test `get_attr('Acct-Status-Type') == 'Start'` is never true.
**Cause:** For an enumerated value you get the number.
**Fix:** Write `== 1` (Start), `== 2` (Stop), `== 3` (Interim-Update).

**Symptom:** The log has `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead ``.
**Cause:** The SQL of a write uses a time or random function.
**Fix:** Compute the value in Lua with `now_unix()` and pass it in as a parameter.

**Symptom:** The log has `sql_query: 1 placeholder, 0 values`.
**Cause:** The SQL has `?`, but no parameter array was passed, or the array has the wrong length.
**Fix:** Add the second argument, for example `sql_query(sql, {user})`.

**Symptom:** The log has `attribute value must be a string or number, got boolean`.
**Cause:** The value given to `set_attr` is a boolean, a table or `nil`.
**Fix:** Pass a string or a number.

**Symptom:** The script returns `'challenge'`, but the NAS receives an Access-Reject, and the log has `Access-Challenge for client … not sent: '…' is not allowed in an Access-Challenge`.
**Cause:** The challenge set a standard attribute other than `Reply-Message`, `State`, `Idle-Timeout` and `Session-Timeout`.
**Fix:** Set only these in a challenge. Set the other attributes when you accept.

**Symptom:** A login gets no reply, `rradius-bench` shows `timeouts 1`, and the log has `not answered: the authentication backend could not be used`.
**Cause:** A database call of the script failed or timed out (the cluster has no leader, the query is too slow, an external database is unreachable).
**Fix:** Look at the ERROR line just before it to find which call it was. If the query is too slow, add an index, or raise `lua.query_timeout_ms`.

**Symptom:** The log has `script exceeded the execution limit of 1000 ms`.
**Cause:** The script's own computation takes too long. Common causes are a loop with no end condition, or processing a lot of data in Lua.
**Fix:** Leave the data processing to SQL. Raise `lua.max_execution_ms` only when you really need to.

**Symptom:** A script calling `http_request` gets `attempt to call a nil value`.
**Cause:** The configuration has no `[lua.http]`, so the function does not exist in scripts.
**Fix:** Add `[lua.http]` with `allowed_hosts`.

**Symptom:** `http_request` returns `nil` and `http://… is not in lua.http.allowed_hosts`.
**Cause:** The host, port or http/https of the URL is not exactly the same as the allow list. A host written alone allows only HTTPS on the default port.
**Fix:** Write the allow list the same as the URL, for example `api.example.com:8443` or `http://10.0.0.5:8080`.

**Symptom:** A script written like `disconnect-disabled.lua` makes the log say `send_disconnect: not supported by this program (CoA needs rradius-enterprise)`.
**Cause:** The core does not send CoA or Disconnect.
**Fix:** Remove that part. An accounting request still returns `'accept'` as usual.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [11 802.1X and EAP](11-eap.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [13 Accounting and sessions](13-accounting.md)
- [15 Scenarios: network access for organizations](15-access-recipes.md)
- [16 Clusters and high availability](16-cluster.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix D: Lua API reference](93-lua-reference.md)
