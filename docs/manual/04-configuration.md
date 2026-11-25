# 04 Configuration file basics

> After this chapter you can:
> - Read the structure of the configuration file and know which sections are required
> - Split the configuration into several files with `include`, and keep secrets in files with the right permissions
> - Check and inspect the configuration with `check`, `config show`, `config diff` and `schema`
> - Reload the configuration with SIGHUP, and tell which changes need a restart
> - Decide how the server stops with `[shutdown]`

Prerequisites: you have done [chapter 03](03-quick-start.md) and have a `rradiusd` that starts. Every command in this chapter runs in one working directory. You do not need a real NAS.

In this chapter:

- [Structure of the configuration file](#structure-of-the-configuration-file)
- [A configuration from scratch](#a-configuration-from-scratch)
- [Relative paths](#relative-paths)
- [Splitting the configuration into several files](#splitting-the-configuration-into-several-files)
- [Keeping secrets in files](#keeping-secrets-in-files)
- [Checking the configuration: check](#checking-the-configuration-check)
- [Viewing the effective configuration: config show](#viewing-the-effective-configuration-config-show)
- [Comparing two configurations: config diff](#comparing-two-configurations-config-diff)
- [Looking up setting keys: schema](#looking-up-setting-keys-schema)
- [Applying changes without downtime: reload](#applying-changes-without-downtime-reload)
- [Stopping the server: shutdown](#stopping-the-server-shutdown)
- [runtime and log](#runtime-and-log)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Structure of the configuration file

rradius has one main configuration file, in TOML format. Commands take its path with `--config <path>`. Without `--config` they read `/etc/rradius/rradius.toml` (the second line of `rradiusd --version` prints this default path).

TOML opens a table with `[table-name]`, adds one entry to an array of tables with `[[table-name]]`, and a table holds `key = value` lines. The rradius configuration has the top-level keys and tables below. The list matches the top-level `properties` in the output of `rradiusd schema`:

| Key or table | Required | Purpose | Main chapter |
| --- | --- | --- | --- |
| `dictionary` | No | Path of the main dictionary file; when absent, the default dictionary path is used (`/usr/share/rradius/dictionary` for the package) | [06](06-dictionary.md) |
| `include` | | Other configuration files to read along with this one | This chapter |
| `[lua]` | Yes | The top-level Lua script and its resource limits | [10](10-lua-policies.md) |
| `[cluster]` | Yes | The built-in cluster database; a single server needs it too | [16](16-cluster.md) |
| `[radius]` | At least one of this and `[tacacs]` | RADIUS listeners and clients | [05](05-radius-clients.md) |
| `[tacacs]` | At least one of this and `[radius]` | TACACS+ listeners and clients | [12](12-tacacs.md) |
| `[auth]` | | The auth chain | [07](07-users-and-chains.md) |
| `[accounts]` | | SQL run by the `user` command | [07](07-users-and-chains.md) |
| `[sessions]` | | SQL run by the `session` command | [13](13-accounting.md) |
| `[databases.<name>]`, `[queries.<name>]` | | External databases and named queries | [09](09-external-databases.md) |
| `[metrics]`, `[log]` | | HTTP port for metrics and health checks, and logging | [17](17-monitoring.md) |
| `[runtime]` | | Worker thread settings | [19](19-performance.md) |
| `[shutdown]` | | Wait times when stopping | This chapter |

A few rules you must know:

- **Every table rejects unknown keys.** When you mistype a key, `check` names the file and line and lists the keys that table accepts. See [Common mistakes](#common-mistakes).
- **The server does not start without the required parts.** Without `[lua]` you get `the [lua] section is required`. Without `[cluster]` you get `the [cluster] section is required`. With neither `[radius]` nor `[tacacs]` you get `at least one of [radius] or [tacacs] must be configured`.
- **A service exists only if its section exists.** Without `[tacacs]` the TACACS+ port is not opened. Without `[metrics]` there is no `/metrics` or `/readyz`, and the `status` command has nothing to read.
- **Error messages never print values.** A value of the wrong type may be a password that lacks its quotes, so the message gives only the line number and the key name.

The type, default and reload behavior of every key are listed in full in [Appendix A](90-config-reference.md). Each table in this chapter also states its defaults and its reload behavior.

## A configuration from scratch

**Goal:** build a single-server configuration that starts and keeps all its secrets in files. The other sections of this chapter use it for their examples.

`rradiusd config init` prints a minimal template of about 30 lines with randomly generated passwords, with English comments. `rradiusd config init --full` prints the full version, where every section has an explanation (settings that are commented out show their defaults); add `--lang zh-TW` to either command to get the comments in Traditional Chinese; it is a good way to find out which keys exist. You need to change one thing yourself: the template writes the cluster passwords and the shared secret of the example client directly in the file. This manual puts them in files that `*_file` points to (see [Keeping secrets in files](#keeping-secrets-in-files)).

The version below is tidied up. Create a working directory and put these files in it:

```toml
# rradius.toml
dictionary = "/usr/share/rradius/dictionary"   # where the dictionary really is, see chapter 06
include = ["conf.d/*.toml"]

[lua]
script = "auth.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"   # openssl rand -hex 32 > raft.secret
secret_api_file = "api.secret"     # openssl rand -hex 32 > api.secret

[metrics]
bind = "127.0.0.1"
port = 9812

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
```

```toml
# conf.d/10-clients.toml
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"   # openssl rand -hex 32 > localhost.secret
```

`auth.lua` is the top-level script. This configuration has no `[auth]`, so the script alone decides every login. Start with a script that recognizes one demo account. How to write scripts is covered in [chapter 10](10-lua-policies.md):

```lua
-- auth.lua
if protocol ~= 'radius' then
  return 'reject'
end
if phase ~= 'authentication' then
  return 'accept'
end
if get_attr('User-Name') == 'alice' and get_attr('User-Password') == 'alice-password' then
  return 'accept'
end
return 'reject'
```

Generate the secret files, tighten their permissions, then check:

```console
$ for f in raft api localhost; do openssl rand -hex 32 > $f.secret; done
$ chmod 600 *.secret
$ rradiusd check --config rradius.toml
configuration ok
```

This configuration has no `[[cluster.nodes]]`, so it is a cluster of one node. The two cluster ports, Raft and API, use the defaults `127.0.0.1:8100` and `127.0.0.1:8200` and listen on the local machine only. Cluster settings are covered in [chapter 16](16-cluster.md).

## Relative paths

Every relative path in the configuration is resolved against the **directory of the main configuration file**, not against the directory you run the command from. If the configuration above lives in `/etc/rradius/rradius.toml`:

| Setting | Value you write | File actually read |
| --- | --- | --- |
| `lua.script` | `auth.lua` | `/etc/rradius/auth.lua` |
| `cluster.data_dir` | `data` | `/etc/rradius/data` |
| `cluster.secret_raft_file` | `raft.secret` | `/etc/rradius/raft.secret` |
| `radius.clients[].secret_file` | `localhost.secret` | `/etc/rradius/localhost.secret` |

An absolute path (one that starts with `/`) is used as written.

> **Note:** Files read through `include` work the same way: a relative path inside them is resolved against the directory of the **main configuration file**, not the directory of the included file itself. The `conf.d/10-clients.toml` above has `secret_file = "localhost.secret"`; it reads the `localhost.secret` next to the main file, not `conf.d/localhost.secret`.

## Splitting the configuration into several files

**Goal:** keep the settings that change often, such as clients and the auth chain, in files of their own, so you do not touch the main file each time. This also lets a configuration management tool deploy one file at a time.

**Configure:** put `include` at the top level of the main file and list the files to read along with it. The template from `config init` already has this line:

```toml
# rradius.toml (excerpt)
include = ["conf.d/*.toml"]
# … (other settings omitted)
```

`include` has no default; without it only the main file is read. It is read together with the main file, so a reload applies it. Then put files in `conf.d/`. Every file must begin with a table:

```toml
# conf.d/20-lab.toml
[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"   # openssl rand -hex 32 > lab.secret
```

**Verify:**

```console
$ openssl rand -hex 32 > lab.secret && chmod 600 lab.secret
$ rradiusd check --config rradius.toml
configuration ok
$ rradiusd config show --config rradius.toml | grep -A3 'radius.clients'
[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"
--
[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"
```

**What you should see:** `config show` prints the contents of all files merged together. Both clients are there, the one from `10-` first and the one from `20-` after it.

The rules for reading files (based on `config.rs`):

- The effect is as if the included files were appended to the main file in order, and the whole were read as one TOML document.
- A name can be a single file (`"clients.toml"`), or it can use the wildcards `*` and `?`. Wildcards may appear only in the file name, the last component: `"conf.d/*.toml"` works, `"*.d/*.toml"` does not.
- The files that one name matches are sorted by path. Use file names that start with a number, such as `10-` and `20-`, to control the order.
- A wildcard does not match file names that start with `.`, so temporary files left by editors are not read by mistake.
- A name with a wildcard that matches no file (for example when `conf.d/` is empty or does not exist) is not an error. A name without a wildcard must be a readable file, or loading fails.
- A file matched by two names is read once. The main file itself is skipped.
- At most 1000 files, and each file at most 1048576 bytes (1 MiB).
- An included file cannot contain `include` itself, and its first line that is not blank or a comment must be a `[table]` or `[[array-of-tables]]` header.
- Each file must be complete TOML on its own: an unclosed string in one file does not swallow the next file.
- **The same table cannot appear in two files.** If the main file already has `[radius]`, no file in `conf.d/` can have `[radius]` again, or you get `duplicate key`. Arrays of tables are not subject to this: `[[radius.clients]]` can be spread over several files and is added in file order.
- For TOML syntax errors and type errors, the message names the file and the line number in that file, not a line number in the merged text. Checks that run after the merge (for example the client checks) start with the name of the main configuration file and have no line number. When such a message contains a name in single quotes (for example `'core'`), it is followed by `(in <file> line <line>)`, which lists the lines that have this name as a quoted string (at most three, the rest shown as `and N more`). Lines of the main file are listed too, so when a name is duplicated you see both places. It is added only when at least one of the lines found is in an included file; when the name is only in the main file, or is not found, the message is the same as without `include`. The match is by name, so when the name also appears in another setting value, one extra place is listed. See [chapter 05](05-radius-clients.md).

> **Warning:** Included files can decide who may log in. When the file itself, or the directory it is in, can be written by its group or by others, `check`, startup and every reload print a warning, for example `included configuration file conf.d/20-lab.toml can be written by its group or by others, which lets them change who may log in`. It is only a warning and the configuration is still read. When you see it, change the permissions so that only the owner can write.

> **Note:** After you move all clients into `conf.d/`, an empty directory or a file name that does not match is not an error for the wildcard, but the result is a `[radius]` with no clients, and `check` fails: `[radius] is enabled but has no [[radius.clients]] entries`.

## Keeping secrets in files

**Goal:** you can show the configuration file to colleagues or keep it in version control without the secrets going with it.

Many secrets have two forms: write the value directly, or point to a file with `*_file`. Exactly one of the two must be set. For example, when a client's `secret` and `secret_file` are both set or both missing, `check` fails: `exactly one of secret or secret_file must be set`. The examples in this manual always use files.

The secret files that the core edition reads:

| Setting | Content | Main chapter |
| --- | --- | --- |
| `radius.clients[].secret_file`, `tacacs.clients[].secret_file` | The shared secret of a client | [05](05-radius-clients.md), [12](12-tacacs.md) |
| `cluster.secret_raft_file`, `cluster.secret_api_file` | The passwords of the two cluster ports | [16](16-cluster.md) |
| `databases.<name>.password_file` | The password of an external database | [09](09-external-databases.md) |
| `auth.handlers[ldap].bind_password_file` | The password of the LDAP service account | [08](08-ldap-and-ad.md) |
| `auth.handlers[netlogon].machine_password_file` | The password of the domain computer account | [08](08-ldap-and-ad.md) |
| `radius.eap.key_file` | The private key of the EAP server certificate | [11](11-eap.md) |

Generate a random shared secret and tighten its permissions:

```console
$ openssl rand -hex 32 > nas1.secret
$ chmod 600 nas1.secret
```

Rules:

- For a client's `secret_file`, trailing newlines (`\n`, `\r`) are removed, so a file written with `echo` or an editor works. An empty file (or one with only newlines) fails: `secret_file 'nas1.secret' is empty`. A file that cannot be read fails: `cannot read secret_file 'nas1.secret': No such file or directory (os error 2)`.
- **Permission warning:** when a file in the table above gives "others" any permission, or lets the "group" write (any bit of `0o027` is set in the mode), startup, `check` and every reload each log one warning, and the file is used anyway. `0600`, `0640` and `0400` do not warn; `0644`, `0660` and `0666` do. The warning gives only the setting name, the path and the mode, never the content:

  ```text
  … WARN rradius_core::common::seal: secret_file: 'lab.secret' has mode 644, which others can reach or its group can write; make it 0600 or 0640
  ```

- **Shared secret too short:** a secret shorter than 16 bytes gives the warning `RADIUS client 'lab-net' has a shared secret shorter than 16 octets; use at least 24 random octets`. TACACS+ clients are the same, with the message starting `TACACS+ client`.
- **Example secret in use:** every secret shipped with the examples contains `change-me`. A client shared secret or a cluster secret that contains it (case-insensitive) gives a warning. For a client it is ``RADIUS client 'lab-net' has the public shared secret shipped with the examples; replace it with a random one, for example from `openssl rand -hex 32` ``, and for the cluster secret it is ``cluster.secret_raft is the public value shipped with the examples; …``.

All three are only warnings and do not stop startup. They do appear in the output of `check`, so a quick look before going live catches them.

> **Note:** The package version that runs under systemd runs as the `rradius` user. With permissions at `0600`, the owner of the file must be `rradius`; or let root own the file, with group `rradius` and permissions `0640`. For the install paths and the user, see [chapter 02](02-install.md).

## Checking the configuration: check

```console
$ rradiusd check --config rradius.toml
configuration ok
$ echo $?
0
```

`check` runs the part of startup that reads the configuration: it reads the configuration file and every included file, reads the secret files, reads the dictionary, compiles the top-level script, reads the migration directory, checks the auth chain and the `[lua.http]` settings, checks the accounting spool directory, and finally prints the shared-secret warnings. It **opens no port, creates no data directory and connects to no host**, so it is safe to run on a production machine while the server is running.

- Everything passes: stdout shows `configuration ok` and the exit code is 0. Warnings (permissions, short secrets and so on) go to stderr and do not change the exit code.
- An error: stderr shows the reason and the exit code is 2. The message starts with the file where the error is, and adds the line number when it can find one.
- A wrong argument (for example `check --config a.toml extra`): it prints `usage: rradiusd check [--config <path>]` and the exit code is 2.

What `check` does not find: a port that another program already uses, other cluster nodes that cannot be reached, and wrong credentials for an external database or LDAP server. For these, read the log after startup.

The recommended flow for changing the configuration: edit the file, run `check`, look at the differences with `config diff`, reload or restart, then read the result line in the log.

## Viewing the effective configuration: config show

`config show` reads the configuration the way the server does, fills in the defaults for the keys you did not write, and prints the whole thing. Below is the result for [A configuration from scratch](#a-configuration-from-scratch) plus `conf.d/20-lab.toml` (`…` stands for two omitted lines of settings that have no effect in the core edition, see [Appendix A](90-config-reference.md)):

```console
$ rradiusd config show --config rradius.toml
# computed on this machine: radius.sockets = 1, cluster.read_connections = 28
dictionary = "/usr/share/rradius/dictionary"
include = ["conf.d/*.toml"]

[lua]
script = "auth.lua"
max_execution_ms = 1000
max_memory_mb = 256
query_timeout_ms = 5000

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"
migrations_wait_ms = 60000
node_id = 1
read_connections = 28
snapshot_every = 100000

[metrics]
bind = "127.0.0.1"
port = 9812

[radius]
bind = "127.0.0.1"
auth_port = 1812
acct_port = 1813
…
limit_proxy_state = "yes"
max_acct_inflight = 4096
max_auth_inflight = 4096
require_message_authenticator = "yes"
status_server = true
sockets = 1

[[radius.clients]]
name = "localhost"
address = "127.0.0.1"
secret_file = "localhost.secret"

[[radius.clients]]
name = "lab-net"
address = "192.0.2.0/24"
secret_file = "lab.secret"

[shutdown]
ready_delay_ms = 0
drain_ms = 5000
```

- The comment on the first line lists the defaults computed for this machine. For example, `radius.sockets` and `cluster.read_connections` depend on the number of CPUs and may differ on another machine.
- The main file and all included files are printed together.
- A secret written directly in the configuration is printed as `<hidden>`; for `*_file`, only the path is printed, not the content of the file.
- Sections that have defaults (for example `[shutdown]` here) are printed even when you did not write them. Optional sections (for example `[log]` and `[auth]`) do not appear when you did not write them.
- When the configuration has an error, `config show` prints the error as `check` does, and the exit code is 2.

When you want to know what a key is right now, this is the most accurate way: it uses the same configuration-reading code as the server.

## Comparing two configurations: config diff

**Goal:** before a reload or a deployment, confirm what exactly changed this time; or compare the configuration of two nodes in a cluster.

Below, the whole configuration (including `conf.d/` and the secret files) is copied to a `new/` directory, and four things are changed: `auth_port` becomes 1822, `drain_ms = 10000` is added to `[shutdown]`, `listen = "127.0.0.2"` is added to `[cluster]`, and the `secret_file` of `localhost` is replaced by a `secret` written directly.

```console
$ rradiusd config diff rradius.toml new/rradius.toml
radius.auth_port: 1812 | 1822
radius.clients[localhost].secret: differs (hidden)
radius.clients[localhost].secret_file: "localhost.secret" | (absent)
shutdown.drain_ms: 5000 | 10000
node-specific:
  cluster.listen: (absent) | "127.0.0.2"
$ echo $?
1
$ rradiusd config diff rradius.toml rradius.toml
no differences
$ echo $?
0
```

- Both configurations get their defaults filled in before the comparison, so "not written" and "written with the default value" are not a difference.
- Each line is `key: left value | right value`; when one side lacks the key, it shows `(absent)`.
- For secrets it says only `differs (hidden)` and prints no value. For `*_file` it compares the path, not the content of the file: if you change the content of a shared secret file, `config diff` does not see it.
- `radius.clients` and `tacacs.clients` are paired by the client's `name` (by `address` when there is no `name`), not by order, so reordering clients or moving them to another file shows no difference.
- `cluster.node_id` and `cluster.listen` are meant to differ on every node, so they are listed at the end under `node-specific:` and do not affect the exit code.
- Exit codes: 0 when there are no differences, 1 when there are, 2 when either configuration has an error. When there are only `node-specific` differences, it prints `no differences` first, then lists them, and the exit code is 0.

## Looking up setting keys: schema

`schema` prints the JSON Schema of the configuration file: the type, default and description of every table and key. `--lang zh-TW` prints the descriptions in Chinese; with no option or with `--lang en` they are in English:

```console
$ rradiusd schema --lang zh-TW > schema.json
$ jq -r '.properties | keys | join(" ")' schema.json
accounts auth cluster databases dictionary include log lua metrics queries radius runtime sessions shutdown tacacs
$ jq '.properties.shutdown.anyOf[0].properties.drain_ms' schema.json
{
  "default": 5000,
  "description": "停止接收新請求後，最多等處理中的請求多久，毫秒。",
  "format": "uint64",
  "minimum": 0,
  "type": [
    "integer",
    "null"
  ]
}
```

- This schema is generated from the code that reads the configuration, so it is updated whenever the program is. When the manual and the schema disagree, trust the output of the version you have.
- An editor that supports JSON Schema can use it to complete key names and check spelling.
- Defaults that depend on the machine (`radius.sockets` and so on) show, in the schema, the value of the machine that built it. For the real value, read `config show`.
- A wrong language prints `usage: rradiusd schema [--lang en|zh-TW]` and the exit code is 2.

## Applying changes without downtime: reload

**Goal:** after you change clients, shared secrets, the auth chain or scripts, make the change take effect without interrupting service.

**How to trigger it:** send SIGHUP to the server process.

```console
$ kill -HUP <pid>
$ systemctl reload rradius      # the systemd unit sends SIGHUP with ExecReload
$ service rradius reload        # the FreeBSD rc script
```

**What happens:** the server reads the main file, the included files, the secret files and the scripts again, with the same code it used at startup.

- **All or nothing.** The new configuration is fully built and checked first, and only then swapped in at once. If any step fails, one error line is logged, the server keeps serving with the old configuration, and the process does not exit.
- **Requests in progress are not affected.** A request already being handled finishes with the script, auth chain and clients it started with; only later requests use the new ones.
- **Repeated signals:** when another SIGHUP arrives during a reload, one more reload runs after this one finishes. A SIGHUP that arrives during startup takes effect once the server begins serving.
- **The result is one log line** and never quotes the contents of the configuration file (a line of the file may hold a password).

**Verify:** while the server from [A configuration from scratch](#a-configuration-from-scratch) is running, add `conf.d/20-lab.toml` (see [Splitting the configuration into several files](#splitting-the-configuration-into-several-files)) and send SIGHUP:

```console
$ kill -HUP $(pgrep -f 'rradiusd serve')
$ curl -s http://127.0.0.1:9812/metrics | grep '^rradius_reload'
rradius_reload_total{result="ok"} 1
rradius_reload_total{result="error"} 0
rradius_reload_last_success_timestamp_seconds 1795384713
```

The server log:

```text
… INFO rradius_core::reload: configuration reloaded from rradius.toml
```

**What you should see:** a reload has three possible results, and each one is visible:

| Case | Log | Metrics |
| --- | --- | --- |
| Success, everything applied | `INFO … configuration reloaded from <file>` | `rradius_reload_total{result="ok"}` goes up by 1, and `rradius_reload_last_success_timestamp_seconds` is set to the Unix time of this reload |
| Success, but some changes need a restart | `WARN … configuration reloaded from <file>; these need a restart to apply: <list>` | Same as above |
| Failure, nothing swapped | `ERROR … reload failed, keeping the running configuration: <reason>` | `rradius_reload_total{result="error"}` goes up by 1 |

The reason for a failure is the same as the message from `check`, for example a syntax error in an included file or a script that does not compile:

```text
… ERROR rradius_core::reload: reload failed, keeping the running configuration: conf.d/20-lab.toml: line 6: `address`: invalid basic string, expected `"`
… ERROR rradius_core::reload: reload failed, keeping the running configuration: cannot compile script 'auth.lua': syntax error: 11: Expected <eof>, got 'retrun'
```

After you fix the problem, send SIGHUP again and it is applied. To avoid this, run `check` before you reload.

### What reload applies

- The top-level script (`lua.script` and its content), `query_timeout_ms` and `max_execution_ms` in `[lua]`, and `[lua.http]`.
- The whole `[auth]`: the auth chain, handlers, scripts, `[auth.lockout]`, `[auth.password_limits]` and so on. Failure counts for lockout stay in the process, and a reload does not clear them.
- The RADIUS and TACACS+ client lists, including each client's settings and shared secret (secret files are read again). For a removed client, later requests are dropped as coming from an unknown source.
- `radius.require_message_authenticator` and `radius.limit_proxy_state`. What was learned with `auto` is kept after a reload (chapter 05).
- `[radius.eap]` (only new EAP logins use the new settings), `enable` and `authorize_groups` in `[tacacs]`, `[log.trace]`, and `[shutdown]`.
- `[radius.accounting_log]` is reopened, which works with log rotation; you can also use it to change the file name (chapter 13).
- `[accounts]` and `[sessions]` do not need a reload at all: the `user` and `session` commands read them each time they run.

### What needs a restart

The settings below are not applied on a reload. When one differs from the running value, the `WARN` line of the reload lists its name, and all other changes are still applied:

| Name | Description |
| --- | --- |
| `dictionary` | The path of the dictionary |
| `lua.max_memory_mb` | The memory limit of the Lua VM |
| `[cluster]` | The whole cluster configuration, including migrations |
| `[metrics]`, `[log]`, `[runtime]` | The whole table (except `[log.trace]`, which can be reloaded) |
| `[databases]`, `[queries]` | Adding, removing or changing any external database or named query |
| `radius.bind`, `radius.auth_port`, `radius.acct_port` | The listening address and ports |
| `radius.max_auth_inflight`, `radius.max_acct_inflight`, `radius.status_server`, `radius.sockets`, `radius.recv_buffer_bytes` | |
| `queue` in `[radius.accounting_spool]` and `[radius.accounting_log]` | |
| `tacacs.bind`, `tacacs.port`, `tacacs.max_connections`, `tacacs.max_connections_per_source`, `tacacs.max_body_len`, `tacacs.idle_timeout_ms` | |
| `[radius]`, `[tacacs]` | Adding or removing a whole section |

For example, if you change the authentication port and the script together, then send SIGHUP:

```text
… WARN rradius_core::reload: configuration reloaded from rradius.toml; these need a restart to apply: radius.auth_port
```

The script is already the new one, but the server is still on the old port. This warning appears again on every reload until you restart: the server compares against the configuration it started with.

> **Note:** A change in the **content of a dictionary file** does not count as a "different configuration": a reload compares only the `dictionary` path. But a reload does read the dictionary files again, and when their content differs from what was read at startup it logs one more `WARN`: `dictionary files changed since the server started; restart to apply: <path>` (at most five files, the rest shown as `and N more`), and it logs again on every reload while they still differ. The server keeps using the dictionary it read at startup, so a script that uses a new attribute gets `unknown attribute`. Always restart after you change the dictionary. See [chapter 06](06-dictionary.md#restart-after-changing-a-dictionary) for details.

## Stopping the server: shutdown

**Goal:** stop without letting the NAS see timeouts: first let the load balancer move traffic away, then wait for the requests in progress to finish.

After it receives SIGTERM (what `systemctl stop` and `docker stop` send) or SIGINT (Ctrl+C), the server does the following in order:

1. `/readyz` switches back to 503 (body `stopping`), while `/livez` still returns 200. Requests are still answered, for `ready_delay_ms`.
2. It stops taking new requests.
3. Drain: it waits for the requests in progress to finish, for at most `drain_ms`.
4. It shuts down the cluster database and the process exits.

If another SIGTERM or SIGINT arrives during this time, the remaining waits are skipped and it goes straight to step 4.

| Key | Default | Reload | Description |
| --- | --- | --- | --- |
| `shutdown.ready_delay_ms` | `0` for a single node; `5000` when `cluster.nodes` has more than one | Yes | Duration of step 1, in milliseconds, 0 to 60000 |
| `shutdown.drain_ms` | `5000` | Yes | The longest wait in step 3, in milliseconds, 0 to 60000 |

0 means no wait. A value above 60000 makes `check` fail: `shutdown.drain_ms must be at most 60000`. The value in effect at the moment stopping begins is the one used, so a reload applies it. See [Appendix A](90-config-reference.md).

**Configure:**

```toml
# conf.d/40-shutdown.toml
[shutdown]
ready_delay_ms = 3000
drain_ms = 2000
```

**Verify:** after the reload, send SIGTERM, look at `/readyz` right away, and send one login within those 3 seconds:

```console
$ kill -HUP <pid>
$ kill -TERM <pid>
$ curl -s -w '%{http_code}\n' http://127.0.0.1:9812/readyz
stopping
503
$ rradius-bench run --server 127.0.0.1 --secret "$(cat localhost.secret)" \
    --user alice --password alice-password --load pap,concurrency=1 --requests 1
…
  pap: 2626/s, sent 1, accept 1, reject 0, challenge 0, accounting 0, timeouts 0, invalid 0, late 0, socket errors 0, retransmits 0
…
```

**What you should see:** a login sent while the server is stopping still gets `accept 1`. The log:

```text
… INFO rradius_core::cli: stopping: /readyz answers 503; requests are answered for 3000 ms more, then those being handled are waited for, for at most 2000 ms
… INFO rradius_auth: authentication protocol="radius" client="localhost" … decision="accept"
… INFO rradius_core::cli: stopping: no more requests taken
… INFO rradius_core::cli: stopping: every request taken was answered
… WARN hiqlite::store::state_machine::sqlite::writer: SQL writer is shutting down
```

When you send another signal during stopping, the log shows `stopping: asked again; not waiting for the requests being handled`. When `drain_ms` runs out with requests still unfinished, one `WARN` line is logged that says how many RADIUS requests and TACACS+ connections were cut off.

- With a load balancer in front, or in a cluster of several nodes, `ready_delay_ms` must be longer than the interval at which the load balancer checks `/readyz` times the number of failures it needs to mark the server down, so that traffic has time to move away.
- A single node has no other server to take over, so by default it does not delay.

> **Note:** Let the server close its database itself before it exits. The systemd unit shipped with the package sets `TimeoutStopSec=60`; if you write your own way of starting it, give it enough time as well, and do not use `kill -9` while it waits.

## runtime and log

- `[runtime]` has only `pin_threads` (default `false`, needs a restart). It pins each worker thread to one CPU and works only on Linux; set to `true` on another platform, `check` fails: `runtime.pin_threads is only supported on Linux`. For when to use it, see [chapter 19](19-performance.md).
- `[log]` sets the log level (default `info`), the format (default `text`) and whether authentication records are written (default `true`). The whole table needs a restart to take effect. `[log.trace]` can be reloaded and is used to record the full requests of a given user or client for a while. See [chapter 17](17-monitoring.md) for details.

## Common mistakes

**Symptom:** `check` prints ``rradius.toml: line 15: `prot`: unknown field `prot`, expected `bind` or `port` `` and exits with 2.
**Cause:** a key name is mistyped. Every table rejects unknown keys.
**Fix:** correct it using the line number in the message and the key names it lists.

**Symptom:** ``conf.d/30-shutdown.toml: line 3: `drain_ms`: invalid type: string, expected u64``.
**Cause:** a value has the wrong type, for example a number written as the string `"5s"`. The message deliberately does not print the value.
**Fix:** use the type after `expected`; write numbers without quotes, and always write durations as integer milliseconds.

**Symptom:** `conf.d/30-bad.toml: line 1: duplicate key`.
**Cause:** an included file declares a table that the main file (or another file) already has, for example a second `[radius]` or `[lua]`.
**Fix:** move the settings back into the file where that table already is; or delete the table from the main file and keep the whole table in one file only. The array of tables `[[radius.clients]]` is not affected.

**Symptom:** ``conf.d/30-bad.toml: line 2: an included file has to begin with a table (`[section]`), not a setting: a setting before its first table would belong to the file before it``.
**Cause:** an included file starts with a `key = value`, for example `dictionary = …`. Top-level keys can be written only in the main file.
**Fix:** move the top-level keys to the main file; an included file begins with a `[table]`.

**Symptom:** ``conf.d/30-bad.toml: line 1: an included file cannot have `include` ``.
**Cause:** an included file has `include` itself; nesting is not supported.
**Fix:** list all the names in the `include` of the main file.

**Symptom:** `extra.toml: cannot read included file: No such file or directory (os error 2)`.
**Cause:** a name in `include` has no wildcard, and the file does not exist.
**Fix:** correct the name, or use a wildcard (no match is not an error).

**Symptom:** `rradius.toml: include: '*.d/*.toml' has a wildcard before its last component; only the file name may have one`.
**Cause:** a wildcard was used in a directory name.
**Fix:** use `*` and `?` only in the file name, the last component; when you have several directories, write one name for each.

**Symptom:** `rradius.toml: [radius] is enabled but has no [[radius.clients]] entries`, although the clients are clearly written in `conf.d/`.
**Cause:** the wildcard in `include` did not match those files (wrong directory or extension), and no match is not an error.
**Fix:** use `config show` to see what was read in; check the file extension, the directory, and that relative paths are resolved against the directory of the main file.

**Symptom:** `nonexist.toml: cannot read configuration: No such file or directory (os error 2)`; or, when `--config` is not given, `/etc/rradius/rradius.toml: cannot read configuration: …`.
**Cause:** the configuration file path is wrong, or `--config` was forgotten. In this case the `serve` command also adds a line `(the default configuration; give --config <path> for another)`.
**Fix:** give the correct `--config`.

**Symptom:** after a reload the log shows `these need a restart to apply: …`, and what you changed did not take effect.
**Cause:** the listed settings take effect only after a restart, see [What needs a restart](#what-needs-a-restart).
**Fix:** schedule a restart. Until then, every reload lists them again.

**Symptom:** you added an attribute to the dictionary and reloaded, the log says `configuration reloaded`, but the script gets `unknown attribute '…'`.
**Cause:** the dictionary is read only at startup. A reload compares only the `dictionary` path to decide whether a restart is needed, but when the content of the dictionary files differs from startup, the log has a `WARN` with `dictionary files changed`.
**Fix:** restart the server.

**Symptom:** `shutdown.drain_ms must be at most 60000`.
**Cause:** both values in `[shutdown]` have an upper limit of 60000 milliseconds.
**Fix:** use 60000 or less.

## Related chapters

- [03 First start and first login](03-quick-start.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [06 Dictionary and attributes](06-dictionary.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [18 Day-to-day operations](18-operations.md)
- [20 Security hardening](20-security.md)
- [Appendix A: Configuration reference](90-config-reference.md)
- [Appendix B: Command reference](91-command-reference.md)
