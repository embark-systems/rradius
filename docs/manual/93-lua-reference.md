# Appendix D: Lua API reference

> After this chapter you can:
> - Look up the signature, return value, errors and limits of every global variable and function in a script
> - Know which Lua type an attribute or database value becomes
> - Know which built-in Lua libraries a script can use and which were removed
> - Check this list against the code yourself

Prerequisites: you have read [chapter 10](10-lua-policies.md). This appendix lists the API only. How to write a policy, and why, is in chapter 10.

The contents of this appendix come from where the script environment is registered in `rradius-core/src/lua_engine.rs` (the `BINDER` constant and `make_binder`) and from `rradius-core/src/script_http.rs`. The error messages were produced on the quick-start and are copied as they appear.

In this chapter:

- [The script environment](#the-script-environment)
- [Global variables](#global-variables)
- [Request attributes](#request-attributes)
- [Cluster database](#cluster-database)
- [External databases](#external-databases)
- [HTTP](#http)
- [Logging and utilities](#logging-and-utilities)
- [CoA and Disconnect](#coa-and-disconnect)
- [Built-in libraries](#built-in-libraries)
- [Form of errors](#form-of-errors)
- [Limits](#limits)
- [How to compare](#how-to-compare)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## The script environment

Scripts are Luau (an extension of Lua 5.1). Each request has its own global table: the names listed in this appendix are bound to that request, and any other name is looked up in the read-only built-in libraries.

- Global variables do not survive to the next request. Tested: a script writes `seen = true`, and the next request reads `seen` as `nil`. To remember something, write it to the cluster database.
- The built-in libraries are read-only. Tested: `string.upper = nil` gives `attempt to modify a readonly table`.
- The value the script `return`s at the end decides the result:

| Return value | Meaning |
| --- | --- |
| `'accept'` | Accept |
| `'reject'` | Reject |
| `'ignore'` | Only meaningful in the `lua` handler of an auth chain: pass to the next handler. A top-level script that returns it is rejecting |
| `'challenge'` | Only valid for a RADIUS login (`protocol == 'radius'` and `phase == 'authentication'`): send an Access-Challenge. Anywhere else it is a reject |
| Any other value, no return, or an error | Reject |

Only the exact same lower-case string counts; `'ACCEPT'` is a reject. For an accounting request, "reject" means no response, and the log says `Lua script did not accept accounting (radius, client localhost)`.

## Global variables

Global variables are set before the script starts and do not change during the request.

| Variable | Type | When it has a value |
| --- | --- | --- |
| `protocol` | string | Always |
| `phase` | string | Always |
| `client` | string | Always |
| `auth_user` | string or `nil` | The `lua` handler in an auth chain |
| `replayed` | boolean | Always |
| `eap_method` | string or `nil` | EAP logins |
| `tls_subject` | string or `nil` | After an EAP-TLS certificate passes |
| `tls_subject_attrs` | table or `nil` | Same as above |

### protocol

The protocol of the request: `'radius'` or `'tacacs'`.

### phase

The phase of the request:

| `protocol` | Values of `phase` |
| --- | --- |
| `'radius'` | `'authentication'` (Access-Request), `'accounting'` (Accounting-Request) |
| `'tacacs'` | `'authentication'`, `'authorization'`, `'accounting'` |

### client

Which client the request came from: the `name` of the `[[radius.clients]]` or `[[tacacs.clients]]` entry in the configuration. The quick-start client is called `localhost`, so the value is `'localhost'`.

### auth_user

The account name after the auth chain has normalized it. For example, when `[auth.username]` sets `lowercase = true`, a login as `Alice` sees `'alice'`, while `get_attr('User-Name')` is still the original `'Alice'` from the request (tested). Only the `lua` handler in an auth chain gets it; in a top-level script it is always `nil`, so use `get_attr('User-Name')`.

### replayed

`true` means this accounting request was replayed from the accounting spool (a temporary store on disk where requests wait to be sent again); every other request is `false`. When replayed, `Acct-Delay-Time` is increased by the number of seconds the request waited in the spool.

### eap_method

The EAP method that authenticates this request:

| Value | When |
| --- | --- |
| `'tls'` | After the EAP-TLS certificate passes, when the auth chain or the top-level script is asked whether this name may log in |
| `'peap'`, `'ttls'` | The inner login inside a PEAP or TTLS tunnel (goes through the auth chain only) |

For a request that is not EAP, it is `nil`.

### tls_subject

The subject of the EAP-TLS certificate, as a string, for example `'CN=laptop-001'`. For other requests it is `nil`.

### tls_subject_attrs

The subject of the same certificate split into a table: each attribute name maps to an array, because a name can repeat, for example `{ CN = { 'laptop-001' } }`. The values have their escape characters removed. For other requests it is `nil`.

```lua
-- policy.lua (excerpt)
local ou = tls_subject_attrs and tls_subject_attrs.OU and tls_subject_attrs.OU[1]
```

## Request attributes

### get_attr

```text
get_attr(name) -> value or nil
```

- **Parameters:** `name`, the attribute name (a string). For RADIUS, use the name in the dictionary, with the same capitalization; for TACACS+, see the table below.
- **Returns:** the value of the first attribute with this name in the request; `nil` if there is none. A name that does not exist is not an error and also gives `nil`.
- **Phases:** all.

### get_attrs

```text
get_attrs(name) -> array
```

- **Parameters:** same as `get_attr`.
- **Returns:** the values of all attributes with this name, in packet order; an empty table `{}` if there are none.
- **Phases:** all.

### set_attr

```text
set_attr(name, value)
```

- **Parameters:** `name`, the attribute name; `value`, a string or a number. For an enumeration you can write the value name from the dictionary or the number; a tagged attribute is written as `name:tag`.
- **Returns:** nothing.
- **Effect:** adds an attribute to the reply, in call order; calling it twice with the same name gives two attributes. Only an accept and a challenge are sent; an Accounting-Response never carries attributes.
- **Phases:** all. For a RADIUS login, the name and value are checked against the dictionary at the moment of the call. In TACACS+ authorization it becomes an AV pair of the reply (`name=value`); the name `server_msg` is a special case and becomes a message shown to the user.
- **Errors:**

| Case | Error message |
| --- | --- |
| The value is a boolean, a table or `nil` | `attribute value must be a string or number, got boolean` |
| The dictionary has no such name | `attribute 'No-Such-Attribute' has an unsupported number` |
| The enumeration has no such value | `'Nonsense' is not a value of 'Service-Type'` |

An Access-Challenge can carry only `Reply-Message`, `State` (at most one), `Idle-Timeout`, `Session-Timeout` and vendor-specific attributes. Other attributes are checked just before sending, so `set_attr` does not fail; instead the whole challenge becomes a reject, and the log says `RADIUS Access-Challenge for client localhost not sent: 'Filter-Id' is not allowed in an Access-Challenge`.

In TACACS+ authorization, one AV pair (`name=value`) can be at most 255 octets: a longer one makes `set_attr` fail at the call (`AV pair 'name' is longer than 255 octets`) and the request is rejected. A reply holds at most 255 AV pairs: more than that returns ERROR, and the log says `TACACS+ authorization for client … has … AV pairs, more than the 255 a reply holds`.

### Types of RADIUS attribute values

| Dictionary type | What `get_attr` returns | What `set_attr` accepts |
| --- | --- | --- |
| `integer`, `date`, `byte`, `short`, `signed`, `integer64` | A number; an enumeration is also a number | A number, or the value name of an enumeration |
| `ipaddr`, `ipv6addr`, `ipv6prefix`, `ipv4prefix`, `ether`, `ifid` | A string in text form, for example `'192.0.2.1'` | A string in text form |
| `string`, `octets` and others | A string of raw bytes | A string |

Tested: `User-Name` is `'bob'`, `NAS-IP-Address` is `'192.0.2.1'`, and `NAS-Port` is the number `7`.

`User-Password` is the decrypted plain text, and only PAP requests have it.

### Names of TACACS+ attributes

TACACS+ has no dictionary. The names are the packet fields and the AV pair names:

| Name | Type | Which phase | Content |
| --- | --- | --- | --- |
| `user`, `port`, `rem_addr` | string | all | User name, the port on the device, the remote address |
| `data` | string | authentication | The data field of the START packet |
| `authen_type`, `priv_lvl`, `authen_service` | number | all | Field values from RFC 8907 |
| `authen_method` | number | authorization, accounting | How the user originally logged in |
| Names of AV pairs, for example `service`, `cmd`, `cmd-arg`, `task_id` | string | authorization, accounting | Each `key=value` or `key*value` argument, with the key as the name |

For AV pairs with the same name (for example several `cmd-arg`), use `get_attrs` to get all of them.

## Cluster database

`sql_query` and `sql_execute` use the database built into the cluster (hiqlite, a SQLite with one copy per node, replicated with Raft). For usage and cautions, see chapter 10.

### sql_query

```text
sql_query(sql, params) -> array of rows
```

- **Parameters:** `sql`, one SQL statement; `params`, optional, an array of values matched to the `?` in order.
- **Returns:** an array with one table per row, with the column names as keys; an empty table if there is no result.
- **Effect:** reads the local copy of the database, read-only.
- **Phases:** all.
- **Errors:** see [Parameters](#parameters) below and this table:

| Case | Error message |
| --- | --- |
| A write | `Sqlite: attempt to write a readonly database` |
| A syntax error | `Sqlite: near "SELEC": syntax error in SELEC 1 at offset 0` |
| No such table | `Sqlite: Error code 1: SQL logic error Some("no such table: nosuch")` |
| The database time of this request is used up | `sql_query timed out: the database calls of one request have 5000 ms together` |

### sql_execute

```text
sql_execute(sql, params) -> number
```

- **Parameters:** same as `sql_query`.
- **Returns:** the number of rows affected. Tested: an `INSERT` of one row returns `1`; `CREATE TABLE` can also be run.
- **Effect:** sent to the cluster leader, and returns after it is replicated to the nodes.
- **Phases:** all.
- **Errors:** besides the errors under [Parameters](#parameters), SQL that uses a function or keyword that each node would compute differently is refused before it is sent:

| Case | Error message |
| --- | --- |
| Uses one of the functions `date`, `datetime`, `julianday`, `now`, `random`, `randomblob`, `strftime`, `time`, `timediff`, `unixepoch` | `` sql_execute: non-deterministic `strftime` is not allowed in writes; pass time values from now_unix() instead `` |
| Uses one of the keywords `current_timestamp`, `current_date`, `current_time` | Same as above, with `` `current_timestamp` `` between the backticks |
| The database time of this request is used up | `sql_execute timed out: the database calls of one request have 5000 ms together` |

### Parameters

The parameter rules are the same for `sql_query`, `sql_execute`, `db_query` and `db_execute`. The function name appears at the start of the message:

| Case | Error message (using `sql_query` as the example) |
| --- | --- |
| The number of `?` and the number of values differ | `sql_query: 1 placeholder, 0 values`, `sql_query: 0 placeholders, 1 value` |
| Uses forms such as `?1` or `:name` | `sql_query: only plain ? placeholders are supported, found ?1` |
| The second argument is not a table | `sql_query: parameters must be an array, not string` |
| The array has a hole (a `nil` in the middle) | `sql_query: parameters must be an array without holes` |
| A value has an unsupported type | `sql_query: parameter 1 has unsupported type table` |
| A number is infinite or NaN | `sql_query: parameter 1 is not a finite number` |

How values are mapped:

| Lua value | Sent to the database |
| --- | --- |
| String | TEXT; BLOB if it is not UTF-8 |
| A number with an integer value | INTEGER |
| Other numbers | REAL |
| `true`/`false` | `1`/`0` |

A `?` inside an SQL string literal is not a parameter. Tested: `sql_query("SELECT '?' AS q")` needs no parameters and returns one row where `q` is `'?'`.

### Types of results

| Database value | Lua |
| --- | --- |
| INTEGER | Number (integer) |
| REAL | Number |
| TEXT | String |
| BLOB | String of raw bytes |
| NULL | `nil`, so the table for that row has no such key |

## External databases

`db_query` and `db_execute` use the PostgreSQL or MySQL database configured in `[databases.<name>]`. For the configuration, see [chapter 09](09-external-databases.md).

### db_query

```text
db_query(name, sql, params) -> array of rows
```

- **Parameters:** `name`, the name in `[databases.<name>]`; `sql`; `params`, optional, rules in [Parameters](#parameters).
- **Returns:** same as `sql_query`.
- **Effect:** sent through the connection pool of that database. Always write `?` in the SQL; for PostgreSQL, rradius rewrites it to `$1`, `$2`, ..., and for MySQL it is sent as it is.
- **Phases:** all.
- **Errors:**

| Case | Error message |
| --- | --- |
| No such name | `db_query: there is no database 'billing' in [databases]` |
| Cannot connect, or the pool cannot get a connection | Messages like `db_query: database billing: pool timed out while waiting for an open connection` |
| A column type that cannot be read | `db_query: database billing: column x has a type that cannot be read; CAST it to text in the SQL` |
| A MySQL unsigned integer larger than a script number can hold | `db_query: database billing: column x holds …, more than a script number can hold; CAST it to text in the SQL` |
| The database time of this request is used up | `db_query timed out: the database calls of one request have 5000 ms together` |

Types of results: integers and floating-point numbers are numbers, a boolean is `1`/`0`, text is a string, binary is a byte string, and NULL is `nil`. PostgreSQL `NUMERIC` and MySQL `DECIMAL` (the usual result of `SUM` and `AVG`) are decimal strings and are not rounded.

Tested (PostgreSQL 17, MySQL 8.4):

| Query | Result |
| --- | --- |
| PostgreSQL `WHERE username = ? OR username = ?` with parameters `{'alice', 'bob'}` | Two rows (the `?` are rewritten to `$1`, `$2` in order) |
| `SELECT '?' AS mark …` | `mark` is `'?'`; a `?` in a string literal is not a parameter |
| PostgreSQL `NUMERIC(8,2)` and its `SUM` | `'12.50'` (a string) |
| PostgreSQL `BOOLEAN` `true` | `1` |
| PostgreSQL `BIGINT` `9007199254740993` | `9007199254740992`: an integer above 2^53 loses precision, so `CAST` it to text |
| MySQL `DECIMAL(8,2)`, `TINYINT(1)` | `'12.50'`, `1` |
| PostgreSQL and MySQL `DATE` | `db_query: database billing: column created has a type that cannot be read; CAST it to text in the SQL` |
| MySQL `BIGINT UNSIGNED` `18446744073709551615` | `db_query: database legacy: column u_big holds 18446744073709551615, more than a script number can hold; CAST it to text in the SQL` |
| A column that does not exist (PostgreSQL) | `db_query: database billing: error returned from database: column "nosuch" does not exist …` |

The database's own error message follows `error returned from database:` and is passed to the script as it is.

### db_execute

```text
db_execute(name, sql, params) -> number
```

- **Parameters:** same as `db_query`.
- **Returns:** the number of rows affected.
- **Effect:** an external database has no cluster replication problem, so non-deterministic functions are not checked, and functions such as `now()` can be used.
- **Phases:** all.
- **Errors:** same as `db_query`, with the message starting with `db_execute:`. Tested: without permission, the message is `db_execute: database legacy: error returned from database: 1142 (42000): UPDATE command denied to user …` (MySQL) or `… permission denied for table radcheck …` (PostgreSQL).

## HTTP

`http_request` and `url_encode` exist only when the configuration has `[lua.http]`; without it they are `nil`, and calling them gives `attempt to call a nil value`. For the configuration, see [Appendix A](90-config-reference.md); for usage, see chapter 10.

### http_request

```text
http_request{ url = …, method = …, headers = …, body = … } -> response, nil
                                                           -> nil, reason
```

- **Parameters:** one table:

| Key | Required | Description |
| --- | --- | --- |
| `url` | Yes | The full URL. The host, port and http/https must be in `lua.http.allowed_hosts` |
| `method` | | `'GET'` (default), `'POST'`, `'PUT'`, `'DELETE'`, `'PATCH'` |
| `headers` | | A table, header names mapped to string values |
| `body` | | A string |

- **Returns:** on success, a table and `nil`: `status` (a number), `body` (a string), `headers` (a table with lower-case names, for example `r.headers['content-type']`). Redirects are not followed automatically: a 302 is returned as a 302, and `headers.location` is the redirect target. On failure, `nil` and a reason string.
- **Phases:** all. An accounting script gives up its accounting slot while it waits for the response.
- **Two kinds of failure:** an option that is written wrongly is a script bug and raises an error; a URL that is not allowed, a connection that fails, or a timeout is a runtime condition, reported to the script as a return value so that the script decides what to do.

Errors for wrongly written options:

| Case | Error message |
| --- | --- |
| The argument is not a table | `http_request: takes a table of options` |
| No `url` | `http_request: url is required` |
| `url`, `method` or `body` is not a string | `http_request: url must be a string` (by key name) |
| An unsupported method | `http_request: method must be GET, POST, PUT, DELETE or PATCH` |
| `headers` is not a table | `http_request: headers must be a table` |
| A header name or value is not valid | `http_request: a header name or value is not valid` |
| Sets `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade` or `TE` | `http_request: the Host header cannot be set` (by name) |

Failure reasons that are returned:

| Reason | When |
| --- | --- |
| `http://127.0.0.1 is not in lua.http.allowed_hosts` | The host, port or http/https is not exactly the same as in the allow list |
| `not an http or https URL` | Another scheme |
| `not a valid URL` | The URL cannot be parsed |
| `a URL with a user name is not allowed` | The URL has a user name or password |
| `a URL with . or .. in its path is not allowed` | The path has `.` or `..` (including when encoded as `%2e`) |
| `a URL with a tab or a line break is not allowed` | The URL has a tab or a line break |
| `a URL with a space or a control character at either end is not allowed` | The URL has a space at the start or end |
| `too many requests at once` | The calls in progress to this host have reached `lua.http.max_concurrent`; there is no queue, the call fails at once |
| `timed out after 2s` | Over `lua.http.timeout_ms` (the message shows the actual limit) |
| `the response is over 1048576 octets` | The response body is over `lua.http.max_response_bytes` |
| `the connection was refused`, `the connection failed`, `the name lookup failed`, `the server's certificate was not accepted`, `the request failed` | Connection and TLS failures |

The reason contains only the host, not the rest of the URL, because the path and query string often carry keys or user names. Every failure writes one WARN line, for example `script http_request refused: …`, `script http_request to 127.0.0.1: the connection was refused`; when the number of calls in progress is used up, it is `script http_request to 127.0.0.1: 64 already waiting`. The result is recorded in the `result` label of the metric `rradius_script_http_requests_total` (`ok`, `timeout`, `error`, `denied`, `too_large`, `busy`).

Login and accounting calls count their calls in progress separately, so no amount of accounting takes a slot from logins.

### url_encode

```text
url_encode(text) -> string
```

- **Parameters:** `text`, a string.
- **Returns:** every byte except English letters, digits and `-`, `.`, `_`, `~` is encoded as `%XX`, so the result is always just one piece of a URL. Tested: `url_encode("o'brien@corp é")` is `o%27brien%40corp%20%C3%A9`, and `url_encode('a b/c')` is `a%20b%2Fc`.
- **Errors:** an empty string, `.` and `..` are treated as a step of the path when the URL is parsed, however they are encoded, so they raise an error: `url_encode: '..' cannot be one piece of a URL`.

## Logging and utilities

### log

```text
log(...)
```

- **Parameters:** any number of values of any type.
- **Effect:** writes one INFO log line: `script <file name> (<protocol>/<phase>, client <client>): <message>`. The arguments are converted to text and joined with spaces; `nil`, booleans and numbers are written as Lua writes them, a table is written as `table: 0x…`; line breaks become spaces; anything over 4096 bytes is cut off.
- **Phases:** all.

```text
… INFO rradius_core::lua_engine: script policy.lua (radius/authentication, client localhost): refused bob
```

### now_unix

```text
now_unix() -> number
```

The current Unix time in seconds, as an integer value. For any time you write to the cluster database, compute it with this and pass it as a parameter.

### random_token

```text
random_token() -> string
```

16 random bytes, written as 32 lower-case hexadecimal characters, for example to use as the `State` of an Access-Challenge.

### totp_verify

```text
totp_verify(secret, code) -> number or nil
```

- **Parameters:** `secret`, the Base32 TOTP secret (upper or lower case is accepted, and the trailing `=` can be left out); `code`, the code the user typed. Use a string, because a leading `0` is lost in a number.
- **Rules:** RFC 6238, SHA-1, 6 digits, 30-second steps, one step before and after the current one is accepted.
- **Returns:** if the code is right, the number of that step (the Unix time divided by 30, as an integer); if it is wrong, `nil`. Record the steps that have been used, so that the same code cannot be used a second time.
- **Errors:** the secret is not Base32 or is empty: `totp_verify: the secret is not valid Base32`.

### wpa_pmk

```text
wpa_pmk(passphrase, ssid) -> string
```

- **Parameters:** `passphrase`, a passphrase of 8 to 63 printable ASCII characters, or a PSK of 64 hexadecimal characters; `ssid`, 1 to 32 bytes.
- **Returns:** the WPA2 PMK, 64 lower-case hexadecimal characters. Tested: `wpa_pmk('password', 'IEEE')` is `f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e`.
- **Note:** each call computes 4096 rounds of PBKDF2, which uses a few milliseconds of execution time.
- **Errors:**

| Case | Error message |
| --- | --- |
| The passphrase has the wrong length or characters | `wpa_pmk: the passphrase must be 8 to 63 printable ASCII characters, or the PSK 64 hex digits` |
| The SSID has the wrong length | `wpa_pmk: the SSID must be 1 to 32 octets` |
| Not UTF-8 text | `wpa_pmk: the passphrase is not text`, `wpa_pmk: the SSID is not text` |

### password_verify

```text
password_verify(stored, given) -> boolean
```

- **Parameters:** `stored`, the stored password, in the same format as the password column of the sql handler (a `{crypt}` or `{clear}` prefix, or a hash without a prefix; see chapter 07); `given`, the password the user typed.
- **Returns:** `true` if they match. If either is `nil`, the result is `false`; a format that is not recognized is also `false`, not an error.
- **Note:** it does not count failures, so `[auth.lockout]` has no control over it. When a script compares passwords itself, it must limit the number of guesses itself.

## CoA and Disconnect

The core has these two names, but calling them always fails.

### send_disconnect

```text
send_disconnect(target, attributes)
```

- **Parameters:** `target`, a string; `attributes`, a table mapping attribute names to a value or an array of values.
- **Behavior in the core:** the call always fails: `send_disconnect: not supported by this program (CoA needs rradius-enterprise)`. If the second argument is not a table, you first get an argument conversion error, for example `bad argument #2: error converting Lua string to table`.

### send_coa

```text
send_coa(target, attributes)
```

- **Parameters:** same as `send_disconnect`.
- **Behavior in the core:** the call always fails: `send_coa: not supported by this program (CoA needs rradius-enterprise)`.

> **Enterprise edition:** these two functions send a Disconnect-Request and a CoA-Request, and return `'ack'`, `'nak'` or `'timeout'`.

## Built-in libraries

The Luau built-in names a script can see (tested by listing every key of `_G`):

```text
_G _VERSION assert bit32 buffer collectgarbage coroutine debug error gcinfo
getmetatable ipairs math newproxy next os pairs pcall print rawequal rawget
rawlen rawset select setmetatable string table tonumber tostring type typeof
unpack utf8 vector xpcall
```

Removed:

| Name | State |
| --- | --- |
| `require`, `loadstring`, `getfenv`, `setfenv` | Do not exist; they are `nil` |
| `io` | Does not exist; a script cannot read or write files |
| `integer` | Does not exist; Luau removed this library (earlier builds of rradius had it), so a script that calls `integer.…` fails |
| `os.execute` and similar | Do not exist; `os.time()` and `os.clock()` can be used |

- `print` exists, but it writes to the program's standard output, not to the log, and under systemd or in a container you usually cannot see it. Always use `log`.
- `_G` is the table of the built-in libraries, not the request's global table. The names in this appendix are not in `_G`, and `_G.get_attr` is `nil`.

## Form of errors

When a function fails, the error that is thrown is a string. It starts with `runtime error: `, followed by the message, then `stack traceback:` and the call locations. If the script does not catch it, the request is rejected and the log gets one ERROR line:

```text
… ERROR rradius_core::lua_engine: Lua script error (radius/authentication): runtime error: probe.lua:40: something went wrong
stack traceback:
…
```

A script can catch the error with `pcall` and decide the result itself:

```lua
-- policy.lua (excerpt)
local ok, err = pcall(send_coa, 'localhost', { ['User-Name'] = 'alice' })
if not ok then
  log('coa failed', err)
end
```

In two cases, catching the error with `pcall` does not change the result:

- The execution time is exceeded: every later step raises the same error, so the result is always a failure.
- The database cannot be reached or times out: even if you catch it, the login gets no response (the log says `Discarded: database`), and TACACS+ returns ERROR, so that the NAS asks another server.

## Limits

| Limit | Setting | Default | When exceeded |
| --- | --- | --- | --- |
| The script's own execution time | `lua.max_execution_ms` | 1000 | `script exceeded the execution limit of 1000 ms` |
| Memory per VM | `lua.max_memory_mb` | 256 | `memory error: not enough memory` |
| The total time of all database calls in one request | `lua.query_timeout_ms` | 5000 | `sql_query timed out: the database calls of one request have 5000 ms together` |

- The execution time does not include time spent waiting for the database, for HTTP or for an accounting slot.
- `lua.query_timeout_ms` is shared by `sql_query`, `sql_execute`, `db_query` and `db_execute`; `http_request` has its own `lua.http.timeout_ms`.
- A change to `lua.max_memory_mb` needs a restart; the others take effect on reload.
- The number of VMs equals the number of CPUs the program can use. Accounting scripts run at most "number of VMs minus 2, at least 1" at the same time, and they give up their slot while they wait for the database or HTTP.

## How to compare

The names listed in this appendix must match the names the code registers. The comparison has two steps, run in the root directory of the core repo.

Step one: extract the names from the code mechanically. The keys of `setmetatable({ … }, meta)` in the `BINDER` constant, plus the names set by `env.<name> =`, are all the names in the script environment:

```console
$ python3 -I - rradius-core/src/lua_engine.rs <<'EOF' > /tmp/source.txt
import re, sys
src = open(sys.argv[1]).read()
binder = re.search(r'const BINDER: &str = r#"(.*?)"#;', src, re.S).group(1)
table = re.search(r'setmetatable\(\{(.*?)\}, meta\)', binder, re.S).group(1)
names = set(re.findall(r'^\s*([a-z_]+)\s*=', table, re.M))
names |= set(re.findall(r'env\.([a-z_]+)\s*=', binder))
print('\n'.join(sorted(names)))
EOF
$ wc -l < /tmp/source.txt
      27
```

Of the 27 names, `radsec_subject` and `radsec_subject_attrs` are the certificate information of a RadSec connection. The core has no RadSec, so they are always `nil`; this appendix does not list them, and the comparison leaves them out explicitly:

```console
$ grep -v -x -e radsec_subject -e radsec_subject_attrs /tmp/source.txt > /tmp/expected.txt
$ grep -E '^### [a-z_]+$' docs/manual/93-lua-reference.md | cut -c5- | sort > /tmp/book.txt
$ diff /tmp/expected.txt /tmp/book.txt && echo same
same
```

In this appendix, only the API names are all-lower-case English headings, so the `grep` on the second line picks up the list in the book.

Step two: confirm at run time. Generate a script from the list in the book that records the type of each name, put it in the quick-start (change `script` in `[lua]` to it, remove `[auth]`, add `[lua.http]`), and log in once:

```console
$ awk '{printf "log(\"%s\", type(%s))\n", $1, $1} END {print "return \"accept\""}' /tmp/book.txt > names.lua
```

The log from the test (excerpted format, 25 lines in all):

```text
… script names.lua (radius/authentication, client localhost): auth_user nil
… script names.lua (radius/authentication, client localhost): client string
… script names.lua (radius/authentication, client localhost): db_execute function
…
… script names.lua (radius/authentication, client localhost): wpa_pmk function
```

All 17 functions are `function`; `protocol`, `phase` and `client` are `string`, and `replayed` is `boolean`; `auth_user`, `eap_method`, `tls_subject` and `tls_subject_attrs` are `nil` in a PAP login to a top-level script. For when they have a value, see [Global variables](#global-variables).

When the code adds or removes a name, the `diff` in step one shows a difference.

## Common mistakes

**Symptom:** calling `http_request` or `url_encode` gives `attempt to call a nil value`.
**Cause:** the configuration has no `[lua.http]`.
**Fix:** add `[lua.http]` and `allowed_hosts`.

**Symptom:** `auth_user` is always `nil` in a top-level script.
**Cause:** only the `lua` handler in an auth chain gets it.
**Fix:** in a top-level script, use `get_attr('User-Name')`.

**Symptom:** `get_attr('Service-Type') == 'Framed-User'` is never true.
**Cause:** an enumeration is returned as a number.
**Fix:** write `== 2`. Only `set_attr` accepts the value name.

**Symptom:** `totp_verify` returns `nil` for the user's input `012345`.
**Cause:** the code was converted to a number, and the leading `0` was lost.
**Fix:** pass the string from `get_attr` as it is; do not use `tonumber`.

**Symptom:** the log has `sql_query: parameters must be an array, not string`.
**Cause:** the second argument was a plain value, for example `sql_query(sql, user)`.
**Fix:** wrap it in an array: `sql_query(sql, {user})`.

**Symptom:** the log has `db_query: there is no database 'billing' in [databases]`.
**Cause:** the name does not match `[databases.<name>]`, or the configuration has no such section.
**Fix:** compare it with the name in the configuration; after changing `[databases]`, restart.

**Symptom:** `_G.get_attr` is `nil`.
**Cause:** `_G` is the built-in libraries, not the request's global table.
**Fix:** write `get_attr` directly.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [07 User accounts and auth chains](07-users-and-chains.md)
- [09 External databases and named queries](09-external-databases.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [12 TACACS+ device administration](12-tacacs.md)
- [Appendix A: Configuration reference](90-config-reference.md)
