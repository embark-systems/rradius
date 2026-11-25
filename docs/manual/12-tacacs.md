# 12 TACACS+ device administration

> After this chapter you can:
> - Let router and switch administrators log in with TACACS+
> - Give privilege levels by role and limit the commands each person can run
> - Authorize commands without writing a script, using authorize groups (`authorize_groups`)
> - Record every login, logout and command in a table
> - Read TACACS+ reply statuses and logs, and fix common problems

Prerequisites: you have read [chapter 03](03-quick-start.md), can start the server and test with `rradius-bench`, and know what an auth chain is ([chapter 07](07-users-and-chains.md)). Authorization and accounting need a Lua script; see [chapter 10](10-lua-policies.md) for the syntax. Everything in this chapter runs on one machine, with `rradius-bench` acting as the device, so you do not need a real router.

In this chapter:

- [How TACACS+ differs from RADIUS](#how-tacacs-differs-from-radius)
- [Who decides a request](#who-decides-a-request)
- [Reply statuses](#reply-statuses)
- [Prepare the lab environment](#prepare-the-lab-environment)
- [Reading the device-admin configuration](#reading-the-device-admin-configuration)
- [Add devices](#add-devices)
- [Start and log in](#start-and-log-in)
- [Authentication types: ASCII, PAP, CHAP, MS-CHAP](#authentication-types-ascii-pap-chap-ms-chap)
- [enable: moving to a higher privilege level](#enable-moving-to-a-higher-privilege-level)
- [Without an auth chain: authenticate in the script](#without-an-auth-chain-authenticate-in-the-script)
- [Authorize with a script](#authorize-with-a-script)
- [Authorize with authorize groups](#authorize-with-authorize-groups)
- [Record every command](#record-every-command)
- [Messages for the user: server_msg](#messages-for-the-user-server_msg)
- [Connection management and limits](#connection-management-and-limits)
- [When the database is unavailable](#when-the-database-is-unavailable)
- [The device side](#the-device-side)
- [Watching TACACS+](#watching-tacacs)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## How TACACS+ differs from RADIUS

RADIUS is mostly used to give users network access: Wi-Fi, VPN, broadband. TACACS+ (RFC 8907) is used to let administrators manage devices: log in to a switch CLI, decide whether they may run `configure`, and record the commands they ran. rradius provides both in one program. `[radius]` and `[tacacs]` need at least one of them, and you can enable both.

How they differ in use:

| | RADIUS | TACACS+ |
| --- | --- | --- |
| Transport | UDP | TCP; one connection can carry several sessions |
| Encryption | Only the password field | The whole packet body is encrypted with the shared secret (the MD5 pad of RFC 8907) |
| AAA | Authentication and authorization in one reply | Authentication, authorization and accounting are three separate requests |
| Unit of authorization | All reply attributes are given at login | The device can ask again for every command |
| Settings in this chapter | `[radius]` (chapter 05) | `[tacacs]` |

The authorization and accounting requests that a device sends carry a list of AV pairs, for example `service=shell`, `cmd=show`, `cmd-arg=version`. Authorization replies are AV pairs too, for example `priv-lvl=15`. The device decides which AV pairs are meaningful; rradius only receives them, hands them to a script or to rules, and sends the result back.

> **Note:** TACACS+ encryption relies only on the shared secret, so its strength is limited. Protect any path outside the management network with a VPN or another method.
>
> The obfuscation in RFC 8907 uses only MD5 and the shared secret. It is not a replacement for real encryption. Keep the devices and rradius on a management network, use long shared secrets (a warning is logged at startup for fewer than 16 bytes; chapter 20), and give each device its own (use several `[[tacacs.clients]]` entries, each with its own `secret_file`).

> **Enterprise edition:** The enterprise edition also provides TACACS+ over TLS (RFC 9887).

## Who decides a request

When a device connects to the TACACS+ port, rradius first matches the source IP against `[[tacacs.clients]]`. If nothing matches, it closes the connection at once without reading a single byte. After that, **every packet** on the same connection is matched again, so if a reload changes the client list, connections that are already open follow the new list too.

Once a client matches, each of the three request types is decided by a different component:

| Request | With `[auth]` | Without `[auth]` |
| --- | --- | --- |
| Authentication | The client's auth chain: the `[auth.chains.<name>]` named by its `auth`, or the default chain if it names none | The top-level script, with `phase` set to `authentication` |
| Authorization | If the client has `authorize_group`: the rules of that authorize group. If not: the top-level script, with `phase` set to `authorization` | Same as the left column |
| Accounting | The top-level script, with `phase` set to `accounting` | Same as the left column |

- The "top-level script" is the one named by `[lua] script`. In the script, the global variable `protocol` is `"tacacs"` and `client` is the client's name.
- The auth chain handles authentication only. Authorization and accounting always go through the script, unless authorization is handed to an authorize group.
- The same script also receives RADIUS requests (if you enabled `[radius]`), so check `protocol` at the top of the script.

## Reply statuses

A TACACS+ reply carries a status. These are the values you see in logs, in test tools and on devices:

| Request | Status | When rradius answers with it |
| --- | --- | --- |
| Authentication | `PASS` | The auth chain or the script accepts |
| Authentication | `FAIL` | Rejected: wrong password, unknown account, enable refused, password too long, and so on |
| Authentication | `GETUSER`, `GETPASS` | During an ASCII login: asks for the user name, asks for the password |
| Authentication | `RESTART` | This authentication type cannot be checked here; asks the device to try another one |
| Authentication | `ERROR` | Cannot decide: database unavailable, packet body not valid |
| Authorization | `PASS_ADD` | Allowed, with the reply AV pairs attached |
| Authorization | `FAIL` | Denied |
| Authorization | `ERROR` | Cannot decide: database unavailable, packet not valid, reply has more than 255 AV pairs |
| Accounting | `SUCCESS` | The script returned `accept` and the record was written |
| Accounting | `ERROR` | The script did not accept |

The difference between `FAIL` and `ERROR` matters. `FAIL` means "the answer is no", and the device tells the administrator the login failed. `ERROR` means "this server cannot answer right now", and the device asks the next TACACS+ server in its configuration.

## Prepare the lab environment

This chapter uses the device-admin scenario. It enables TACACS+ only: administrator accounts live in the `admins` table of the cluster database, authentication goes to the sql handler, and authorization and accounting go to `tacacs.lua`. In the source tree it is at `rradius-server/examples/scenarios/device-admin/`.

As in chapter 03, copy it to a working directory first. Run these commands from the root of the source repository:

```console
$ export PATH="$PWD/target/release:$PATH"
$ mkdir -p ~/rradius-lab
$ cp -R rradius-server/examples/scenarios/device-admin ~/rradius-lab/
$ sed -i.bak "s#^dictionary = .*#dictionary = \"$PWD/share/dictionary\"#" ~/rradius-lab/device-admin/rradius.toml
$ cd ~/rradius-lab/device-admin
$ for f in raft api tacacs; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
```

- The third `.secret` is the shared secret of the TACACS+ client. The value that ships with the scenario is a public demo value, and `check` warns `TACACS+ client 'core-routers' has the public shared secret shipped with the examples`, so replace it right away.
- If the quick-start from chapter 03 is still running, stop it first: the two scenarios use the same cluster ports and metrics port.

Then change three places in `rradius.toml` so you can test locally:

```toml
# rradius.toml (only the parts to change)
[tacacs]
bind = "127.0.0.1"        # was "0.0.0.0"
port = 4949               # was 49
idle_timeout_ms = 300000

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"     # was "10.50.0.0/24"
secret_file = "tacacs.secret"
```

- 49 is the standard TACACS+ port. It is below 1024, so an ordinary account cannot listen on it, and the lab uses 4949 instead. When you need 49 in production, give the program the privilege it needs; see [chapter 02](02-install.md).
- `rradius-bench` connects from `127.0.0.1`, so the client address is changed to that.

```console
$ rradiusd check --config rradius.toml
configuration ok
```

## Reading the device-admin configuration

Without the comments, the configuration file looks like this (the dictionary path on the first line has already been replaced by `sed`):

```toml
# rradius.toml
dictionary = "…/share/dictionary"

[lua]
script = "tacacs.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_dir = "migrations"

[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM admins WHERE username = ? AND enabled = 1"

[tacacs]
bind = "127.0.0.1"
port = 4949
idle_timeout_ms = 300000

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"
secret_file = "tacacs.secret"

[metrics]
bind = "127.0.0.1"
port = 9812
```

Every key of `[tacacs]` (see [Appendix A](90-config-reference.md) for the full types and descriptions):

| Setting | Default | Reload | Meaning |
| --- | --- | --- | --- |
| `tacacs.bind` | `0.0.0.0` | restart | Address to listen on |
| `tacacs.port` | `49` | restart | TCP port to listen on |
| `tacacs.max_connections` | `1024` | restart | Limit on connections open at the same time; must be greater than 0 |
| `tacacs.max_connections_per_source` | `64` | restart | Limit on connections open at the same time from one source address |
| `tacacs.idle_timeout_ms` | `300000` | restart | Close the connection if no complete packet arrives for this long, in milliseconds; must be greater than 0 |
| `tacacs.max_body_len` | `65536` | restart | Limit on the packet body, in bytes; a larger body closes the connection; must be greater than 0 (0 fails at load time: `tacacs.max_body_len must be greater than 0`) |
| `tacacs.enable` | `deny` | yes | How to handle enable requests: `deny` or `login` |
| `tacacs.authorize_groups` | empty | yes | Authorize groups: a name mapped to a list of rules |
| `tacacs.clients` | none, at least one | yes | The devices allowed to connect |

Adding or removing the whole `[tacacs]` section needs a restart.

The demo accounts are in `migrations/2_demo_data.sql`. Passwords are stored as bcrypt hashes:

| Account | Password | Role | Authorization result (decided by `tacacs.lua`) |
| --- | --- | --- | --- |
| `netadmin1` | `netadmin1-password` | `netadmin` | Privilege level 15, all commands |
| `noc1` | `noc1-password` | `noc` | Privilege level 1, only the listed read-only commands |

The `admins` table also has the columns `role` and `enabled`. `user_query` has `AND enabled = 1`, so a disabled account is the same as one that does not exist.

> **Warning:** The accounts and passwords in `2_demo_data.sql` are public demo values. When you turn this scenario into a production setup, delete this file and replace it with your own data.

## Add devices

`[[tacacs.clients]]` uses the same field definitions as `[[radius.clients]]`, but a TACACS+ client uses only these:

| Key | Required | Meaning |
| --- | --- | --- |
| `address` | yes | The device's IP address, or a whole CIDR network |
| `name` | | Name used in logs and metrics; defaults to the value of `address` |
| `secret`, `secret_file` | exactly one of the two | The shared secret. The examples always use `secret_file` |
| `auth` | | Which `[auth.chains.<name>]` to use for this device's authentication; the default chain if not set |
| `authorize_group` | | Which authorize group's rules to authorize with (see [Authorize with authorize groups](#authorize-with-authorize-groups)) |

Any other field on a TACACS+ client makes `check` fail, for example `tacacs client 'lab': eap_methods applies only to RADIUS clients`. How addresses and networks are matched, and the warning for a short shared secret, are the same as for RADIUS clients; see [chapter 05](05-radius-clients.md).

All the devices in one network share one client entry, and so share one shared secret. If devices with different purposes (for example core routers and access switches in a machine room) need different secrets, different auth chains or different authorization rules, split them into separate entries:

```toml
# conf.d/tacacs-clients.toml
[[tacacs.clients]]
name = "core-routers"
address = "10.50.0.0/24"
secret_file = "core.secret"        # openssl rand -hex 32 > core.secret

[[tacacs.clients]]
name = "access-switches"
address = "10.60.0.0/16"
secret_file = "access.secret"      # openssl rand -hex 32 > access.secret
auth = "staff"                     # [auth.chains.staff] must exist first
authorize_group = "noc"            # tacacs.authorize_groups.noc must exist first
```

The client list can be reloaded: after you send `SIGHUP`, both new connections and connections that are already open are matched against the new list. When a source address no longer matches the new list, its connection is closed when the next packet arrives.

## Start and log in

```console
$ rradiusd serve --config rradius.toml
… INFO rradius_core::cli: cluster node 1 started
… INFO rradius_core::cli: cluster node 1 has caught up with the cluster
… INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
… INFO rradius_core::tacacs: TACACS+ Server listening on 127.0.0.1:4949
… INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

The first start also logs two hiqlite lines, `No metadata exists inside the DB yet`. This is normal (chapter 03).

In another terminal, use `rradius-bench` as the device and send one PAP login:

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user netadmin1 --password netadmin1-password --load tacacs-pap,concurrency=1 --requests 1
measured 0.049 s after 0.000 s of warmup

tacacs-pap (fixed-concurrency): concurrency 1, 1 threads, 1 sockets over tcp, busiest worker 0%
  tacacs-pap: 20/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    latency us: p50 48922.6, p90 48922.6, p99 48922.6, p99.9 48922.6, max 48922.6
```

- `--tacacs-port` is the TACACS+ port; the default is 49.
- `accept 1` means a `PASS` was received; `reject 1` is a `FAIL`. The exit codes are the same as in chapter 03: 0 all accepted, 1 some rejected, 2 timeouts or invalid replies, 3 bad arguments and nothing sent.
- Passwords are stored as bcrypt, and checking one takes time, so the rate is only a few dozen per second. This is normal.

`rradius-bench` has five TACACS+ loads:

| `--load` | Request sent |
| --- | --- |
| `tacacs-pap` | PAP authentication, with the user name and password in the first packet |
| `tacacs-chap` | CHAP authentication; the first packet carries the PPP id, a 16-byte challenge and the response. The server's auth chain must be able to check the password; otherwise the server answers `RESTART` (see "When `RESTART` is returned" above) and the bench counts it as `invalid` |
| `tacacs-ascii` | ASCII authentication: waits for `GETUSER`, then for `GETPASS`; three packets in all |
| `tacacs-author` | Authorization request, with the AV pairs `service=shell`, `cmd=show`, `cmd-arg=version` |
| `tacacs-acct` | Accounting START, with the AV pairs `task_id`, `start_time`, `service=shell` |

Try a few more:

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --password wrong --load tacacs-pap,concurrency=1 --requests 1
…
  tacacs-pap: 25/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --password noc1-password --load tacacs-ascii,concurrency=1 --requests 1
…
  tacacs-ascii: 26/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
    rounds per finished conversation: 3.0
…
```

The server log has one line per authentication:

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="netadmin1" account="netadmin1" chain="default" handler="sql" decision="accept"
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="noc1" account="noc1" chain="default" handler="sql" decision="reject"
```

The format is the same as the RADIUS authentication record, except for `protocol="tacacs"`. Authorization and accounting do not write this record.

`rradius-bench` does not print the content of authorization replies. To see the reply AV pairs, or to send requests in ways `rradius-bench` cannot, use the `tacacs_client` that comes with the Python package `tacacs_plus` (`pip install tacacs_plus`):

```console
$ K="$(cat tacacs.secret)"
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authenticate          # enter noc1-password
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=
$ tacacs_client -H 127.0.0.1 -p 4949 -k $K -u noc1 authorize -c service=shell cmd=show cmd-arg=running-config
```

> **Not verified on real hardware:** The `tacacs_client` commands come from the previous manual and were not re-run for this version; its arguments may differ between versions, so go by `tacacs_client --help`. The authorization results in the tables of this chapter were measured with a test client that can decode the reply (see "Authorize with a script").

## Authentication types: ASCII, PAP, CHAP, MS-CHAP

When a device sends an authentication request, it says which type (`authen_type`) it uses. How rradius handles each:

| Type | Handling |
| --- | --- |
| ASCII | Interactive login. If no user name came with the request, it answers `GETUSER` (prompt `Username: `), then `GETPASS` (prompt `Password: `, and asks the device not to echo the input). Only when it has both does it hand the login to the auth chain |
| PAP | The user name and password are in the first packet and go straight to the auth chain |
| CHAP | The auth chain must contain a handler that can check the password, and uses it to check the CHAP response |
| MS-CHAP (v1), MS-CHAPv2 | Same as CHAP |
| ARAP | Always answers `RESTART` |

**Limits of ASCII login** (fixed values, not settings):

- If the user name is blank, it asks again; three blank answers in a row get `FAIL`.
- One connection can have at most 8 ASCII logins unfinished at the same time; any more get `ERROR`.
- An ASCII login must finish within 60 seconds, or it is discarded.
- When the device sends ABORT, the login ends with no reply, and the connection stays open.

A measured ASCII login, three questions and three answers:

```text
GETUSER  server_msg="Username: "
GETPASS  server_msg="Password: " (NOECHO)
PASS
```

**Length limits** (fixed values): a password can be at most 128 bytes, and the user name given as an answer in an ASCII login at most 255 bytes. Longer ones get `FAIL` at once, never reach the auth chain, and do not count toward the failure count of `[auth.lockout]`. The log records only the length, never the content:

```text
… INFO rradius_core::tacacs: TACACS+ authentication refused: the password is longer than 128 octets client="core-routers" user="lab1" authen_type=2 length=129
… INFO rradius_core::tacacs: TACACS+ authentication refused: the username is too long client="core-routers" user="" authen_type=1 length=256
```

**CHAP and MS-CHAP need a password the server can compare.** A CHAP response is computed from the clear-text password, so the server must hold the clear text (or `{clear}`) to check it; the MS-CHAP family needs the clear text or an NT hash. An account stored as bcrypt always fails a CHAP login, and the log shows `handler="no_password_check"`:

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="netadmin1" account="netadmin1" chain="default" handler="no_password_check" decision="reject"
```

Which password formats work for CHAP and MS-CHAP, and how `auth.allow_mschapv1` turns on MS-CHAPv1 (off by default), are covered in [chapter 07](07-users-and-chains.md). PAP and ASCII work with any format.

**When `RESTART` is returned:** In the cases below, rradius answers `RESTART` without running the auth chain or the script, so the device tries another type (usually PAP or ASCII):

- The type is ARAP.
- The type is CHAP or an MS-CHAP type, but this client's auth chain has no handler that can check the password (sql, ldap, or a lua handler with `verifies_password` set). The same holds when there is no `[auth]` and everything is left to the script: the script has no password to compare.
- The type is CHAP or an MS-CHAP type, and the request is an enable request.

A CHAP challenge shorter than 8 bytes, an MS-CHAP challenge that is not 8 bytes, or an MS-CHAPv2 challenge that is not 16 bytes gets `FAIL` and one log line.

An authentication action that is not LOGIN (change password CHPASS, SENDPASS, SENDAUTH) always gets `FAIL`, and the log says `TACACS+ authentication refused: only LOGIN is supported`.

> **Not verified on real hardware:** MS-CHAP and MS-CHAPv2 were checked only against the code and the tests; nobody has logged in with a real device or client. PAP, CHAP and ASCII were measured locally with `rradius-bench` (CHAP with the `tacacs-chap` load against a server that has an auth chain: correct password `accept`, wrong password `reject`), but none of them was verified with a real device.

## enable: moving to a higher privilege level

When you type `enable` on a device, the device sends an authentication request whose service field is ENABLE. `tacacs.enable` decides the answer:

| Value | Behavior |
| --- | --- |
| `deny` (default) | Always answers `FAIL`; does not ask for a password, run the auth chain or run the script |
| `login` | Treated as an ordinary login; the auth chain or the script decides |

With the default `deny`, the log is:

```text
… INFO rradius_core::tacacs: TACACS+ enable request refused: [tacacs].enable is "deny" client="core-routers" user="netadmin1" priv_lvl=15
```

Change it to `login`:

```toml
# rradius.toml (add one line in [tacacs])
[tacacs]
bind = "127.0.0.1"
port = 4949
idle_timeout_ms = 300000
enable = "login"
```

`enable` can be reloaded. After you send `SIGHUP`, `netadmin1` gets `PASS` when it enables with its own password, and `FAIL` when the password is wrong. Any other value makes `check` fail: `tacacs.enable must be "deny" or "login", not "ask"`.

`login` uses the login password, not a separate enable password. The auth chain and the script can see two numeric attributes, `authen_service` (2 for enable) and `priv_lvl` (the level to move to). To handle enable separately, look at these two values in a lua handler or in the script. The example in the next section does this.

**Choosing on the device side:**

- Do not hand enable to rradius unless you set `enable = "login"`. The default `deny` answers `FAIL`, and a device that receives `FAIL` does not fall back to its local enable password.
- To keep the local enable password: on the device, use `aaa authentication enable default enable`. The privilege level is decided by the `priv-lvl` in the authorization reply at login, so administrators usually do not need enable.
- To use `login`: on the device, use `aaa authentication enable default group RRADIUS enable` (Cisco IOS syntax, not verified on real hardware). The administrator then enables with their own login password, so restrict it in the script or lua handler, for example by letting only the `netadmin` role pass when `authen_service == 2`.

## Without an auth chain: authenticate in the script

device-admin authenticates with the sql handler, which is the simplest way. Sometimes you want to look at more at authentication time, for example "only netadmin can enable". In that case you can remove `[auth]` and let the top-level script authenticate by itself.

Delete the `[auth]` and `[[auth.handlers]]` sections from `rradius.toml`, add `enable = "login"` to `[tacacs]`, and then add the authentication part to `tacacs.lua` after `local user = get_attr('user')`:

```lua
-- tacacs.lua (added after local user = get_attr('user'))
if phase == 'authentication' then
  local rows = sql_query("SELECT password, role FROM admins WHERE username = ? AND enabled = 1", {user})
  local account = rows[1]
  if not account or not password_verify(account.password, get_attr('data')) then
    return 'reject'
  end
  -- enable (authen_service 2) only for netadmin
  if get_attr('authen_service') == 2 and account.role ~= 'netadmin' then
    set_attr('server_msg', 'enable is for netadmin only')
    return 'reject'
  end
  return 'accept'
end
```

- During authentication, `user` in the script is the account and `data` is the password (for PAP, the password in the first packet; for ASCII, the answer to `GETPASS`). `authen_type` is the number of the authentication type (1 for ASCII, 2 for PAP).
- `password_verify` checks the stored password the same way the auth chain does, and works with bcrypt and the other formats (chapter 10).

Measured after sending `SIGHUP`:

| Request | Reply |
| --- | --- |
| `noc1` logs in with PAP | `PASS` |
| `noc1` with a wrong password | `FAIL` |
| `netadmin1` logs in with ASCII | `GETUSER`, `GETPASS`, `PASS` |
| `noc1` enable | `FAIL`, with `server_msg` set to `enable is for netadmin only` |
| `netadmin1` enable | `PASS` |
| Anyone with CHAP | `RESTART` (the script has no password to compare CHAP against) |

The authentication record in the log becomes `chain="script" handler="script"`:

```text
… INFO rradius_auth: authentication protocol="tacacs" client="core-routers" source=127.0.0.1 user="noc1" account="noc1" chain="script" handler="script" decision="accept"
```

> **Note:** When you authenticate in the script, the login failure lockout of `[auth.lockout]` does not apply: `password_verify` does not count failures. When you need lockout, use an auth chain (chapter 07).

When only some devices need special authentication rules, you do not have to remove `[auth]` altogether. Define a chain with a lua handler in `[auth.chains.<name>]` and let the clients of those devices pick it with `auth` (chapter 07).

## Authorize with a script

When an administrator has logged in and the device wants to open a shell, it sends an authorization request; after that it sends one more for every command (if the device is set up for command authorization). When the client has no `authorize_group`, these requests go to the top-level script, with `phase` set to `authorization`.

The attributes the script can see:

| Attribute | Content |
| --- | --- |
| `user`, `port`, `rem_addr` | The account, the terminal name on the device, and the address the administrator connected from |
| Each AV pair | Read by name, for example `get_attr('service')` is `shell`. `name=value` and `name*value` are read the same way |
| Repeated AV pairs | Use `get_attrs`, for example `get_attrs('cmd-arg')` returns all the arguments |
| `priv_lvl`, `authen_method`, `authen_type`, `authen_service` | Numbers from the request header |

What the script returns:

- `accept`: answers `PASS_ADD`. Each attribute the script set with `set_attr` becomes one `name=value` AV pair in the reply.
- Anything else: answers `FAIL`.

This is how device-admin's `tacacs.lua` decides (the key parts):

```lua
-- tacacs.lua (excerpt)
if protocol ~= 'tacacs' then
  return 'reject'
end
local user = get_attr('user')
-- … (accounting part omitted)
local rows = sql_query("SELECT role FROM admins WHERE username = ? AND enabled = 1", {user})
local role = rows[1] and rows[1].role
if not role or get_attr('service') ~= 'shell' then
  return 'reject'
end
local cmd = get_attr('cmd')
if not cmd or cmd == '' then
  set_attr('priv-lvl', role == 'netadmin' and 15 or 1)   -- opening a shell: return the privilege level
  return 'accept'
end
if role == 'netadmin' then
  return 'accept'
end
-- noc: allow only the commands listed in READ_ONLY; show may be followed only by items listed in SHOW
```

- No `cmd` (or an empty `cmd`) means the device wants to open a shell, and the script returns the privilege level.
- `noc` uses an allow list, not a deny list: commands can be written in too many ways to list them all.

Measured results (sent with a test client that can decode the reply AV pairs; `rradius-bench` does not print the reply content):

| Account | AV pairs sent | Reply |
| --- | --- | --- |
| `netadmin1` | `service=shell cmd=` | `PASS_ADD`, `priv-lvl=15` |
| `noc1` | `service=shell cmd=` | `PASS_ADD`, `priv-lvl=1` |
| `noc1` | `service=shell cmd=show cmd-arg=version` | `PASS_ADD`, no AV pairs |
| `noc1` | `service=shell cmd=show cmd-arg=running-config` | `FAIL` |
| `noc1` | `service=shell cmd=configure` | `FAIL` |

You can also test with `rradius-bench`, which sends `show version`:

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --load tacacs-author,concurrency=1 --requests 1
…
  tacacs-author: 2006/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user nobody --load tacacs-author,concurrency=1 --requests 1
…
  tacacs-author: 5916/s, sent 1, accept 0, reject 1, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`tacacs-author` does not need `--password`: an authorization request carries no password, and the device sends it only after a successful login.

Limits on the reply (fixed values): one AV pair, name included, is at most 255 bytes, and a `set_attr` with a value that is too long makes the script fail. One reply holds at most 255 AV pairs; more than that answers `ERROR` and logs `has … AV pairs, more than the 255 a reply holds`.

## Authorize with authorize groups

Many devices need only simple rules: "on this group of switches, only these commands are allowed". For this you can skip the script and use an authorize group.

Authorize groups are written in `[tacacs.authorize_groups]`: each key is a group name, and its value is an array of rule strings. A client picks a group with `authorize_group`:

```toml
# rradius.toml (add after [tacacs], before [[tacacs.clients]])
[tacacs.authorize_groups]
noc = [
  "permit service=shell cmd= {priv-lvl=1 autocmd=\"show users\"}",
  "deny service=shell cmd=configure",
  "permit service=shell cmd=show cmd-arg=(version|interfaces|clock)",
  "permit service=shell cmd=(ping|traceroute|exit)",
]

[[tacacs.clients]]
name = "core-routers"
address = "127.0.0.1"
secret_file = "tacacs.secret"
authorize_group = "noc"
```

How each rule is written:

```text
permit|deny  pattern pattern …  {reply AV pairs …}
```

- The first word is `permit` or `deny`.
- Then come zero or more patterns, separated by spaces. Each pattern is a regular expression that must match **the whole** of one AV pair in the request (`name=value` or `name*value`, as the device sent it). `cmd=` matches only an empty `cmd=`, never `cmd=show`.
- A rule applies only if each of its patterns matches some AV pair.
- At the end you can add a `{…}` block with the AV pairs to send back for `permit`, separated by spaces and sent in the order written. `deny` cannot have a reply.
- Put a value that contains a space or `}` in double quotes, for example `autocmd="show users"`. The double quotes are sent to the device as they are.
- Patterns do not understand double quotes, and they do not match the three fields `user`, `port` and `rem_addr`.

How it decides: it looks, in the order written, for **the first rule that applies**. If it is `permit`, the answer is `PASS_ADD` with its reply; if it is `deny`, the answer is `FAIL`; if no rule applies, the answer is also `FAIL`.

Measured results for the group above:

| AV pairs sent | First rule that applies | Reply |
| --- | --- | --- |
| `service=shell cmd=` | Rule 1 | `PASS_ADD`, `priv-lvl=1`, `autocmd="show users"` |
| `service=shell cmd=configure` | Rule 2 | `FAIL` |
| `service=shell cmd=show cmd-arg=version` | Rule 3 | `PASS_ADD`, no AV pairs |
| `service=shell cmd=show cmd-arg=running-config` | None | `FAIL` |
| `service=shell cmd=ping cmd-arg=192.0.2.1` | Rule 4 | `PASS_ADD`, no AV pairs |
| `service=ppp protocol=ip` | None | `FAIL` |

Things to know:

- For a client with `authorize_group`, authorization is decided **entirely** by the rules. The top-level script does not run, so there is no `server_msg` either. Authentication and accounting work as before.
- An authorize group is a group of devices, not a user group in LDAP or a database. The rules cannot see who is running the command: on the same device, `netadmin1` and `noc1` get the same answer. To authorize by the user's role, use the script from the previous section.
- Groups can be reloaded, and are replaced together with the client list. After you change the rules and send `SIGHUP`, the next authorization request follows the new rules (measured).
- A group can have no rules, which means everything is denied.

**Points to watch when writing rules:**

- **Use an allow list, not a deny list.** Rules match only the AV pairs the device sends. A subcommand that is not listed (for example `show tech-support`) and an abbreviation typed on the device (for example `show run` instead of `show running-config`) are not caught by `deny cmd=show cmd-arg=running-config`, but they match `permit cmd=show`. The `noc` group above permits only the listed commands, and everything else falls into "no rule applies" and is denied, which is safer. What a device actually sends for an abbreviation depends on the device (not verified on real hardware).
- **Arguments are not matched one by one.** Each pattern of a rule applies as soon as it matches **any** AV pair in the request, so `cmd-arg=interfaces` also lets through `show interfaces` followed by other arguments. For a device to be really read-only, rely on what the privilege level on the device itself can do; the authorization rules are a second line of defense.
- **A device may send `cmd*` when it opens a shell.** The separator in an AV pair is `=` (mandatory) or `*` (optional), and the match follows what the device sent. When the device sends `cmd*` (with an empty value), write the pattern as `cmd\*`; writing `cmd=` does not apply:

  ```toml
  [tacacs.authorize_groups]
  core = [
    'permit service=shell cmd\* {priv-lvl=15}',
    'permit service=shell cmd=.*',
  ]
  ```

- **Always write rules in single quotes.** `\*` is an invalid escape in a TOML double-quoted string, so the configuration file cannot be read. A single-quoted string does no escape processing and hands the backslash to the pattern unchanged. If you write the rules above in double quotes, `check` fails with `rradius.toml: line 39: missing escaped value, expected …` (followed by the list of valid escape characters; the line number is the line in your file). The rules earlier in this section use double quotes because they contain only `\"`, an escape TOML already understands; to let the pattern see a backslash, use single quotes.

When a rule is wrong, startup, reload and `check` all fail and name the group and the rule number (counted from 1). When a reload fails, the server keeps running with its previous configuration:

| What you wrote | Error message |
| --- | --- |
| `"allow service=shell"` | `tacacs.authorize_groups.ops: rule 1: starts with 'allow'; it has to start with permit or deny` |
| `"permit service=shell cmd=(show"` | `tacacs.authorize_groups.ops: rule 1: pattern 'cmd=(show' is not a valid regular expression: …` |
| `"permit service=shell {priv-lvl=15"` | `tacacs.authorize_groups.ops: rule 1: the reply has no closing }` |
| `"permit service=shell {priv-lvl=15} x"` | `tacacs.authorize_groups.ops: rule 1: text after the reply's closing }: ' x'` |
| `"deny service=shell {priv-lvl=15}"` | `tacacs.authorize_groups.ops: rule 1: deny cannot have a reply: FAIL carries no AV pairs` |
| `"permit service=shell {priv-lvl}"` | `tacacs.authorize_groups.ops: rule 1: reply AV pair 'priv-lvl' has no = or *` |
| Unpaired double quotes in the reply | `tacacs.authorize_groups.ops: rule 1: the reply's double quotes are not paired` |
| `authorize_group = "nosuch"` | `tacacs client 'lab': unknown authorize_group 'nosuch' (no tacacs.authorize_groups.nosuch)` |
| `authorize_group` in `[[radius.clients]]` | `radius client '<name>': authorize_group applies only to TACACS+ clients` |

Each message is also prefixed with the configuration file name, for example `rradius.toml: `.

## Record every command

When a device is set up for command accounting, it sends an accounting request for every login, every logout and every command. rradius hands it to the top-level script, with `phase` set to `accounting`. Only when the script returns `accept` does it answer `SUCCESS`; any other result answers `ERROR`, so the device knows the record was not saved.

The AV pairs in an accounting request are decided by the device. Common ones are:

| AV pair | Meaning |
| --- | --- |
| `task_id` | The number of this task; the start and the stop use the same one |
| `start_time`, `stop_time` | The start or stop time; a request with neither is an intermediate update (watchdog) |
| `cmd`, `cmd-arg` | The command and its arguments; there can be several arguments |
| `service` | For example `shell` |
| `elapsed_time` and others | Depends on the device |

device-admin writes them into the `tacacs_log` table:

```lua
-- tacacs.lua (excerpt)
if phase == 'accounting' then
  local event = (get_attr('stop_time') and 'stop') or (get_attr('start_time') and 'start') or 'update'
  local cmd = get_attr('cmd') or ''
  for _, arg in ipairs(get_attrs('cmd-arg')) do
    if arg ~= '<cr>' then
      cmd = cmd .. ' ' .. arg
    end
  end
  sql_execute([[INSERT OR IGNORE INTO tacacs_log (device, task_id, event, username, rem_addr, cmd, logged)
                VALUES (?, ?, ?, ?, ?, ?, ?)]],
    {client, get_attr('task_id') or '', event, user or '', get_attr('rem_addr') or '', cmd, math.floor(now_unix())})
  return 'accept'
end
```

```sql
-- migrations/1_schema.sql (excerpt)
CREATE TABLE tacacs_log (
  device TEXT NOT NULL,          -- name of the [[tacacs.clients]] entry
  task_id TEXT NOT NULL,
  event TEXT NOT NULL,           -- 'start', 'stop' or 'update'
  username TEXT NOT NULL,
  rem_addr TEXT NOT NULL,        -- where the administrator connected to the device from
  cmd TEXT NOT NULL,             -- the full command; an empty string for login and logout
  logged INTEGER NOT NULL,
  PRIMARY KEY (device, task_id, event)
);
CREATE INDEX tacacs_log_user ON tacacs_log (username, logged);
```

- The command and its arguments are sent separately; the script puts them back into one line and drops the `<cr>` the device adds.
- The primary key is `(device, task_id, event)`, together with `INSERT OR IGNORE`: when a device resends the same record, only one row stays in the table. An accounting script must be safe to run more than once; the reason is in [chapter 13](13-accounting.md).
- **`tacacs_log` grows without limit, and rradius never cleans it up.** Every command is one row. Delete old rows regularly, the same way as for accounting tables; see [chapter 18](18-operations.md). How long to keep them depends on your company's audit requirements; check before you delete.

A measured accounting request for `show version` (AV pairs: `task_id=42 start_time=1795000000 service=shell cmd=show cmd-arg=version cmd-arg=<cr>`) got the reply `SUCCESS`, and this row appeared in the table:

```text
device=core-routers
task_id=42
event=start
username=noc1
rem_addr=192.0.2.10
cmd=show version
logged=1795384714
```

Send it with `rradius-bench`:

```console
$ rradius-bench run --server 127.0.0.1 --tacacs-port 4949 --secret "$(cat tacacs.secret)" \
    --user noc1 --load tacacs-acct,concurrency=1 --requests 1
…
  tacacs-acct: 3185/s, sent 1, accept 0, reject 0, challenge 0, accounting 1, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

`accounting 1` means a `SUCCESS` was received.

To turn these records into metrics such as "how many commands each person ran per day", use named queries ([chapter 09](09-external-databases.md)).

> **Note:** TACACS+ accounting has no accounting spool, and is not written to `[radius.accounting_log]`: both of those handle RADIUS accounting only. When the database cannot be written, the script fails and the device receives `ERROR`; it is up to the device to resend or to try the next server.

## Messages for the user: server_msg

A value set by the script with `set_attr('server_msg', '…')` is put into the `server_msg` field of the reply, and most devices show it to the administrator. It applies to:

- The `PASS` and `FAIL` of an authentication reply. When authentication is accepted, a `server_msg` set by a handler of the auth chain counts too.
- The `PASS_ADD` and `FAIL` of an authorization reply. It does not become an AV pair and does not count toward the 255-byte limit on AV pairs.
- The `SUCCESS` and `ERROR` of an accounting reply.

If it is set several times, the last value is used. A value longer than 1024 bytes is not sent; the log gets one ERROR line, but the decision stays as it was. An authorization decided by an authorize group has no `server_msg`.

For example, add one line in `tacacs.lua` before it rejects a `noc` command:

```lua
-- tacacs.lua (excerpt)
if not READ_ONLY[cmd] then
  set_attr('server_msg', 'noc accounts may only look')
  return 'reject'
end
```

In the measured run, `configure` from a `noc` account got `FAIL`, with `server_msg` set to `noc accounts may only look`.

## Connection management and limits

**Single-connection mode.** When a device sets the single-connection flag in the first packet of a connection, rradius sets the same flag in its first reply, which means it agrees to run several sessions on that connection. Later replies do not carry the flag. With or without the flag, rradius accepts several sessions on the same connection. Measured: the flag in the first reply is `0x4`, in the second `0x0`, and both logins are `PASS`.

**Connection limits.**

- `max_connections`: the limit on all connections. At the limit, a new connection is closed at once.
- `max_connections_per_source`: the limit for one source address. An IPv4 address and its IPv4-mapped IPv6 address count as one source.
- Connections that are not from a client in the configuration do not count toward either limit.
- A closed connection writes at most one log line per second and is counted in `rradius_tacacs_connections_total{result="refused"}`.

Measured with `max_connections_per_source = 1` and two connections at the same time, the second one is closed:

```text
… WARN rradius_core::tacacs: TACACS+ connection from 127.0.0.1 closed: it already holds max_connections_per_source = 1 connections
```

> **Note:** When many devices connect from the same address through NAT, the default of 64 may not be enough; raise `max_connections_per_source`.

**Idle and write timeouts.** A connection that receives no complete packet for `idle_timeout_ms` (default 300000 ms, which is 5 minutes) is closed; the time a script spends running is not counted as idle. A connection is also closed when a reply cannot be written within 10 seconds (fixed value). A closed connection gives up its place in `max_connections`.

**Packet checks.** In these cases the connection is closed without a reply:

- The packet body is longer than `max_body_len`.
- The version is not the TACACS+ major version.
- The sequence number is even or 255 (RFC 8907 says the device sends only odd numbers).
- The "unencrypted" flag is set: rradius always needs a shared secret.

When a packet can be decoded but a field is not valid (privilege level above 15, authentication type above 6, service above 9, an accounting request with none of the START, STOP or WATCHDOG flags, and so on), rradius answers `ERROR` for that type of request without running the auth chain or the script, and counts it in `rradius_tacacs_malformed_total`. When the body cannot be decoded at all (most often because the shared secrets differ), it answers `ERROR` and then closes the connection; see [Common mistakes](#common-mistakes).

## When the database is unavailable

When the cluster database is unavailable (for example a call times out, or the script has to write and the cluster has no leader) and a decision cannot be made, authentication and authorization answer `ERROR`, not `FAIL`, so the device asks the next TACACS+ server. When the data itself says "no" (unknown account, wrong password), the answer is still `FAIL`. The log:

```text
TACACS+ authentication for client <name> answered ERROR: the authentication backend could not be used
TACACS+ authorization for client <name> answered ERROR: the cluster database could not be used
```

When the cluster has `[cluster] stop_without_leader_ms` set and this node has had no leader for longer than that, authentication and authorization always answer `ERROR`, and the script does not even run. Accounting is still handed to the script; when the script cannot write to the database it answers `ERROR`. For this setting and the behavior of the cluster, see [chapter 16](16-cluster.md).

So set at least two TACACS+ servers on the device, ideally pointing to different nodes of the cluster.

> **Not verified on real hardware:** Authentication answering `ERROR` with `stop_without_leader_ms` set was measured in [chapter 16](16-cluster.md), which simulates three nodes on one machine. Authorization, accounting, and the replies when a database call times out were checked only against the code and the specification, not on a real multi-machine cluster.

## The device side

Every vendor's commands are different. This section lists only what must be configured on the device; for the actual commands, see the device's documentation:

- **Server address, port and key.** The address is that of an rradius node, the port is `tacacs.port` (usually 49 in production), and the key is the content of the client's `secret_file`.
- **At least two servers.** When the cluster has several nodes, point them to different nodes. The device asks the next server only when rradius answers `ERROR` or does not answer.
- **Use TACACS+ for authentication (login), and keep a local account as the last fallback.** When none of the TACACS+ servers can be reached, administrators can still log in with a local account on the device.
- **Authorization:** authorize when opening a shell (exec) to get `priv-lvl`; to check each command, also turn on command authorization.
- **Accounting:** turn on accounting for exec and for commands, so you get records of logins, logouts and every command.
- **Source address.** The source interface the device uses to send TACACS+ must match the `address` of the `[[tacacs.clients]]` entry.

As an example with Cisco IOS, here is a set of commands that ties these items together; replace the address and the key with your own:

```text
aaa new-model
tacacs server rradius-1
 address ipv4 10.50.0.100
 key <content of tacacs.secret>
aaa group server tacacs+ RRADIUS
 server name rradius-1
!
aaa authentication login default group RRADIUS local
aaa authorization exec default group RRADIUS local
aaa authorization commands 1 default group RRADIUS local
aaa authorization commands 15 default group RRADIUS local
aaa accounting exec default start-stop group RRADIUS
aaa accounting commands 1 default stop-only group RRADIUS
aaa accounting commands 15 default stop-only group RRADIUS
```

> **Warning:** Always keep `local` as the fallback, and make sure the local emergency account can log in before you change the configuration. When rradius is down or the network is cut, the device falls back to the local account; without a fallback you lock yourself out of the device. Turn on authentication only (`aaa authentication login`) first and confirm you can log in, then turn on exec authorization, and turn on command authorization (`aaa authorization commands`) last: with a wrong authorization rule, even `exit` may be denied. With several rradius servers, list several `server name` lines in `aaa group server`; the device tries them in order.

> **Not verified on real hardware:** This chapter was not tested with a real router or switch. The above lists the settings TACACS+ devices usually need; the IOS commands come from the previous manual and may differ between versions, so confirm them on a lab device before going live.

## Watching TACACS+

`status` has a TACACS+ section:

```console
$ rradiusd status --config rradius.toml
…
TACACS+
  Open connections  0
  Authentication    0.0/s (11)
  Authorization     0.0/s (19)
  Accounting        0.0/s (2)
  Malformed         0.0/s (0)
…
```

The matching metrics are `rradius_tacacs_connections_open`, `rradius_tacacs_connections_total{result}`, `rradius_tacacs_requests_total{type}`, `rradius_tacacs_malformed_total` and `rradius_tacacs_response_seconds`. An ASCII login counts only once in `requests_total{type="authen"}`, however many packets go back and forth. For the meaning of each metric, see [chapter 17](17-monitoring.md) and [Appendix C](92-metrics-reference.md).

When `Malformed` goes up, the most common cause is a wrong shared secret on one device.

## Common mistakes

**Symptom:** `rradius-bench` prints `lengths: the fields of the reply do not add up to its length, so the shared secret may be wrong`, the result is `invalid 1`, and the exit code is 2. Server log: `TACACS+ connection from 127.0.0.1 closed: a body did not decode, so the shared secret may be wrong`.
**Cause:** The shared secret on the device and the one on the client differ. `rradius_tacacs_malformed_total` goes up.
**Fix:** Make sure the key on the device is the same as the content of `secret_file`. The newline at the end of the file does not count as part of the secret.

**Symptom:** A device cannot connect, and the server log says `TACACS+ connection from unknown client 127.0.0.1 closed`.
**Cause:** The source address is not in the `address` of any `[[tacacs.clients]]`. Behind NAT, the address seen is the one after NAT.
**Fix:** Add that address or network to a client and send `SIGHUP`.

**Symptom:** Sometimes the connection works and sometimes it is dropped, with the log `TACACS+ connection from 127.0.0.1 closed: it already holds max_connections_per_source = 1 connections` (the address and number are the values from the measured run; you see your own source and setting).
**Cause:** One source opened too many connections, commonly several devices behind NAT, or a monitoring script that does not close its connections.
**Fix:** Raise `max_connections_per_source`; this takes a restart to apply.

**Symptom:** A CHAP login fails with the log `handler="no_password_check" decision="reject"`, while the same account can log in with PAP.
**Cause:** The account's password is stored as a hash (for example bcrypt), which CHAP cannot check.
**Fix:** Make the device use PAP or ASCII; or store those accounts in a format that can be compared (chapter 07; consider the risk of the database leaking).

**Symptom:** When a device logs in with CHAP, the log says `TACACS+ authentication type not supported here: asking the client to restart with another`.
**Cause:** This client's auth chain has no handler that can check the password (for example there is no `[auth]` and everything goes to the script), or the request is an enable request.
**Fix:** Add a sql or ldap handler, or make the device use PAP or ASCII.

**Symptom:** `enable` always fails, with the log `TACACS+ enable request refused: [tacacs].enable is "deny"`.
**Cause:** `tacacs.enable` has the default `deny`.
**Fix:** On the device, use privilege-level authorization (give `priv-lvl` at login), or set `tacacs.enable` to `login`.

**Symptom:** You changed the port, a limit or `idle_timeout_ms` in `[tacacs]` and sent `SIGHUP`, but nothing changed, and the log says `configuration reloaded from rradius.toml; these need a restart to apply: tacacs.max_connections_per_source, tacacs.idle_timeout_ms`.
**Cause:** These keys need a restart to take effect.
**Fix:** Schedule a server restart. Only `enable`, `authorize_groups` and the client list can be reloaded.

**Symptom:** `check` prints `[tacacs] is enabled but has no [[tacacs.clients]] entries`.
**Cause:** There is a `[tacacs]` section but no client.
**Fix:** Add at least one `[[tacacs.clients]]`.

**Symptom:** `check` prints `tacacs client 'lab': unknown auth chain 'nosuch' (no [auth.chains.nosuch])`.
**Cause:** The client's `auth` points to an auth chain that does not exist.
**Fix:** Define that chain in `[auth.chains.nosuch]` first (chapter 07), or change it to an existing name.

**Symptom:** `check` prints `tacacs.idle_timeout_ms must be greater than 0` or `tacacs.max_connections must be greater than 0`.
**Cause:** These two values cannot be 0.
**Fix:** Change them to a positive number, or delete the line to use the default.

**Symptom:** `check` prints `rradius.toml: line N: missing escaped value, expected …`, pointing at a rule in `[tacacs.authorize_groups]`.
**Cause:** The rule is written in double quotes and contains a backslash such as `cmd\*`, which a TOML double-quoted string does not accept.
**Fix:** Use single quotes (see "Authorize with authorize groups").

**Symptom:** Authorization is always `FAIL`, and none of the `log` calls in the script print anything.
**Cause:** This client has `authorize_group`, so authorization is handed to the rules and the script does not run; when no rule applies the answer is `FAIL`.
**Fix:** Check whether the rule patterns match the whole AV pair the device sent (for example `cmd=show` does not match `cmd=show interfaces`, because the arguments are in separate `cmd-arg` pairs), or remove `authorize_group` and use the script.

**Symptom:** Accounting keeps getting `ERROR`, and the server log has `Lua script did not accept accounting` or a script error.
**Cause:** The script did not return `accept`, or it failed while running.
**Fix:** Fix the script according to the log. When the top of the script branches on `protocol` and `phase`, do not let TACACS+ accounting fall through to `return 'reject'`.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md): how clients are matched, and the shared secret
- [07 User accounts and auth chains](07-users-and-chains.md): auth chains, password formats, `auth.chains`
- [08 LDAP and Active Directory](08-ldap-and-ad.md): log in to devices with directory accounts
- [09 External databases and named queries](09-external-databases.md): turn accounting records into metrics
- [10 Writing policies in Lua](10-lua-policies.md): `get_attr`, `get_attrs`, `set_attr`, `sql_execute`
- [13 Accounting and sessions](13-accounting.md): why an accounting script must be safe to run more than once
- [16 Clusters and high availability](16-cluster.md): `stop_without_leader_ms`
- [17 Monitoring, status and logs](17-monitoring.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix C: Metrics reference](92-metrics-reference.md)
