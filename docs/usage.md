# rradiusd usage reference

rradiusd is an AAA (authentication, authorization, accounting) server that supports RADIUS and TACACS+. Authentication can be handed to the auth chains in the configuration file (SQL, LDAP, Lua); authorization and accounting are handled by Lua scripts. Data is stored in an embedded hiqlite (SQLite plus Raft replication) cluster, which can run as a single node or as a multi-node high-availability cluster.

This is a reference document that lists every setting and behavior. For explanations by topic, deployment scenarios and example configurations you can start as they are, see the [user manual](manual/README.md).

## 1. Building and starting

```bash
cd rradius-server
cargo build --release
./target/release/rradiusd serve --config /etc/rradius/rradius.toml
```

`rradiusd --version` prints the version and the default configuration file, for example:

```
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
```

**Default configuration file:** When `--config` is not given, `serve`, `check` and all management commands (`user`, and the enterprise edition's commands) read the default configuration file. It is fixed at build time: if the environment variable `RRADIUS_PREFIX` was set when you built, it is `$RRADIUS_PREFIX/etc/rradius/rradius.toml`; if it was not set (or was an empty string or `/`), it is `/etc/rradius/rradius.toml`. The prefix must be an absolute path, and a trailing `/` is removed. Environment variables at run time do not affect it. To check which one a build uses, look at the second line of `--version`. For example, installed under `/usr/local`:

```bash
RRADIUS_PREFIX=/usr/local cargo build --release
./target/release/rradiusd        # reads /usr/local/etc/rradius/rradius.toml
```

When you build with cargo, changing `RRADIUS_PREFIX` triggers a rebuild automatically; a value set when building the enterprise edition works the same way. The dictionary follows this prefix too: when the configuration file has no `dictionary`, it reads `$RRADIUS_PREFIX/share/rradius/dictionary` (in the example above, `/usr/local/share/rradius/dictionary`).

**Release archive:** `rradius-<version>-<platform>.tar.gz`, where the platform is `macos-aarch64`, `linux-x86_64` or `linux-aarch64`. The `.sha256` file next to it can be checked with `shasum -a 256 -c`. The unpacked directory layout is the same as the repo, so the example configurations and the relative paths in the documents all work as they are:

| Path | Contents |
| --- | --- |
| `bin/rradiusd` | The server and the management commands |
| `share/dictionary*` | RADIUS dictionaries |
| `rradius-server/examples/` | Examples, including the nine scenarios of the user manual |
| `docs/usage.md`, `docs/manual/` | This document and the user manual |
| `README.md`, `VERSION` | Notes, and the version and the commit it was built from; the Linux builds also state the glibc version they need |

- **glibc of the Linux builds:** They are built on Debian 12 (bookworm), and the required version is written in the archive's `VERSION` and `README.md`. 0.1.0 needs glibc 2.34 or later, and was confirmed to run on 2026-11-08 on Debian 12, Ubuntu 22.04 (2.35) and Rocky Linux 9 (2.34). On older distributions, use the Docker image (section 1.1) or build it yourself.
- **For a production deployment,** put `share/` in a fixed location (for example `/usr/share/rradius`) and point `dictionary` in the configuration file there (you can leave it out when the dictionary is at the default dictionary path, `$RRADIUS_PREFIX/share/rradius/dictionary`); put the executable in your `PATH`.
- **Making a release archive yourself:** `packages/tarball/release.sh [--target <platform>]... [--out <directory>] [--commit <rev>] [--no-ci-check]`. Without `--target` it builds only the local platform; the Linux builds are made inside Docker with the build stage of the project's `Dockerfile`, the same as the Docker image (arm64 also supports 64 KiB pages). The contents come only from the given commit; uncommitted changes and ignored files are not packaged. After packaging, the script unpacks the archive and runs `check` with the executable inside it against each scenario's configuration (scenarios that have `make-certs.sh` or `setup.sh` need their certificates or keys generated first, so they are not part of the check). If any check fails, the archive is not kept. Before anything is built, the script asks GitHub (with `gh`) whether the Release workflow (`release.yml`, which runs every other workflow for one commit) has passed for the commit; start it with `gh workflow run release.yml --ref <ref>`. `--no-ci-check` skips this for a build that is not a release, for example one made from a fork.

- **Start the service with the `serve` subcommand:** `rradiusd serve [--config <path>]`. Without `--config` it reads the default configuration file (see above); if the file cannot be read, the error message gives the path and suggests using `--config` to point to another file, with exit status 2. When `serve` is followed by other arguments, it prints its usage, with exit status 2.
- **With no arguments it only prints the usage:** `rradiusd` and `rradiusd help` (or `-h`, `--help`) print the usage of all subcommands to stdout, with the default configuration file on the last line, and exit with status 0; they read no configuration file and start nothing. With wrong arguments, the same usage is printed to stderr with exit status 2; when the first argument is `--config` or `--check` (the old form), one more line suggests using `serve --config <path>` or `check --config <path>` instead, and nothing is started or checked.
- `rradiusd check [--config <path>]` only checks the configuration, see below. `[radius.accounting_spool] dir` is also checked: it must be a directory you can write to, and when it does not exist yet, its nearest existing parent directory must be writable. It writes one temporary file and deletes it, and does not create the directory or touch the spooled contents. The check uses the account that runs `check`, so the result can differ when that is not the account that runs the service.
- A configuration error (a misspelled field, a missing required item, a script syntax error and so on) always fails at startup with exit status 2, and the error message names the configuration file path and the field or file that is wrong.
- Relative paths in the configuration file are all relative to the directory that contains the configuration file, whatever the working directory at startup is.
- The build does not need a separate Lua or LuaJIT installation: Luau is compiled together with the program.
- **SQLite is built with `SQLITE_ENABLE_QPSG`.** The SQLite used by the database is compiled together with the program. The repo's `.cargo/config.toml` sets `LIBSQLITE3_FLAGS="-DSQLITE_ENABLE_QPSG"`, which applies when you build with `cargo` inside the repo; the `Dockerfile` and `packages/tarball/release.sh` do the same. When you build outside the repo (for example by copying only the `rradius-server/` directory), you must set this environment variable yourself; when `LIBSQLITE3_FLAGS` is already set in your shell, `.cargo/config.toml` does not override it, so add `-DSQLITE_ENABLE_QPSG` to it yourself. `packages/tarball/release.sh` checks whether the executable in the archive has it enabled, and does not package it if not. Reason: since SQLite 3.54, queries that have a JOIN and also filter by a parameter (for example `session-limit.lua` in the broadband scenario), and `LIMIT ?`, are planned again on every parameter binding, and every read binds again. On 2026-11-08, PAP logins of the broadband scenario (concurrency 64) measured on an Apple M4 Max were about 17,000 per second without this flag and about 46,000-47,000 with it, the same as SQLite 3.53. The flag keeps the query plan from changing with the parameter values; the difference only shows when `ANALYZE` has been run and statistics exist, and the server does not generate statistics. `test_sqlite_built_to_keep_query_plans` in `cargo test` checks that the SQLite that was built has it enabled.

**Checking the configuration only:**

```bash
./target/release/rradiusd check --config /etc/rradius/rradius.toml
```

It runs the same checks as startup: the configuration file, the dictionary, the migration directory, script syntax, and the LDAP `ca_file`. When everything passes, it prints `configuration ok` to stdout and exits with status 0; when there is a problem, the error is written to stderr and the exit status is 2. It does not open ports, create the data directory, or connect to LDAP or other nodes, so problems such as a wrong LDAP account password or a wrong SQL query cannot be found this way.

The example configuration is in `rradius-server/examples/rradius.toml` and can be started as it is:

```bash
cd rradius-server
cargo run -- serve --config examples/rradius.toml
```

### 1.1 Docker

The image is built from the `Dockerfile` in the project root. The example configuration is in `docker/config/`; it authenticates with the `sql` handler and writes accounting to the `accounting` table.

A single node:

```bash
docker/init-secrets.sh
docker compose up --build
```

Three nodes, each with different host ports (`1812`/`1813`, `2812`/`2813`, `3812`/`3813`):

```bash
docker/init-secrets.sh
docker compose -f compose.cluster.yaml up --build
```

To check that the service responds (Status-Server, an Access-Reject for a user that does not exist, and that the accounting script can write; each run leaves one test record for `o'brien` in the `accounting` table):

```bash
docker/smoke-test.py --auth-port 1812 --acct-port 1813
```

- **Secrets:** `docker/init-secrets.sh` generates the three secrets under `docker/secrets/`; this directory is not in git. The NAS must use the contents of `radius.secret`.
- **Adding users:** add a numbered SQL file in `docker/config/migrations/` (files that have already been applied cannot be changed, see section 2.5), then restart.
- **client:** The example allows three private networks, because the source address the container sees is decided by Docker. Change it to your devices' addresses.
- **TACACS+:** Not enabled in the example. To use it, add `[tacacs]` to the configuration and publish `49/tcp` in Compose.
- **Metrics:** Not enabled in the example. To use it, add `[metrics]` and change `bind` to an address the container can be reached on from outside (see section 2.8). RadSec (`[radius.tls]`) needs rradius-enterprise.
- **Viewing status:** The example configuration turns on `[metrics]` (`127.0.0.1:9812` inside the container, with no port opened to the outside); use `docker compose exec rradius rradiusd status --config /etc/rradius/rradius.toml --watch` to see what this node is doing (see section 2.8).
- **Reload:** After you change the scripts or clients in `docker/config/`, run `docker compose kill -s HUP rradius` to apply them without a restart (see section 2.9).
- **Data:** Kept in a named Compose volume. `docker compose down --volumes` deletes the data.
- **Container privileges:** Every service in the example Compose files sets `read_only: true` (read-only root file system), `tmpfs: [/tmp]`, `cap_drop: [ALL]` and `no-new-privileges`. The server runs as an ordinary user (uid 10001) and writes only to the data volume (`/var/lib/rradius`) and `/tmp`, so these restrictions do not get in the way; the single-node and three-node smoke tests both pass with these settings. If something you add yourself (for example log files, the accounting spool, or generated certificate files) must write to another path, mount a volume for it, or it fails because the file system is read-only. A container that is already deployed must be recreated with `docker compose up --detach` for this to apply.
- **EAP:** Not enabled in the example. To use it, add `[radius.eap]` and put the certificates in `docker/config/` (see section 2.12).
- **Accounting spool:** Not enabled in the example. To use it, `[radius.accounting_spool].dir` must point to a path inside a volume (for example `/var/lib/rradius/spool`; `/etc/rradius` is read-only), or replacing the container loses accounting that was already acknowledged (see section 2.11).
- **Stopping:** Both the SIGTERM sent by `docker stop` and the SIGINT from Ctrl+C do the following in order: make `/readyz` return 503 and keep answering normally for a while, then stop accepting new requests and wait for the requests in progress to be answered, then close the database cleanly and wait for the records still in the buffer to be written, for at most 5 seconds (see section 2.16 for the details and the settings of the two waits). SIGKILL does none of this, and the local database is rebuilt at the next start. Stopping also works during startup: if the signal arrives while it waits for the cluster or applies migrations, it shuts the node down and exits with status 0, and the next start continues the work. While a migration file that has been running for more than 25 seconds is in progress, it waits the full 25 seconds and then exits; this does not count as a clean shutdown, and the next start rebuilds the local database. If the signal arrives before the node is up, it first waits for the node to come up (at most 25 seconds) and then shuts down cleanly; if the node cannot come up within 25 seconds (for example the cluster has no majority), it exits right away, which is not a clean shutdown, and the data directory may be left with a `lock` file, and the next start rebuilds the local database.
- **Shutting down a multi-node cluster takes about 15 seconds:** By default it first waits 5 seconds for the load balancer to take this node out (`[shutdown] ready_delay_ms`); requests in progress usually finish quickly (it waits at most `drain_ms`, 5 seconds by default); after that hiqlite waits another 9.5 seconds before leaving the cluster (it is still the leader during this time), so that the other nodes can elect a new leader. So give `docker stop` more time (`docker stop -t 30`, or `stop_grace_period: 30s` in Compose, which `compose.cluster.yaml` already sets); the systemd default of 90 seconds is enough. If SIGKILL arrives because the time ran out, the next start rebuilds the local database.
- **A broken node exits by itself:** When the node's Raft core stops because of an error (for example a follower that has fallen too far behind, see section 2.2 below), or when hiqlite panics internally, the process logs `exiting so that a restart recovers it` and exits with status 1. So make sure it restarts automatically: set `Restart=on-failure` in systemd, and `restart: unless-stopped` in Docker (the example Compose files both set it).

## 2. Configuration overview

The configuration file is in TOML format and is made of the following sections:

| Section | Required | Purpose |
| --- | --- | --- |
| `dictionary` | No | Path of the RADIUS dictionary file (FreeRADIUS format, for example `share/dictionary` in the project); a relative path is relative to the directory of the configuration file. When it is not written, it is the default dictionary path fixed at build time: `$RRADIUS_PREFIX/share/rradius/dictionary`, or `/usr/share/rradius/dictionary` when no prefix is set (or it is an empty string or `/`); use `--version` to see the actual value. The deb, rpm and FreeBSD packages all install the dictionary at this location |
| `include` | No | Other files to read together, for example `conf.d/*.toml` ("Splitting the configuration into several files" below) |
| `[lua]` | Yes | The top-level Lua script and the Lua time limits |
| `[cluster]` | Yes | The embedded database node and the cluster |
| `[auth]` | No | Auth chains; when absent, `[lua].script` authenticates |
| `[radius]` | No | Enables RADIUS |
| `[radius.accounting_spool]` | No | Writes accounting to local disk first when the database fails (section 2.11) |
| `[radius.accounting_log]` | No | Writes every accounting request that was answered as one line of JSON to a text file (section 2.18) |
| `[sessions]` | No | The SQL that `session list` and `session show` use to look up online sessions (section 2.19) |
| `[queries.<name>]` | No | Named read-only queries: every node runs them periodically in the background, the results become gauge metrics, and the `query` command can also run one once (section 2.20) |
| `[radius.eap]` | No | EAP, for 802.1X and WPA-Enterprise (section 2.12) |
| `[tacacs]` | No | Enables TACACS+ |
| `[metrics]` | No | Enables the metrics endpoint (section 2.8) |
| `[log]` | No | Log level, format and the authentication log (section 2.10) |
| `[accounts]` | No | The SQL that runs when `rradiusd user` manages accounts (section 2.14) |
| `[shutdown]` | No | The two waits after SIGTERM is received (section 2.16) |
| `[databases.<name>]` | No | External MySQL and PostgreSQL databases, for the `sql` handler and `db_query` (section 2.17) |
| `[runtime]` | No | `pin_threads`: pin each worker thread to one CPU, on Linux and FreeBSD only (section 2.4) |

At least one of `[radius]` and `[tacacs]` is required. All sections reject fields they do not recognize; a misspelled field makes startup fail and names the field.

**Enterprise edition features:** The following features need rradius-enterprise, and are described in the rradius-enterprise usage document. When the configuration file uses these sections or fields, rradiusd fails at startup, on SIGHUP and in `check`, and the message says rradius-enterprise is needed.

| Feature | Setting or command | Purpose |
| --- | --- | --- |
| RadSec | `[radius.tls]`; `tls_name`, `coa_sender` and `max_radsec_connections` of `[[radius.clients]]` | RADIUS over TLS (RFC 6614) |
| Proxy | `[proxy]` | Forwards RADIUS requests to other servers by realm, including RadSec upstreams, finding upstreams through DNS, and spooling accounting when all upstreams are down |
| Disconnect and change of authorization | `send_disconnect`, `send_coa`, the `disconnect` and `coa` commands, `[radius.coa]` | Asks a NAS to disconnect a session or change its authorization (RFC 5176), and receives requests from other systems and forwards them to the NAS |
| Dynamic clients | `[radius.dynamic_clients]`, `[accounts.clients]`, the `client` command | Looks up UDP clients that are not in the configuration file from the database |
| Finding the WPA-PSK from the four-way handshake | `[radius.dpsk]`, `[accounts.dpsk]`, the `dpsk` and `dpsk-seal` commands | One WPA-PSK per person, with no need to register devices first |
| EAP-AKA' | `aka-prime` in `methods`, `[radius.eap.aka]`, `[accounts.aka]`, the `aka` and `aka-seal` commands | USIM cards you issue yourself |
| EAP session resumption | `resumption_ms`, `shared_tickets` and `ticket_seal_file` in `[radius.eap]` | Skips the full handshake when a client reconnects |
| OCSP | `ocsp` and `ocsp_url` in `[radius.eap]`, `[radius.tls]` and `[tacacs.tls]` | Checks revocation when the CA only offers OCSP and does not publish a CRL |
| TACACS+ over TLS | `[tacacs.tls]`; `tls_name` of `[[tacacs.clients]]` | RFC 9887 |
| Two-factor | `[mfa]`; `mfa` of `[[radius.clients]]` and `[[tacacs.clients]]`; the `otp` command | After the password, asks for a 6-digit code from an authenticator app (TOTP) or an HOTP token |
| Diameter and SWx | `[diameter]` | Gets the authentication vectors of EAP-AKA' from the operator's HSS (3GPP SWx) |

The minimal configuration:

```toml
dictionary = "/usr/share/rradius/dictionary"

[lua]
script = "auth.lua"

[cluster]
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[radius]
[[radius.clients]]
address = "127.0.0.1"
secret_file = "local.secret"
```

**Checking the configuration file in your editor:** `rradiusd schema` prints the JSON Schema of the configuration file to standard output; it reads no configuration file and starts nothing.

```bash
rradiusd schema > rradius.schema.json                  # field descriptions in English
rradiusd schema --lang zh-TW > rradius.schema.json     # field descriptions in Traditional Chinese
```

Write `#:schema ./rradius.schema.json` on the first line of `rradius.toml`, and editors that support TOML schemas (for example Taplo, and Even Better TOML for VS Code) complete sections and fields as you edit, and mark misspelled fields, type errors and values that are not in an enumeration.

- **The schema is generated by the program that reads the configuration,** so it matches the program's rules; generate it again after you change versions.
- **Enterprise edition sections are not in the rradiusd schema,** and writing them is marked as an unrecognized field; the schema from the enterprise edition's `rradiusd schema` includes them. A few fields listed in the table above (for example `resumption_ms` of `[radius.eap]` and `tls_name` of `[[radius.clients]]`) are in the schema of both editions; with rradiusd, `check` reports that rradius-enterprise is needed.
- **The schema does not replace `check`.** It only looks at structure and types; the SQL parameter rules (section 2.14), whether files exist and their permissions, conditions between fields (for example only one of `secret` and `secret_file` may be set), the contents of certificates and keys, the upper and lower limits of numbers and so on are still checked only by `check` and by the startup checks.
- **Every field has a one-sentence description,** which the editor shows during completion and when you hover over a field. The default is English, `--lang zh-TW` switches to Traditional Chinese, and any other value exits with status 2. The description only gives the purpose and the unit; the details and limits are still those of this document; the description of each top-level section is a URL that links to the matching section of this document.

**Splitting the configuration into several files:** The top level of the main configuration file can have `include`, which lists the files to read together; a configuration file without it is not affected at all.

```toml
include = ["conf.d/*.toml"]     # before the first table; a top-level setting like dictionary
dictionary = "/usr/share/rradius/dictionary"
```

- **It reads as if the text were appended to the main file.** Paths are relative to the directory of the main configuration file (absolute paths also work); the file-name part can use `*` (any characters) and `?` (one character), but the directory parts cannot. Files are loaded in byte order of the path; when the list has several entries, in the order of the list. A file name that starts with `.` is not matched by `*` or `?` (editor temporary files, hidden files), unless that part of the pattern itself starts with `.`.
- **No matching file is not an error** (an empty or missing `conf.d/`); a name without wildcards that points to a file that cannot be read is an error.
- **The first meaningful line of an included file (not a blank line, not a comment) must start with `[`,** that is, a table (`[radius]`, `[[radius.clients]]`); top-level settings (`dictionary`) can only be written in the main file, otherwise they would fall into the last table of the previous file and quietly change the meaning, so they are rejected at load time. An included file cannot have `include` itself.
- **No overriding.** The same setting or the same table appearing in two files is an error (the same as within one file: `duplicate key`), and the later one does not override the earlier one; the elements of arrays such as `[[radius.clients]]` accumulate in file order.
- **Relative paths are always relative to the directory of the main configuration file,** including `key_file` and `secret_file` inside included files.
- **Password files with permissions that are too open produce a warning.** When the client's `secret_file`, the cluster's `secret_raft_file` and `secret_api_file`, the database's `password_file`, LDAP's `bind_password_file` and `machine_password_file`, and `key_file` of `[radius.eap]` (the private key of the EAP-TLS, PEAP and TTLS server certificate; whoever can read it can impersonate this server) are read, and the file permissions let the group write or give others any access (for example 0644), startup and every reload (SIGHUP) log one WARN line that gives the setting name, the path and the permissions, and not the contents. It only warns and does not refuse: the password files of the examples and of Docker are 0644 and still start. Once the permissions are tightened to 0600 or 0640, the warning stops. 0600 or 0640 is recommended for private key files. The keys that protect table contents (`ticket_seal_file`, `key_file` of EAP-AKA' and so on) are a different matter, and wrong permissions there still refuse startup.
- **Syntax and type errors point to the included file and the line in it:** `/etc/rradius/conf.d/20-radius.toml: line 3: ...`, without showing the setting's value. For checks that run after merging (for example a duplicate client name), the message only names the main configuration file; when the message has a name in single quotes, `(in <file> line <line>)` is added after it, listing the lines where that name is written as a quoted string (at most three, the rest as `and N more`); when the name is only in the main file or is not found, nothing is added.
- **`config show` prints the merged settings of all files (including the `include` line itself, so the output is not a configuration file; do not save it and use it as one);** `check`, `config diff` and reload (SIGHUP) all read the included files: after you change an included file you must send SIGHUP for it to take effect, and when there is an error the whole configuration stays as it was.
- **Every included file must be complete TOML by itself** (an unclosed string or array is rejected, and the error points to that file); a byte order mark (BOM) at the start of a file is ignored; a file must be smaller than 1 MiB, and `include` can match at most 1000 files; a file matched by two names is read only once, and a match on the main configuration file itself is skipped.
- **When an included file or the directory it is in is writable by the group or others,** one WARN line is logged, because it can change who is allowed to log in; when the main configuration file itself or its directory is writable by the group or others, startup, reload and `check` also each log one WARN line (`configuration file … can be written by its group or by others`); being readable only does not warn; the files must be protected like the main file, and to put passwords in them use `*_file`.
- **Older servers do not recognize `include`** (an unknown field, and loading is refused); keep this in mind when you mix old and new versions.

**Viewing the effective configuration, comparing nodes, generating a starter configuration:** The three `config` subcommands all work from the same schema, start nothing and write no files; rradius-enterprise has them too, and they include the enterprise edition's sections.

```bash
rradiusd config show --config /etc/rradius/rradius.toml   # effective settings with defaults filled in
rradiusd config diff node1.toml node2.toml                 # compare the settings of two nodes
rradiusd config init --lang zh-TW > rradius.toml           # a minimal starter configuration file (about 30 lines), comments in Traditional Chinese
rradiusd config init --full > rradius.toml                 # the full commented version, listing every setting and default
```

- **`config show`:** First loads the configuration the same way as `check` (when there is an error, it prints the same message and exits with status 2), then fills in the defaults of the omitted fields in the sections the file already has, and prints it as TOML. Sections the file does not have (for example no `[tacacs]`) are not added. Defaults that are computed at startup (`[radius] sockets`, `[cluster] read_connections`) use the values computed on the machine where this command runs, and the first line says so. Without `--config` it reads the default configuration file.
- **Passwords are always shown as `<hidden>`,** for example the client's `secret`, `secret_raft`, `secret_api` and LDAP's `bind_password`. The output can be pasted to other people, but file paths (for example `secret_file`) are still shown.
- **`config diff`:** Both files get their defaults filled in before they are compared, so a default written on one side and omitted on the other is not a difference. One differing field per line: `radius.max_auth_inflight: 4096 | 8192`, and `(absent)` when only one side has it. `radius.clients` and `tacacs.clients` are paired by `name` (by `address` when there is no name), so a different order is not a difference, and the path is written like `radius.clients[core-switches].secret`; other lists (for example `auth.handlers`) are compared by position, because the order matters. When passwords differ, it only writes `differs (hidden)`. `cluster.node_id` and `cluster.listen` are listed at the end under `node-specific:` and do not count as differences. When they are the same it prints `no differences` with status 0; when there are differences the status is 1; when the configuration has an error or the arguments are wrong, the status is 2. It does not read the files the configuration refers to (Lua scripts, password files, certificates), and only compares the paths.
- **`config init`:** Prints a minimal starter configuration file (about 30 lines) that you can save and use as it is: `dictionary` (the value is the default dictionary location of this version, the same as the third line of `--version`; change it if the dictionary is somewhere else), `include = ["conf.d/*.toml"]`, `[lua] script`, `[cluster]` (without `[[cluster.nodes]]`, so it is a single node listening on `127.0.0.1`), `[radius]` with one example client, and `[metrics]` open on `127.0.0.1:9812` (the `status` command needs it, see section 2.8; this port has no authentication, so do not bind it to an outside address). `secret_raft`, `secret_api` and the example client's `secret` are 32 random alphanumeric characters, different every time; the other nodes of the cluster must use the same cluster secrets, and the client's `secret` must also be set on the NAS. Other fields are not listed. With `--full` it prints the full commented version: the same working settings, and all other fields listed as comments with their defaults and descriptions, which you set by removing the `#`. `--lang` selects English or Traditional Chinese, and applies to both. After you save the file, create `auth.lua` (and change `dictionary` too if the dictionary is not at the default location), and `check` passes.

### 2.1 `[lua]`

| Field | Default | Description |
| --- | --- | --- |
| `script` | (required) | The top-level script |
| `query_timeout_ms` | 5000 | How long all `sql_query`/`sql_execute` calls of one request can wait for the database in total; it must be greater than 0. Each call gets the time that is left, and calls after it is used up fail at once with a timeout; it counts time waiting for the database, and time waiting for an execution slot is not counted |
| `max_execution_ms` | 1000 | The upper limit of a script's execution time in one request (not counting time waiting for the database); it must be greater than 0 |
| `max_memory_mb` | 256 | The most memory each Lua VM may use (MiB); it must be greater than 0, and a change needs a restart. The number of VMs equals the number of CPU cores, so the worst case is the product of the two (section 4.1) |

The two time limits apply to both the top-level script and the `lua` handler in the auth chains.

The top-level script is responsible for:
- RADIUS accounting (Accounting-Request)
- TACACS+ authorization and accounting
- Authentication that no auth chain covers: there is no `[auth]` in the configuration, or `[auth]` has only named chains and the client does not specify `auth`

The top-level `script = "…"` and `query_timeout_ms` of `[cluster]` from older configurations are no longer accepted; at startup the message points to the new locations (`[lua].script`, `[lua].query_timeout_ms`).

### 2.2 `[cluster]`

| Field | Default | Description |
| --- | --- | --- |
| `data_dir` | (required) | Data directory |
| `secret_raft` / `secret_raft_file` | (one of the two) | Password for Raft traffic, at least 16 characters |
| `secret_api` / `secret_api_file` | (one of the two) | Password for API traffic, at least 16 characters |
| `node_id` | 1 | This node's id. Must be ≥ 1 and appear in `nodes` |
| `listen` | `127.0.0.1` without `[[cluster.nodes]]`, `0.0.0.0` with it | Listen address (IPv4 or IPv6). A single node has no other nodes to connect to, and the database API is protected only by a shared secret, so by default it is not exposed. When `[[cluster.nodes]]` is present and `listen` is not set, loading the configuration (startup, reload, `check`) logs a warning (`cluster.listen is not set`). Change it to this host's private or WireGuard address. To keep `0.0.0.0`, write `listen = "0.0.0.0"` explicitly and the warning goes away |
| `migrations_dir` | (none) | Directory of SQL files applied at startup, see section 2.5 |
| `migrations_wait_ms` | 60000 | Time limit at startup for the cluster to become ready and for the local replica to catch up (each stage is timed separately). Must be greater than 0, see "Catching up with the cluster at startup" below |
| `stop_without_leader_ms` | (not set) | When there has been no leader for longer than this, authentication and Status-Server requests are not answered. Must be greater than 0. Restart after changing it, see "When the database is unavailable" below |
| `snapshot_every` | 100000 | Number of Raft log entries between two snapshots. Must be greater than 0, see below |
| `read_connections` | twice the worker threads, at most 32 | Number of connections that read the local database, 1 to 128, see below |
| `[[cluster.nodes]]` | single node: id 1, Raft `127.0.0.1:8100`, API `127.0.0.1:8200` | Cluster members. Each has `id`, `raft` and `api` (`host:port`) |

Password files are read with the trailing newline removed. In a multi-node cluster every node lists all members (itself included). A node with id 1 must exist, and ids must not repeat:

```toml
[cluster]
node_id = 2
data_dir = "data"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[[cluster.nodes]]
id = 1
raft = "10.0.0.1:8100"
api = "10.0.0.1:8200"
[[cluster.nodes]]
id = 2
raft = "10.0.0.2:8100"
api = "10.0.0.2:8200"
[[cluster.nodes]]
id = 3
raft = "10.0.0.3:8100"
api = "10.0.0.3:8200"
```

This node listens on the `raft` and `api` ports of its own entry in `nodes`. Writes (`sql_execute`) are replicated to all nodes through Raft and need a majority of the cluster to be available. Reads (`sql_query`) read the local replica.

**Nodes do not need the system's CA certificates.** Connections between nodes are plain HTTP (see below) and use no certificates. But building the HTTP client for those connections fails if the system has no CA certificate at all, so rradius includes a built-in set of root certificates. A host without `ca-certificates`, or a minimal container, can still form a cluster. LDAP over TLS (section 3.5) and the script function `http_request` (section 4.11) do not use the built-in certificates. They still read the system certificates or `ca_file`.

**Three-node cluster throughput.** Measured on 2026-11-08 on one Apple M4 Max, on Linux (arm64) in Docker. Each of the three nodes was limited to 3 CPUs (0–2, 3–5, 6–8) and connected over 127.0.0.11 to .13. Three `rradius-bench` programs each hit one node, sharing CPUs 9–13, two runs each. Logins are the broadband scenario's PAP (`sql` plus `lua` handlers). Accounting writes the database once per Accounting-Start (`acct-sql.toml`). The two single-node columns use the same configuration, with all three load programs hitting the same node:

| Case | One node, 3 CPUs (per second) | One node, 9 CPUs (per second) | Three nodes, 3 CPUs each (per second) |
| --- | --- | --- | --- |
| Login, concurrency 192 | 54,900–55,800, p99 5.4–5.7 ms | 73,000–73,200, p99 4.2–4.3 ms | 110,600–118,000, p99 3.2–3.8 ms |
| Login, concurrency 768 | 51,600–54,500, p99 18–19 ms | 66,000–66,200, p99 14–15 ms | 105,000–113,600, p99 9–12 ms |
| Accounting, concurrency 192 | 68,000–68,100, p99 6.6–6.8 ms | 116,400–116,500, p99 5.2–5.4 ms | 73,700–74,100, p99 8.5–9.6 ms |
| Accounting, concurrency 768 | 66,000–66,700, p99 13–14 ms, 480–630 timeouts per round | 98,600–100,900, p99 13–14 ms | 102,400–107,200, p99 20 ms |

- **Logins are handled on each node separately** and read the local replica, so three nodes give about twice one node with 3 CPUs, and 50 to 70 percent more than one node with the same 9 CPUs. The CPUs of all three nodes were fully used, but the gain is not threefold. The more CPUs are busy at the same time, the more CPU time each login takes (about 54 µs on one node with 3 CPUs, about 74 µs on three nodes, about 97 µs on one node with 9 CPUs). This machine has performance cores and efficiency cores, and the virtual machine's CPUs may land on either kind. This was not checked.
- **Accounting writes all go through the leader** and wait for a majority of nodes to write the log entry, so more nodes do not raise the write ceiling. At concurrency 192 it is only a little above one node with 3 CPUs, with a higher p99. At concurrency 768 it is about the same as one node with 9 CPUs, and it has none of the timeouts of one node with 3 CPUs.
- **The three nodes were on the loopback of one machine, so there was no network delay.** Between separate hosts, every write also waits for round trips between nodes (longer across data centers), and the accounting numbers will be lower than here. Logins are not affected.

**Read connections `read_connections`:** Reads such as the `sql` handler's account lookup and a script's `sql_query` must first get a read connection. When there are not enough connections, requests queue until others finish reading. The default is twice the worker threads (the CPUs available to the process), at most 32. Each connection has its own page cache and files, so a host with many CPUs should not open hundreds by default, and the measurements showed no benefit beyond 14. You can set 1 to 128. Writes are not affected and still use a single write connection.

- Measured on 2026-11-08 on Linux (arm64) in Docker (server on CPUs 0–6, `rradius-bench` on 7–13, 2 runs each), with the broadband scenario's PAP login (two database reads per login): with 4 connections (the former value), concurrency 64 gave 46,800–47,100 per second and p50 1.32–1.33 ms. With the default of 14 connections it gave 74,500–75,300 and p50 0.76–0.77 ms. At concurrency 1024 it rose from 44,200–44,400 to 57,400–58,200. With pinning on (section 2.4), 14 connections reach about 90,000 per second at concurrency 64. An experimental build with 28 connections was almost the same as 14.
- The cost is memory: each connection has its own page cache, and after the same load 14 connections use about 27–47 MB more than 4 (159–185 MB against 112–158 MB). You can lower it when memory is tight. Restart after changing it.

**Snapshot interval `snapshot_every`:** Each time this many Raft log entries have accumulated (one entry is one write, or one batch of writes committed together), the node copies the whole database as a snapshot, after which the entries before it can be discarded. Writes wait during the copy, and the larger the database, the longer it takes, so snapshots that are too frequent slow the service down. But the entries between two snapshots must be kept, and after an abnormal exit the restart must replay them from the last snapshot.

- Measured on 2026-11-08 on Linux (arm64) in Docker, with the broadband scenario's PAP at concurrency 256 plus 2,000 accounting writes per second, for 12 minutes. With 10,000, snapshots ended up every 5–6 seconds, each copying 150–350 MB, and in 4 ten-second intervals throughput fell to 15,000–43,000 per second with p99 42–54 ms. With 100,000 only 10 snapshots were taken, the average was 57,273 per second (54,365 with 10,000), the lowest interval was 54,232, and p99 never went above 15 ms.
- The cost (same machine, stop after 90,000 writes, then start again): after a normal stop (SIGTERM) both settings restart within 0.1 seconds. **After an abnormal exit (SIGKILL, power loss) the restart takes 0.95 seconds with 10,000 and 10.3 seconds with 100,000**, because the entries since the last snapshot must be replayed, about 0.1 ms each. The data directory is also larger (75 MB against 26 MB).
- When there are few writes and you care more about recovering quickly after an abnormal exit, lower it to, for example, 10,000 or 20,000. Restart after changing it.
- hiqlite also uses this value to decide how far behind a node must be before it receives a snapshot instead of catching up entry by entry (twice this value, 200,000 entries by default; hiqlite's own default is 20,000).

**Connections between nodes are not encrypted, and there is no TLS setting to turn on.** In a multi-node cluster the `raft` and `api` ports are plain text:

- **Anyone who can listen on that network can read the data being replicated,** including the passwords in the `users` table and accounting records.
- **`secret_api` is sent in plain text:** a node puts it in an HTTP header when it asks for the members. Anyone who captures it, and can reach the `api` port, can run SQL on the database, for example add or change users.
- **`secret_raft` is not sent directly, but the contents of the connection are not protected:** anyone who can intercept the connection can tamper with what follows the handshake.

So run the connections between nodes over a network you trust. Across data centers or untrusted segments, put these connections inside WireGuard or IPsec and set the `raft` and `api` addresses to addresses on that network. Either way, use a firewall to limit both ports to the other nodes, and bind them to that interface with `listen`. The default `0.0.0.0` listens on all interfaces, so a warning is logged when `listen` is not written explicitly.

**Protect the connections between nodes with WireGuard.** Here is an example with three nodes: physical addresses `192.0.2.1` to `192.0.2.3`, WireGuard network `10.99.0.0/24`, interface `wg0`, using Linux and `wg-quick`. NAS devices still connect to each node's physical address, and `[radius]` and `[tacacs]` need no change.

1. Each node generates its own keys. The private key stays on that host:

   ```sh
   umask 077
   wg genkey | tee /etc/wireguard/private.key | wg pubkey > /etc/wireguard/public.key
   ```

2. Each node writes `/etc/wireguard/wg0.conf` and lists the other two nodes as peers. For node 1:

   ```ini
   [Interface]
   Address = 10.99.0.1/24
   ListenPort = 51820
   PrivateKey = <contents of node 1's private.key>

   [Peer]   # node 2
   PublicKey = <contents of node 2's public.key>
   Endpoint = 192.0.2.2:51820
   AllowedIPs = 10.99.0.2/32

   [Peer]   # node 3
   PublicKey = <contents of node 3's public.key>
   Endpoint = 192.0.2.3:51820
   AllowedIPs = 10.99.0.3/32
   ```

   With `AllowedIPs` set to just the peer's one address, WireGuard accepts only packets encrypted with that peer's key and coming from that address, so no other node can impersonate it. Enable it with `systemctl enable --now wg-quick@wg0`.

3. In `[cluster]`, use the WireGuard addresses, and set `listen` to this node's WireGuard address instead of the default `0.0.0.0`. For node 1:

   ```toml
   [cluster]
   node_id = 1
   listen = "10.99.0.1"
   data_dir = "data"
   secret_raft_file = "raft.secret"
   secret_api_file = "api.secret"

   [[cluster.nodes]]
   id = 1
   raft = "10.99.0.1:8100"
   api = "10.99.0.1:8200"
   [[cluster.nodes]]
   id = 2
   raft = "10.99.0.2:8100"
   api = "10.99.0.2:8200"
   [[cluster.nodes]]
   id = 3
   raft = "10.99.0.3:8100"
   api = "10.99.0.3:8200"
   ```

4. Use a firewall so the `raft` and `api` ports accept connections only from `wg0`. `listen` alone is not enough: by default Linux also accepts packets that arrive on the physical interface with `10.99.0.1` as the destination. With nftables, for node 1:

   ```
   table inet rradius_cluster {
     chain input {
       type filter hook input priority 0; policy accept;
       udp dport 51820 ip saddr { 192.0.2.2, 192.0.2.3 } accept
       udp dport 51820 drop
       tcp dport { 8100, 8200 } iifname { "wg0", "lo" } accept
       tcp dport { 8100, 8200 } drop
     }
   }
   ```

   `lo` is the interface used when the host connects to its own address.

5. rradius must start after `wg0` is up, otherwise it cannot bind the `listen` address. With systemd, add this to the rradius service (`systemctl edit <service name>`):

   ```ini
   [Unit]
   Requires=wg-quick@wg0.service
   After=wg-quick@wg0.service
   ```

6. Check: on the physical interface you should not capture `8100` and `8200`, only WireGuard's UDP. Seeing plain text on `wg0` is normal, because that is the decrypted content.

   ```sh
   tcpdump -ni eth0 'tcp port 8100 or tcp port 8200'   # should show no packets
   tcpdump -ni eth0 udp port 51820                     # traffic between the nodes
   wg show wg0                                          # every peer has a recent latest handshake
   ```

Things to watch:

- **When you add or remove a node, change every host.** The new node must be added to every other host's peers and firewall source addresses, and to `[[cluster.nodes]]` as well. After editing the files you can apply them with `wg syncconf wg0 <(wg-quick strip wg0)` without interrupting existing connections.
- **There is no key revocation.** To replace a key, first generate the new key on that node, then update the `PublicKey` of that peer on every other host. During that time this node cannot reach the others, so change one at a time and wait for it to catch up before the next, so the cluster keeps a majority throughout.
- **You do not need to adjust the MTU.** `wg-quick` sets `wg0` a little smaller automatically, and TCP adapts.
- **An environment that already has IPsec can use it too, with the same principle:** put the node addresses on the protected network, bind `listen` to that address, and let the firewall accept only the protected traffic.

#### Catching up with the cluster at startup

After a node starts, with or without `migrations_dir`, it first waits for a leader, then writes a marker of its own through the leader (the table `rradius_nodes`), and starts listening only when its local replica has read it. Writes are applied in order, so reading the marker means the accounts, NAS devices and keys written to the cluster during the downtime are already local, and the first response can see them. When this is done it logs `cluster node N has caught up with the cluster`.

- Each of the two waits is limited by `migrations_wait_ms`. When no usable leader appears, see "Restarting with no majority" below. When there is a leader but the node cannot catch up, it exits with status 1 if there is a `migrations_dir` (the same as a failed migration). Without one it logs a WARN line (`serving before catching up`) and serves as usual, and in that case answers may come from old data. A node that has started serving keeps catching up in the background and logs `has caught up with the cluster` when done.
- After a long downtime with a lot of data to apply, catching up takes longer.

#### When the database is unavailable

- **No response, rather than a rejection.** When a login cannot be decided because the cluster database timed out, no leader was found, or the number of waiting writes is full, RADIUS sends no reply and counts it in `rradius_radius_discarded_total{reason="database"}`. TACACS+ authentication and authorization return ERROR, and the device asks the next server (RFC 8907). The same applies when the `ldap` handler's directory is unavailable (every URL cannot be reached or times out, the service account's bind is refused or fails, the search fails), and when password verification times out (`sql`, the `ldap` `attribute` mode, including waiting for slow hash rounds beyond `timeout_ms`). These are also counted in `reason="database"`: this label covers the database, LDAP, domain controllers and verification timeouts, and the name does not change. This applies to the `sql` handler, a `lua` handler that returns `reject` or fails (when it returns `ignore`, the next handler is tried as usual), a top-level script that has no accept, and EAP conversations. When it is certain that the account does not exist, the password is wrong, or the script's database calls all succeeded, the request is still rejected with Access-Reject (FAIL).
- **Writes have a limit.** At most 16384 writes can wait to be handed to the cluster, and when that is full they fail immediately. A batch that has been handed over waits at most 30 seconds, and then its slots are released and failure is reported. When there has been no usable leader for more than 5 seconds, new writes fail immediately and are not queued. So an isolated node does not run out of memory from queuing forever.
- **A write handed to the cluster is reported only when its result is known.** While a combined-commit `sql_execute` (an INSERT, UPDATE, DELETE or REPLACE that returns no rows) is still queued, it is abandoned when the request's database time limit is reached, and it is never handed over afterwards. Once it has been handed to the cluster (it may wait while the leader takes a snapshot or when the leader changes), it waits for the cluster's reply and is no longer abandoned midway, for at most 30 seconds. So "failure reported, but written later" remains only when there was no result within those 30 seconds. In that case the call reports an unknown result (the log says `may have been committed`) and it does not count as the database being unavailable: accounting is neither spooled nor answered, and the NAS retransmits. The retransmit may duplicate it once, so accounting scripts must still be safe to run repeatedly. When a login gets no response, writes the script has already made (an assigned IP, a used verification code) may also remain in this case. The cost is that when the leader is very slow, a request waits up to 30 seconds longer and holds its slot. The waiting time still counts toward that request's database time limit, so later calls in the same request may time out at once, and accounting may therefore go to the spool and run again on replay. Statements with `RETURNING` and others that are not committed together still follow the time limit as before, and may still be written after the timeout.
- **An isolated node reads old data.** Reads use the local replica and do not need a majority, so read-only logins are still answered. But an account disabled on the other side, a deleted NAS or a revoked key is still valid on this node, and SIGHUP does not help, until the network recovers.
- **`stop_without_leader_ms`:** When it is set and the node has had no usable leader for longer than this, it answers no Access-Request and no Status-Server sent to the authentication port (`reason="no_leader"`), and TACACS+ authentication and authorization return ERROR, so that the NAS and load balancers treat it as failed and ask other nodes. Accounting continues as usual (it goes to the spool when there is one). The cost is that when only this one node of the cluster is left, it also stops serving read-only logins, so it is not set by default.
- **Restarting with no majority.** When one of three nodes is left, the running node keeps serving read-only logins. When it restarts, hiqlite first waits about 6 seconds for a leader, then rradius waits for a usable leader up to `migrations_wait_ms`. If neither finds one:
  - Without `stop_without_leader_ms`, and with a local replica that has applied every file in `migrations_dir` (number, name and content all match), or without a `migrations_dir`: it logs a WARN line (`serving without a leader before catching up`) and serves from the local replica, with `rradius_cluster_caught_up` at 0. After the other nodes return and a leader is elected, it catches up in the background, logs `has caught up with the cluster`, and the metric becomes 1.
  - When the directory has files the local replica has not applied: it exits with status 1 (`not ready`, the message names the file), because without a leader they cannot be applied. When an applied file has been changed or renamed: it exits with status 2.
  - With `stop_without_leader_ms` set: it exits with status 1, whether or not there is a `migrations_dir`.
  - With a `migrations_dir`, the starting node itself is also limited by `migrations_wait_ms`, so it must be greater than about 6 seconds (the default is 60 seconds), otherwise it times out and exits before hiqlite returns.
  - The cost: a node that was down for a long time and restarts during a network partition serves with the data from before it stopped. For example, an account disabled in the meantime can still log in. If this matters, set `stop_without_leader_ms`.
- **A follower that has fallen too far behind breaks and must be restarted.** When, while a follower is stopped or disconnected, the cluster writes more than `snapshot_every` entries (100,000 by default), and the leader takes a snapshot and discards the old log, hiqlite 0.16 cannot read its own log after the follower returns, and the Raft core stops (it logs `quit RaftCore ... Integrity: Attempt to read non-existent data`). This was reproduced on 2026-11-10 on three nodes with the default settings by writing 130,000 entries while one node was paused. With `snapshot_every = 50` it reproduces every time. The node would then answer from a replica that no longer follows the cluster, so rradius detects this, shuts down cleanly and exits with status 1 (see section 1, "Stopping"). After it restarts it catches up and recovers. Without an automatic restart, start it by hand.
- **Stopping the leader does not hand over leadership first.** hiqlite 0.16 waits 9.5 seconds before shutting down (still as leader). After that the other nodes need 1.5 to 3 seconds to elect a new one, and writes time out during the election. Clients reconnecting can extend the outage further (about 15 seconds was measured when the leader was stopped by force, see [chapter 16 of the manual](manual/16-cluster.md#across-data-centers); a normal stop was not measured). So during an upgrade, stop the leader last (the node where `rradius_cluster_is_leader` is 1).
- **How errors are classified.** Timeouts, no leader found, nodes that cannot be reached, an internal channel that is closed, and snapshot failures all count as the database being unavailable. A wrongly written SQL statement or a constraint violation (for example UNIQUE) is a statement error and is still rejected or returned as an error. The `user` admin command exits with status 3 for the former.
- **A usable leader:** a follower knows who the leader is. A leader gets confirmation from a majority within 5 seconds (an isolated leader still thinks it is the leader, and this tells them apart).

#### Cluster state metrics

Each node reads its own Raft state once per second and provides it at `/metrics` (section 2.8):

| Metric | Meaning |
| --- | --- |
| `rradius_cluster_leader` | 1 when there is a usable leader, otherwise 0 |
| `rradius_cluster_is_leader` | 1 when this node is the leader |
| `rradius_cluster_applied_index` | Number of the last entry the local replica applied. Compare it with other nodes to see how far behind it is |
| `rradius_cluster_seconds_without_leader` | Seconds without a usable leader, sometimes 0 |
| `rradius_cluster_caught_up` | 1 when, after startup, the local replica has caught up with the cluster. 0 when it is serving from the local replica without a leader and has not caught up |

During an upgrade, stop the node where `rradius_cluster_is_leader` is 1 last. Stopping the leader requires a new election, and writes time out for about 15 seconds (see [chapter 16 of the manual](manual/16-cluster.md#across-data-centers)).

#### Adding and removing nodes: the `cluster` command

Adding a node while running needs no command: the new node's `[[cluster.nodes]]` lists all current members (itself included) and its `data/` is empty, and after it starts it joins by itself. The list must include the current leader, otherwise it cannot join (see [chapter 16 of the manual](manual/16-cluster.md#add-a-node)). To remove a node and to view the members, use the `cluster` command. For the steps and notes, see [chapter 16 of the manual](manual/16-cluster.md#add-a-node).

```sh
rradiusd cluster members --config /etc/rradius/rradius.toml
rradiusd cluster remove --config /etc/rradius/rradius.toml 4
rradiusd cluster remove --force --config /etc/rradius/rradius.toml 3
```

- **`members`** lists every member by id: `voter` (a voting member) or `learner` (only replicates data, and a node that is joining is briefly this), `leader`, and the Raft and API addresses. A member this configuration does not list is marked `(not in this configuration)`. An id this configuration lists but the cluster does not have gets its own line, `not in the cluster (listed in this configuration)`. The exit code is still 0 when there are differences.
- **`remove`** first finds the leader, confirms that the id is a member, that the node is already down (its API address does not answer within 2 seconds), and that enough voters remain after the removal. Only then does it ask the leader to remove it, wait for the cluster to confirm, and print `removed node N`. When only 1 or 2 voters would remain, add `--force`. The last one cannot be removed. A node that is still answering cannot be removed.
- **Finding the leader:** it asks each node in the configured order and uses only the answer of a node that can see a leader, so a node that was removed but is still running (it cannot see a leader) does not mislead the command. A node that does not answer at all (for example a host that is off, or packets being dropped) is moved to the end in the next round, so the command need not wait its 5 seconds every round.
- **Exit code 3 does not always mean nothing was done.** If the connection fails or times out after the remove request was sent, the cluster may already have removed that node. Check with `cluster members` first, then decide whether to run it again.
- The command connects through the API port (the `api` address) with `secret_api`, and rradius does not need to run locally.

| Case | Standard output | Exit code |
| --- | --- | --- |
| List members | One line per member | 0 |
| Removal done | `removed node 4` | 0 |
| The id is not a cluster member | `no node 9 in the cluster` | 1 |
| The leader refuses the removal | (none; the error is on standard error) | 1 |
| Wrong command or configuration, that node still answers, too few voters would remain, `secret_api` differs from the cluster | (none; the reason is on standard error) | 2 |
| No node reports a leader within 30 seconds | (none; standard error says the cluster did not respond) | 3 |
| No response after the remove request was sent, or it did not take effect within 30 seconds | (none; standard error says the removal may have completed, check with `cluster members`) | 3 |

### 2.3 `[radius]` and `[tacacs]`

```toml
[radius]
bind = "0.0.0.0"     # default 0.0.0.0; can also be a list, for example ["192.0.2.1", "2001:db8::1"]
auth_port = 1812     # default 1812
acct_port = 1813     # default 1813
max_auth_inflight = 4096  # default 4096, authentication requests handled at the same time
max_acct_inflight = 4096  # default 4096, accounting requests handled at the same time
# sockets = 7       # sockets opened per address and port; on Linux the default equals the number of worker threads, other platforms allow only 1
# recv_buffer_bytes = 4194304  # receive buffer of each socket (SO_RCVBUF); the system default is used when omitted

[[radius.clients]]
name = "core-switches"          # optional; address is used as the name when absent; must be unique within the list
address = "192.168.2.0/24"      # an IP or a CIDR network
secret_file = "core.secret"     # or secret = "...", choose one
auth = "vpn"                    # optional; selects a named auth chain

[tacacs]
bind = "0.0.0.0"     # default 0.0.0.0
port = 49            # default 49 (needs privileges)
max_body_len = 65536 # default 65536, a larger packet closes the connection; must be greater than 0
max_connections = 1024 # default 1024, connections open at the same time
max_connections_per_source = 64 # default 64, connections open at the same time from one source address, must be greater than 0
idle_timeout_ms = 300000 # default; the connection is closed when no complete packet arrives within this time, must be greater than 0
enable = "deny"      # default; an enable request gets FAIL directly. With "login" it is authenticated like a login (section 4.6)
# TACACS+ over TLS ([tacacs.tls]) needs rradius-enterprise

[[tacacs.clients]]
name = "router-1"
address = "10.0.0.5"
secret_file = "router-1.secret"
# authorize_group = "group15"   # optional; authorization is decided by rules instead, see "TACACS+ authorize groups" below
```

**When `bind` is a list:**

- Every address serves authentication and accounting, and the reply is sent from the socket that received the request. When you list an explicit address, that address is the source. The source of a wildcard address is decided by routing; when the host has several addresses, the NAS may drop the reply because the source does not match. In that case list explicit addresses.
- With more than one address, an IPv6 address receives IPv6 only, so `["0.0.0.0", "::"]` receives both. **`["::", "10.0.0.1"]` receives IPv4 only on `10.0.0.1`.** With a single address, behavior is the same as before whether or not it is written as a list: whether `"::"` also receives IPv4 is decided by the system.
- A wildcard address cannot be listed together with another address of the same family. For example, `["0.0.0.0", "192.0.2.1"]` fails at startup, because the wildcard already holds the port on every address of that family. An IPv4-mapped address (`::ffff:10.0.0.1`) is also not allowed in a list of several addresses; write it as IPv4.
- **When binding fails, the process exits** with status 1 and a message that gives the address and port. For `[tacacs]` and `[metrics]`, a bind failure is still only one error line, and these can have only one address.

**`sockets`: how many receive loops each port has.**

- On Linux, each authentication and accounting port of each address opens `sockets` SO_REUSEPORT sockets, each with its own receive loop. The kernel assigns each packet to one of them by source address and port, and the same source always lands on the same one, so a retransmission is handled by the same loop and de-duplication and replies are not affected; the reply is sent from the socket that received the request. The default equals the number of worker threads (the number of CPUs available to the process), and the startup log records `RADIUS: <N> sockets on each address and port`.
- **On other platforms it can only be 1.** macOS and similar systems do not spread packets over several sockets, and a value above 1 makes startup fail. 0 also fails.
- **Another process of the same user can bind the same port.** SO_REUSEPORT lets processes with the same effective UID that also set this option share the port, and packets are split between them. When you start a second rradius with the same configuration, it fails first on the cluster port and exits, so it never touches the RADIUS port; but another configuration with a different cluster port and the same RADIUS port takes part of the traffic. Run it under a dedicated account, and set `sockets = 1` if you need the earlier behavior (then startup fails when the port is in use).
- A change needs a restart; SIGHUP records the settings that need a restart.
- For measurements see section 2.4.

**`recv_buffer_bytes`: the receive buffer of each socket.**

- When omitted, `SO_RCVBUF` is not set and each socket gets the system default (`net.core.rmem_default` on Linux, `net.inet.udp.recvspace` on macOS and FreeBSD), the same as before.
- When set, `SO_RCVBUF` is set to this size on the authentication and accounting port of every address and on every one of the `sockets`. The system only needs to allow this limit (`net.core.rmem_max` on Linux, `kern.ipc.maxsockbuf` on macOS and FreeBSD); you do not have to raise the system default that affects every program. The range is 4096 to 1073741824, and startup fails outside it.
- The startup log records `RADIUS: receive buffer <actual size> bytes on each socket (asked <configured value>)`. On Linux the value read back is twice the configured value (the kernel counts its bookkeeping space), which is normal.
- **If the size cannot be obtained, only a warning is logged and the server keeps serving.** Linux truncates it to `rmem_max`; macOS and FreeBSD refuse a value above the limit and the socket keeps the system default; both log one WARN line that names the sysctl to change. The same configuration deployed to a machine with a lower limit still starts, and `rradiusd tuning` (section 8.1) lists limits that are too low.
- The upper bound of memory use is `recv_buffer_bytes` × `sockets` × number of addresses × 2 ports; for example 14 sockets, one address and 8 MiB is 224 MiB. A buffer is actually occupied only while packets pile up.
- A change needs a restart; SIGHUP records the settings that need a restart.

- A client is matched to the source address by the longest prefix, regardless of the order in which clients are listed; listing the same network twice makes startup fail.
- `name` must be unique within one list (`[[radius.clients]]` or `[[tacacs.clients]]`), and when `name` is not written, `address` counts as the name; a duplicate makes startup and `check` fail with exit code 2 and the message `duplicate client name '…'`. The name is used to separate metrics and logs, and to find the client when the accounting spool is replayed. Earlier versions accepted configurations with duplicate names, so rename them before upgrading. Names that differ in case are different names, and the RADIUS and TACACS+ lists may use the same name.
- A source that is inside no client network is ignored (RADIUS does not reply, and TACACS+ closes the connection immediately).
- A secret file must not be empty after the trailing newline is removed.
- Startup fails when a section exists but has no clients.

#### TACACS+ authorize groups `[tacacs.authorize_groups]`

Decide TACACS+ authorization with one rule per line, without writing a script. Rules apply by the device that connects: for a device whose `[[tacacs.clients]]` sets `authorize_group`, authorization looks only at the rules of that group.

```toml
[tacacs.authorize_groups]
group15 = [
  'permit service=shell cmd\* {priv-lvl=15}',
  'permit service=shell cmd=.*',
]
readonly = [
  'permit service=shell cmd\* {priv-lvl=1}',
  'permit service=shell cmd=show cmd-arg=(version|interfaces|clock)',
  'permit service=shell cmd=(ping|traceroute|exit)',
]

[[tacacs.clients]]
name = "core-routers"
address = "10.50.0.0/24"
secret_file = "tacacs.secret"
authorize_group = "group15"
```

- **Format:** `permit|deny <pattern> ... [{<AV> <AV> ...}]`, separated by spaces. An AV pair inside `{}` is written as `name=value` (or the optional `name*value`); for `permit` it is returned to the device unchanged; `deny` cannot have `{}`.
- **When a value contains spaces, wrap it in double quotes,** for example `{shell:roles="network-admin vdc-admin"}`. Spaces between the quotes and `}` are part of the value, and the quotes themselves are also sent to the device: what you write is what is sent, so the example above sends the single AV pair `shell:roles="network-admin vdc-admin"`. If the device does not want the quotes, do not write them. Quotes are not supported in the pattern part.
- **Always use single quotes.** A TOML double-quoted string does not accept sequences such as `\*`, and reading the configuration fails.
- **A pattern is a regular expression that must match the whole** of one AV pair in the request, including the `=` or `*` in the middle. So `cmd=show` does not match `cmd=showx`, and `cmd\*` matches the `cmd*` that a device sends when it opens a shell (EXEC authorization). Matching is done on bytes, and `.` matches any byte. `user`, `port` and `rem_addr` do not take part in matching.
- **A rule holds when each of its patterns matches at least one AV pair.** A rule with no pattern matches everything.
- **The first rule that holds decides:** `permit` returns PASS_ADD and `deny` returns FAIL. When no rule holds, the result is FAIL, and an empty group (`[]`) makes every authorization FAIL. To exclude specific commands, put `deny` before the broader `permit`.
- **To limit which commands a device may run, write an allow list; do not rely on a `deny` list.** A rule compares only the commands and arguments the device sends: an abbreviation on the device (`show run`) and a subcommand that is not listed (`show tech-support`) are not caught by `deny cmd=show cmd-arg=running-config`, but they match `permit cmd=show`. The `readonly` group above permits only the listed commands, and the rest fall to "no rule holds" and are refused. Also, a rule holds as soon as each pattern matches some AV pair of the request, so `cmd-arg=interfaces` also permits further arguments added after `show interfaces`; real read-only access depends on what the privilege level on the device (`priv-lvl`) can do, and the authorization rules are a second barrier.
- **A device that has `authorize_group` set does not run script authorization.** Authentication and accounting are unchanged. Every user of the same group's devices gets the same authorization; when you need to decide by user or role, do not set `authorize_group` on those devices and use a script instead (section 4.6).
- **Limits:** there is no way to write a value that contains spaces without sending the quotes, and there are no escape characters; only PASS_ADD is returned, with no PASS_REPL and no `server_msg`.
- **Checks:** at startup, on SIGHUP and in `check`, these fail: a rule that does not start with `permit`/`deny`, an invalid regular expression, `{}` that is not paired or has content after `}`, double quotes inside `{}` that are not paired, an AV pair without `=`/`*` or with an empty name, more than 255 octets or more than 255 AV pairs, an empty group name, an `authorize_group` that points to a group that does not exist, and `authorize_group` set on a `[[radius.clients]]`. The message names `tacacs.authorize_groups.<name>` and the number of the rule.
- Rules are applied with SIGHUP, and the next request on an already open connection uses the new rules (section 2.9).
- **Measurement: rules are faster than a script.** Measured on 2026-11-09 on Linux (arm64) in OrbStack on an Apple M4 Max, `main` `5ef8248`, with the server pinned to CPUs 0–6 and `rradius-bench` to 7–13, `tacacs-author,concurrency=64,threads=7` (`service=shell`, `cmd=show`, `cmd-arg=version`), three runs per row, none with timeouts or `invalid`:

  | Authorization method | requests/s | p50 / p99 | Server CPU per request |
  | --- | --- | --- | --- |
  | Script that always accepts | 364,700–372,900 | 0.16 / 0.44–0.47 ms | about 14 µs |
  | Script that decides by `service` and `cmd` and sets `priv-lvl` | 311,800–317,900 | 0.19 / 0.50–0.51 ms | about 17–18 µs |
  | 1 rule, `permit service=shell cmd=show {priv-lvl=15}` | 439,700–469,400 | 0.12–0.13 / 0.37–0.38 ms | about 7 µs |
  | 20 rules, the first 19 do not hold | 407,600–429,600 | 0.13 / 0.40–0.45 ms | about 8.4–9.0 µs |

  - Rules do not go through Lua, and the CPU per request is less than half of a script; 19 extra rules that do not hold cost about 1.5–2 µs more.
  - The server used only about 3 CPUs here, and throughput was limited by 64 connections with one request at a time each (64 divided by p50), not by CPU.

TACACS+ over TLS (`[tacacs.tls]`) needs rradius-enterprise.

Dynamic clients (`[radius.dynamic_clients]`) need rradius-enterprise.

### 2.4 Overload and retransmission

**RADIUS retransmission detection** (per RFC 5080): packets with the same source IP, source port, Identifier and Request Authenticator are treated as retransmissions.

- The original request is still being processed: the packet is dropped silently.
- The original request was already answered, within the last 10 seconds: the original response is sent again, without running a script or querying LDAP or SQL. This is also true when the response was an Access-Reject.
- Packets that fail verification do not affect the decision.

**Accounting scripts must still prevent duplicate writes themselves.** When a device updates `Acct-Delay-Time` and sends again, it uses a new Identifier, so for the server it is a new packet and is not detected. Use `Acct-Session-Id` plus `Acct-Status-Type` as a unique key, for example by creating a `UNIQUE` index and using `INSERT OR IGNORE`.

**Overload protection:**

- When RADIUS exceeds `max_auth_inflight` or `max_acct_inflight`, packets are dropped silently without an Access-Reject, and the device retransmits. Authentication and accounting limits are counted separately, so accounting does not take places from authentication.
- The accounting scripts that **run** at the same time (RADIUS and TACACS+ both count) do not exceed the number of CPU cores minus 2, and at least 1; the rest wait in turn. The worker threads that remain are used for logins and other requests. A script that is waiting for the database gives up its place. The time spent waiting for a place does not count toward `max_execution_ms` and `query_timeout_ms`. So when accounting surges, mainly accounting itself becomes slower, and logins are not delayed by seconds.
- Every memory allocation of the server takes at least one cache line (128 bytes on macOS aarch64, 64 bytes on other platforms), so that the data of different Lua VMs does not share a cache line and slow each other down: when not aligned, a compute-only accounting script on macOS drops from about 274 to 22 records per second. The cost is memory: a small allocation also takes a whole cache line (with the system allocator used before, up to about 3.8 times more than unaligned was measured on macOS). Memory is requested from jemalloc, not from the system allocator. With the system allocator used before, memory on macOS kept growing under steady load and was not reclaimed (the broadband scenario in which every login runs a script (an example in rradius-enterprise) grew to 4.7 GB in 40 seconds of continuous logins), and the packet path on Linux was also slower.

  **When you build it yourself for arm64 Linux,** jemalloc fixes the page sizes it supports at build time, and by default it takes the value of the build machine. If you build on a machine with 4 KiB pages and run on a kernel with 16 or 64 KiB pages (for example some ARM servers and the Raspberry Pi 5 kernel), the server fails at startup. Setting `JEMALLOC_SYS_WITH_LG_PAGE=16` at build time makes it run on 4, 16 and 64 KiB pages; the project's Dockerfile already does this on arm64.

  The table below is measured with `rradius-bench` (see [Performance measurements](#9-performance-measurements)), with the two versions interleaved and run 3 times each, the server and the tool on the same machine. macOS is a 14-core Apple M4 Max; Linux is Docker's Linux on the same machine (aarch64, 14 CPUs, Debian bookworm). Compute-only is `acct-cpu.toml` at concurrency 64, login is PAP in `accept.toml` at concurrency 64, and memory is `accept.toml` with continuous logins for 60 seconds:

  | | macOS, system allocator | macOS, jemalloc | Linux, system allocator | Linux, jemalloc |
  | --- | --- | --- | --- | --- |
  | Compute-only (records/s) | 232–287 | 236–276 | 227–268 | 240–260 |
  | Login (requests/s) | about 72,100 | 70,900–75,500 | 102,900–103,900 | 123,500–125,800 |
  | Memory after 60 seconds (the specification's "Steady load") | 827 MB, still growing | 197 MB, flat after 15 seconds | 172–268 MB | 271–490 MB, fluctuating |

  On Linux, jemalloc keeps some more memory (0.27–0.49 GB measured), and idle memory is returned to the system gradually over jemalloc's default time; 5 seconds after the load stopped it had not all been returned, but it does not keep growing. The Linux jemalloc in the table was built with 4 KiB pages; with the Dockerfile's 64 KiB build, the same measurement kept memory at 286–291 MB, with logins at 126,500 requests/s and compute-only at 257–265 records/s (2 runs each).

  `cargo test --release --test scenarios_test -- --ignored` runs "Steady load": the broadband scenario logs in continuously for one minute, and between 30 and 60 seconds memory must not grow by more than twenty percent plus 16 MiB. It takes one minute and needs an idle machine, so it does not run by default. The number of runs is small, and the machine gets hotter as it runs, so the numbers in one row get lower from round to round; they are only suitable for comparing two versions, not as the capability of the server.
- **Several receive sockets and CPU pinning.** On Linux each port has by default as many receive loops as worker threads (`[radius] sockets`, section 2.3). You can also pin the worker threads to CPUs:

  ```toml
  [runtime]
  pin_threads = true   # default false, Linux and FreeBSD only
  ```

  Each worker thread runs on only one CPU, assigned in turn from the CPUs available to the process, and the startup log records `pinned <N> runtime worker threads to CPUs [...]`. Threads created later (the threads that handle database queries and password hashing) get all the CPUs the process could originally use again, and are not pinned to the CPU of some worker thread. When pinning fails, a warning is logged and the server runs unpinned as usual. **It is off by default,** because when CPUs are shared with other processes (a shared host, a container with a CPU quota) pinning can be slower; turn it on only when this machine runs only rradius and its CPUs are not limited. Setting `true` on other platforms makes startup fail, and a change needs a restart.

  Under Linux, to keep rradius on a fixed set of CPUs, for example apart from other services, restrict the whole process: start it with `taskset -c 0-3`, or set `CPUAffinity=0-3` in the systemd unit. The number of worker threads and the limit on simultaneous checks of slow password hashes (`{crypt}`, `{argon2}` and PBKDF2) both equal the number of CPUs allowed. A change needs a restart.

  The table below was measured on 2026-11-08 on Docker's Linux (arm64) on the same Apple M4 Max, with the server pinned to CPUs 0–6 and `rradius-bench` pinned to 7–13 (`taskset`), 2 runs each. These are numbers inside a virtual machine, where pinning is especially sensitive to virtual machine scheduling; measure real hardware separately:

  | Setting | `accept.toml` PAP, concurrency 64 | Concurrency 1024 | Broadband scenario PAP, concurrency 1024 |
  | --- | --- | --- | --- |
  | `sockets = 1` (before) | 181,000–189,000 requests/s, p99 1.46–1.52 ms | 193,000–207,000 requests/s, about 1,744 timeouts per run | 43,800–43,900 requests/s |
  | Default (7 sockets) | 235,000–244,000 requests/s, p99 1.41–1.47 ms | 222,000–238,000 requests/s, 0–9 timeouts | 46,600 requests/s |
  | Default plus `pin_threads = true` | 314,000–315,000 requests/s, p99 0.61–0.62 ms | 298,000–299,000 requests/s, 0–11 timeouts | 59,500–60,700 requests/s |

  With one socket the server used only 42–46 seconds of 84 CPU seconds; the limit was the single receive loop, not the CPU. At concurrency 1024, about 1,744 requests per run with one socket got no response; the likely cause is that packets arriving at the same moment exceed the receive buffer of one socket, and spread over several sockets there are almost none. macOS does not spread packets over several sockets (measured: 1 and 7 sockets both give about 80,000 requests per second), so only 1 can be used.

- The functions available to scripts (`get_attr`, `set_attr`, `sql_query`, `log` and so on) are created only once per VM and per script. Each request creates only its own global table, and the functions in that table are a few small Lua functions that pass the data of this request to the shared functions. So the stack traceback of a script error has one more location that starts with `rradius:`; those are these small functions, not the location of the script's error. Before, a dozen functions were rebuilt for every request, which took about 18% of the server's busy time in a script that only returns `accept`; now it is about 3%. The table below was measured with `rradius-bench` interleaved with the version before the change, on a 14-core Apple M4 Max with macOS, the server and the tool on the same machine:

  | Measurement (4 runs each) | Before | After |
  | --- | --- | --- |
  | `accept.toml` PAP, concurrency 64 | 66,300–68,400 requests/s, p99 1.50–2.00 ms | 67,500–80,400 requests/s, p99 1.06–1.66 ms |
  | `accept.toml` server CPU per login | 118–124 µs | 79–96 µs |
  | Broadband scenario PAP at 30,000 requests per second, server CPU over 12 seconds | 56.4–63.5 seconds, p50 0.18–0.21 ms | 43.6–48.3 seconds, p50 0.13–0.15 ms |
  | Broadband scenario PAP, concurrency 64 | 41,900–47,300 requests/s | 43,700–46,700 requests/s |

  In the broadband scenario at concurrency 64, throughput did not change within the spread of the measurement: every login needs a database query, so it measures waiting time, not CPU. For a script that only returns `accept`, most of the remaining time on macOS is spent sending the reply (`sendto` on the same UDP socket takes about 60%). The machine gets hotter as it runs, so the numbers in one row get lower from round to round; they are only suitable for comparing two versions.

  **On Linux, sending the reply is not the bottleneck.** The same `accept.toml` PAP run on Docker's Linux (aarch64, 14 CPUs, Debian bookworm) on the same machine:

  | Load | Throughput | p50 | CPU of each server worker thread | Whole machine idle |
  | --- | --- | --- | --- | --- |
  | Concurrency 64 | about 202,000 requests/s | 0.25 ms | about 25% | 70% |
  | Concurrency 256 | about 227,000 requests/s | 0.57 ms | about 24% | 67% |
  | Concurrency 1024, tool with 4 or 8 threads | about 229,000–230,000 requests/s | 0.56 ms | about 30% | 85–87% |

  In `perf` samples, `sendto` takes only about 11% of the busy time. The macOS kernel sends one at a time on the same socket and Linux does not have this, so the reply is still sent from the socket that received the request. When throughput stopped at about 230,000 requests per second, no thread had used up its CPU, yet each thread spent 30–40% of its time waiting for a CPU (`%wait` in `pidstat`), which is probably the scheduling of the Docker Desktop virtual machine; so this environment cannot measure the upper limit of the server, and that needs a separate Linux machine with the load tool on another machine. Also, the authentication record is written by one log thread, which used about 37% of a CPU at 230,000 requests per second and becomes the limit at about 2.5 times more load; the authentication record does not depend on the log level, and when you do not need it, turn it off with `auth = false` in `[log]` (see [section 2.10](#210-logging-log)).
- Separate limits do not mean complete isolation: authentication and accounting still share CPU and the database, and accounting scripts are still better when they are light. Authentication and authorization scripts do not have this limit, and an authentication script that uses a lot of CPU still affects the others. The table below was measured with `rradius-bench` (see [Performance measurements](#9-performance-measurements)): a local 14-core macOS machine over loopback, 3 runs each, with logins sent at 100 per second:

  | Accounting load | Concurrent requests | Accounting throughput | Login p50 | Login maximum |
  | --- | --- | --- | --- | --- |
  | None | 0 | none | 0.36–0.48 ms | 1.8–3.5 ms |
  | One database write each | 256 | 40,900–41,500 requests/s | 0.95–1.02 ms | 9.3–10.5 ms |
  | One database write each | 4096 | 40,900–41,700 requests/s | 1.13–1.24 ms | 22–43 ms |
  | About 30 ms of compute-only work each | 256 | 283–300 requests/s | 0.19–0.28 ms | 486–667 ms |
  | About 30 ms of compute-only work each | 4096 | 280–289 requests/s | 0.20–0.31 ms | 593–660 ms |

  The two database-write rows were measured again after concurrent writes were merged into one commit (see the next item); when every write was committed on its own, accounting throughput was about 21,000 requests/s, login p50 0.41–0.76 ms and maximum 7.1–17 ms. Now accounting does twice as much and the server is busier, so logins are slightly slower. In the two compute-only rows, a `rate_not_met` warning appeared in 5 of 6 runs, so the login numbers are for reference only. At concurrency 4096, 1 request was dropped in 2 of 3 runs (`server_discarded_overload`), and that request timed out, so the exit code of those two runs was 2.

- **Concurrent writes are committed together.** `sql_execute` calls that wait at the same time in different requests are merged into one batch, if they are INSERT, UPDATE, DELETE or REPLACE statements that return no rows, and are committed in one transaction and one Raft record, up to 256 per batch and up to 4 batches in flight at the same time. The database applies writes with only one thread, and every commit has a fixed cost, so when each was committed on its own, accounting writes topped out at about 21,000 per second on the macOS machine of the table above, and after merging at about 41,000 (much higher on Linux, see the next item); at concurrency 256, p99 fell from 47–74 ms to 12–15 ms; under a light load of 1,000 per second the latency is the same as before (p50 0.18 ms). Each call still gets its own number of affected rows. When one statement in a batch fails, the batch is split in half and each half is committed again, until the one that fails is committed alone, so only its call fails; the number of times is recorded in `rradius_db_split_batches_total`. If this number keeps rising, some statements fail often (for example a retransmitted accounting request hits a UNIQUE), and you can change them to a form that does not fail, such as `INSERT OR IGNORE`. Other statements (with `RETURNING`, `SELECT`, `COMMIT`, `ROLLBACK` and so on) are still committed one by one as before. When the cluster is unavailable, all calls of the batch return the same error and are not retried. For a write whose call has already timed out, what has not yet been handed to the cluster is not written, because the caller has already been told that it failed and the accounting request may already have gone into the spool; **what has already been handed to the cluster is reported only after the cluster returns the result** (see section 2.2, "When the database is unavailable"). The cost is that **a very slow statement in a batch (for example a large DELETE) makes the other writes in the same batch wait with it**, so such work should be done in small batches (see [chapter 18 of the user manual](manual/18-operations.md#purging-old-data)). Several `sql_execute` calls of one script are not in the same batch, because the script waits for each write to finish.
- **The accounting write limit on Linux.** Measured on 2026-11-08 on Docker's Linux (arm64) on the same Apple M4 Max, with the server pinned to CPUs 0–6 and `rradius-bench` pinned to 7–13, `acct-sql.toml` (one database write for each Accounting-Start), two runs each:

  | Case | Concurrency 256 (requests/s) | Concurrency 1024 (requests/s) |
  | --- | --- | --- |
  | Data directory in a folder that Docker Desktop shares from the Mac | 91,800–92,200, p99 13 ms | 88,600–89,700, p99 22 ms |
  | Data directory on the container's own disk | 95,700–108,200, p99 6–8 ms | 82,700–94,400, p99 15–19 ms |
  | Same, with the number of accounting scripts running at the same time changed from 5 to 7 (experimental build) | 115,200 | 111,300–112,800 |

  - **The limit per batch and the number of batches at the same time are not the limit:** an experimental build changed the batches at the same time from 4 to 16 and each batch from 256 to 1,024, and all four combinations were between 74,000 and 88,000 requests per second, with no difference.
  - **When the data is in a shared folder,** the only SQLite write thread spends a large part of its time writing the WAL through FUSE, so it is slower than on the container's disk and p99 is about double. In a production deployment the data directory should be on a local disk.
  - **The number of accounting scripts running at the same time (CPUs minus 2, see above) affects the result by up to about twenty percent at high concurrency,** and with as many as VMs there is no difference. This limit deliberately leaves worker threads for logins, so it was not changed.
  - **The limit is not in the cluster's commit flow.** Measured layer by layer in the same environment on 2026-11-15: with the same pragmas, plain SQLite with a sequential key does about 1.8 million rows/s and with a random TEXT primary key about 150,000 rows/s (almost every row dirties one 4 KB page); a standalone hiqlite test program writes about 500,000–930,000 rows/s; and when the request path of rradius does not write to the database, about 360,000 requests/s. So the Raft commit is not the limit.
  - **With database writes added, about 110,000 requests/s is limited by latency, not by CPU.** At concurrency 256 each request takes about 2.2 ms: waiting for an accounting place about 0.2 ms, the database call about 1.3 ms (transaction about 0.5, queue about 0.3, result sent back about 0.5), and getting the place back about 0.2 ms; a batch averages only about 22 rows, and of 7 CPUs only about 2 were used. With the server limited to 2, 4, 7 and 10 CPUs the results were 68,000, 91,000, 110,000 and 126,000 requests per second in that order. At concurrency 1024 and above, waiting for an accounting place takes most of the latency.
  - **The primary key of the table affects the limit.** If the accounting table uses a random string as its primary key, the limit of the single write thread is about 150,000 rows per second; with an increasing integer primary key the server side still reaches only about 120,000, so this is not the bottleneck today, but when you need a higher write rate, choose an increasing primary key.
  - These are numbers inside an OrbStack VM, where the cost of waking a thread on another CPU is high, and real hardware may differ. Normal loads are far below this limit: 1 million online sessions reporting every 5 minutes is about 3,300 accounting requests per second.
- When the sequence number of a TACACS+ packet is even or 255, the connection is closed directly with no reply (RFC 8907 section 4.1).
- When a TACACS+ packet does not have the right format after decryption (usually because the shared secret differs), the server replies with ERROR and then closes the connection, logs `a body did not decode, so the shared secret may be wrong`, and counts it in `rradius_tacacs_malformed_total`. Per RFC 8907 section 10.5.2, this connection accepts no new requests; the device reconnects. An error that has nothing to do with the secret, such as a mismatched sequence number, gets only an ERROR reply and the connection stays open.
- **single-connection:** when the device sets the single-connection flag on the first packet of the connection, the reply also sets this flag, meaning that the server accepts several sessions on the same connection (RFC 8907 section 4.3), so the device does not have to reconnect each time. A device that did not set the flag can also keep using the connection, and rradius does not close it by itself when a session ends.
- When TACACS+ exceeds `max_connections`, the new connection is closed immediately. When connections open at the same time from one source address exceed `max_connections_per_source` (default 64), its new connections are also closed immediately and do not take places from `max_connections`, so when one host in a network opens its full number of connections, other hosts can still connect; source addresses are compared in canonical form, an IPv4-mapped IPv6 address and the IPv4 address it maps to count as the same source, IPv6 is not merged by prefix, and each address counts as one source. When many devices hide behind one address (NAT, a jump host), raise `max_connections_per_source` above the number of connections they open at the same time. An idle connection from a client in the configuration also takes quota; it is closed and released when no complete packet arrives within `idle_timeout_ms`, or when a response cannot be written within 10 seconds. The time a script runs does not count as idle. A device that does not send packets for a long time has to reconnect, which usually does not affect service.
- When packets are dropped or connections closed, at most one line per port per second is logged, for example `RADIUS accounting overloaded: discarded 37 packets`.

These three settings must be greater than 0. The defaults are estimates. A suitable value is roughly "throughput × the retransmission timeout of the device"; a larger value does not help, because the device has already stopped waiting.

### 2.5 Table migrations

rradius itself has no tables. The tables used by the `sql` handler and scripts can be written as SQL files and placed in `[cluster].migrations_dir`, and the server applies them to the whole cluster at startup.

```toml
[cluster]
migrations_dir = "migrations"   # resolved against the directory of the configuration file
migrations_wait_ms = 60000      # time limit for waiting for the cluster to be ready, default 60000, must be greater than 0
```

```
migrations/
  1_users.sql
  2_reply.sql
```

**File name rule:** `<number>_<name>.sql`, with numbers that start at 1, are consecutive and do not repeat. The directory must not contain other files or subdirectories.

**When they are applied:** before RADIUS/TACACS+ requests are handled. The flow is to check the directory and file names first (the node is not started yet, so errors show immediately), wait for the cluster to be ready, apply the files that are not applied yet, and then wait for the local replica to catch up. So scripts and handlers can assume that the tables exist. When `migrations_dir` is not set, these steps are not run and there is no extra waiting; however, whether or not it is set, a node that starts for the first time has to wait until it joins the cluster before it begins to serve.

**Restart:** files that are already applied are skipped and only new files are run. The record is kept in the `_migrations` table, one row per file, including the SHA-256 of the content.

**A single file must not exceed 2,000,000 bytes:** a migration file holds the structure of tables (`CREATE TABLE`, `ALTER TABLE`, indexes) and a small amount of initial data; a normal file is only a few KB and never meets this limit. At startup, the files that are waiting to be applied are sent to the cluster in batches in order of number, with a total of no more than 2,000,000 bytes per batch (a batch is one write of the cluster, and one entry of the hiqlite log is at most 2,097,118 bytes, and a larger one makes the database of the node panic; measured on 2026-11-18: 2,090,000 bytes works and 2,100,000 panics, and the limit leaves a little room for the file name and hash), so it does not matter if many files add up to more. **But a single file larger than 2,000,000 bytes cannot be split:** the node exits with status 2, the message names that file and its size, and nothing is sent to the cluster. Such a file is usually a large amount of data (tens of thousands of rows or more) written as `INSERT`: split the file, or do not put it in a migration and write it with a program or an admin command.

**Applied files cannot be changed:** changing the content or renaming makes startup fail. To change the structure, add a new file. Rollback is not supported.

**One transaction per file:** when a file fails, all changes of that file are rolled back and no record is left; the files before it stay applied. Fix that file and restart.

**Multiple nodes:**

- The directory content of every node must be the same. When you add a file, **update the files on all nodes first, then restart them one by one**.
- When a node has fewer files than were applied (it has not yet got the new files), it logs one warning line and starts as usual.
- Do not start nodes with different directory contents at the same time. When hiqlite meets an inconsistency, it panics in the write thread, and all writes of that node fail. rradius checks at startup, but it cannot exclude this completely when nodes start at the same time.

**Status codes:**

| Status code | Cause |
| --- | --- |
| 2 | The directory does not exist, a file name or number is wrong, an applied file was changed or renamed, a single pending file exceeds 2,000,000 bytes, or a file failed to run; the message names the file, except when `_migrations` cannot be read after a failed run, where the message explains this and keeps the SQL error |
| 1 | The cluster was not ready within `migrations_wait_ms` (for example most nodes are not started yet), a cluster error occurred while applying, or the local replica did not catch up |

`migrations_wait_ms` limits three stages separately: connecting to the cluster, applying the files, and the local replica catching up, and each stage is timed on its own. After connecting to the cluster, when "applied migrations" is read while the cluster is in the middle of an election (no leader), this is not a failure: the node re-reads every 200 ms until a leader answers, and the limit is still `migrations_wait_ms`; only when it expires does the node exit with status 1 and the last error. **It must be greater than the run time of the longest SQL file:** not only on this node; when other nodes run a slow file, this node also has to wait, and a node that rejoins needs about the same time to replay it. A timeout ends with status 1, and files that were already sent still finish running in the cluster; raise the value and restart. Raise this value before a large data change.

**A single file that takes more than 30 seconds:** a cluster node waits only 30 seconds for one write, no matter how large `migrations_wait_ms` is, so when a single file runs longer than 30 seconds, the first start always exits with status 1 after about 30 seconds, with the message `local client write timed out` and an explanation of this limit. The file has already been sent to the cluster and keeps running to the end; it is not cancelled because of this exit. After it finishes, start again with `migrations_wait_ms` set longer than the file takes, and you will see that it is already applied and the server serves normally. Measured on 2026-11-15 on an Apple M4 Max: for a `CREATE TABLE ... AS WITH RECURSIVE` of 400 million rows, the first start exited after about 55 seconds, and after raising `migrations_wait_ms` to 180000 the second start served after about 116 seconds (the machine had other load at that time); 120 million rows were applied in about 20 seconds with no timeout. When a single node restarts, it also has to replay that write, so the second start takes about as long as the file. When the file can be split into several smaller files, splitting is less likely to meet this limit.

A table that was already created by hand can be put into `1_*.sql` and brought under management by writing it with `CREATE TABLE IF NOT EXISTS`.

### 2.6 RADIUS security settings

```toml
[radius]
require_message_authenticator = "yes"  # default yes
limit_proxy_state = "yes"              # default yes
status_server = true                   # default true

[[radius.clients]]
address = "10.0.0.9"
secret_file = "old-switch.secret"
require_message_authenticator = "no"   # overrides the value of [radius]
```

**Short shared secrets:** when the secret of a RADIUS or TACACS+ client is shorter than 16 octets, startup, SIGHUP and `check` all log a warning (`has a shared secret shorter than 16 octets`), but startup is not refused. 16 is the lower limit that RFC 8907 section 10.5.1 and RFC 2865 recommend; the earlier threshold was 12 octets, so secrets of 13–15 octets start to show the warning after the upgrade. We recommend at least 24 random octets. The content of TACACS+ packets is protected only by this secret.

**Secrets shipped with the examples:** when the shared secret of a RADIUS or TACACS+ client, or the cluster's `secret_raft` or `secret_api`, contains `change-me` (case-insensitive), startup, SIGHUP and `check` all log a warning (`has the public shared secret shipped with the examples`; for the cluster it is `cluster.secret_raft is the public value shipped with the examples`) that names the client or the setting, does not contain the secret, and does not refuse startup. These are public values shipped with the examples that everyone knows; replace them with random values (for example `openssl rand -hex 32`) and the warning goes away. A tutorial environment that follows the examples or the manual shows this warning, which is expected.

**Upgrade note: by default an Access-Request without Message-Authenticator is dropped.** An old device that does not send this attribute must be set to `no` or `auto` after the upgrade, or authentication fails and the log shows `lacking a Message-Authenticator`.

| Setting | `yes` | `no` | `auto` |
| --- | --- | --- | --- |
| `require_message_authenticator` | Drop when absent | Allow absent, then look at `limit_proxy_state` | Decided by the first request: it becomes `yes` if the attribute is present and `no` if absent, and one warning line is logged when it becomes `no` |
| `limit_proxy_state` | Drop when Proxy-State is present but Message-Authenticator is absent | Allow, **with BlastRADIUS risk** | Decided by the first request: it becomes `no` (one warning line is logged) if Proxy-State is present but Message-Authenticator is absent, and `yes` otherwise |

- `limit_proxy_state` takes effect only when Message-Authenticator is not required.
- `auto` can be used only for a client that is a single host; for a network, startup fails. The learned value is learned again after a restart.
- If the first packet of `auto` has no Message-Authenticator, the source cannot be verified, and an attacker can forge this packet to downgrade the client. So at the moment it is learned as `no`, the server logs one warning line for that source (`the first request had no Message-Authenticator, so ... (auto) became no`, naming the client and the source address, once per source per process). If you can, set `yes` explicitly, and if not, set `no` explicitly.
- These settings are mitigations. A client set to `no` or `auto` is still at risk, and the real fix is RadSec (RADIUS over TLS, which needs rradius-enterprise); for a device that cannot use RadSec across an untrusted network, use a VPN.

**Response:** Access-Accept and Access-Reject always carry Message-Authenticator, as the first attribute. Access and Accounting responses copy Proxy-State from the request in order. When a response exceeds 4096 octets, an Access-Reject is sent instead and the event is logged (the same for an EAP Access-Challenge, which ends that conversation), and when even the Access-Reject does not fit, no response is sent.

**Status-Server** (RFC 5997): for health checks by a load balancer, without creating a test account. It does not run scripts and does not go through retransmission detection. The authentication port returns Access-Accept and the accounting port returns Accounting-Response. The request must carry a valid Message-Authenticator, regardless of the settings above. When the limit of requests handled at the same time is reached, there is no response.

### 2.7 RadSec (RADIUS over TLS)

RadSec (`[radius.tls]`) needs rradius-enterprise.

### 2.8 Metrics `[metrics]`

```toml
[metrics]
bind = "127.0.0.1"  # default
port = 9812         # default
```

`GET /metrics` returns the Prometheus text format. Without this section the port is not opened. **This port has no authentication and no TLS**, so bind it only to an internal address.

If another program already uses the port, the server does not exit: the log gets one line, `ERROR … metrics: cannot listen on <address>: …`, and RADIUS and TACACS+ keep serving, but there is no `/metrics`, `/livez` or `/readyz`. If the TACACS+ port is in use, it behaves like RADIUS: startup ends with exit code 1 (`TACACS+ server error: …`).

The same port also answers two health checks, for Kubernetes probes or load balancers (for examples see `deploy/kubernetes/` and chapter 16 of the manual):

| Path | Response |
| --- | --- |
| `GET /livez` | Always 200 `ok`: the process is still responding. It does not look at the database or the cluster |
| `GET /readyz` | 503 `stopping` when SIGTERM or SIGINT has been received and the server is stopping (section 2.16); 503 `no leader` when `[cluster] stop_without_leader_ms` is set and there has been no usable leader for longer than that, that is, when the node deliberately does not answer authentication (section 2.2, "When the database is unavailable"); 503 `database <name> unreachable` when an external database with `required_for_ready` cannot be reached, with several names sorted alphabetically and separated by `, ` (section 2.17); otherwise 200 `ok`. When all three apply at once, only `stopping`, `no leader` or the database one is returned, in that order |

- The body is always one line of `text/plain`. The query string does not change the result; methods other than GET get 405, and other paths get 404. The limits on connections, timeouts and header size are shared with `/metrics`.
- Without `stop_without_leader_ms`, `/readyz` stays 200 except while stopping: a node with no leader still answers read-only logins from its local replica, and counting it as not ready would make the load balancer take every node out.
- The metrics port is opened only after the node has caught up with the cluster and the protocol ports are open, so during startup neither path can be reached; in Kubernetes, use a startupProbe to wait for it.

| Item | Description |
| --- | --- |
| `rradius_radius_packets_total{kind="auth\|acct"}` | Requests that passed the checks and the retransmission detection and were actually handled |
| `rradius_radius_responses_total{code="access_accept\|access_reject\|access_challenge\|accounting_response"}` | Responses sent, not counting retransmission-cache replies and Status-Server |
| `rradius_radius_client_responses_total{client="...",code="access_accept\|access_reject\|access_challenge\|accounting_response"}` | Responses sent to each client, counted the same way as the line above. Every client in the configuration starts at 0; a reload adds new ones and removes those no longer in the configuration. Each client has four series, so watch this when you have many clients |
| `rradius_radius_status_server_total` | Status-Server requests answered |
| `rradius_radius_duplicates_total` | Retransmissions answered from the retransmission cache, or dropped because the request is still being handled |
| `rradius_radius_discarded_total{reason="overload\|unknown_client\|invalid\|database\|no_leader"}` | Packets that got no response; `invalid` covers malformed packets, failed verification and packets that do not meet the Message-Authenticator policy; `database` is a login with no decision because a backend could not be used (the cluster database, an external database, LDAP, a domain controller, or a password verification that timed out), and `no_leader` is a request that `stop_without_leader_ms` made the server leave unanswered (section 2.2, "When the database is unavailable") |
| `rradius_radius_response_seconds{kind="auth\|acct"}` | The time from starting to handle a request to sending the response, as a Prometheus histogram (`_bucket{le}`, `_sum` in seconds, `_count`). It is counted from when the packet passes the retransmission check until the response is handed to the sender; in the enterprise edition, requests that arrive over RadSec are included. The bucket upper bounds are fixed at 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 25, 50, 100, 250, 500 ms and 1, 2.5, 5, 10 s, plus `+Inf`. Retransmission-cache replies, Status-Server and packets that get no response are not counted, so `_count` is lower than `rradius_radius_packets_total` |
| `rradius_cluster_leader`, `rradius_cluster_is_leader`, `rradius_cluster_applied_index`, `rradius_cluster_seconds_without_leader` | The cluster state as seen by this node; see section 2.2, "Cluster state metrics" |
| `rradius_tacacs_connections_total{result="accepted\|refused"}` | TACACS+ connections |
| `rradius_tacacs_connections_open` | Current number of TACACS+ connections |
| `rradius_tacacs_requests_total{type="authen\|author\|acct"}` | TACACS+ requests |
| `rradius_tacacs_malformed_total` | START, authorization and accounting requests that were answered with ERROR because the decrypted fields do not follow RFC 8907. A count that keeps rising usually means a wrong shared secret |
| `rradius_tacacs_response_seconds{type="authen\|author\|acct"}` | The time from reading a whole TACACS+ packet to writing the response, as a histogram with the same buckets as the RADIUS metric above. Only responses that were written successfully are counted; connections that drop before the response, or that are closed without a response, are not |
| `rradius_log_dropped_lines_total` | Log lines lost because the output could not keep up or a write failed (section 2.10) |
| `rradius_db_batched_writes_total` | `sql_execute` calls committed in the same transaction as other writes (section 2.4) |
| `rradius_db_split_batches_total` | Write transactions that were split because one of the statements failed (section 2.4) |
| `rradius_script_http_requests_total{result="ok\|timeout\|error\|denied\|too_large\|busy"}` | `http_request` calls from scripts (section 4.11) |
| `rradius_eap_sessions_open` | EAP conversations in progress (section 2.12) |
| `rradius_eap_sessions_total{method="tls\|peap\|ttls",result="accept\|reject\|timeout\|overload\|replaced\|evicted"}` | EAP conversations that have ended. A conversation already counted as `reject` is not counted again when it later times out or is evicted; `overload` is a conversation left unanswered because `max_sessions_per_device` was full, `replaced` is an old conversation replaced because the same device started again, and `evicted` is a conversation evicted because the table was full, starting with those still waiting at EAP Identity |
| `rradius_acct_spool_written_total` | Accounting requests written to the spool (section 2.11) |
| `rradius_acct_spool_replayed_total` | Requests that the script accepted during replay |
| `rradius_acct_spool_rejected_total` | Requests moved to `rejected.log` because running them again during replay gave the same result |
| `rradius_acct_spool_full_total` | Requests left without a reply because the spool was full or a write failed |
| `rradius_acct_spool_corrupt_total` | Spool files that were corrupted and renamed to keep them |
| `rradius_acct_spool_bytes` | Bytes waiting to be replayed; a value that stays above 0 means the data in the database is behind |
| `rradius_accounting_log_written_total` | Records written to the accounting log file (section 2.18) |
| `rradius_accounting_log_dropped_total` | Records not written to the accounting log file because the queue was full |
| `rradius_accounting_log_failures_total{stage="open\|write"}` | Number of failures reopening (`open`) or writing (`write`) the accounting log file; each `write` is one batch |
| `rradius_netlogon_logons_total`, `rradius_netlogon_channel_setups_total`, `rradius_netlogon_open_connections` | The `netlogon` handler (section 3.7) |
| `rradius_external_db_failures_total{database="...",reason="unavailable\|mistake\|unknown"}` | Failed calls for each external database (section 2.17), counting the `sql` handler and also `db_query` and `db_execute`. `unavailable` means the database could not be used (including timeouts), `mistake` means an error in the statement or the values (including those rejected before sending, for example a wrong number of values), and `unknown` is anything else. A `db_query` that names a database that does not exist is not counted; a `db_query` or `db_execute` called after the request's time (`[lua].query_timeout_ms`) has already run out never reached the database and is not counted either; the `required_for_ready` check is not counted. It is not output when there is no `[databases]` |
| `rradius_query_<name>{<label>="..."}` | With `[queries.<name>]`, one gauge per query: the result of the last successful run, one sample per row, where the value is the `value` column and the labels are the columns of `labels` (section 2.20) |
| `rradius_query_failures_total{query="...",reason="timeout\|error\|shape"}` | Number of failures for each query: `timeout` is a timeout, `error` is the database refusing or being unreachable, and `shape` is a result whose columns or types are wrong. On a failure the gauge keeps its previous value. Not output when there is no `[queries]` |
| `rradius_query_dropped_rows_total{query="...",reason="null_value\|over_limit"}` | Rows of a successful run that did not make it into the gauge: the value column is NULL, or the row is beyond `max_rows`. Not output when there is no `[queries]` |
| `rradius_query_last_success_timestamp_seconds{query="..."}` | The time of each query's last success (Unix seconds); 0 if it has never succeeded. Not output when there is no `[queries]` |
| `rradius_external_db_connections{database="...",state="in_use\|idle"}` | Connections in each external database's pool: `in_use` are those handed to a call that is using them, plus those being set up (a connection that is not yet established also takes a slot), and `idle` are open and unused. The `required_for_ready` check connection is not included |
| `rradius_external_db_connections_max{database="..."}` | The pool's limit, which is `pool_size`. When `in_use` is always equal to it, calls are queuing for a connection |
| `rradius_external_db_reachable{database="..."}` | Only for databases with `required_for_ready`: 1 if the check can connect, otherwise 0 (section 2.17) |
| `rradius_reload_total{result="ok\|error"}` | Result of reloads |
| `rradius_password_checks_total{scheme="...",result="ok\|failed"}` | Number of comparisons between a login and a stored password, split by the format of the stored value and the result; for the fixed set of formats and how to use it to retire old formats, see section 5 |
| `rradius_reload_last_success_timestamp_seconds` | The time of the last successful reload, or 0 if there has been none |
| `rradius_radius_requests_in_progress{kind="auth\|acct"}` | Number of login and accounting requests being handled now |
| `rradius_radius_requests_limit{kind="auth\|acct"}` | The limit on concurrent handling, which is `max_auth_inflight` and `max_acct_inflight` (section 2.4). When the number being handled gets close to it, further packets are dropped with `overload` |
| `rradius_build_info{version="..."}` | Always 1; `version` is the version of this program, the same as the first line of `--version` |
| `rradius_start_time_seconds` | The time the process started (Unix seconds) |

Values are counted per node from startup and are not reset by a reload; Prometheus adds them up for the cluster. The exception is `rradius_radius_client_responses_total`: the series of a client removed by a reload disappears, and starts again from 0 if the client is added back later.

#### What this node is doing right now: the `status` command

Without Prometheus, `status` reads this node's own `/readyz` and `/metrics` directly, takes two samples 1 second apart, and lists the per-second rates and the current values:

```bash
rradiusd status [--config <path>] [--cluster] [--watch | --json | --tui]
```

- **`[metrics]` is required.** The command connects using `bind` and `port` from the configuration file; if `bind` is `0.0.0.0` or `::`, it connects to `127.0.0.1` or `::1` instead. So run it on the node itself, with the same configuration file.
- **With no options** it takes two samples, prints once and exits, in about 1 second. **`--watch`** prints once a second until Ctrl-C; on a terminal each output overwrites the previous one, and when redirected to a file or pipe the outputs follow one another.
- **Units of byte items.** Byte items such as `In bytes` and `Out bytes` are shown in text, `--watch` and `--tui` as B, KiB, MiB, GiB and TiB (powers of 1024, one decimal place), for example `58.6 KiB/s (1.0 MiB)`; `--json` and `/metrics` do not convert: `rate` is bytes per second and `total` is bytes.
- **`--tui`** opens a full-screen display with the sections above, one section at a time, updated every second. **The left and right arrow keys or Tab** switch sections, **the up and down arrow keys** select an item in a section (an item that has a rate gets a line chart of the last 60 seconds below), **`p`** pauses and resumes, and **`q`, Esc or Ctrl-C** quits and restores the terminal. Used together with `--cluster`, a member table is added at the top, and **`[` and `]`** select "all" (the sum of all members, selected at the start) or one member, and the sections below show the selected one; a member that cannot be reached is marked unreachable in the table and the display does not end. It cannot be combined with `--json` or `--watch`, and when standard output is not a terminal it exits with status 2 and writes nothing. Sections added by the enterprise edition (RadSec, Proxy and so on) automatically become a section of the display. The RADIUS and TACACS+ sections also have `In bytes` and `Out bytes`, the byte rates of protocol packets (not including IP, UDP, TCP or TLS headers, and not including RadSec and proxy traffic).
- **When a counter gets smaller** (the process restarted between the two samples), that item's per-second rate is shown as 0, and the start of the output notes that the counters were reset.
- **Metrics that do not exist are not listed,** and a section with no value at all is left out, for example the last-success time of Reload before any reload has happened. The metrics of most features are output whether or not the feature is configured (with the value 0), so a section of zeros can appear for a feature you do not use; do not use the presence of a section to decide whether a feature is on.

These sections are listed in order:

| Section (`key`) | Contents |
| --- | --- |
| Node (`node`) | The `/readyz` answer (`ok`, `stopping`, `no leader`, `database ... unreachable`), the cluster state (whether it is the leader, whether there is a leader, whether it has caught up, the applied index, the seconds without a leader), the version, the seconds running |
| Reload (`reload`) | The number of successful and failed reloads, the time of the last success |
| RADIUS (`radius`) | Per-second rates of login and accounting requests, the four kinds of response, retransmissions and the five discard reasons, the number of requests being handled and the limit, and the p50 and p99 (in milliseconds) of the login and accounting response times over the interval |
| Busiest RADIUS clients (`radius_top_clients`) | The 5 clients with the most responses over the interval and their per-second response rates |
| External databases (`databases`) | Only with `[databases]`. For each database (by name), connections in use, idle and the limit (`<name>.in_use`, `.idle`, `.max`), the per-second rates of the three kinds of failure (`<name>.failures_unavailable`, `.failures_mistake`, `.failures_unknown`), and, when `required_for_ready` is set, whether it can be reached (`<name>.reachable`, `yes` or `no`) |
| Queries (`queries`) | Only with `[queries]`. For each query (by name), how many seconds ago the last success was (`<name>.last_success_age_seconds`, `never` if it has never succeeded), how many samples there are now (`<name>.rows`), and the per-second rate and total of failures (`<name>.failures`, the three reasons added together) |
| TACACS+ (`tacacs`) | Open connections, the per-second rates of the three kinds of request and of malformed packets, and the p50 and p99 (in milliseconds) of the response times of the three kinds of request over the interval |
| EAP (`eap`) | EAP conversations in progress |
| Accounting spool (`acct_spool`) | Bytes waiting to be replayed, and the per-second rates of replayed, given up and not written to the spool |
| Accounting log (`acct_log`) | Per-second rates of accounting log writes, drops, open failures and write failures (`written`, `dropped`, `failures_open`, `failures_write`); not listed when there is no `[radius.accounting_log]` |
| Netlogon (`netlogon`) | Open connections, the per-second rates of the four logon results |
| Log (`log`) | The per-second rate of lost log lines |

The enterprise edition has sections of its own, at the end (see the enterprise edition documentation). The number in parentheses after a per-second rate is the total since startup.

**The p50 and p99 of response times** (`auth_p50_ms`, `auth_p99_ms`, `acct_p50_ms`, `acct_p99_ms`; for TACACS+ they start with `authen_`, `author_` and `acct_`) are estimated from the number each bucket of `rradius_radius_response_seconds` and `rradius_tacacs_response_seconds` grew by between the two samples: the bucket that the value falls into is found, the value is interpolated linearly between its lower and upper bounds, and rounded to 0.001 ms (1 µs). So they are estimates, and the accuracy is limited by the bucket width (for example, for a value in the 2.5 to 5 ms bucket you only know it is in that range); anything above 10 s is shown as 10000. If there was no request of that kind in the interval, or the counters were reset, that item is not listed.

What the text output looks like (an excerpt; the numbers are only illustrative):

```text
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Version                   0.1.0
  Seconds running           3605

RADIUS
  Auth requests             1520.0/s (5480213)
  Access-Accept             1498.0/s (5401120)
  Access-Reject             22.0/s (79093)
  Discarded: overload       0.0/s (0)
  Auth in progress          37
  Auth limit                4096
```

**`--json`** prints one line of JSON each time, for scripts or monitoring programs:

```json
{"address": "127.0.0.1:9812", "interval_seconds": 1.0, "ready": {"status": 200, "text": "ok"}, "counter_reset": false,
 "sections": [{"key": "radius", "title": "RADIUS", "items": [
   {"key": "auth_requests", "label": "Auth requests", "rate": 1520.0, "total": 5480213},
   {"key": "auth_limit", "label": "Auth limit", "value": 4096}]}]}
```

(The real output is on one line.) Each item has either `value`, or `rate` and `total`. The order of sections and items is the same as in the text output; a `key` stays the same in later versions (except when an item is removed), so programs should use `key` and not `label` or `title`.

| Exit code | Meaning |
| --- | --- |
| 0 | The result was printed. A 503 from `/readyz` (stopping, no leader, a required database unreachable) is also 0, and the state is written in the output |
| 2 | A wrong option, the configuration file cannot be read, or the configuration file has no `[metrics]` |
| 3 | The metrics port cannot be reached, there was no answer within 2 seconds, or the answer does not look like the rradius format. With `--watch` it does not exit because of this; it retries every second and explains on stderr; the first per-second rate after recovery is the average from the last successful sample, so the seconds shown at the start are that period |

Without `--cluster`, `status` looks at one node only.

**`--cluster`** looks at the whole cluster at once, and can be used together with `--watch`, `--json` and `--config`:

- **Where the members come from:** it asks each node's cluster management API in the order of `[[cluster.nodes]]` (using `secret_api`), waiting at most 2 seconds for each, and uses the member list and the leader as seen by the first one that answers; a leader is not required. If none answers, it uses the nodes listed in the configuration file, and the start of the output says so. If `secret_api` is rejected, the exit code is 2.
- **Each node's metrics address:** the host of the member's api address, combined with the port of `[metrics]` in this node's configuration file (`10.0.0.2:8200` → `10.0.0.2:9812`; IPv6 keeps its square brackets; names are resolved as usual). The node the command runs on uses its own address, the same as without `--cluster`.
- **Prerequisite:** every node's `[metrics] bind` has to be an internal address that the other nodes can reach (for example a WireGuard address), with the same port on every node; the default `127.0.0.1` cannot be reached by the others and is listed as unreachable. If the ports differ, you can only run it on each node separately. `/metrics` has no authentication, so when you bind an internal address, limit the sources with a firewall or WireGuard. With hostNetwork in Kubernetes, `bind = "0.0.0.0"` is enough.
- **Sampling:** all members are sampled twice at the same time, 1 second apart, and each node's numbers are calculated the same way as for a single-node `status`. A member that cannot be reached is listed as unreachable, the reason is listed under the table, and it is not counted in the total.
- **`--watch`:** as with a single node, each node's previous sample is the starting point of the next one, and it prints once a second; the member list is queried again every 10 seconds. A member that has just become reachable shows unreachable that time, with the note "per-second rates are available from the next sample".

The text output has one row per node, and the total at the end (the numbers are only illustrative):

```text
rradius cluster status over 1.0 s, members from the cluster

node   role      ready        auth/s  acct/s  discarded/s  in progress  address
1      leader    ok           1520.0  30.0    0.0          12           10.99.0.1:9812
2      follower  ok           1488.0  28.0    0.0          9            10.99.0.2:9812
3      follower  unreachable  -       -       -            -            10.99.0.3:9812
total            2/3          3008.0  58.0    0.0          21

node 3: cannot reach the metrics port at 10.99.0.3:9812: Connection refused (os error 61)
(every node's [metrics] bind has to be an address this host can reach, on the same port as here)
```

`role` is `leader`, `follower` or `learner`: for a member that can be reached it comes from its own metrics, and for one that cannot, from the member list. `discarded/s` is the total of the five discard reasons, and `in progress` is the login and accounting requests being handled.

`--cluster --json` prints one line of JSON each time:

```json
{"cluster": true, "members_from": "cluster", "interval_seconds": 1.0,
 "nodes": [{"id": 1, "role": "leader", "address": "10.99.0.1:9812", "reachable": true, "status": {...}},
           {"id": 3, "role": "follower", "address": "10.99.0.3:9812", "reachable": false, "error": "cannot reach ..."}],
 "total": {"auth_per_second": 3008.0, "acct_per_second": 58.0, "discarded_per_second": 0.0, "in_progress": 21, "reachable": 2, "nodes": 3}}
```

`members_from` is `cluster` or `configuration`. Each node's `status` is the object that `status --json` prints on that node (including the enterprise edition's sections); a member that cannot be reached has `error` instead of `status`. These `key` values stay the same in later versions.

Exit codes for `--cluster`: 0 means at least one node was sampled successfully (also 0 if the others cannot be reached); 2 is a wrong option, a configuration file that cannot be read, no `[metrics]`, or `secret_api` rejected; 3 means no node could be reached (with `--watch` it does not exit, and looks again every second).

### 2.9 Reload (SIGHUP)

```bash
kill -HUP <pid>
docker compose kill -s HUP rradius
```

On SIGHUP the server rereads the configuration file (including `secret_file`) and applies it, and writes one line with the result to the log: `configuration reloaded from ...` or `reload failed, keeping the running configuration: ...`.

- **Applied:** the top-level script, auth chains and their scripts, the time limits in `[lua]`, the client lists and settings for RADIUS and TACACS+ (including `[tacacs.authorize_groups]` and `[tacacs].enable`), `[radius.eap]` (affecting new conversations only), `[lua.http]`, and `file` in `[radius.accounting_log]` (the file is reopened; see section 2.18).
- **Applied only after a restart:** `dictionary` (the contents of dictionary files are not reread either; when the contents differ from what they were at startup, the log gets an extra line, `dictionary files changed since the server started; restart to apply: ...`, on every reload), `[lua].max_memory_mb`, `[tacacs].idle_timeout_ms`, `[cluster]` (including migration), `[log]` (except `[log.trace]`), all listen addresses and ports, `max_auth_inflight`, `max_acct_inflight`, `max_connections`, `max_connections_per_source`, `max_body_len`, `status_server`, `idle_timeout_ms`, `[metrics]`, `[radius.accounting_spool]`, `[databases]`, `[queries]`, `queue` in `[radius.accounting_log]`, and adding or removing `[radius]`, `[tacacs]` and `[radius.accounting_log]`. When any of these settings has changed, the log lists their names, and everything else is applied as usual.
- **Applied only if everything succeeds.** The checks are the same as at startup; if any one fails, everything stays as it was and the process keeps serving. Fix the problem and send SIGHUP again.
- Requests in progress finish with the old contents and are not interrupted. TACACS+ connections that are already open are not dropped by this, but the next packet is handled by the new list: if a client is removed the connection is closed, and after a secret change the new secret has to be used.
- Values learned by `auto` and the retransmission cache are kept. Every reload rebuilds the LDAP connection pools.
- Each node reloads on its own; to update a whole cluster, update the file on each node and send the signal on each.

### 2.10 Logging `[log]`

```toml
[log]
level = "info"   # error, warn, info (default), debug
format = "text"  # text (default), json
auth = true      # default true, write authentication records
```

**Upgrade note: all messages are written to stderr, and every line has a time and a level.** Startup messages that used to go to stdout also moved to stderr, so external scripts that read stdout or match whole lines have to be changed accordingly.

| Format | Example |
| --- | --- |
| `text` | `2026-11-05T01:02:03.000000Z  INFO rradius_server::radius: RADIUS Server listening on ...`, with no color codes |
| `json` | One object per line, with `timestamp`, `level`, `target` and `message`, and other fields at the same level |

- **`level`** affects only rradius's own messages. Other libraries are fixed at `warn` and above, and a normal cluster start and shutdown also produces a few such messages, for example `No metadata exists inside the DB yet`; before a node has joined there may be `Raft node has not been initialized`, which stops after it joins. A three-node start shut down after 25 seconds measured 27 lines in total, 13 of them `ERROR` from `hiqlite`.
- **Errors before the configuration is loaded and fatal startup errors** are written directly to stderr, without the time and level prefix, so that they are always written before the process ends.
- If a value from outside contains a newline or control characters, the `text` format escapes them and the `json` format uses JSON escaping, so one record always takes exactly one line. The same applies to `log()` in scripts.

**Tracing a single user or client:** no need to turn on global `debug`, and no restart.

```toml
[log.trace]
users = ["alice"]    # matches the request's User-Name, exact match only
clients = ["nas1"]   # matches the client name, which must be a name in [[radius.clients]]
minutes = 30         # required, 1 to 1440
```

```text
2026-11-05T01:02:03.000000Z  INFO rradius_trace: request client="nas1" source=10.0.0.1 user="alice" code=1 attributes="User-Name = alice, User-Password = <hidden>"
2026-11-05T01:02:03.000000Z  INFO rradius_trace: response client="nas1" source=10.0.0.1 user="alice" code=2 attributes="Filter-Id = staff"
```

- **`[radius]` is required.** A mistyped name in `clients` makes both startup and reload fail.
- **It stops automatically when time is up,** counted from when the configuration was loaded or reloaded. If you leave it in the configuration file, every SIGHUP starts the timer again, so delete it when you are done. Each node keeps its own timer, and the request you want to look at may be on another node, so set it on every node involved.
- **SIGHUP applies it,** without the restriction that the other `[log]` settings need a restart, and it is not affected by `level`.
- **It writes personal data,** so how long it is kept and who can read it are the deployer's responsibility. Attributes that the dictionary marks `encrypt` or `secret`, attributes whose name contains `pass`, `secret`, `key`, `token`, `chap`, `arap`, `-pw`, `-cpw`, `digest`, `signature`, `psk`, `pmk` or `credential` (case-insensitive), and the standard attributes 2, 3, 24, 60, 69, 70, 79 and 80 (`User-Password`, `CHAP-Password`, `State`, `CHAP-Challenge`, `Tunnel-Password`, `ARAP-Password`, `EAP-Message`, `Message-Authenticator`, whatever the dictionary calls them) are shown as `<hidden>`. **It looks only at the attribute, not at the contents of the value:** a custom attribute that carries a secret under another name, or a secret inside a value (for example `Cisco-AVPair = "vpdn:l2tp-tunnel-password=..."`), is not caught, so if a script can send such values, do not turn tracing on for that user. When a user types a password into the account field, it appears in `user`.
- **Scope:** RADIUS only, not TACACS+. Packets answered from the retransmission cache, packets that fail verification and Status-Server are not written. The match is on the outer name, so for the anonymous names of PEAP and TTLS use `clients` instead, or list the names actually used on the outside. Text values are shown up to 128 characters, and other values up to 64 octets in hexadecimal. Attributes that are not in the dictionary do not appear. Both lines are written after handling is finished, so their times are the same and do not show how long the handling took; when a script hangs, even the request line does not appear until it ends.
- **Be careful using `clients` under heavy traffic:** each request produces two lines, and when the output cannot keep up they are dropped, like other log lines.

**Authentication records:** one record is written for each authentication decision, regardless of `level`, and none is written when `auth = false`.

```text
2026-11-05T01:02:03.000000Z  INFO rradius_auth: authentication protocol="radius" client="nas1" source=10.0.0.1 user="Alice@Corp" account="alice" chain="vpn" handler="sql" decision="accept"
```

| Field | Contents |
| --- | --- |
| `protocol` | `radius` or `tacacs` |
| `client` | The name of the client that matched |
| `source` | The source IP address |
| `user` | The name received, or an empty string if there was none |
| `account` | The name used to look up the account (section 3.6). An empty string if there is no name, or the name was rejected by the chain |
| `chain` | The name of a named chain, `default`, `script` when there is no chain, or `eap` for an EAP conversation |
| `handler` | The handler that made the decision: `sql`, `lua`, `ldap`, `script`, and for EAP conversations `eap-tls`, `eap-peap`, `eap-ttls`; `none` when all were skipped; `no_password_check` when `all_accept` rejects because no one compared a password; `normalization` when the name was rejected. When `all_accept` accepts, it is the last handler that accepted |
| `decision` | The result the device actually received: `accept`, `reject`, `challenge`. When the response could not be sent and was turned into a reject, this says `reject` |

- **Only requests that reached a decision get a record.** Retransmission-cache replies, overload drops, failed verification and packets from unknown clients do not; see the metrics in section 2.8. Accounting and TACACS+ authorization do not get one either.
- **Passwords, CHAP or MS-CHAP responses and the TACACS+ `data` are not recorded.** But when a user types a password into the account field, it appears in `user`, and the server cannot tell.
- **The records contain personal data such as accounts.** How long they are kept and who can read them are the deployer's responsibility; set `auth = false` if you do not need them.

**Not a reliable audit source:** the log is written by a background thread that buffers at most 128000 lines. When the output cannot keep up, lines are dropped, and lines are also lost when a write fails (for example the receiving process ended or the disk is full); both are counted in `rradius_log_dropped_lines_total` and do not slow down authentication, so watch that metric. SIGTERM and SIGINT wait up to 5 seconds before exiting for the buffer to be written; beyond that the rest is lost and not counted; on SIGKILL or a crash, the contents of the buffer are lost in the same way.

**Effect on throughput:** on the same Apple M4 Max machine (14 cores), with a release build and `rradius-bench` (`accept.toml`, PAP, concurrency 64, 10 seconds), stderr redirected to a file, and the authentication record switched on and off, two runs each:

| `auth` | First run (req/s) | Second run (req/s) |
| --- | --- | --- |
| `true` | 74732 | 74004 |
| `false` | 74023 | 74569 |

The difference between the two is smaller than the difference between two runs of the same setting, so no effect could be measured this time. A record is about 190 octets, which at this rate is about 14 MB written per second. The tool and the server are on the same machine; if stderr goes somewhere slower than a local file, lines may start to be dropped.

**At higher rates:** measured on 2026-11-08 on the same machine in Docker's Linux (arm64), with the server limited to CPUs 0–6 with core pinning on (section 2.4), `rradius-bench` limited to 7–13, PAP with `accept.toml`, two runs each:

| Authentication record | stderr goes to | Concurrency 64 (req/s) | Concurrency 1024 (req/s) | CPU used by the thread that writes the records | Dropped records |
| --- | --- | --- | --- | --- | --- |
| `auth = true` | file | 316,200–316,300 | 305,500–306,500 | 72% | 0 |
| `auth = true` | pipe (as if handed to journald) | 285,400–316,900 | 289,500–303,300 | 86–87% | 0 |
| `auth = false` | file | 344,000–344,500 | 313,400–325,800 | 0 | — |

- **The records are written by one thread.** At this rate, turning the records on is about 9% slower, which is about the share this thread uses out of these 7 CPUs.
- **With a pipe it already uses about 87% of a CPU,** and on this machine a little more speed (roughly 340,000 requests per second and above) would saturate it. When it is saturated, new records are simply dropped and the server keeps working; how many were dropped can be seen only in `rradius_log_dropped_lines_total` (section 2.8). When your login rate gets close to this range, watch that metric; set `auth = false` if you do not need every record.
- Few ordinary deployments see hundreds of thousands of logins per second on a single node.

### 2.11 Accounting spool `[radius.accounting_spool]`

When the cluster loses its majority of nodes, devices retransmit a few times and then give up, and that accounting is lost. With this section configured, an accounting request whose database write fails is first written to a local file, and only after it is on disk is the reply sent; a background task later hands it back to the script to replay. Without this section the behavior is completely unchanged.

```toml
[radius.accounting_spool]
dir = "spool"              # required, relative to the configuration file
max_bytes = 1073741824     # default 1 GiB
replay_interval_ms = 5000  # default, how long to wait before trying again when replay meets a database failure
replay_window = 1          # default, how many to replay at once (1–1024); see "Replaying at the same time" below
```

- **Only failures caused by the database are spooled.** A request is spooled only when its `sql_query` or `sql_execute` failed because of a timeout or a cluster error, whatever the script finally returns: if the script catches it with `pcall` and then returns `'accept'`, it is spooled too, because that write may never have happened, and it is judged again at replay. The script's own bugs, SQL syntax errors and returning `'reject'` are not spooled; as before, there is no reply, so errors are not hidden.
- **At least once.** If the process is interrupted between the script finishing and the progress being updated, the same record runs again, so scripts must be safe to run repeatedly (see section 4.4). Another source is a write that is not committed in a batch (for example one with `RETURNING`): if it completes after the timeout, the request is already in the spool and runs again at replay; a batched write is waited for after it is handed over, so it does not do this by itself; but if it waits longer than the time limit, the database calls after it in the same request time out immediately, and that accounting can still end up in the spool and the whole script runs again at replay (section 2.2, "When the database is unavailable"). So an accounting script with several database calls must also be safe to run repeatedly. After a power failure more may run again: progress is written to disk only when 64 or more records have been completed in total and when switching segments, so up to 64 more records can run again; with `replay_window` greater than 1, progress is updated after a batch is settled, so it is at most 63 records plus one batch. If the `position` file is corrupted (for example it became empty after a power failure), one ERROR line is logged and replay starts again from the beginning of the oldest segment, up to 16 MiB. All accounting scripts in `rradius-server/examples/` are safe to run repeatedly, and have tests; `disconnect-disabled.lua` does not send a disconnect on replay, because that is already an old report.
- **Replay.** By default it goes in the order written, one record at a time (see the next item when `replay_window` is greater than 1), using the script and client settings at that time. The global variable `replayed` is `true`, and `Acct-Delay-Time` has the time spent in the spool added to it. Scripts that compute times should use it, otherwise an old report looks as if it just happened. When the database fails again, the same record is retried after `replay_interval_ms`, and the requests behind it keep waiting.
- **Replaying at the same time (`replay_window`).** Default 1. When set larger, up to that many records are read at a time (not crossing spool files) and handed to scripts together, and settled in the order written up to the first one that has to be retried; after that, that one is retried alone first, and the batch resumes only when it has a result; records behind it that the script already accepted run again (this is "at least once" anyway). Scripts running at the same time are limited by the accounting script slots as usual, and share them with live accounting.
  - **Why it is off by default:** within one batch, the Start, Interim-Update and Stop of the same session may run at the same time or in reversed order. When the script only records each one with `INSERT OR IGNORE`, or handles them separately by `Acct-Status-Type` without depending on order, it is safe to turn on; when the script relies on order to maintain session state (for example updating on Stop the row that Start added), keep 1.
  - **Starting from 8 is recommended.** Measured on 2026-11-09 in OrbStack's Linux (arm64): the server had 7 CPUs, the accounting script wrote one row with `INSERT OR IGNORE`, `query_timeout_ms = 1` sent `acct-start` for 5 seconds to accumulate about 43,000–46,000 records, and then the time to replay them with the normal timeout restored, until `rradius_acct_spool_bytes` was 0, two runs each:

    | `replay_window` | Records replayed per second |
    | --- | --- |
    | 1 | 4,600–4,700 |
    | 8 | 16,300–17,200 |
    | 32 | 14,900–25,700 |

    With one at a time each record takes about 0.2 ms, mostly spent waiting for the database write to complete (the cluster commit), not on the spool itself. With a large window everything was replayed in under 3 seconds, timed at 0.1-second intervals, and the two runs at 32 differ a lot, so all you can tell is that it is faster than 8; if you really accumulate more records, measure in your real environment.
- **When a record is given up.** Only when running it again gives the same result: the client no longer exists, the script ended normally but did not return `'accept'`, or the script ended because of a SQL syntax error, a wrong parameter or a constraint violation. These are moved to `rejected.log` and logged. If the script catches a SQL error and then fails for another reason, that counts as another kind of failure; rethrowing a SQL error that this request caught earlier counts as giving up. A case where the parent row of a foreign key arrives only later is also given up, so the script has to handle the order itself, or use `INSERT ... ON CONFLICT` so that a duplicate request does not fail. The file is not cleaned up automatically and is not replayed; look at it yourself.
- **All other failures are retried, and the queue gets stuck.** An unavailable database, a Lua error, going over `max_execution_ms`, an error thrown because an HTTP API the script called failed, and a script that catches an unavailable database or an unknown failure but returns `'accept'` are all retried for the same record after `replay_interval_ms`, because the device does not send again a request that has been answered. When a script keeps failing, `rradius_acct_spool_bytes` stays above 0, and it recovers automatically once the script is fixed. A script that catches an error like a SQL syntax error and returns `'accept'` counts as completed, because running it again gives the same result.
- **The end of a file at startup.** If the last record is incomplete, it is cut off, because it was never replied to. A damaged length field whose value still looks like a record looks exactly the same, and that could be a request that was replied to, so the cut-off content is first appended to `truncated.log` in the spool directory and one WARN line is logged. This file is not cleaned up automatically and is not replayed; it usually holds only one half-written record, so look at it yourself. If `truncated.log` cannot be written (for example the disk is full), nothing is cut off and startup goes on as usual, new requests are written to a new file, and the old file is later renamed whole to `.corrupt` with one ERROR line logged. An end that is all zeros (common after a power failure) is simply removed and nothing is kept; if there is content after the zeros it counts as corruption, and the whole file is renamed to `.corrupt`.
- **When full, or when the disk fails.** There is no reply, `accounting spool ...` is logged, `rradius_acct_spool_full_total` is counted, and the device retransmits, the same as without this section. Content that failed partway through writing is cut off and does not affect later requests. For the size of each record see the measurements below, from which you can work out how long it will last.
- **Queries are delayed.** The data gets into the database only after the reply, so policies that depend on it (quotas, concurrent sessions) do not see this accounting until recovery. **The order can also be reversed:** after the cluster recovers, new accounting is written directly while older records in the spool are still waiting to be replayed, so the Stop of a session can reach the database before its Start; scripts must not assume an order, for example by using `Acct-Status-Type` and times and not treating the last record as the one that counts. `rradius_acct_spool_bytes` above 0 means there is a lag, and you can alert on it.
- **A node failure loses data.** Each node replays only its own files, and no other node takes over. If the disk is permanently damaged, accounting that was confirmed but not yet replayed is lost, so put it on persistent storage. When several nodes recover at the same time they may write at the same time, and the cluster handles that.
- **Personal data.** The files are raw packets, containing personal data such as `User-Name` and addresses, and how long they are kept depends on how long the database outage lasts. The directory and file permissions are 0700 and 0600, which should be taken into account for backups and when disks are discarded.
- **Files.** `segment-<n>.log` is the data, `position` is the replay progress, and a `.corrupt` ending marks a file that was corrupted and renamed to keep it. An incomplete record at the end is caused by an interrupted write; it was not yet replied to, and is cut off and logged. Any other corruption renames the whole file to keep it, and later requests are written to a new file.
- **The reply time is decided by `query_timeout_ms`.** Right after the cluster loses its majority, writes fail only when they time out (all calls of one request together take at most `query_timeout_ms`), and during that time the device gets no reply; after there has been no leader for more than 5 seconds, writes fail immediately. The default 5 seconds is longer than the retransmission interval of many devices, and retransmitted packets are treated as being handled and dropped, so consider lowering it. The limit on concurrent handling is `max_acct_inflight`, so the throughput limit is about "the limit ÷ the timeout".
- **Other.** Not applicable to TACACS+. SIGTERM stops the replay, the data stays on disk, and it continues at the next start. Changing this section needs a restart (section 2.9).

**Measurements:** Apple M4 Max (14 cores), local SSD, release build and `rradius-bench`, `acct-start`, 10 seconds. Each record is about 106 octets (a packet from this tool, including the header and client name of each record; the real size grows with the attributes a device sends), so 1 GiB is about 10 million records.

| Case | Concurrency | Throughput (req/s) | p50 |
| --- | --- | --- | --- |
| Without this section, first and second run | 64 | 74404, 75578 | 0.8 ms |
| With this section, database normal, first and second run | 64 | 74593, 74430 | 0.8 ms |
| Simulated outage (every query times out at 1 ms), first and second run | 64 | 7979, 7974 | 8 ms |
| Simulated outage | 256 | 16894 | 15 ms |
| Three nodes, two lost, `query_timeout_ms = 500` | 64 | 128 | 509 ms |
| Three nodes, two lost, `query_timeout_ms = 100` | 64 | 588 | 108 ms |
| Three nodes, two lost, `query_timeout_ms = 100` | 256 | 2260 | 113 ms |

When the database is normal, the difference between having this section and not is smaller than the difference between two runs of the same setting, so no effect could be measured this time. The simulated-outage numbers are the limit of writing to the spool itself; several at once are combined into one fsync, so the higher the concurrency, the faster it goes. In a real outage the limit comes from waiting for the timeout, and the throughput is about "concurrency ÷ timeout", independent of the disk. fsync numbers also differ between disks, so measure again in your real environment.

### 2.12 EAP `[radius.eap]`

WPA2/WPA3-Enterprise and wired 802.1X need EAP. rradius supports EAP-TLS (client certificate), PEAP-MSCHAPv2 (account and password) and EAP-TTLS (account and password, inner PAP or MS-CHAPv2). Without this section, `EAP-Message` is passed to the script like any other attribute. **This feature has not been tested on real wireless access points, switches, or Windows, iOS and Android,** only with wpa_supplicant's `eapol_test` (see section 7).

```toml
[radius.eap]
cert_file = "eap-server.pem"   # required, the server certificate the client sees
key_file = "eap-server.key"    # required
client_ca_file = "eap-ca.pem"  # required when methods has tls (setting it without tls fails startup), the CA that issues client certificates
crl_file = "eap-ca.crl"        # optional, tls only (setting it without tls fails startup), PEM, lists revoked certificates
methods = ["tls"]              # default; can list "tls", "peap", "ttls"
fragment_size = 1000           # default, 256 to 1400
session_timeout_ms = 30000     # default
max_sessions = 4096            # default
max_sessions_per_client = 1024 # default
max_sessions_per_device = 4    # default
match_identity = true          # default
```

EAP-AKA' (`aka-prime` in `methods`), session resumption (`resumption_ms`, `shared_tickets`) and OCSP (`ocsp`) need rradius-enterprise.

- **A valid certificate does not mean the login is allowed.** After the handshake succeeds, the name is passed to this client's auth chain, or to the top-level script for a client without a chain. A rejection is an Access-Reject, so a disabled account does not have to wait for the CRL, and reply attributes such as the VLAN can depend on the user.
- **How the chain decides.** The password is not compared after a certificate is accepted, so the handler has to decide separately whether the account can be used. When `all_accept` contains `sql` or `ldap`, one of them has to accept for the login to pass (section 3.2); a script after them accepting alone is rejected; `ldap` without `certificate_filter` does not count as accepting. To let only a script judge the certificate, do not put `sql` or `ldap` in the same chain, or use `first_match`:
  - **`sql`:** By default it uses `user_query`, and rejects when the first column is NULL, empty, or only a prefix such as `{clear}` or `{nthash}`, the same as a password login, so clearing the password also disables certificate login. When you use only certificates and store no password, set `certificate_query`, for example `SELECT 1 FROM users WHERE username = ? AND enabled = 1`; it accepts when the query finds a row. `default_user` does not apply.
  - **`ldap`:** A search alone cannot tell that an account is locked, so set `certificate_filter` to accept certificate logins, and write the disabling conditions into it, for example `(&(uid={user})(!(pwdAccountLockedTime=*)))` for OpenLDAP and `(&(sAMAccountName={user})(!(userAccountControl:1.2.840.113556.1.4.803:=2)))` for AD. These two examples depend on how the directory is configured, so check them on your own directory first. It accepts when the search finds the entry and it matches `required_groups`, with no bind. Without the setting it is skipped and the next handler decides, and it does not connect to the directory.
  - **`lua`:** The script decides. The script can use the global variables `eap_method` (`"tls"`), `tls_subject` (the certificate subject) and `tls_subject_attrs` (a table split by field). **To judge a certificate field, use `tls_subject_attrs`, not `find` on `tls_subject`:** a certificate whose CN is `x, OU=admins` has the text `CN=x\, OU=admins`, which still contains `OU=admins`.
- **Keys in memory.** The MSK, the NT hash and the keys derived from it, and the PAP password are overwritten after use in rradius's own code, and `Debug` does not print the password. This only narrows the exposure: copies inside the TLS, hash and LDAP libraries, the `User-Password` that a script receives, and passwords read from the database cannot be cleared. To protect against core dump leaks, turn off core dumps at the system level.
- **Where the name comes from.** The name of a certificate is its DNS SANs and email SANs; the CN is used only when it has neither. If the SAN cannot be read, or contains a name that is not text, the certificate is rejected and the CN is not used instead. A CN in the name is accepted only as UTF8String, PrintableString, IA5String or NumericString; other types are not used to log in even when `tls_subject` can read them. The EAP Identity has to match one of the names, ignoring case, or it is rejected. When clients fill the Identity with an anonymous value, set `match_identity = false` to use the first name. The name always uses the spelling in the certificate, and the `User-Name` the script sees is that name. When the script or chain does not return a `User-Name`, the Access-Accept carries this name, so the device's logs and accounting use it instead of the value the client filled in.
- **Certificate login does not strip the realm.** When a chain sets `strip_realm`, a name that changes in more than case is rejected: for example `alice@partner.example` does not log in as `alice`, because the certificate proves the full name. Store the full name in the database, or let these clients use a chain without `strip_realm`. `lowercase` only lets ASCII letter case through: a change to a non-ASCII character (for example `É` or the Kelvin sign) is also rejected, because the Kelvin sign in lower case is the `k` of a different account.
- **One authentication must stay on one node.** The TLS state of a conversation exists only on the node that received it and cannot be taken over by another node. A normal primary/standby setup is fine: when the node switches, the client authenticates again. **Behind a load balancer, pin the node by source,** otherwise each round lands on a different node and fails, because the `State` is not found on the other node.
- **Message-Authenticator is always required.** A packet with `EAP-Message` and without this attribute is dropped, whatever the client's `require_message_authenticator` says (RFC 3579).
- **A conversation is bound to the device.** Every round has to match the `State`, the client and the `Calling-Station-Id`; another device cannot take it over.
- **A conversation has an overall limit.** A conversation has at most 256 requests and lives at most four times `session_timeout_ms` (2 minutes by default), however busy it is. So after a SIGHUP updates the CRL, conversations in progress under the old settings end within this time at the latest. A fragment with no data but the More flag ends the conversation.
- **The load from unauthenticated conversations is limited.** Anyone can open a conversation before the handshake, so when `max_sessions` and `max_sessions_per_client` are full, one conversation in that scope is evicted and counted in `result="evicted"`: first from the conversations still at the EAP Identity, and only if there are none, from all of them, always the one that has gone longest without a request. A conversation that holds a place by sending one packet is evicted first, so when random MACs flood the table, users who have already answered one round are not affected, and neither are those waiting on a slow LDAP. This only raises the cost: an attacker who is willing to answer one more round is on the same level as a normal conversation. When the traffic is so heavy that new users are replaced before they answer the first round, raise the limits; a `result="evicted"` that keeps increasing is the reminder. When `max_sessions_per_device` (by client plus `Calling-Station-Id`) is full, the server does not answer and the device retransmits, and it is counted in `result="overload"`. When all users go through one controller or RadSec proxy, that is one client, so raise `max_sessions_per_client` if peak logins are high.
- **Identifier mismatch.** A Response whose Identifier differs from the waiting Request is dropped silently (RFC 3748 section 4.1); the conversation continues as usual and it counts as one round; you can see it only at debug level.
- **A device starts over.** When the same device opens a new conversation, its old conversation that is still at the EAP Identity is replaced and counted in `result="replaced"`. A conversation whose handshake has started is not affected. A request without `Calling-Station-Id` cannot be matched to a device, so it replaces nothing and `max_sessions_per_device` does not apply to it.
- **Other limits.** A TLS message over 64 KiB ends the conversation. A conversation idle for longer than `session_timeout_ms` is dropped.
- **TLS.** EAP-TLS supports 1.2 and 1.3; PEAP and TTLS use only 1.2. System root certificates are not used; see CRL below for revocation checking. Only the secure cipher suites of rustls are supported, so very old clients may not connect.
- **Why PEAP and TTLS do not use TLS 1.3 (an evaluation, not yet verified on real hardware):** RFC 9427 defines how TTLS (and TEAP, EAP-FAST) export keys over TLS 1.3; PEAP is outside its scope, and each vendor implements it on its own. From Windows 11 22H2, PEAP and TTLS use TLS 1.3 by default, but use 1.2 when the server offers only 1.2, so connections are not affected (Microsoft's documentation also says NPS does not support TLS 1.3). The community reports problems with TTLS on Windows 11 over TLS 1.3 with session resumption; wpa_supplicant supports it from 2.10. The benefit of TLS 1.3 for PEAP and TTLS is mainly one fewer round trip; forward secrecy is already there with ECDHE in TLS 1.2, and the inner password is in an encrypted tunnel anyway. So it is not done for now; it will be evaluated when a client supports only TLS 1.3, or when testing on real hardware shows a need.
- **Certificates.** The EKU of the server certificate should include serverAuth, which some clients check (to my knowledge, not yet verified on real hardware). For high login volumes, use an ECDSA P-256 certificate: every login is a full TLS handshake, and on the same machine ECDSA takes about one third of the CPU per login of RSA 2048, with about 2 to 3 times the throughput (the measurements in section 9, "EAP logins"). ECDSA compatibility (compiled from research, not yet verified on real hardware):
  - **Use the P-256 curve, not P-384.** There are reports that Android 7.0 supports only P-256 among the elliptic curves, and a P-384 certificate chain fails on that version (community reports, not official documentation). The same applies to the CA and intermediate certificates.
  - **Windows 10 and later** enable suites such as `TLS_ECDHE_ECDSA_WITH_AES_128/256_GCM` by default, ahead of the RSA suites (Microsoft's Schannel documentation).
  - **Most clients that can connect with an RSA certificate can also use ECDSA:** the TLS 1.2 of rradius offers only suites with ECDHE and AES-GCM or ChaCha20, so the client has to support ECDHE; the TLS implementations found so far that support these suites also support ECDSA signatures. For macOS, iOS and Android this is to my knowledge.
  - **When unsure, keep RSA for now:** the server uses one certificate at a time, so before switching to ECDSA, connect once with each kind of device to check.
- **Keys.** They are returned to the device as `MS-MPPE-Recv-Key` and `MS-MPPE-Send-Key`; the MSK and TLS contents are not written to the logs.
- **Authentication records.** Each conversation that ends in accept or reject writes one record, with `chain` set to `eap` and `handler` set to `eap-tls`, `eap-peap` or `eap-ttls` by method; the rounds in between write nothing. A request that belongs to no conversation (for example an unknown `State`) also gets an Access-Reject, but only a WARN log is written. A conversation that times out has no record; look at `result="timeout"`. When PEAP rejects a password, the log and record are written right then, without waiting for the client to finish, and the `account` of the record is the normalized name.
- **Reload.** SIGHUP rereads this section and the certificates; new conversations use the new settings, and conversations in progress finish with the old ones, including `match_identity` and `session_timeout_ms`; the same applies when the section is removed, and conversations in progress finish EAP before ending. Send SIGHUP after the CRL is updated.
- **The CRL must cover every CA.** When `crl_file` is set, every CA in the certificate chain needs a matching CRL, and the certificate is rejected when the revocation status cannot be found; the log says `UnknownRevocationStatus`. With an intermediate CA, concatenate the CRLs of the root CA and the intermediate CA into the same file.

**Methods per client**

```toml
[radius.eap]
methods = ["tls", "peap"]

[[radius.clients]]
name = "wifi-corp"
address = "10.0.0.1"
secret_file = "wifi-corp.secret"
eap_methods = ["tls"]          # certificates only, the client cannot switch to passwords

[[radius.clients]]
name = "wifi-guest"
address = "10.0.0.2"
secret_file = "wifi-guest.secret"
auth = "guest"
eap_methods = ["peap", "tls"]  # offers peap first, the client can switch to tls with a Nak
```

- When `methods` has more than one method, **every RADIUS client must set `eap_methods`,** or startup fails. A client can switch methods with a Nak, so adding a method for one NAS must not open it to all NAS, otherwise a NAS that uses only certificates would also accept passwords.
- The order of the list is the order of the offer, and it can list only methods that are in `methods`. For a client that does not run EAP, set `eap_methods = []`; its EAP requests get an EAP-Failure.
- With only one method, you do not have to set it.

**Which one to use?** If every device can be issued a certificate, use `tls`. To use accounts and passwords, it depends on how the passwords are stored: for plain text or `{nthash}`, use `peap`, which every operating system has built in; for only a hash (for example `{argon2}`), or for the `bind` mode of `ldap`, use the inner PAP of `ttls`.

**PEAP-MSCHAPv2**

- **The client must verify the server certificate.** MS-CHAPv2 itself is weak, and the protection comes entirely from the TLS tunnel. When a client is not set to verify the server certificate, a fake access point can get the hash of the password; this is the most common deployment mistake.
- **A chain is required.** Startup fails when `methods` has `peap` and there is no `[auth]`; when a client has no chain, the conversation ends with an EAP-Failure and the log gives the reason.
- **The password must be readable.** MS-CHAPv2 compares the plain text or the NT hash, so the password of the `sql` handler has to be plain text or `{nthash}`; the `ldap` handler has to use `password_check = "attribute"`, and **the `bind` mode cannot be used**.
- **The password must really be compared.** The login passes only when a handler has compared the password; when a `lua` handler returns `accept`, it is treated as a rejection, and the `MS-CHAP2-Success` it sets itself is also dropped, so a script mistake does not turn into skipping the password check. The inner keys are not sent; the keys the device gets come from the TLS tunnel.
- **Names.** The chain and the `account` of the authentication record use the inner name, and the `user` of the authentication record is the outer name; the outer name can be fake and is not used for any decision. The Access-Accept carries the account name normalized by the chain as `User-Name`, unless the chain returned one itself. `default_user` does not apply.
- **Inner name.** Over 253 octets it is rejected and the chain is not run, the same as TTLS.
- **Scope.** Only PEAPv0, inner EAP-MSCHAPv2 and TLS 1.2. There is no PEAPv1, cryptobinding, inner GTC, password change or client certificate. TLS 1.3 is not offered, because there is no data on how clients support it. The inner part has at most 8 rounds, normally 4. A wrong password is not retried.
- **Script.** A `lua` handler in the chain sees `eap_method` as `"peap"` and `tls_subject` as `nil`.

**EAP-TTLS**

- **The client must verify the server certificate, and here it matters more than for PEAP.** The inner PAP sends the plain text password into the TLS tunnel. When the client does not verify the server certificate, a fake access point gets the password directly, not only a hash.
- **Inner PAP:** The `sql` handler can compare a hash and the `ldap` handler can use the `bind` mode, which PEAP cannot do. A name over 253 octets or a password over 128 octets is rejected directly.
- **The password must really be compared.** The handler that passes must be `sql`, `ldap`, or a `lua` with `verifies_password = true`. A `lua` handler without that setting that returns `accept` is treated as a rejection, and the log says `without checking a password`. The reason is that a script written for EAP-TLS often only checks whether the account is enabled, and adding `ttls` to `eap_methods` must not make it pass any password; the inner method is also chosen by the client itself. In an `all_accept` chain, one handler that compared the password and accepted is enough; when `sql` cannot find the name, it returns "skip", which does not count as having compared.
- **Inner MS-CHAPv2:** The same as PEAP: the password has to be plain text or `{nthash}`, and a `lua` handler cannot pass it. The challenge has to be the value exported from the TLS tunnel; one chosen by the client itself is rejected.
- **A script must not log passwords.** With the inner PAP, a `lua` handler in the chain can see `User-Password`. A script that compares the password itself has to set `verifies_password = true` and has to make sure it does not write it to the log. The server itself does not log it. The script sees `eap_method` as `"ttls"`.
- **Otherwise the same as PEAP:** a chain is required, `default_user` does not apply, the `account` and the `User-Name` of the Access-Accept use the normalized inner name, the outer name decides nothing, and a rejection writes a record right away. A missing inner name is rejected.
- **Scope.** Only TTLSv0 and TLS 1.2, with only PAP and MS-CHAPv2 inside. There is no inner EAP, CHAP, MS-CHAPv1 or client certificate. An AVP with the M flag that is not recognized ends the conversation. **It has not been tested on real devices,** and the default inner method differs between operating systems, so check it when you deploy.

Measurements (Docker Desktop, Linux aarch64 container, 14 cores, Apple M4 Max, release build, `eapol_test` and rradius on the same machine; 2048-bit RSA certificates, default `fragment_size` of 1000, `match_identity` on):

| Item | TLS 1.2 | TLS 1.3 |
|------|---------|---------|
| RADIUS round trips per login | 6 | 7 |
| Time per login (average of 50 in a row, including starting `eapol_test`) | 5.7 ms | 6.1 ms |
| Server CPU (4 ticks of 10 ms for 50 logins, only a rough value) | about 0.8 ms | about 0.8 ms |

- **Memory per open conversation:** about 3.6 KB right after the EAP-TLS Start is sent, and about 13.6 KB after the ClientHello is received and the server certificate is sent (2000 of each opened, looking at the difference in process RSS). So the default `max_sessions` full is about 56 MB.
- **Worst case:** when the peer sends fragments up to near the 64 KiB limit and then stops, each conversation takes about 85 KB (1000 opened, looking at the difference in process RSS), and this can be done unauthenticated. So with `max_sessions` full it is about 350 MB, and with a single client at `max_sessions_per_client` it is about 87 MB; use these numbers to estimate memory. After the idle timeout, memory returns to the original level.
- **Logins during a flood:** new conversations were opened with random MACs from the same client (500, 2000 and about 37000 per second, the last being the limit of this environment) while logging in continuously from that client; in all three traffic levels about 1500 logins each succeeded. In this environment the gap between two rounds is only a few milliseconds. These numbers were measured before the change to evict conversations that sent only an Identity first. Now only the time from the Identity to the first response has to be caught: the gap has to be shorter than `max_sessions_per_client` divided by the new conversations per second, for example 1024 divided by 2000 is about 0.5 seconds; this is a calculated value, and the real gap on wireless networks has not been measured.
- **Unauthenticated load:** answering one ClientHello takes about 0.46 ms of CPU (TLS 1.3, 2000 in a row).
- **Certificate chain length:** with a client sending an RSA-4096 leaf certificate (1442 bytes) and an intermediate CA (1094 bytes), TLS 1.3 completes the login, far below the 64 KiB limit.

These numbers represent only the environment above and are not a performance guarantee, so the defaults stay unchanged.

### 2.13 Proxy `[proxy]`

The proxy (`[proxy]`) needs rradius-enterprise.

### 2.14 Account management `[accounts]`

When accounts are kept in a cluster table, use `rradiusd user` to add, change the password of, enable, disable, delete and view accounts. Writes go through the cluster and all nodes agree immediately, so the next login sees them, with no migration, restart or SIGHUP.

You decide the structure of the table, so the SQL to run for each action is written in `[accounts]`:

```toml
[accounts]
scheme = "bcrypt"   # default; see the table below
add = "INSERT INTO users (username, password) VALUES (?1, ?2)"
passwd = "UPDATE users SET password = ?2 WHERE username = ?1"
enable = "UPDATE users SET enabled = 1 WHERE username = ?1"
disable = "UPDATE users SET enabled = 0 WHERE username = ?1"
delete = "DELETE FROM users WHERE username = ?1"
show = "SELECT username, enabled FROM users WHERE username = ?1"
```

- **`?1` is the account name and `?2` is the value stored for the password.** Only these two numbered parameters can be used, not an unnumbered `?`, so that a wrong order cannot store the password in the account column. Every statement has to use `?1`: without it, for example an `UPDATE users SET password = ?2` that is missing the `WHERE`, would change every account. `add` and `passwd` have to use `?2`, and the other actions must not. A `?` inside a string or comment does not count. When this is violated, startup, `check` and the commands all fail and name the setting.
- Every action can be omitted; an omitted action ends with status 2 when run, saying which setting is missing.
- `disable` has to be designed together with `user_query`: for example, `user_query` has to select only accounts with `enabled = 1`, otherwise disabling does not block the login.
- You decide which columns `show` selects, and they are printed as they are, so do not select the password column.

**Commands:**

```bash
rradiusd user add --config /etc/rradius/rradius.toml alice
rradiusd user passwd --config /etc/rradius/rradius.toml alice
rradiusd user disable --config /etc/rradius/rradius.toml alice
rradiusd user show --config /etc/rradius/rradius.toml alice
```

When the configuration file is the default one (section 1), you can omit `--config`, for example `rradiusd user show alice`.

- **The password is read only from standard input,** not placed on the command line (where it would stay in the shell history and the process list). On a terminal it asks twice and shows no input; two different passwords or an empty password are rejected and nothing is written. When reading from a pipe it uses the first line, for example for a batch import: `printf '%s\n' "$PW" | rradiusd user add --config c.toml alice`.
- **Names are normalized the same way as at login:** the `[auth.username]` of the default chain is applied (strip the realm, lower case, the `allowed` check), so `user add Alice@Corp` is stored as `alice` with `strip_realm = "suffix"` and `lowercase = true`. The `[auth.chains.<name>.username]` of a named chain is not applied.
- **The api port through the cluster:** it connects with the `nodes` addresses and `secret_api` of `[cluster]`, so the cluster has to be running with most nodes online, and the host running the command has to be able to reach the api port. The command does not start a node or create a data directory.
- **`secret_api` is the key to the whole database.** Anyone who can read the configuration file can already use it to change any table, so make the configuration file and `secret_api_file` readable only by administrators, and run the commands on the node or over WireGuard (section 2.2).

| `scheme` | Stored as | Good for |
| --- | --- | --- |
| `bcrypt` (default) | `{crypt}$2b$10$...` | General use. Verification is deliberately slow: measured on 2026-11-07 with `rradius-bench` (PAP, concurrency 64) on a 14-core Apple M4 Max at about 281 logins per second, which is not enough for high login volumes |
| `sha512-crypt` | `{crypt}$6$...` | High login volumes with PAP only. It still has a salt and many hash rounds, and verification is much faster |
| `nthash` | `{nthash}...` | MS-CHAPv2, MS-CHAPv1, PEAP, and MS-CHAPv2 in EAP-TTLS (sections 5.1 and 5.2); PAP also works |
| `clear` | `{clear}...` | CHAP (for example PPPoE), which can only compare plain text. A database leak is a password leak |

`scheme` affects only passwords written afterwards; existing accounts keep their original format and the server still verifies them (section 5).

| Result | Output | Status code |
| --- | --- | --- |
| Done | `added alice`, `password changed for alice`, `enabled alice`, `disabled alice`, `deleted alice`; `show` prints one line `column=value` per column, with a blank line between rows | 0 |
| No matching account (`passwd`, `enable`, `disable` and `delete` changed 0 rows, or `show` found nothing) | `no account alice` | 1 |
| The database refuses, for example `add` for an account that already exists | An error message (stderr) | 1 |
| The usage, configuration, name or password is wrong and nothing was sent; the node refuses the `[cluster] secret_api` (it answers immediately, and the message has `refused the api secret` and the node address) | An error message (stderr) | 2 |
| The cluster cannot be reached, or does not answer within 30 seconds | An error message (stderr) | 3 |

No output or error message contains the password or the stored value.

The management commands for dynamic clients, DPSK keys and SIM cards (`client`, `dpsk`, `aka`, with the SQL written in `[accounts.clients]`, `[accounts.dpsk]` and `[accounts.aka]`) need rradius-enterprise.

### 2.15 One WPA-PSK per person

On one WPA2-PSK SSID, giving every user their own password (vendors call it DPSK, iPSK, MPSK or PPSK) lets you tell people apart on devices that do not support 802.1X, revoke one person's password, and assign a VLAN per person. An AP does it in one of two ways:

| Approach | What the AP sends | What rradius does |
| --- | --- | --- |
| **MAC authentication, reply with this device's PSK** | An Access-Request with the device's MAC as `User-Name` | A script or the `sql` handler looks up the password by MAC and puts it in the reply, see "Replying with the PSK by MAC" below |

The other approach is to find out which key was used from the 4-way handshake (`[radius.dpsk]`), which needs rradius-enterprise.

#### Replying with the PSK by MAC

The device's MAC has to be registered first. The AP sends an Access-Request by MAC, and the script looks up the password by MAC and puts it in the reply. The attributes each vendor uses:

| AP | Reply attribute | How to write it |
| --- | --- | --- |
| Cisco iPSK | `Cisco-AVPair` | `set_attr('Cisco-AVPair', {'psk-mode=ascii', 'psk=' .. passphrase})` |
| Aruba MPSK | `Aruba-MPSK-Passphrase` | `set_attr('Aruba-MPSK-Passphrase', passphrase)`, encrypted automatically according to the dictionary |
| Meraki, Mist, hostapd (`wpa_psk_radius=1` or `2`) | `Tunnel-Password` | `set_attr('Tunnel-Password', passphrase)`, encrypted automatically according to RFC 2868 |
| Ruckus | `Ruckus-DPSK` | `set_attr('Ruckus-DPSK', '\0' .. hex_to_bytes(wpa_pmk(passphrase, ssid)))`, with a 0x00 in front of the PMK; you have to write `hex_to_bytes` yourself |

- Go by each vendor's documentation for attribute names and value formats; the table above has not been verified on real devices.
- The value of `Cisco-AVPair` is sent in plain text, so use it only on a trusted network or over RadSec.
- `wpa_pmk` runs 4096 rounds of PBKDF2 each time, about a few milliseconds; with many devices, store the computed PMK instead of computing it at every login.

### 2.16 Shutdown `[shutdown]`

When the first SIGTERM or SIGINT is received, it does these in order:

1. **`/readyz` returns 503 `stopping`,** but RADIUS and TACACS+ answer as usual for `ready_delay_ms`, giving the load balancer or Kubernetes time to take this node out and send traffic elsewhere. `/livez` does not change.
2. **It stops accepting:** it no longer reads the RADIUS UDP port, and closes the TACACS+ port (new connections are refused); TACACS+ connections that are already open close after sending the answer to the request in hand and do not read the next one (an ASCII login in progress can finish, up to its 60 second limit; during that time other requests on the connection are not answered, and the connection is closed directly); the accounting spool stops replaying and the data stays on disk.
3. **It waits for requests in progress,** for at most `drain_ms`. When everything has been answered it logs `stopping: every request taken was answered`; when time runs out with work unfinished it logs a WARN, for example `stopping: after 5000 ms, 1 RADIUS requests were still being handled and 0 TACACS+ connections open; they are cut off`, and those requests get no answer, so the device retransmits to another server.
4. When `[radius.accounting_log]` is set, it finishes writing its queue (at most 5 seconds, see section 2.18), then closes the database (with several nodes hiqlite also waits 9.5 seconds, see section 1), writes out the logs, and exits with status 0.

```toml
[shutdown]
ready_delay_ms = 5000 # step 1; default: 5000 when [cluster].nodes has more than one node, otherwise 0
drain_ms = 5000       # step 3; default 5000
```

- Both values are 0 to 60000 (milliseconds); a larger value fails startup. A reload (SIGHUP) applies them, and shutdown uses the settings from the moment the signal arrived.
- **A single node does not wait in step 1 by default,** because no other server can take over, and waiting longer only adds seconds of no answers. With a load balancer in front, set it to at least "probe interval x number of failures" plus a little, for example the readinessProbe in the Kubernetes example runs every 5 seconds and takes the node out after 1 failure, so the default of 5 seconds is just enough.
- **A second SIGTERM or SIGINT skips steps 1 and 3 that are not finished waiting,** logs `stopping: asked again; not waiting for the requests being handled`, and goes directly to step 4; the database still closes normally, and this does not shorten that.
- **The grace period has to be long enough:** with several nodes it takes up to about `ready_delay_ms` + `drain_ms` + 9.5 seconds + writing the logs (at most 5 seconds), about 25 seconds by default, so `docker stop -t 30`, `stop_grace_period: 30s` and `terminationGracePeriodSeconds: 30` in Kubernetes are all enough. When you raise these two values, raise the grace period with them, otherwise a SIGKILL follows and the next startup has to rebuild the local database.
- A signal during startup (while waiting for the cluster or applying migrations) does not go through these steps; see section 1, "Stopping".

### 2.17 External databases `[databases]`

If the account table or accounting table is already in MySQL or PostgreSQL and another system keeps maintaining it, you do not need to move it into the cluster. List the database here. The `sql` handler selects it with `database` (section 3.3), and scripts use `db_query` and `db_execute` (section 4.3).

```toml
[databases.billing]
kind = "postgresql"              # required, "mysql" or "postgresql"
host = "db.example.com"          # required
port = 5432                      # default: mysql 3306, postgresql 5432
database = "radius"              # required
user = "rradius"                 # required
password_file = "billing.secret" # or password = "...", set only one
tls = "verify"                   # default; "verify" or "off"
ca_file = "db-ca.pem"            # optional; without it the system root certificates are trusted
pool_size = 8                    # default, maximum connections per node
connect_timeout_ms = 5000        # default, time limit for opening one connection
required_for_ready = false       # default; when true, an unreachable database makes /readyz return 503
```

- **Name.** Only ASCII letters, digits and `_`, for example `billing`. Connection URLs are not supported.
- **Password.** `password_file` has its trailing newline removed and must not be empty. A relative path is resolved against the directory of the configuration file. With neither `password` nor `password_file`, no password is sent. `rradiusd config show` does not display `password`. PostgreSQL does not read `~/.pgpass`, but it does read environment variables such as `PGPASSWORD`: if the configuration file has no password and the environment has `PGPASSWORD`, it is sent.
- **TLS.** `verify` checks the certificate chain and the host name (PostgreSQL `verify-full`, MySQL `VERIFY_IDENTITY`). A certificate not issued by `ca_file` cannot connect and counts as the database being unavailable. `ca_file` is read by `check` and at startup; startup fails if it cannot be read or contains no certificate. It cannot be set when `tls = "off"`. `off` does not encrypt, and when `host` is not the local machine startup logs a WARN, because the password and the data are sent in clear text.
- **Without TLS, MySQL accounts must use `mysql_native_password`.** The MySQL 8 default `caching_sha2_password` needs an RSA exchange of the password when there is no TLS, and rradius does not implement that. MySQL 8.4 disables `mysql_native_password` by default: start it with `--mysql-native-password=ON`, then create the account with `CREATE USER ... IDENTIFIED WITH mysql_native_password BY '...'`. Over TLS there is no such limit.
- **Connection pool.** At startup only the pool is created and nothing connects; the first request that needs the database opens the connection. So the server starts even when the database is down, and only the requests that use it fail. Each node has its own pool, and the number of connections the database allows must be at least the number of nodes × `pool_size`.
- **Needs a restart.** A reload (SIGHUP) reports that `[databases]` needs a restart, and the running pools keep being used. If a handler uses a name that did not exist before the restart, the reload fails and says a restart is needed.
- **`pool_size` sets the throughput at full load,** roughly `pool_size` ÷ the round-trip time of one query. Measured (Apple M4 Max, PostgreSQL 17 in local Docker, PAP with the `sql` handler, clear-text passwords): `pool_size = 8` gives about 13300 per second and 32 gives about 19900 per second; on the same machine the built-in database gives about 48000 per second. At 2000 per second both have a p99 of about 0.35 ms. With a more distant network each round trip takes longer, so more connections are needed for the same throughput.
- **`required_for_ready` and `/readyz`.** For a database set to `true`, the node checks in the background from startup: on a connection that is not in the pool, it runs `SELECT 1` every 5 seconds (counted from the start of the previous check), reconnects first when there is no connection or the last check failed, and each check is capped by `connect_timeout_ms`. 2 consecutive failures count as unreachable, and 1 success restores it. Before the first successful check after startup it also counts as unreachable, so a new node whose account database is unreachable does not receive traffic first. While unreachable, `/readyz` returns 503 `database <name> unreachable` (section 2.8), `/livez` is not affected, and requests are still handled (those that use the database fail as usual). A state change logs a WARN (unreachable) or INFO (restored); other failed checks are logged only at debug.
  - **How soon it is noticed:** at worst about 2 × (5 seconds + `connect_timeout_ms`), which is about 20 seconds by default. Recovery takes at most 5 seconds + `connect_timeout_ms`. Measured (PostgreSQL 17 in OrbStack Docker, `connect_timeout_ms = 1000`, container stopped): 503 after 9.7 seconds, and 200 0.4 seconds after the database responded. When the database host is disconnected and packets are dropped, each check uses up the whole `connect_timeout_ms`, which is close to the worst case. The interval and the count cannot be configured at present.
  - **Why a separate connection and a background check:** a full pool (heavy traffic, slow queries) does not mean the database is unreachable, and checking through a pooled connection would make all nodes unready together at peak load. Judging by request failures would leave no chance to recover once the load balancer stops sending traffic. Connecting on each `/readyz` could exceed the default 1-second Kubernetes probe timeout.
  - **When to set it:** only when "without this database it cannot respond correctly". All nodes share the same database, so when it is down all nodes become unready and the load balancer has nowhere to send traffic. Do not set it for a database used by only a few requests. Each node holds one more connection to it, and the number of connections the database allows must be increased accordingly.
  - **Metrics:** `rradius_external_db_failures_total`, `rradius_external_db_connections` and `rradius_external_db_connections_max` exist for every database, and `rradius_external_db_reachable` exists only for the ones that are set (section 2.8). The External databases section of `status` also lists them.
- **Time limits.** The `timeout_ms` of the `sql` handler includes the time spent waiting for the pool to give a connection. A script's `db_query` and `db_execute` share one request's `[lua] query_timeout_ms` with `sql_query` and `sql_execute`.
- **On failure.** Failure to connect, a timeout, a TLS failure and errors that mean the database is busy (PostgreSQL SQLSTATE classes 08, 53 and 57, and 40001 and 40P01; MySQL 1040, 1053, 1205, 1213 and 1317) count as the database being unavailable. Logins get no response, so the device retransmits or tries another server. When an accounting spool is set (section 2.11), accounting is written to the spool and replayed after recovery. Errors in the statement itself, such as a syntax error or a constraint violation (for example a duplicate primary key), are rejected for logins. For accounting they get no response, and when replayed from the spool they are moved to `rejected.log`.
- **Placeholders.** Only `?`, bound in order. PostgreSQL sends them as `$1`, `$2`, and MySQL sends them unchanged. A `?` inside a single-quoted string, a double-quoted name or a comment (`--`, `/* */`) does not count. PostgreSQL `::text` casts can be used. `?1`, `:name` and `$name` cause an error at startup or at call time. Not supported: `?` inside square brackets (for example `a[?]` for a PostgreSQL array: the brackets are taken as name quotes, and the count does not match, which is an error), PostgreSQL `$$` strings (a `?` inside is taken as a placeholder), PostgreSQL `?` operators (jsonb `?`, `?|`, `?&`; use functions such as `jsonb_exists()` instead), and MySQL quotes escaped with a backslash (`'it\'s'`; write `'it''s'`).
- **PostgreSQL parameters have types and are not converted automatically:** a string is bound as `text`, an integer and `true`, `false` are bound as `bigint` (1, 0), and other numbers are bound as `double precision`. When a column is of type `boolean`, `inet`, `timestamp` and so on, convert in the SQL, for example `CAST(? AS inet)` or `?::inet`, and for a boolean column write `active = (? <> 0)`. Without the conversion PostgreSQL returns a type error, which counts as an error of the statement. MySQL converts by itself.
- **Value types.** Integers (including MySQL `UNSIGNED`, which is an error above 2^63−1) and floating-point numbers → number; boolean → 1, 0; text → string; binary → string; `NUMERIC`/`DECIMAL` (the result of `SUM`, `AVG`) → a string of decimal digits, for example `"12.50"`, so use `tonumber` when you need to calculate; NULL → `nil`. Other types (date and time, geometric types and so on) cause an error that names the column; `CAST` them to text or a number in the SQL.

### 2.18 Accounting log file `[radius.accounting_log]`

Writes every accounting request that was answered as one JSON line, appended to a text file, for systems that read the file directly (for example a log collector or your own billing script). It needs no database and does not affect how scripts write to a database. Without this section no file is written.

```toml
[radius.accounting_log]
file = "/var/log/rradius/accounting.jsonl"  # required, a relative path is resolved against the directory of the configuration file
queue = 65536                               # default; maximum number of entries waiting to be written to the file, 1 to 16777216
```

- **Checks:** if a field name is misspelled, `queue` is out of range, the directory of `file` does not exist, or the file already exists but cannot be appended to, startup, SIGHUP and `check` all fail and name the field. `check` does not create the file.
- **Content of a line:** for example

  ```json
  {"time":"2026-11-15T08:00:00.123Z","client":"nas1","source":"10.0.0.5","transport":"udp","handling":"script","attributes":{"Acct-Status-Type":"Start","User-Name":"alice","Acct-Session-Id":"s-1","Class":["a","b"]}}
  ```

  `time` is the time of receipt (RFC 3339, UTC, to the millisecond), `client` is the name of the client, `source` is the source address, `transport` is `udp` (`radsec` for RadSec in the enterprise edition), and `handling` is `script` (the script accepted it) or `spool` (put into the accounting spool, section 2.11; `proxy` for the proxy in the enterprise edition). `attributes` is an object whose keys are the attribute names in the dictionary. An attribute that is not in the dictionary is written as `Attr-<number>` with a value in hexadecimal starting with `0x`, and an attribute that appears more than once is written as an array.
- **Port ranges (RFC 8045):** when a request carries the `IP-Port-Range` attribute (an operator puts many subscribers behind one public address, and the NAS or CGN uses it to report allocated and released ports), `attributes` gets one more key at the end, `IP-Port-Range`: an array with one object for each `IP-Port-Range`, whose keys are the dictionary names of the sub-attributes (`IP-Port-Range-Alloc`, `IP-Port-Range-Ext-IPv4-Addr`, `IP-Port-Range-Range-Start`, `IP-Port-Range-Range-End`, `IP-Port-Range-Local-Id` and so on), in packet order; it is an array even when there is only one. The original flattened keys (`IP-Port-Range-Range-Start` and so on, written as an array when a name repeats) are kept as before, so existing readers are unchanged. For a request without this attribute, the line is the same as before. A malformed attribute with invalid sub-attributes appears in neither place, and sub-attributes that are not in the dictionary are not written. Use `accounting find` to query (see below).
- **Only answered requests are recorded.** Only a request for which an Accounting-Response was sent is recorded. A NAS retransmission answered from the retransmission cache is not recorded again, and a request the script rejected or did not answer is not recorded.
- **File:** opened in append mode and created if it does not exist, with permissions 0640 (further limited by umask). There is no compression, no rotation by date, no upload and no deletion of old files; rotation is left to a tool such as logrotate (see the example below). In a cluster each node writes a file on its own machine, and a line has no node number, so to merge them tell the nodes apart by path or file name.
- **Does not slow down replies.** After the reply, only this one entry is put on a queue, and a dedicated thread takes a batch every 10 ms and writes it to the file (no fsync), so a line appears in the file about 10 ms later at most. When the queue is full (more than `queue` entries), the new entry is not written and is counted in `rradius_accounting_log_dropped_total`, and accounting is still answered. When a write fails (for example the disk is full), that batch is not written and is counted in `rradius_accounting_log_failures_total{stage="write"}`, with one WARN line (at most one line per minute for the same error), and the next batch is tried as usual.
- **Written out at shutdown.** On SIGTERM (section 2.16), after all requests in progress have been answered, the entries in the queue are written before exiting, so accounting answered during `ready_delay_ms` and `drain_ms` is in the file too. It waits at most 5 seconds; if it cannot finish, it logs a WARN and the rest is not written. On SIGKILL, what is still in the queue (normally about 10 ms of entries) is lost.
- **Reload:** when SIGHUP succeeds, the current batch is written first, and then the file is opened again with the (possibly new) `file`, so changing `file` needs no restart, and logrotate relies on this. If it cannot be opened, it is counted in `failures_total{stage="open"}`, a WARN is logged, and the original file continues to be written. If the directory of the new `file` does not exist, the whole reload fails (section 2.9). Adding or removing this section or changing `queue` needs a restart.
- **Personal data:** the lines contain personal data such as `User-Name` and addresses; manage the permissions and the retention period yourself.

**Rotation:** with logrotate, first move the file away, then send SIGHUP so that rradius opens a new file:

```
/var/log/rradius/accounting.jsonl {
    daily
    rotate 30
    missingok
    notifempty
    nocreate
    compress
    delaycompress
    postrotate
        systemctl kill -s HUP rradius.service
    endscript
}
```

- **`nocreate`:** the new file is created when rradius opens it again, with permissions 0640.
- **`delaycompress`:** the few lines written after the move and before SIGHUP go into the moved file and are not lost. Delaying the compression to the next rotation means a file that is still being written is not compressed. If the SIGHUP reload fails because of another error in the configuration file, the file is not opened again either and the moved file continues to be written, so watch `rradius_reload_total{result="error"}`.
- **Do not use `copytruncate`.** Lines written after the copy and before the truncation are lost.
- If you do not use systemd, replace `postrotate` with a command that sends SIGHUP to the rradiusd process (section 2.9).

**Finding who used a public address and port: `accounting find`.** When an operator uses CGNAT, the question is "at a given time, who had this public address and port". `rradiusd accounting find` reads only the log files above. It does not connect to the cluster and needs no configuration file, so it can run on files that were copied out as a backup:

```
rradiusd accounting find --ip <public IPv4> --port <port> --at <time> [--file <path>]... [--json]
```

- `--at` is RFC 3339 and must have a time zone, for example `2026-11-25T10:00:00Z` or `2026-11-25T18:00:00+08:00`. It can have fractional seconds.
- `--file` can be given several times. `-`, or no `--file` at all, reads standard input. A `--file` whose name ends in `.gz` is decompressed automatically (it can be several gzip streams joined together; for a damaged or truncated file the exit code is 2 and the file name is named). Standard input is not decompressed, so use `zcat accounting.jsonl.2.gz | rradiusd accounting find …`. The files need not be sorted by time first: the command sorts by event time itself, but lines with `IP-Port-Range` and `Stop` are loaded into memory all at once, so choose the files by date first.
- **Holding rules (RFC 8045):** when `IP-Port-Range-Alloc` is `Allocation`, holding of that public address and of the ports from `Range-Start` to `Range-End` begins. `Deallocation` ends the holding of the same subject (the same `IP-Port-Range-Local-Id`; if there is none, the same client and `Acct-Session-Id`; if there is none, the same client with `User-Name` and `Framed-IP-Address`), the same address and overlapping ports, as a whole (it is not split). When a release has no start and end ports, it ends all holdings of that subject. **A `Stop` with the same client and `Acct-Session-Id` also ends all its holdings.** The RFC does not specify this; it is a decision of this command, and the result's `ended_by` is marked `stop`. A holding with neither a release nor a Stop is considered still held.
- **Time:** the event time of each line is taken from `Event-Timestamp` first, otherwise from `time` minus `Acct-Delay-Time`, otherwise from `time`. The result's `time_source` says which was used: `Event-Timestamp`, `received-minus-delay` or `received`.
- **Output:** one line per holding. `--json` prints an array (keys `ext_ip`, `port_start`, `port_end`, `allocated`, `released`, `ended_by`, `time_source`, `identity`, `attributes`). `attributes` holds only the `User-Name`, `Acct-Session-Id`, `Framed-IP-Address`, `NAS-Identifier`, `NAS-IP-Address` and `client` that were in that line, and **does not assume a user name is always present**.
- **Exit codes:** 0 means at least one holding was listed, 1 means none, and 2 means a bad argument or file, or a line that is not a JSON object (the message names the file and the line number).
- **What it cannot see:** an allocation without `Ext-IPv4-Addr` or without start and end ports, and a range without any identity (`Local-Id`, `Acct-Session-Id`, or `User-Name` plus `Framed-IP-Address`), cannot be matched, and the command prints the number of such ranges on standard error. A standalone CGN that has only private addresses and must be matched with the BNG's sessions is not supported in this version, and neither are IPv6 public addresses.
- **Retention:** this command stores no data. How long the log files are kept is decided by logrotate or newsyslog and (in the enterprise edition) `keep_days`. The legal retention period is for you to decide, and there is no default.

**Differences from the enterprise edition's packet journal:** `[accounting_export.packets]` in rradius-enterprise records the same requests with the same fields in each line, plus `node`. Both can be on at the same time, and the same request is then written in both places.

| | This section | Enterprise packet journal |
| --- | --- | --- |
| Format | JSON Lines, not compressed | CSV or JSON Lines, gzip |
| Rotation | external tool moves the file, plus SIGHUP | automatic daily rotation |
| Where it goes | the one file `file` | a directory and S3, old files deleted by `keep_days` |
| `node` field | no | yes |
| Maximum wait at shutdown | 5 seconds | 2 seconds |

**Cost:** measured on 2026-11-15 on an Apple M4 Max (14 cores, macOS) with a release build and `rradius-bench` `acct-start` (concurrency 64, the script accepts everything and writes no database), alternating on and off, 4 runs each: throughput 59414 → 59141 per second (about 0.5% less), and the server's CPU per request 108.6 → 113.6 µs (about 4.6% more), with no drops. A packet from this tool is about 550 bytes per line, so 59 thousand per second is about 32 MB/s; the real size varies with the attributes the NAS sends.

### 2.19 Online sessions `[sessions]`

To find out who is online now, or which sessions a user has, use `rradiusd session list` and `rradiusd session show`. rradius itself has no session table: a session is a row that your accounting script writes into your own table, so which rows count as "online" and what the columns are called are written by you in the query, as with `[accounts]` (section 2.14). **The core has no built-in session table, so it does not make every accounting request write once more.**

```toml
[sessions]
list = "SELECT username, nas, session_id, started FROM accounting WHERE stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
show = "SELECT username, nas, session_id, started FROM accounting WHERE username = ?1 AND stopped IS NULL AND updated > strftime('%s', 'now') - 900 ORDER BY started, session_id"
```

- **Both statements are read-only.** They must begin with `SELECT` or `WITH` (case-insensitive, with leading whitespace allowed), so these two commands do not change data. `list` has no parameters. `show` uses exactly one `?1`, which is the user name; `?` without a number, `?2` and named parameters cannot be used. If they are wrong, startup, SIGHUP and `check` all fail and name `sessions.list` or `sessions.show`. Both can be omitted; the command whose statement is not set exits with status 2 and says which setting is missing.
- **The name is normalized as at login.** With `strip_realm = "suffix"` and `lowercase = true`, `show Alice@Corp` looks up `alice`. A name rejected by `allowed` in `[auth.username]` exits with status 2 (the same as the `user` command).
- **Only the commands read `[sessions]`,** the running server does not look at it, so changing it needs no restart and no SIGHUP (SIGHUP still checks it, and a mistake makes the reload fail).
- **Runs through the cluster's api port,** connecting with the `nodes` addresses and `secret_api` of `[cluster]`. It reads consistently (it sees accounting that was just written), and it does not start a node or create a data directory. The cluster must be running, a majority of the nodes must be online, and the host running the command must be able to reach the api port. `secret_api` is the key to the whole database, so make the configuration file readable only by administrators (section 2.14).

```bash
rradiusd session list --config /etc/rradius/rradius.toml
rradiusd session show --config /etc/rradius/rradius.toml alice
rradiusd session list --json
```

When the configuration file is the default one (section 1), `--config` can be omitted.

- **Output:** by default one block per row, each line `field=value` (the same as `user show`), with a blank line between blocks, and NULL is an empty value:

  ```
  username=alice
  nas=10.0.0.5
  session_id=s-1
  started=1794729600

  username=alice
  nas=10.0.0.5
  session_id=s-3
  started=1794729900
  ```

  `--json` prints a JSON array with one object per row, whose keys are the field names (in the order selected): NULL is `null`, integers and decimals are numbers, text is a string, and binary is a lower-case hexadecimal string. The number of rows is decided by the query's own `LIMIT`, and the command sets no limit of its own.
- **Exit status:**

  | Status | Meaning |
  | --- | --- |
  | 0 | at least one row was printed |
  | 1 | no rows: `list` prints `no sessions`, `show` prints `no sessions for <name>`; with `--json` it prints `[]` |
  | 2 | the command or configuration is wrong (no `[sessions]`, a missing statement, a missing name, a rejected name), a node rejected `[cluster] secret_api` (returned at once, without waiting 30 seconds), or the database rejected the statement (for example the table does not exist); the reason is on standard error |
  | 3 | the cluster cannot be reached or did not answer within 30 seconds |

**Two ready-made queries.** Both were run against a temporary cluster in the tests (`rradius-server/tests/session_test.rs` and the quick-start scenario):

- **The `accounting` table used by the scenarios** (`rradius-server/examples/scenarios/*/migrations/1_schema.sql`, one row per session, `stopped` is the end time): this is the example above, and it uses `updated` to leave out rows from a NAS that crashed and sent no Stop (a row not updated for 15 minutes does not count as online). The configuration file of the quick-start scenario already carries this section.
- **The `active_sessions` table of `examples/simultaneous-use.lua`** (a Stop deletes the row, and `last_seen` is the time it was last seen):

  ```toml
  [sessions]
  list = "SELECT username, nas, session_id, last_seen FROM active_sessions WHERE last_seen > strftime('%s', 'now') - 900 ORDER BY last_seen, session_id"
  show = "SELECT username, nas, session_id, last_seen FROM active_sessions WHERE username = ?1 AND last_seen > strftime('%s', 'now') - 900 ORDER BY last_seen, session_id"
  ```

**When you write your own query:**

- **What you get is a snapshot of accounting data, not the live state of the NAS.** A NAS that crashed without sending a Stop, or accounting that arrives late, makes the rows differ from reality, so the query should leave out stopped rows and rows that have not been updated for too long. The command does not separately confirm that a session still exists.
- **For the enterprise edition's `disconnect --user` and `coa --user` to use the same `show`,** the result needs the two fields `nas` and `session_id`: `nas` is the name of a client, or an address that matches a client (the same convention as `session_query` of `[radius.coa]`; see the enterprise edition documentation). `username` can be selected as well, and when it is not selected the name from the command line is used. A Session ID is unique only within one NAS, so the two fields are used together.
- **External databases in `[databases]` are not supported in this version:** the commands query only the cluster's database.
- **The commands only read;** they do not disconnect anyone. Disconnecting users and sending CoA are in the enterprise edition (the `disconnect` and `coa` commands; see section 4.8 of the enterprise edition documentation).

### 2.20 Named queries `[queries]`

The core does not know your tables: the address pool, connection slots and plans are tables from the examples, and operators rename or replace them. To monitor them (how many addresses are left in a pool, how many connection slots are taken), write read-only SQL in the configuration file. Each node runs it periodically in the background and the result becomes a Prometheus gauge. The same definition can also be run once with `rradiusd query`.

```toml
[queries.pool_usage]
sql = "SELECT pool, state, count(*) AS addresses FROM ip_pool GROUP BY pool, state"
help = "Addresses of each pool by state."
labels = ["pool", "state"]
value = "addresses"
```

| Field | Description |
| --- | --- |
| Name (the `<name>` of `[queries.<name>]`) | Lower-case letters, digits and underscores, starting with a letter. The metric name is `rradius_query_<name>`. `list` (the command) and `failures_total`, `dropped_rows_total`, `last_success_timestamp_seconds` (they would collide with the aggregate metrics) cannot be used |
| `sql` | Required. Starts with SELECT or WITH and has no parameters (`?`, `?1`) |
| `help` | The HELP text of the metric; when not set it is a sentence containing the query name |
| `labels` | The columns whose values become labels; the label name is the column name, and the column must be text or an integer. They cannot repeat and cannot be the numeric column. There are no labels by default |
| `value` | The numeric column, an integer or a real number. Default `value`. A row where this column is NULL is skipped |
| `interval_ms` | The interval from the start of one run to the start of the next, in milliseconds, at least 5000, default 30000 |
| `timeout_ms` | The longest one run may take, in milliseconds, not greater than `interval_ms`, default 5000 |
| `max_rows` | The most rows that become samples in one run, 1 to 10000, default 200. The extra rows are dropped and counted in `rradius_query_dropped_rows_total`. This limits the number of samples, not the work the database does: rows are truncated after the database has returned all of them, so for a large result without `GROUP BY`, add your own `LIMIT` in the SQL |

- **Runs in the background:** the first run happens right after the server starts and the database is ready, and then once per interval, reading from **this node's own replica** (the same as `sql_query`). If the previous run has not finished, the next one does not start. A timeout (`timeout_ms`) only means it stops waiting; the read on the database side may still be running, so the query must use an index and be fast. `/metrics` reads only the stored result and does not query the database, so it does not slow down the metrics endpoint.
- **Shape of the result:** each row is one sample of the gauge. The same set of labels cannot appear in two rows (that counts as a `shape` failure), so use `GROUP BY` in the SQL or make the label columns unique.
- **On failure:** a timeout, a rejection by the database, or wrong column names or types in the result mean this run is not applied. **The last successful value is kept**, `rradius_query_failures_total{query,reason}` goes up by one, and one WARN line is logged (at most one line per minute for the same query and the same reason). To tell that a value is stale, use `rradius_query_last_success_timestamp_seconds`: `time() - rradius_query_last_success_timestamp_seconds{query="pool_usage"} > 120` means two minutes without a success.
- **Cluster:** every node reads its own replica and outputs the same numbers. In Prometheus expressions use `max()`, not `sum()` (which counts the same number N times): `max by (pool) (rradius_query_pool_usage{state="free"})`.
- **Cost:** the query must have an index it can use, or it scans the whole table (when SQLite has no statistics, an index that covers only part of the condition may lose to the primary key); check with `EXPLAIN QUERY PLAN`. The two example queries (chapters 14 and 15) both use a covering index and take under 5 milliseconds each on data with 110 thousand addresses and 200 thousand connection slots. Against the address pool example with 110 thousand addresses, logins at 64 concurrency were sent for 60 seconds each (3 runs per setting, a server with 7 CPUs). The median without queries is 19,125 logins per second, and with queries on (30 seconds or 5 seconds interval) it is 19,254 and 19,251, with no measurable difference (p50 about 2.9 milliseconds in all cases).
- **Needs a restart to apply:** adding, removing or changing `[queries]` all need a restart (section 2.9).

**Commands:**

```bash
rradiusd query list --config /etc/rradius/rradius.toml        # which queries exist: name, help, interval, SQL (newlines written as \n)
rradiusd query pool_usage --config /etc/rradius/rradius.toml  # run once, print as a table
rradiusd query pool_usage --json                              # the same result as a JSON array
```

`query <name>` runs that query **immediately** through the cluster's API port, without reading the cache, so it is not limited by `max_rows` and it can also show NULL. A name that is not in the configuration file, a wrong configuration file, or an unreachable cluster gives a non-zero exit status and one line of explanation (the same as the `session` command). `status` has a Queries section (section 2.8).

## 3. Auth chains `[auth]`

An auth chain runs several handlers in the configured order. Each handler returns "accept", "reject", "ignore" (hand over to the next handler) or "challenge" (only the `lua` handler, and only for RADIUS authentication).

### 3.1 Structure

```toml
# Default chain: used by clients that do not set auth
[auth]
policy = "first_match"

[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"

# Named chain: a client selects it with auth = "vpn"
[auth.chains.vpn]
policy = "all_accept"

[[auth.chains.vpn.handlers]]
type = "ldap"
# ...

[[auth.chains.vpn.handlers]]
type = "lua"
script = "vpn_rules.lua"
```

- `[[auth.handlers]]` is the default chain and can be left out. The `policy` under `[auth]` belongs to the default chain and can be set only when there is an `[[auth.handlers]]`.
- `[auth]` needs at least the default chain or one named chain. A named chain needs at least one handler.
- If a client's `auth` points to a chain that does not exist, startup fails, and the message contains the client name and the chain name.
- Each chain is built once at startup. Clients that use the same chain (RADIUS and TACACS+ alike) share it, and they share its LDAP connection pool too.

Selection rule: a client with `auth` uses that named chain. A client without `auth` uses the default chain. With no default chain, `[lua].script` is used.

Chains are used only for RADIUS Access-Request and TACACS+ authentication (PAP at START, ASCII login at the last CONTINUE). Authorization and accounting are still handled by `[lua].script`.

### 3.2 Combining rule `policy`

| policy | Behavior |
| --- | --- |
| `first_match` (default) | Runs the handlers in order. The first handler that does not ignore decides the result. On accept, only that handler's reply attributes are returned |
| `all_accept` | Runs the handlers in order. Any reject or challenge stops the chain. At least one accept is needed to pass. Reply attributes are merged in order. If the chain has handlers that check the password (`sql`, `ldap`, `netlogon`, and a `lua` with `verifies_password` set), at least one of them must accept, otherwise the result is reject. Before they accept, a script that does not check the password cannot return `challenge` either. If nobody has accepted and no such handler comes later, the script does not run, so a name that is not found leaves none of the script's writes behind. This also applies to certificates: a certificate proves a name, not that the account may be used |

In both modes, if every handler ignores, the result is reject.

`all_accept` also fits "check the password first, then do one more thing": for example, a `lua` handler after a `sql` handler that allocates an IP address (`Framed-IP-Address`) from a table-based pool to a user who passed authentication. Someone with a wrong password never reaches the later handlers. For an example and explanation, see [manual chapter 14](manual/14-isp-recipes.md#ipv4-address-pool-ip-pool). The same approach is used to limit each user's simultaneous connections and traffic ([chapter 14, "Plans and usage control"](manual/14-isp-recipes.md#plans-and-usage-control-usage-control)). For the scenario where users have no password and are authenticated by the line identity from the DHCP Option 82 that the BNG relays, see [chapter 14, "Authenticate by line identity"](manual/14-isp-recipes.md#authenticate-by-line-identity-line-identity). For returning a fixed address, route network and IPv6 prefix for each user, see [chapter 14, "Fixed addresses and prefixes"](manual/14-isp-recipes.md#fixed-addresses-and-prefixes-static-ip). For sending different attributes for the same rate plan depending on the BNG vendor, see [chapter 14, "Return rates by BNG"](manual/14-isp-recipes.md#return-rates-by-bng-rate-limit). For allocating the delegated prefix of an IPv6 user dynamically from a pool, see [chapter 14, "IPv6 prefix pool"](manual/14-isp-recipes.md#ipv6-prefix-pool-prefix-pool).

**`first_match` does not check the password for you:** when `sql`, `ldap` or `netlogon` cannot find the name, it returns "ignore" and hands over to the next handler. If a script that only checks policy comes after it, a user that does not exist passes without a password. So put the policy script in `all_accept`, or set `verifies_password` and compare the password yourself.

### 3.3 The `sql` handler

| Field | Default | Description |
| --- | --- | --- |
| `user_query` | (required) | The query that gets the user's password. Exactly one `?` (the user name). The first column of the result is the password |
| `certificate_query` | none | After an EAP-TLS certificate has passed, this query is used to find the user instead. Exactly one `?`. If a row is found, the user is accepted and the first column is not looked at. When it is not set, `user_query` is used, and a NULL, prefix-only or empty first column is rejected (section 2.12) |
| `reply_query` | none | The query that gets the reply attributes. Exactly one `?`. Returns two columns, `(attribute, value)` |
| `default_user` | none | When the user is not found, the password of this account is used for verification (anyone who knows that password passes, so use it with care). On a pass, `account` in the auth record is this account, and `user` is the name that was sent |
| `timeout_ms` | 5000 | Time limit for each query. Must be greater than 0. With `database`, it includes the time spent waiting for a connection. Checking the password has a limit of its own of the same length, which covers a slow hash waiting for its turn and being computed; past it the backend counts as unavailable (section 2.2) |
| `database` | none | The name of a `[databases.<name>]` (section 2.17). The query runs on that MySQL or PostgreSQL database, and the placeholder is still written `?`. When it is not set, the cluster's own database is queried. Startup fails if the name does not exist |

- User not found: ignore. More than one row found: reject and log. A password column that is NULL, an empty string, or only a prefix such as `{clear}`: reject.
- The number of `?` is counted in characters, so do not use `?` inside a string literal in the query.
- For the supported password formats, see section 5.

### 3.4 The `lua` handler

```toml
[[auth.handlers]]
type = "lua"
script = "site_rules.lua"   # resolved against the configuration file's directory
# verifies_password = true  # optional, default false: this script compares User-Password, see EAP-TTLS in section 2.12
```

The script returns `'accept'`, `'reject'`, `'ignore'` (skip) or `'challenge'` (only for RADIUS authentication). For the script API, see section 4. `phase` is `"authentication"`.

### 3.5 The `ldap` handler

| Field | Default | Description |
| --- | --- | --- |
| `urls` | (required) | One or more `ldaps://` or `ldap://` URLs, tried in order |
| `bind_dn` | (required) | The DN of the service account |
| `bind_password` / `bind_password_file` | (one of the two) | The service account password. It cannot be empty, or loading reports an error: an empty password is the same as an anonymous bind |
| `base_dn` | (required) | Where the search starts |
| `filter` | (required) | The search filter, with exactly one `{user}`, for example `(uid={user})` |
| `certificate_filter` | none | After an EAP-TLS certificate has passed, this search filter is used instead, with exactly one `{user}`. When it is not set, this handler ignores certificate logins (section 2.12) |
| `password_check` | `"bind"` | `"bind"`: bind with the DN that was found and the user's password. `"attribute"`: read the password attribute and compare |
| `password_attribute` | none | Required when `password_check = "attribute"`, for example `userPassword` |
| `reply_attributes` | none | Maps reply attribute names to LDAP attribute names, for example `{ "Filter-Id" = "radiusFilterId" }`, in the order written |
| `timeout_ms` | 5000 | Time limit for each URL, from getting a connection to the last operation |
| `ca_file` | none | An extra CA to trust (PEM), resolved against the configuration file's directory |
| `allow_plaintext` | `false` | When `false`, `ldap://` is always upgraded with StartTLS. When `true` and `ldap://` is used, startup, reload and `check` all warn that the connection is not encrypted |
| `pool_size` | 4 | Upper limit of connections kept for each URL |
| `required_groups` | none | The user must belong to one of these groups, otherwise reject |
| `group_attribute` | `memberOf` | The attribute of the user entry that the groups are read from |
| `group_base_dn`, `group_filter` | none | When both are set, a search is used instead. `group_filter` has one `{dn}`, for example `(member={dn})`. Cannot be set together with `group_attribute` |
| `group_reply` | none | Reply attributes to add when the user belongs to a group, see below |

Behavior:
- `{user}` is escaped according to RFC 4515, so a user name cannot change the structure of the search filter.
- Not found: ignore. More than one entry: reject and log.
- `bind` mode accepts only PAP. An empty password is always rejected (so it is not treated as an anonymous login). `invalidCredentials` is an ordinary reject. Other bind errors are treated as server errors.
- `attribute` mode supports PAP, CHAP when the password is stored in clear text or as `{clear}`, and also MS-CHAPv2 and MS-CHAPv1 (sections 5.1 and 5.2).
- The next URL is tried only after a connection failure, a dropped connection or a timeout. A URL that cannot be reached is moved to the end for 30 seconds, then tried once more (an error reply from the server does not count as unreachable; this starts over after a reload). This covers a connection that drops during the service account bind, the search or the user's bind. An error returned by the server, for example a wrong service account password or a failed search, is always rejected and logged. It is not ignored, and the next URL is not tried.
- **When the directory cannot be used, there is no reply, and it does not count as a failure.** This covers: every URL unreachable or timed out, the service account bind rejected or failing, an error in the search (including the group search), and a timeout while comparing the stored password with `password_check = "attribute"`. The result is still a reject and is logged as usual, but it is marked "unavailable". RADIUS does not reply (counted in `rradius_radius_discarded_total{reason="database"}`), TACACS+ replies ERROR, and it does not count towards `[auth.lockout]` (section 3.8). When the server rejects the user's own bind (a wrong password, or a code other than invalidCredentials such as a disabled account; but busy, unavailable and serverDown, which mean the server cannot handle the request, count as a directory failure), when the user is not found, or when the user is not in `required_groups`, that is the answer for this user. It is rejected as usual and counted as a failure. When a single server has a directory failure, the NAS sees a timeout, not an Access-Reject.
- Certificates are always verified (the CAs the system trusts plus `ca_file`).
- Connections are reused between authentications: before a connection is taken, it is checked to be still open, and the service account binds again within half of `timeout_ms`. A connection that errors or times out is discarded and replaced by a new one.

```toml
[[auth.handlers]]
type = "ldap"
urls = ["ldaps://dc1.example.com", "ldaps://dc2.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
reply_attributes = { "Filter-Id" = "radiusFilterId" }
```

**Groups:** limit logins by group, or give different attributes:

```toml
[[auth.handlers]]
type = "ldap"
# ...
required_groups = ["cn=vpn,ou=groups,dc=example,dc=com"]

[[auth.handlers.group_reply]]
group = "cn=staff,ou=groups,dc=example,dc=com"
attributes = { "Filter-Id" = "staff" }
```

- **Not being in the group and a wrong password get the same reply,** so an outsider cannot tell them apart. The decision is made after the password has been verified.
- **`group_filter` is not a restriction by itself.** If neither `required_groups` nor `group_reply` is set, setting `group_attribute`, `group_base_dn` or `group_filter` fails at startup, because no query would ever be made.
- For a directory without `memberOf`, use `group_base_dn` and `group_filter`. The search runs with the service account before the user's login, so each authentication makes one more search. On a local OpenLDAP, a debug build measured each authentication going from about 603 microseconds to about 662 microseconds.
- To expand nested groups in AD, you can write `group_filter = "(member:1.2.840.113556.1.4.1941:={dn})"`. `group_attribute` looks only at direct membership on the entry. This form has not been tested on AD.
- **DN comparison is not case-sensitive (ASCII only) and ignores spaces next to commas,** but otherwise it must be written the same way as the directory returns it. If the writing differs, there is no match, and the result is a reject or no attributes added. It never gives extra permissions.
- A failed group search is treated as a server error: reject and log.
- `group_reply` is added after `reply_attributes` in the configured order, and several entries can match at the same time. If an attribute name or value is wrong, that authentication is rejected and `RADIUS response attribute error` is logged. `check` finds this kind of error early.

### 3.6 Normalizing user names `username`

The name a device sends often carries a domain or differs in case, for example `alice@corp.example` or `CORP\Alice`. Each chain can set how the name used to look up the account is computed from the received name:

```toml
[auth.username]          # default chain; a named chain uses [auth.chains.<name>.username]
strip_realm = "suffix"   # default none
lowercase = true         # default false
allowed = "[a-z0-9._-]+" # optional
```

| `strip_realm` | What is removed | Result for `CORP\Alice@Corp` |
| --- | --- | --- |
| `none` | Nothing | `CORP\Alice@Corp` |
| `suffix` | Everything from the last `@` | `CORP\Alice` |
| `prefix` | Everything up to the last backslash | `Alice@Corp` |
| `both` | `prefix` first, then `suffix` | `Alice` |

- **The processing order is fixed:** strip the realm, lowercase, then check `allowed`. So `allowed` only has to consider the processed name, and does not need to list the realm or upper case.
- **`allowed` must match the whole name,** so you do not need to add `^` or `$` yourself. `[a-z]+` does not let `a'; --` through.
- **When the result is empty or does not match `allowed`,** the whole chain rejects at once and runs no handler, for `first_match` and `all_accept` alike. The log records only the client and the reason, not the name, because a name can contain control characters.
- **A request with no name** is passed to the handlers as usual, the same as when this section is not present.
- **Nothing is inherited:** the default chain's settings do not apply to named chains. A chain without this section uses the received name as it is.
- `[auth.username]` can be set only when there is an `[[auth.handlers]]`, the same as `policy`. Startup fails if the settings are wrong. On SIGHUP it takes effect together with the chain, with no restart.

How each handler uses the names:

| Handler | Uses the processed name | Uses the received name |
| --- | --- | --- |
| `sql` | `user_query`, `reply_query` | Verifying the password |
| `ldap` | The search in `filter` | Verifying the password in `attribute` mode |
| `lua` | The global variable `auth_user` | `get_attr('User-Name')` |
| `netlogon` | The account that logs on to the DC (with `domain\` removed as well) | Computing the challenge hash |

- **MS-CHAPv2 always verifies with the received name,** because the client computed its response with it (see section 5.1). The attributes in the response and `get_attr` do not change either.
- **These settings are not applied to:** accounting packets, TACACS+ authorization, and the top-level script when there is no chain. They have to handle names themselves, and `auth_user` is `nil` in these scripts.
- **Before turning on `lowercase`, make the data lower case.** Otherwise `Alice` in the table is not found. Accounts that differed only in case would also point to the same row.
- **Before setting `allowed`, check it in a test environment.** A pattern that is too strict blocks legitimate users.
- There is no Unicode normalization (NFC and so on), and no chain selection by realm.

### 3.7 The `netlogon` handler (MS-CHAPv2 for Active Directory)

AD does not give out the NT hash over LDAP, so the `sql` and `ldap` handlers cannot verify MS-CHAPv2 for an AD account. The `netlogon` handler gives the MS-CHAPv2 response to a domain controller (DC) to verify. It sets up a Netlogon secure channel (AES) with the DC using this node's computer account, and then sends the challenge hash and the NT-Response with a network logon (`NetrLogonSamLogonEx`). When the DC accepts, the user session key it returns is the "hash of the NT hash". `MS-CHAP2-Success` and the MPPE keys are computed from it, and they are the same as what the `sql` handler computes from the password. So PEAP-MSCHAPv2, RADIUS MS-CHAPv2 and TACACS+ MS-CHAPv2 can all log in with AD accounts. The server does not have to join the domain, and Samba is not needed.

```toml
[[auth.handlers]]
type = "netlogon"
domain = "CORP"                               # NetBIOS domain name
servers = ["dc1.corp.example", "dc2.corp.example"]
computer_name = "RRADIUS1"                    # this node's computer account, without $
machine_password_file = "netlogon-rradius1.secret"
required_groups = ["S-1-5-21-1004336348-1177238915-682003330-1110"]

[[auth.handlers.group_reply]]
group = "S-1-5-21-1004336348-1177238915-682003330-1111"
attributes = { "Filter-Id" = "staff" }
```

| Field | Default | Description |
| --- | --- | --- |
| `domain` | (required) | The NetBIOS name of the domain, for example `CORP`, not the DNS name |
| `servers` | (required) | The DNS or NetBIOS names of the DCs, used in order, at least one. IP addresses cannot be used: the logon request must carry the DC's own name, and a Windows DC that receives an address replies `STATUS_INVALID_COMPUTER_NAME`. The node must be able to resolve these names (usually with AD's DNS) |
| `port` | none | The DC's fixed Netlogon port. When it is not set, the endpoint mapper on port 135 is asked first, and the port it gives is remembered. If the DC restarts with a different port and the old port cannot be reached, it is asked again |
| `computer_name` | (required) | This node's computer account, without the trailing `$`, at most 15 characters |
| `machine_password_file` | (required) | The password file of the computer account, resolved against the configuration file's directory. A trailing newline is removed. Only the NT hash is kept after loading |
| `timeout_ms` | 2000 | Time limit for the connection, for setting up the secure channel, and for one logon, each separately. Must be greater than 0 |
| `connections` | 4 | The most connections kept for each DC. Must be greater than 0 |
| `required_groups` | none | A list of SIDs. The user must belong to at least one of them to be accepted, otherwise reject |
| `group_reply` | none | Reply attributes to add when the user belongs to a group with that SID, in the configured order. Several can match at the same time |

**One computer account for each node.** A DC keeps only one secure channel for each computer account, so two nodes that share an account break each other's channel. In a cluster, each node's configuration needs its own `computer_name` and password file. For how to create them, see manual chapter 08, "Prepare the computer account".

How DC replies are handled:

| DC reply | Result |
| --- | --- |
| Success | Accept, with `MS-CHAP2-Success`, `MS-MPPE-Send-Key`, `MS-MPPE-Recv-Key` and the matching `group_reply` |
| `STATUS_NO_SUCH_USER` | Ignore, hand over to the next handler |
| `STATUS_WRONG_PASSWORD`, `STATUS_ACCOUNT_DISABLED`, `STATUS_ACCOUNT_LOCKED_OUT`, `STATUS_PASSWORD_EXPIRED`, `STATUS_PASSWORD_MUST_CHANGE`, `STATUS_ACCOUNT_EXPIRED`, `STATUS_INVALID_LOGON_HOURS`, `STATUS_INVALID_WORKSTATION` | Reject, the status name is logged at INFO |
| Other failures | Reject, the hexadecimal code is logged at INFO |
| Logon succeeded as a guest account | Reject. The request itself also asks the DC not to fall back to the guest account |
| The reply is not authoritative (`Authoritative` is false) | Try the next DC |

- **Only MS-CHAPv2 is handled.** PAP, CHAP and certificate logins are always ignored, without connecting to the DC. To verify PAP against AD, use the `bind` mode of the `ldap` handler.
- **Groups are given as SIDs,** for example `S-1-5-21-…-1110`. The comparison uses the groups the DC gives in the logon reply: the domain SID plus each group RID, and the extra SIDs (other domains, universal groups). No separate LDAP query is needed. You can look up a SID with PowerShell's `Get-ADGroup staff | Select SID`. Not being in the group and a wrong password get the same reply.
- **Account name:** the logon uses the name processed by the chain (section 3.6), with any `domain\` prefix removed. The domain is always the `domain` setting. The challenge hash uses the received name, the same as local verification.
- **Connection reuse and DC failover:** the connections to each DC share one secure channel and are reused between requests. For one request, a DC gets at most one try on an idle connection and two on new ones; a failure on the idle connection does not count against the new ones. A dropped connection by itself does not rebuild the channel. When the DC says the channel is no longer valid (`STATUS_ACCESS_DENIED`, an RPC fault, or a seal or bind that is refused), the channel is discarded; if the request still has a new connection to try, that one sets the channel up again, and it is the only further try. This is how the first request that finds a restarted DC serving again is answered. If no try succeeds, the next DC is used. A connection that does not reply within `timeout_ms` is closed. If it was a connection reused after sitting idle (it may have been dropped silently by a firewall or NAT), the other idle connections are closed too and the request goes on to a new connection. If a new connection does not reply either, the next DC is used.
- **When no DC can be used,** it is the same as when the `sql` handler's database cannot be used: RADIUS does not reply and it is counted in `rradius_radius_discarded_total{reason="database"}`, TACACS+ replies ERROR so that the device retransmits or switches server (section 2.2, "When the database is unavailable"), and an ERROR is logged. Setup problems such as a wrong computer account password or a DC that does not support AES also log an ERROR.
- **Secrets stay out of the log:** the computer account password, the session key, the user session key, the NT-Response and the MPPE keys never appear at any log level, and they are cleared after use.
- `check` and startup only check the configuration and the password file, and do not connect to a DC. The channel is built at the first request.
- **A Samba AD DC must allow MS-CHAPv2:** Samba's default `ntlm auth = ntlmv2-only` does not accept MS-CHAPv2, and replies `STATUS_WRONG_PASSWORD` even when the password is right. Add `ntlm auth = mschapv2-and-ntlmv2-only` to `[global]` in `smb.conf` on every DC and restart.
- **Performance:** measured on one Mac against a Samba 4.17 DC in a container, with `connections` at 1, 4 and 16 the rate was about 1600, 3700 and 8300 logons per second. The bottleneck is the DC. If the DC can take it, you can raise `connections`.

Metrics (section 2.8):

| Item | Description |
| --- | --- |
| `rradius_netlogon_logons_total{result="accept\|reject\|no_such_user\|unavailable"}` | MS-CHAPv2 logons handed to a DC for verification |
| `rradius_netlogon_channel_setups_total{result="ok\|failed"}` | The number of secure channel setups. A `failed` count that keeps rising is usually a wrong computer account password |
| `rradius_netlogon_open_connections` | Netlogon connections that are open now |

**Limits:**
- There is no automatic domain join, and the computer account password is not rotated. The computer account must be created in advance and given a password (manual chapter 08, "Prepare the computer account"). If the password is later reset in AD, update the password file at the same time and reload.
- Only the AES secure channel is supported. Old DCs with AES disabled (before Windows Server 2008) are not supported.
- It has been tested on a Samba 4.17 AD DC with `docker/interop-netlogon/run.sh` (PEAP-MSCHAPv2 and MPPE keys, the `DOMAIN\` prefix, various rejects, groups, rebuilding the channel after a DC restart, a wrong computer account password). It has also been tested on a Windows Server 2022 DC (PEAP-MSCHAPv2 and MPPE keys, the `DOMAIN\` prefix, groups, a wrong password, a disabled account, not in the group, a user that does not exist).

### 3.8 Limiting failed logins `[auth.lockout]`

The core does not limit the number of password guesses by itself. Writing `[auth.lockout]` turns it on. Without it, nothing changes.

```toml
[auth.lockout]
max_failures = 10             # failures within the window that cause a lockout, default 10
window_seconds = 300          # how long counting lasts from the first failure, default 300
lock_seconds = 300            # how long the lockout lasts, default 300
max_entries = 100000          # the most keys each node remembers, default 100000
# account_max_failures = 50   # optional: total failures for one account from all origins (must be greater than max_failures)
# max_in_flight = 50          # optional: logins waiting for a decision for one key at the same time, default 5 times max_failures
```

- **Key:** the same account (lower case) plus the origin device: `Calling-Station-Id` for RADIUS, and `rem_addr` for TACACS+ (empty if absent). When the same key reaches `max_failures` failures within the window, logins are rejected at once for `lock_seconds`, even with the right password, and the password is not even checked. Another device is not affected. The lockout is not extended while it lasts.
- **Logins in progress:** a login takes a slot before the decision and gives it back after. When the logins of one key waiting for a decision reach `max_in_flight` (default 5 times `max_failures`, must be greater than 0), a further login is rejected at once and counted in `rradius_auth_lockout_rejected_total`. Logins that are waiting do not use up the failure allowance, so when many devices reconnect at the same time with correct passwords and no failure counted, none are blocked or locked. When `account_max_failures` is set, the number of slots for an account from all origins is `max_in_flight × account_max_failures ÷ max_failures` (rounded up). Failures counted in the window take slots too: logins waiting plus failures counted must stay below `max_in_flight` (below `max_failures`, if that is larger), except that a key that is not locked and has no login waiting always gets one slot. The trade-off: wrong logins are let through before enough of them have failed, so, whether they arrive together or one after another, up to `max_in_flight − max_failures` extra guesses (by default 4 times `max_failures`) can be made in a window before the lock. To be stricter, set `max_in_flight` lower, but not below `max_failures`: a value below `max_failures` only limits the number in progress, does not make the lock start sooner, and blocks correct logins that reconnect at the same time. If you upgrade without changing the configuration, the concurrent allowance is the default 5 times, which is wider than in earlier versions.
- **Only real rejects are counted:** a database, LDAP or domain controller that is down, and a password check that times out (`unavailable`), are not counted. EAP-TLS certificate logins and EAP-AKA'/SIM are not counted. A successful login resets this key's count to zero. A name that does not exist is counted too, so the lockout does not reveal which accounts exist.
- **Scope:** logins covered by an auth chain: RADIUS PAP, CHAP and MS-CHAP, the inner method of PEAP/TTLS, and TACACS+ authentication (across connections, so a reconnect does not reset it). **Not covered:** logins with no auth chain that are verified directly by `[lua].script`, and the second-factor flow of TACACS+.
- **Each node counts on its own,** in memory: a restart resets it to zero, and in a cluster an attacker can spread across nodes, so the real limit is about the number of nodes times the setting. A reload of the configuration keeps the counts. After `[auth.lockout]` is removed and the configuration is reloaded, the counts are cleared.
- **The origin field is filled in by the sender.** Someone who holds the shared secret can use a different fake `Calling-Station-Id` each time and get around the "account plus origin" lock. To stop that, set `account_max_failures`: when one account reaches this many failures from all origins, it is locked for all origins. The cost is that someone guessing passwords can also use this to lock a victim's account, so it is not set by default.
- **The count table has a limit.** When it is full, expired entries are dropped first, then the oldest unlocked ones. Locked entries are not pushed out. When all of them are locked, a new key is not tracked and is counted in `rradius_auth_lockout_untracked_total`, and at most one line a minute is logged: `the table of N keys is full of locks`.
- **Logging and statistics:** each new lockout logs one INFO line: `auth lockout: client <name>: account "<account>" from "<origin>" is locked (this origin)`. The text in brackets can also be `all origins`. It does not contain the password. Metrics: `rradius_auth_lockout_rejected_total` (logins rejected because of a lockout), `rradius_auth_lockouts_total` (lockouts started), `rradius_auth_lockout_untracked_total`. When it is not set, none of these three is output.

## 4. Lua scripts

### 4.1 Runtime environment

- Scripts run on Luau (based on Lua 5.1; it supports syntax such as `+=` and does not support `goto`).
- All scripts (the top-level script and the `lua` handlers) are compiled once at startup. A syntax error makes startup fail and names the file.
- Sandbox: the built-in libraries are read-only. There is no `io`, `os.execute`, `require`, `loadstring`, `getfenv` or `setfenv`; calling them raises an error, and you only find out when the script reaches that line. `string`, `table`, `math`, `os.time`, `os.clock` and similar are available.
- Each request has its own global variables, which are not shared between requests. Write data that must survive across requests to the database.
- **Memory limit:** each VM uses at most `[lua].max_memory_mb`. When a script that needs to allocate goes over it, the script gets `memory error: not enough memory`. It is handled like any other script error and the process does not exit; other scripts running on the same VM at that moment may hit it too. When the VM's usage is above half of the limit at the end of a script, a full collection runs. This blocks one worker thread for the duration, and no new request is assigned to that VM. Another one runs only after usage has grown by a quarter of the limit since the last collection, or exceeds seven eighths of the limit. **Collection is incremental:** if a script repeatedly builds very large strings, it can fail even though it keeps none of them, because collection cannot keep up. Measured with the default 256 MiB, building an 8 MiB string 400 times works and 16 MiB fails. The limit does not cover memory outside Lua, such as the results of a pending database query.
- A script that runs longer than `[lua].max_execution_ms` in one request is aborted and treated as a reject. Time spent waiting for the database does not count. The time is checked only every 64 loop back-edges or function calls, and on every one after the limit is passed. `pcall` can catch this error, but the next loop iteration or call raises it again, and if the script catches it and still returns `accept`, that counts as a failure too. So at most 64 more steps run before the script is aborted, and a loop with a slow body is stopped a little later. A script that runs for a long time yields periodically and does not block other requests.
- When a script fails (including a timeout), the request is treated as a reject. But when the failure or reject is because a database is unavailable (`sql_query` or `sql_execute` timed out or found no leader, or `db_query` or `db_execute` could not use an external database, even if the script caught the error with `pcall`), an authentication request gets no response and TACACS+ replies ERROR (section 2.2, "When the database is unavailable"). The error is logged as `Lua script error (<protocol>/<phase>): <message>`, where the location in the message is the script path and line number, for example `/etc/rradius/auth.lua:3: attempt to index nil with 'y'`.

### 4.2 Global variables

| Name | Content |
| --- | --- |
| `protocol` | `"radius"` or `"tacacs"` |
| `phase` | `"authentication"`, `"authorization"` or `"accounting"` |
| `client` | The `name` of the matched client, or its `address` string when no name is set |
| `auth_user` | In a `lua` handler in an auth chain, the name used to look up the account (see section 3.6), or `nil` when there is no name; `nil` in every other script |
| `replayed` | Whether this accounting request comes from a replay of the spool (see section 2.11); `false` otherwise |
| `eap_method` | `"tls"`, `"peap"` or `"ttls"` for an EAP authorization request (see section 2.12); `nil` otherwise |
| `tls_subject` | When `eap_method` is `"tls"`, the subject of the certificate, for example `CN=alice, O=Example`, with values escaped as in RFC 4514, in the same order as in the certificate. All string types are decoded to text. A value that is not text or cannot be decoded is written as `#` followed by the whole value in DER hexadecimal, for example `O=#1c0400000061`. A field that is not in the name table uses its OID, for example `2.5.4.97=...`. `nil` otherwise |
| `tls_subject_attrs` | In the same case, a table split by field, with values not escaped, for example `tls_subject_attrs.OU[1]`. A value that is not text also starts with `#`, and cannot be told apart from a text value that itself starts with `#`, so do not rely on such values when matching. `nil` otherwise |

### 4.3 Functions

| Function | Description |
| --- | --- |
| `get_attr(name)` | Gets a request attribute; returns `nil` if it does not exist. If there are several attributes with that name, returns the first |
| `get_attrs(name)` | Gets an array of all attributes with that name, in the same order as in the packet; returns an empty table if there are none |
| `set_attr(name, value)` | Adds a reply attribute (a string or a number), kept in the order of the calls. A tagged attribute can take a tag after the name, for example `Tunnel-Type:1` |
| `sql_query(sql [, params])` | Reads the local database replica and returns an array of rows, each row a table of "column name → value"; read-only |
| `sql_execute(sql [, params])` | Writes through the cluster and replicates, and returns the number of affected rows |
| `db_query(name, sql [, params])` | Runs a query on `[databases.<name>]` (section 2.17) and returns the same format as `sql_query`; it can also write, but then it returns rows |
| `db_execute(name, sql [, params])` | Runs a write on `[databases.<name>]` and returns the number of affected rows |
| `now_unix()` | The current Unix time (seconds) |
| `totp_verify(secret, code)` | Verifies a TOTP code as in RFC 6238 (HMAC-SHA-1, 6 digits, 30-second steps, one step before and after allowed). Returns the time step on a match and `nil` otherwise; the script fails if `secret` is not Base32 |
| `password_verify(stored, given)` | Checks whether `given` is the password for the stored value `stored`, the same way the auth chain checks a PAP password, and returns `true` or `false`. It supports all stored formats (plain text, `{clear}`, `{crypt}`, `{argon2}`, `{SHA}`, `{nthash}` and so on, see section 5). Slow hashes are computed outside the request thread. Queueing and checking slow hashes have no time limit and do not count toward `max_execution_ms`. The cost limits of `[auth.password_limits]` apply, a hash without a prefix is not treated as a password, the warning for a digest without a prefix applies, and the check counts toward `rradius_password_checks_total`. `stored` must be text. It returns `false`, without making the script fail, when `stored` or `given` is `nil`, when `stored` is empty after the prefix is removed, or when the rules above reject it. The warning names the request's `User-Name` and does not contain the stored value or the password. **No failure counting, no lockout:** a login verified directly by a script is not protected by `[auth.lockout]`; to limit guessing, use an `[auth]` auth chain instead |
| `random_token()` | Returns a random value of 32 hexadecimal characters, which can be used to build a `State` |
| `wpa_pmk(passphrase, ssid)` | Returns the WPA2 PMK (PBKDF2-HMAC-SHA1, 4096 rounds, 32 octets) as 64 lowercase hexadecimal characters. `passphrase` must be 8–63 printable ASCII characters (or a PSK of 64 hexadecimal characters, returned as it is) and `ssid` must be 1–32 octets, otherwise the script fails; the message does not contain the passphrase. Each call runs 4096 rounds and takes a few milliseconds, see section 2.15 |
| `send_disconnect`, `send_coa` | Ask the NAS to disconnect a session or change its authorization (RFC 5176). They need rradius-enterprise; in rradiusd, calling them makes the script fail |
| `log(...)` | Writes one log line, see section 4.9 |
| `http_request(options)` | Calls an HTTP API on the allowlist. Only available when `[lua.http]` is set, see section 4.11 |
| `url_encode(text)` | Encodes a value as one part of a URL. Only available when `[lua.http]` is set, see section 4.11 |

Database value conversion: INTEGER, REAL → number; TEXT, BLOB → string; NULL → `nil`.

Do not use `RETURNING` with `sql_execute`: the statement writes, but the call fails because it returns rows. When you need the value after the write, read it with `sql_query` after writing.

`sql_execute` rejects constructs whose result can differ between nodes, for example `datetime()`, `random()` and `CURRENT_TIMESTAMP`, even when the name is quoted or has a comment in the middle. These words are fine as column or table names, but a name followed by a parenthesis counts as a call. The only exceptions are table names after `INSERT INTO`, `CREATE TABLE`, `REFERENCES` and `WITH`, so `CREATE INDEX i ON "date"(a)` is also rejected; put that in a migration. When you need the time, get it with `now_unix()` and write it:

```lua
sql_execute("INSERT INTO sessions VALUES (?, ?)", {id, math.floor(now_unix())})
```

**Parameters:** always pass values that come from the request in `params`; never join them into the SQL string. These values are controlled by the device or the user, and joining them into the string is SQL injection.

```lua
local rows = sql_query("SELECT plan FROM users WHERE username = ?", {get_attr('User-Name')})
```

- `params` are bound in order to the `?` in the SQL. If the counts differ, the script fails and no SQL is sent. A `?` inside a string, a quoted name or a comment does not count.
- Only `?` is supported, not `?1`, `:name`, `@name` or `$name`.
- A string is bound as TEXT, and a string that is not valid UTF-8 is bound as BLOB. A number with an integer value is bound as INTEGER and any other number as REAL. `true` and `false` are bound as 1 and 0.
- Tables, functions, NaN and infinity make the script fail. A `nil` in the middle of the array fails, and a `nil` at the end is dropped, which fails with a count mismatch only if the SQL still needs that value. **Check values that may be missing first:** `{get_attr('User-Name')}` is an empty array when there is no such attribute, and fails with a count mismatch. To compare with or write NULL, write it in the SQL itself (`IS NULL`, `NULL`).
- Without `params`, or with `nil`, there are no values, so a `?` in the SQL fails. A `?` inside a string, a quoted name or a comment does not count.
- The check above for constructs whose result differs between nodes looks only at the SQL text, not at the parameter values.

**External databases:** the parameter rules of `db_query` and `db_execute` are the same as above, and a count mismatch likewise sends nothing. The script fails if `name` is not in `[databases]`. An external database is not replicated through the cluster, so `db_execute` does not check for constructs such as `now()`. For how failures are handled and the limits on placeholders and value types, see section 2.17.

```lua
local rows = db_query('billing', 'SELECT plan FROM users WHERE username = ?', {get_attr('User-Name')})
db_execute('billing', 'INSERT INTO acct (id, octets) VALUES (?, ?)', {get_attr('Acct-Session-Id'), get_attr('Acct-Output-Octets')})
```

### 4.4 Return values

| Return | Result |
| --- | --- |
| `'accept'` | Accept |
| `'ignore'` | Skip, in a `lua` handler in an auth chain; treated as a reject when used as the top-level script |
| `'challenge'` | For RADIUS authentication, replies Access-Challenge and the chain ends there; treated as a reject for TACACS+ and in other phases |
| Any other value, no return value, or an error | Reject |

**The return value in the accounting phase has a special meaning:** `'accept'` means the packet was recorded, and once the device receives the acknowledgement it does not send it again. Any other result means it was not recorded, so the device resends it, the script runs again, and each run records one line. Therefore:

- When you deliberately do not want to record some packets, return `'accept'`, not `'reject'`; otherwise the device keeps resending until it runs out of retries.
- The script must be safe to run more than once: if it fails halfway, the resend runs it again. Prevent duplicates with a `UNIQUE` index and something like `INSERT OR IGNORE`.
- When the cluster loses its majority, writes time out and no accounting request gets a response until the cluster recovers or the device runs out of retries. When an accounting spool is configured, these requests are written locally first and then answered (see section 2.11).

**Attributes an Access-Challenge can carry:** `Reply-Message`, at most one `State`, vendor attributes, `Idle-Timeout` and `Session-Timeout` (RFC 2865 section 4.4). If it carries any other attribute, it is changed to Access-Reject and the reason is logged. The device sends `State` back unchanged in the next request, and the script reads it with `get_attr('State')`.

**Two-step verification example:** see [`rradius-server/examples/two-step-totp.lua`](../rradius-server/examples/two-step-totp.lua). The tables it needs are at the top of the file, and the tests run this script directly. The password column stores a prefixed hash (for example `{crypt}$6$...` or `{argon2}...`), checked with `password_verify`. The password step is deliberately not rate limited (otherwise anyone who knows an account name could lock the victim out). To limit it, check the password in the `[auth]` auth chain, let `[auth.lockout]` do the counting, and have the script handle only the code. The first request checks the password and returns `'challenge'`; the second request brings back `State`, and its `User-Password` is the code. The example handles the following, and a script you write yourself must handle them too:

- **A challenge can be used once and expires:** `State` is stored in the database, and the number of rows `DELETE` returns tells you whether this request got it.
- **A code can be used once:** store the time step that `totp_verify` returns and accept only a newer time step. This is not possible if you only return right or wrong, and RFC 6238 also requires protection against reuse.
- **Limit the number of guesses:** a 6-digit code can be guessed by brute force; the example locks the account for 5 minutes after 5 wrong guesses in a row. Only someone who knows the password can trigger the lock, and the number of guesses at the password itself is not limited, so handle that separately.
- **An account without a password:** reject `NULL` or an empty string before comparing, otherwise a request without a password matches.
- **Always use parameters:** `User-Name` and `State` are both externally controlled and must be passed in `params`.
- **Several nodes:** the second request can reach another node, whose database replica may lag slightly and not find the row that was just written, and a lock may take effect a little later. The `DELETE` and `UPDATE` the example uses go through the cluster and are not affected.

**Push approval example:** see [`rradius-server/examples/push-approval.lua`](../rradius-server/examples/push-approval.lua). After the password passes, it uses `http_request` (section 4.11) to ask an external push service to send a notification to the user's phone, then polls until the request is approved, denied or times out, and accepts only on approval. It is a `lua` handler in a `policy = "all_accept"` chain, placed after the handler that checks the password (`sql`, `ldap`), so a person with the wrong password never gets a push. The top of the file describes the service interface it assumes; replace the URLs and fields with those of the service you use. The tests run this script against a fake service and cover approval, denial and timeout. Note:

- **The NAS's RADIUS timeout must be longer than `WAIT_SECONDS`** (30 seconds in the example), otherwise the NAS gives up or resends while the user is still looking at the phone. For pam_radius this is the timeout on the server line in `/etc/pam_radius_auth.conf`.
- **A login that is waiting holds a slot of `max_auth_inflight`** (section 2.3) until it ends. When many people wait at the same time they push other logins out, so raise the value according to how many wait at once.
- **`timeout_ms` of `[lua.http]` must be greater than `POLL_SECONDS`,** because on each poll the example lets the service wait up to that long before answering. `max_execution_ms` does not include the wait.

### 4.5 RADIUS attributes

**Dynamic VLAN:** `Tunnel-Type`, `Tunnel-Medium-Type` and `Tunnel-Private-Group-Id` are tagged attributes, and `Tunnel-Password` is encrypted automatically as in RFC 2868. The reply column names of the `sql` handler can use the same notation.

```lua
set_attr('Tunnel-Type:1', 'VLAN')
set_attr('Tunnel-Medium-Type:1', 'IEEE-802')
set_attr('Tunnel-Private-Group-Id:1', '100')
```

**Address types:** `set_attr` (and `reply_query` of the `sql` handler) and `get_attr` all use the same text format:

```lua
set_attr('Framed-IPv6-Prefix', '2001:db8::/64')
set_attr('Delegated-IPv6-Prefix', '2001:db8:1::/48')
set_attr('Framed-Interface-Id', '0011:2233:4455:6677')
```

| Type | Text | Note |
| --- | --- | --- |
| `ipaddr` | `192.0.2.1` | |
| `ipv6addr` | `2001:db8::1` | |
| `ipv6prefix` | `2001:db8::/64` | The bits after the prefix must be 0; `2001:db8::1/64` makes the script fail. Always send the full 18 octets |
| `ipv4prefix` | `192.0.2.0/24` | The bits after the prefix must be 0 |
| `ifid` | `0011:2233:4455:6677` | Four groups, each exactly 4 hexadecimal characters |
| `ether` | `00:11:22:aa:bb:cc` | Six groups, each exactly 2 hexadecimal characters; only `:` is accepted as the separator |
| `combo-ip` | An IPv4 or IPv6 address | |

Hexadecimal characters in the input are case-insensitive, and `get_attr` always returns lowercase. With a wrong format, or a number, `set_attr` makes the script fail and the error message names the attribute.

**When `reply_query` finds a value in the wrong format,** there is no script that can fail, so the Access-Accept is changed to an Access-Reject even though the password is correct, and `RADIUS response attribute error` is logged. Check the values in the table before you go live.

**Upgrade note: `get_attr` on an `ipv6addr` attribute (for example `NAS-IPv6-Address`) used to return 16 raw bytes and now returns text.** A script that treats it as bytes must be changed.

**Extended attributes and TLVs:** the `extended`, `long-extended`, `evs` and `tlv` types of RFC 6929 are all read and written by the name of the attribute inside them, and the `format=` of the dictionary takes effect, so vendor attributes such as WiMAX can be used.

```lua
local status = get_attr('Frag-Status')
set_attr('Response-Length', 4096)
set_attr('IPv6-6rd-IPv4MaskLen', 8)
set_attr('IPv6-6rd-BR-IPv4-Address', '192.0.2.1')
set_attr('WiMAX-Release', '2.0')
```

- **Only adjacent ones are merged.** Attributes of the same `tlv` that are set one after another go into the same attribute; for example the 6rd above is one attribute 173. Another attribute in between splits them. To send two separate TLVs, the only way for now is to put another attribute in between. When something does not fit, a new TLV starts at the level where it no longer fits, all the way up to a new attribute, and a single value is never cut in two, so a value that did not fail in `set_attr` also fits when sent.
- **Length.** `long-extended` and vendor attributes with a continuation (`format=1,1,c`) are fragmented automatically and reassembled when received. For other types, the script fails if the value does not fit. The length field of a TLV is a single octet, so in a format that fragments, a TLV value is at most 253 octets. A format that does not fragment must also fit in the attribute itself, which is normally at most 251, and at most 245 for a vendor attribute in the default format. Each level of nesting takes 2 octets less. The whole response is still limited to 4096 octets.
- **The container itself cannot be set.** `set_attr('IPv6-6rd-Configuration', ...)` and `set_attr('Extended-Attribute-1', ...)` make the script fail.
- **An invalid attribute is ignored.** When the fragments are incomplete or a TLV length is wrong, the script cannot see anything inside that attribute, and the rest of the request is processed as usual (RFC 6929 section 2.8). A TLV type that the dictionary does not define is skipped.
- **Access-Challenge:** attributes of type `evs` cannot be included, like other extended attributes; a vendor attribute (Vendor-Specific) can.
- **Encrypted attributes in a TLV:** in the dictionary there are only `WiMAX-hDHCP-DHCP-RK` and `WiMAX-vDHCP-DHCP-RK`. They cannot be set with `set_attr`, and `get_attr` returns the value undecrypted.
- **Upgrade notes:**
  - `format=` used to have no effect. Vendor attributes with `format=2,1`, `2,2` or `4,0` (for example Lucent, Starent, USR) used to be read wrongly, and only now are the correct attributes read.
  - Container names such as `get_attr('WiMAX-Capability')` and `get_attr('Extended-Attribute-1')` used to return raw bytes and now return `nil`; use the names of the attributes inside.
  - Unrecognized options or extra fields on `VENDOR` and `BEGIN-VENDOR` in the dictionary used to be ignored and now make loading fail.

`encrypt=` in the dictionary can only be 1, 2 or 3, and anything else is reported as an error at load time; the options can be separated by commas or spaces. `set_attr` supports `encrypt=1` (the User-Password method of RFC 2865 section 5.2, with the value zero-padded to a multiple of 16, 1 to 128 bytes, and not combined with `has_tag`) and `encrypt=2`. It does not support attributes with `encrypt=3`, nor `abinary`, nor definitions with `has_tag` or `encrypt` inside extended attributes and TLVs. All of these make the script fail.

- `get_attr` uses the attribute names of the dictionary, for example `User-Name` and `NAS-IP-Address`; Vendor-Specific attributes are split into the individual vendor attributes. Attributes that are not in the dictionary do not appear.
- Types: `integer`, `date`, `byte`, `short`, `signed`, `integer64` → number; the address types in the table above → text, or a raw byte string when the value has the wrong length; everything else → raw byte string.
- `User-Password` is decrypted to plain text first.
- The name given to `set_attr` must be in the dictionary, and the value can be a value name defined in the dictionary, for example `set_attr('Service-Type', 'Framed-User')`. If the name does not exist, the script fails and the request is rejected.
- An accounting script sends an Accounting-Response only when it returns `'accept'`. When the script fails (including a query timeout or exceeding the execution limit) or returns any other value, there is no response and the device resends (RFC 2866 section 2: do not respond when the packet cannot be recorded). The exception is when an accounting spool is set: a failure caused by the database is written locally first and then answered (section 2.11).

### 4.6 TACACS+ attributes

- Common fields: `user`, `port`, `rem_addr`.
- Authentication: there are also `data` (the password for PAP and ASCII; for CHAP and MS-CHAPv2, the PPP id, the challenge and the response, see below) and `authen_type` (a number: 1 for ASCII, 2 for PAP, 3 for CHAP, 6 for MS-CHAPv2).
- **ASCII interactive login** (RFC 8907): when START carries no account name, the server replies GETUSER and then GETPASS, which does not echo the input. Authentication runs the auth chain or the script only after both the account name and the password are obtained, which are read from `user` and `data` just as with PAP. An empty account name is asked for at most three times; each connection can have at most 8 unfinished logins, and each must finish within 60 seconds.
- **Length limits:** a TACACS+ password (the `data` of PAP, the password of an ASCII login, and the code of a second factor) is at most 128 bytes, and the account name of an ASCII login is at most 255 bytes. The password limit is the same as for the RADIUS `User-Password` (16 to 128 bytes, a multiple of 16). A longer one gets FAIL and does not reach the auth chain or the script, so it does not count toward `[auth.lockout]`; one INFO line is logged (client, account name and length, not the content), and the ASCII login session ends. The limit is fixed and cannot be configured. A long passphrase over 128 bytes cannot log in through TACACS+.
- **Upgrade note:** the START of an ASCII LOGIN used to be handed directly to the script; now the server asks for the account name and password first. A script must read the password from `data`.
- The START of authentication also carries `priv_lvl` (0–15) and `authen_service` (a number: 1 for a normal login, 2 for enable).
- **An enable request** (the device's `enable` command, with `authen_service` 2) is decided by `[tacacs] enable`:
  - `"deny"` (the default): replies FAIL at once, does not ask for a password, does not run the chain or the script, and logs `TACACS+ enable request refused`. A device that receives FAIL does not fall back to the next method (for example a local enable password), so do not put rradius in the enable method list on the device, or use `"login"` instead.
  - `"login"`: like a login, the user's own password is checked through the chain or the script. A `lua` handler in the chain or the top-level script can use `authen_service` and `priv_lvl` to limit who may enable, for example to let only certain roles through; without such a check, **everyone who can log in can enable**.
- **Upgrade note (enable):** an enable request used to be treated as a normal login, and anyone who could log in could enable with their own password. Now it is denied by default. If you need the old behavior, set `enable = "login"`, and limit it in the script.
- **Only the LOGIN action is supported.** Changing a password (CHPASS), SENDPASS and SENDAUTH always get FAIL, with no prompt and no chain or script run (RFC 8907: an unsupported option gets FAIL). Changing a password used to be treated as a login and answered with PASS, and the device would think the password had been changed.
- Authorization and accounting: each `key=value` or `key*value` argument uses `key` as the name, for example `get_attr('cmd')`. There are also the packet header fields `priv_lvl` (the user's current privilege level, 0–15), `authen_method`, `authen_type` and `authen_service` (all numbers). The names use an underscore, to tell them apart from arguments such as `priv-lvl` that the device sends; the rules of an authorize group match only arguments and do not look at these fields.
- **CHAP, MS-CHAP (v1) and MS-CHAPv2 logins** (RFC 8907 sections 5.4.2.3, 5.4.2.4 and 5.4.2.5, for example a device using PPP dial-up) **need an auth chain** (section 3). The chain does the check, and a single START gets PASS or FAIL directly, using the same handlers as RADIUS CHAP, MS-CHAP and MS-CHAPv2:
  - CHAP can only check a plain text or `{clear}` password, and MS-CHAP and MS-CHAPv2 need plain text or `{nthash}` (sections 5.1 and 5.2); the `ldap` handler must use `attribute` mode.
  - When the CHAP challenge is shorter than 8 bytes, the MS-CHAP `data` is not exactly 1 + 8 + 49 bytes (PPP id, 8-byte challenge, LM response 24, NT response 24, flags 1) or the flags do not set the NT bit (bit 0; only the LM response), or the MS-CHAPv2 challenge is not 16 bytes, the reply is FAIL, it is logged, and the chain does not run (the recommendation and requirement of RFC 8907).
  - MS-CHAP and MS-CHAPv2 only reply PASS: MS-CHAPv2 carries no authenticator response (RFC 8907 does not define where it goes), so a PPP client that needs mutual authentication may fail because of this, and in that case use RADIUS; the MPPE keys are also not given to the device through TACACS+.
  - For a client with no chain, a client whose chain has only `lua` handlers without `verifies_password`, and for enable requests, CHAP, MS-CHAP and MS-CHAPv2 still get RESTART and the device can fall back to PAP: the script has no password to compare.
- **An unsupported authentication method** (ARAP, and the cases in the item above) gets RESTART for its START and does not run the chain or the script, and the device can retry with PAP or ASCII; a device that does not support RESTART treats it as FAIL (RFC 8907). Non-LOGIN actions such as changing a password, and a denied enable, still get FAIL.
- **For a client with `authorize_group` set, authorization does not run the script;** the rules of the authorize group decide (section 2.3), and the script handles only its authentication and accounting.
- When authorization is accepted, the attributes added with `set_attr` are returned as `name=value` AV pairs (PASS_ADD). Each AV pair can be at most 255 bytes, and one reply can have at most 255 AV pairs; beyond that the reply is ERROR and it is logged.
- When an accounting script returns `'accept'` the reply is SUCCESS, and on an error or any other return value it is ERROR, and the connection stays open.
- **The message for the user, `server_msg`:** when the script calls `set_attr('server_msg', '...')`, the text goes into the `server_msg` field of the reply, which the device usually shows to the user (RFC 8907). It can be carried by authentication PASS and FAIL, authorization PASS_ADD and FAIL, and accounting SUCCESS and ERROR; in authorization it does not become an AV pair.
  - When authentication goes through a chain, a `server_msg` set by a handler (for example a `lua` handler) is carried only on accept: when the chain rejects, no attributes are kept.
  - Over 1024 octets it is not sent and an error is logged, and the decision stays the same. The content is sent as it is, so ASCII only is recommended.
  - The rules of an authorize group do not carry `server_msg`.

### 4.7 Example

```lua
-- Accounting: record the session; the reply is not affected
if phase == 'accounting' then
  local id = get_attr(protocol == 'radius' and 'Acct-Session-Id' or 'task_id')
  if id then
    sql_execute("INSERT INTO acct (id, ts) VALUES (?, ?)", {id, math.floor(now_unix())})
  end
  return 'accept'
end

-- TACACS+ authorization: allow only show commands
if phase == 'authorization' then
  return get_attr('cmd') == 'show' and 'accept' or 'reject'
end

-- Authentication (when there is no [auth]): query a table
local user = protocol == 'radius' and get_attr('User-Name') or get_attr('user')
local rows = user and sql_query("SELECT 1 AS ok FROM allowed WHERE name = ?", {user}) or {}
if #rows == 1 then
  if protocol == 'radius' then
    set_attr('Filter-Id', 'std.ppp')
  end
  return 'accept'
end
return 'reject'
```

A script is written by the administrator, who trusts the SQL in it. Pass request content in `params` (see section 4.3); do not build it into the SQL string.

### 4.8 Disconnect and change of authorization (CoA)

Disconnect and change of authorization (`send_disconnect`, `send_coa`, the `disconnect` and `coa` commands, and the forwarding of `[radius.coa]`) need rradius-enterprise.

### 4.9 Logging with `log`

```lua
log('quota left', left, 'for', get_attr('User-Name'))
```

This writes one log line at the `info` level: `script /etc/rradius/auth.lua (radius/accounting, client nas1): quota left 42 for bob`. It is not written when `level` is set to `warn` or `error` (see section 2.10).

- Arguments are converted to strings and joined with spaces; newlines are replaced with spaces; anything over 4096 octets is truncated.
- It does not affect the script's decision, and there is no rate limit, so writing too much fills the log.
- **Do not log passwords.** `get_attr('User-Password')` and the TACACS+ `data` are plain text.

### 4.10 Example scripts: quota, simultaneous use, IP assignment

These are under `rradius-server/examples/`. The tables they need are at the top of each file, and the tests run these scripts directly.

| Script | Purpose | Limits to know |
| --- | --- | --- |
| [`quota-and-hours.lua`](../rradius-server/examples/quota-and-hours.lua) | A cumulative time quota and the hours when login is allowed (UTC); replies `Session-Timeout` | Not a hard limit: time is counted only at Stop, and `Session-Timeout` is the time left at the moment of login, so logging in twice at the same time can use twice the quota. When you need a hard limit, combine it with `simultaneous-use.lua` with the limit set to 1. `quota_sessions` is the usage: deleting a user's rows recalculates it, which can be used for a monthly reset. Before a reset, a session ID reused after a NAS reboot is treated as a resend and not counted |
| [`simultaneous-use.lua`](../rradius-server/examples/simultaneous-use.lua) | Limits the number of simultaneous sessions, shared across the whole cluster | Not exact: a connection counts only from its Accounting-Start, so simultaneous logins may all pass. The NAS must send Interim-Update periodically, otherwise a connection is no longer counted after it times out. Old rows can be deleted at any time |
| [`mac-auth.lua`](../rradius-server/examples/mac-auth.lua) | MAC authentication: `User-Name` and `Calling-Station-Id` must be the same address, then it looks up a table and replies a VLAN | A MAC address is not secret, and anyone who can read it can impersonate it, so use this only for devices with no other option, and have the VLAN limit what can be reached. It only matches against a list and is not authentication |
| [`ip-pool.lua`](../rradius-server/examples/ip-pool.lua) | Assigns a `Framed-IP-Address`, never to two people at the same time | The NAS must send Interim-Update periodically to extend the lease, and bring this address and `User-Name` back in accounting packets, and only packets from the holder count. A request with no `User-Name` or an empty one is rejected. When the same user gets the same address again, an old Stop resent by the NAS still releases it. When the pool is nearly full, a lagging replica can cause a few requests to be rejected |

The accounting packets for `quota-and-hours.lua` and `simultaneous-use.lua` must carry `User-Name` and `Acct-Session-Id`. When they are missing or empty, the packet is not counted and so not limited, and the script logs one line with `log()`; make sure the NAS sends both attributes. The tables of both scripts are keyed by client plus session ID, so different NASes using the same ID do not affect each other.

The accounting scripts can all run more than once safely, which suits the accounting spool (see section 2.11). `simultaneous-use.lua` and `ip-pool.lua` subtract `Acct-Delay-Time`, so an old report is not taken as recent news.

These scripts only check policy and do not verify passwords. Set them both as `[lua].script` (accounting always goes through it) and as a `lua` handler after `sql` or `ldap` in a `policy = "all_accept"` chain. **The order matters:** when they come before those handlers, the script runs before the password has been verified, and a script that writes, such as `ip-pool.lua`, can take addresses for arbitrary names even though the request is finally rejected.

**A read right after a write may not find the data:** `sql_query` reads the local replica, and when `sql_execute` returns, the local replica has not necessarily applied the write yet. In a three-node test, some requests really did not find the row just written. To tell whether a write succeeded, use the number of affected rows that `sql_execute` returns and do not query again. `ip-pool.lua` does exactly this.

### 4.11 Calling an HTTP API with `http_request`

A script can call an external HTTP API, but you must first list the allowed hosts in the configuration. Without `[lua.http]`, neither `http_request` nor `url_encode` exists.

```toml
[lua.http]
allowed_hosts = ["api.example.com", "billing.internal:8443", "http://legacy.internal:8080"]
timeout_ms = 2000            # default, from connecting to reading the whole response
max_response_bytes = 1048576 # default
max_concurrent = 64          # default, per host; accounting and the other phases are counted separately
ca_file = "internal-ca.pem"  # optional, a CA to trust in addition to the system roots
```

```lua
local response, problem = http_request{
  url = 'https://api.example.com/users/' .. url_encode(get_attr('User-Name')),
  method = 'GET',                             -- default; GET, POST, PUT, DELETE, PATCH
  headers = { Authorization = 'Bearer ...' }, -- optional
  body = '...',                               -- optional
}
if not response then
  log('billing API:', problem)
  return 'reject'
end
return response.status == 200 and 'accept' or 'reject'
```

- **Return value:** when a response is received, it is `{status=, body=, headers=}`, and 4xx and 5xx count too; header names are lowercase. When the call is denied, times out, fails to connect, gets too large a response, or too many requests are waiting, it returns `nil` and a message, and the script decides whether to reject or let the request through. Only wrong parameters are a Lua error.
- **Allowlist:** the host and port are matched exactly, and wildcards are not supported. When only a host is written, only the default port is allowed. Only HTTPS is allowed; for a plain-text host, write `http://host:port`, which applies to that entry only.
- **Redirects are not followed, and proxies from environment variables are not used.** The script gets the 3xx, and to follow it you call again, which is matched against the allowlist again.
- **Always use `url_encode` to join an external value into a URL.** It makes the script fail on an empty string, `.` or `..`, and a URL with a `.` or `..` path, or with whitespace or control characters at either end, is also rejected; otherwise the login name `..` could reach other paths on the same host. **Be especially careful when you decide that an account exists only from `status == 200`.**
- **These cannot be set:** `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, `TE`; otherwise it is an error.
- **The timeout must be shorter than the device's retransmission timeout,** so that the device does not keep resending while the script waits. `max_execution_ms` does not include the wait, so when you make several calls in a row there is no upper bound on the total time, the same as with several `sql_query` calls.
- **DNS:** the allowlist matches names and does not check the resolved address, so when DNS is tampered with, a request may go to the internal network. At most 16 name lookups run at the same time (12 for logins and 4 for accounting, an estimate); beyond that they queue, and the wait counts toward `timeout_ms`.
- **The log does not contain the path, the query string, headers or content,** only the host and a fixed error kind. Take care yourself not to put keys in what the script writes with `log()`.
- When there is an HTTPS host but this machine has no trusted root certificates at all, startup fails; set `ca_file`. Changes to the settings take effect on SIGHUP.
- Metrics: `rradius_script_http_requests_total{result="ok|timeout|error|denied|too_large|busy"}`.

## 5. Password formats

The password column of the `sql` handler and the `attribute` mode of the `ldap` handler support:

| Format | Description |
| --- | --- |
| Plaintext | A value that does not start with `{` |
| `{clear}` | Plaintext; required as a prefix when the plaintext itself starts with `{` |
| `{crypt}` | crypt(3) hash |
| `{SHA}` | Base64 SHA-1 |
| `{SSHA}` | Base64 salted SHA-1 |
| `{nthash}` | NT hash (hexadecimal) |
| `{argon2}` | PHC string |
| `{MD5}`, `{SHA224}`, `{SHA256}`, `{SHA384}`, `{SHA512}` | Base64 hashes, **for migration only** |
| `{SMD5}`, `{SSHA224}`, `{SSHA256}`, `{SSHA384}`, `{SSHA512}` | Base64 "hash + salt", **for migration only** |
| `{PBKDF2-SHA1}`, `{PBKDF2}`, `{PBKDF2-SHA256}`, `{PBKDF2-SHA512}` | `<iterations>$<salt>$<hash>`; the salt and hash are adapted Base64 (`.` instead of `+`, no padding), the same as OpenLDAP pw-pbkdf2 (tested with values it generated). `{PBKDF2}` is SHA-1 like `{PBKDF2-SHA1}`, with a 20-octet hash. **For migration only** |
| `{X-PBKDF2}` | `<algorithm>:<iterations>:<salt>:<hash>`; the algorithm is `HMACSHA1` or `HMACSHA2+224`, `+256`, `+384`, `+512`; the iteration count is the Base64 of a 4-octet big-endian number; the salt and hash are Base64 (with or without padding); the hash length equals the output length of the algorithm. Like PBKDF2, it is verified outside the threads that handle requests. **For migration only** |
| `{NS-MTA-MD5}` | Format of an old mail server: a 32-character hexadecimal MD5(salt, 0x59, password, 0xF7, salt), followed by a 32-character salt. **For migration only** |

These migration formats exist so that you do not have to reset passwords when moving accounts from an old system. `{MD5}`, `{NS-MTA-MD5}`, SHA-2 without a salt, and PBKDF2 derived from SHA-1 are especially weak. Use the formats recommended below for new accounts. The iteration count of PBKDF2 is set by the hash itself; it is verified outside the threads that handle requests and is capped by "Cost limits on stored hashes" below. A malformed value never matches.

**A value with no prefix that starts with `$2a$`, `$2b$`, `$2y$`, `$1$`, `$5$`, `$6$` or `$argon2` is not treated as plaintext:** it is almost certainly a hash whose prefix was lost during import. If it were compared as plaintext, anyone who obtained the hash could log in by sending the hash string as the password. Such a value never matches for PAP, CHAP, MS-CHAP and MS-CHAPv2. The server logs one WARN per user per process (it names the user and does not include the stored value), for example `the stored password looks like a hash with no scheme prefix and is refused; add {crypt} or {argon2} before it, or {clear} if it is the password itself`. There are three fixes: add `{crypt}` to crypt hashes such as bcrypt and SHA-crypt, add `{argon2}` to argon2, and add `{clear}` to a plaintext password that really starts with these characters. There is no switch; `{clear}` is the explicit way to allow it.

**A value with no prefix that looks like a digest is still compared as plaintext, but with a warning:** it is made only of hexadecimal characters and has a length of 32, 40, 56, 64, 96 or 128 (hexadecimal NT hash, MD5, SHA-1, SHA-2), or it is standard Base64 with padding that decodes to 16, 20, 28, 32, 48 or 64 bytes (for example `W6ph5Mm5Pz8GgiULbPgzG37mj9g=`). Anyone who obtains the database can log in by sending this string as the password, so the server logs one WARN per user per process (it names the user and does not include the stored value): `the stored password looks like a digest with no scheme prefix and is compared as plaintext; add its prefix ({nthash}, {MD5}, {SHA}, {SHA256}, ...), or {clear} if it is the password itself`. Add the prefix of the right format (`{nthash}`, `{MD5}`, `{SHA}`, `{SHA256}` and so on). If it really is such a plaintext password (for example a random password of 32 hexadecimal characters for a device), add `{clear}` and the warning stops. The server only warns and does not refuse, because such plaintext passwords can really exist. This rule is the same for PAP, CHAP, MS-CHAP and MS-CHAPv2. The warning list holds at most 1024 users per process; after that no more warnings are logged and the comparison is unchanged. Salted digests with no prefix (for example an SSHA with no prefix) and values of other lengths are not recognized. To find such values, check whether `ok` in `rradius_password_checks_total{scheme="none"}` is unreasonably high.

CHAP works only with plaintext or `{clear}`; PAP supports all formats. The comparison runs in constant time. A stored value that is an empty string, or `{clear}` followed by nothing, always counts as no password, and no password matches.

`{crypt}` picks the algorithm from the content of the hash, including DES, `$1$` (MD5), `$5$` (SHA-256), `$6$` (SHA-512) and bcrypt (`$2a$`, `$2b$`, `$2y$`). For new deployments, use `{argon2}` or bcrypt or `$6$` in `{crypt}`. When the accounts are in the cluster's table, `rradiusd user add` and `user passwd` generate bcrypt, `$6$`, `{nthash}` or `{clear}` according to `scheme` in `[accounts]` (section 2.14). In other cases, use an existing tool, such as the `argon2` command or `htpasswd -B`.

Verification of `{crypt}` and `{argon2}` is slow on purpose, so it runs outside the threads that handle requests. No more checks run at the same time than there are CPU cores, and the rest wait in a queue. `timeout_ms` of the `sql` handler covers the queue and the verification, and a request that times out gets no response rather than a rejection. The cost of `{argon2}` is set by the parameters in the hash itself, so it has an upper limit; see "Cost limits on stored hashes" below.

### Which formats are weak and how to phase them out

There is no switch that turns off any format: most of them are needed to move accounts, or by protocols such as CHAP and MS-CHAP, and turning one off would lock some people out. To phase one out you first need to know which ones are still in use, so there are statistics for that.

- **Weak, for migration only:** `{MD5}`, `{SMD5}`, `{SHA}`, `{SSHA}`, `{SHA224}`, `{SHA256}`, `{SHA384}`, `{SHA512}` and their salted versions, and `{NS-MTA-MD5}`: they are all a single hash round, so an attacker who gets the database can guess very quickly. `{PBKDF2}` and `{PBKDF2-SHA1}`, derived from SHA-1, have iterations but use an old algorithm. DES and `$1$` (MD5-crypt) inside `{crypt}` are also old formats.
- **Weak, but needed by protocols, so they cannot be phased out:** `{nthash}` (MD4 with no salt), plaintext and `{clear}`. CHAP needs plaintext; MS-CHAP, MS-CHAPv2 and PEAP need plaintext or `{nthash}` (section 5.1). Protect these accounts with access control on the database, and do not let accounts that do not need them use them.
- **For new accounts:** bcrypt, `$5$` or `$6$` in `{crypt}`, or `{argon2}` (see above).

**Cost limits on stored hashes (`[auth.password_limits]`):** the cost of a slow hash is written in the stored value. Someone who can write their own password value (for example, when the directory lets users change `userPassword`) can set a huge cost and then use as many logins as there are CPUs to occupy all the slow-hash slots, so that other people's logins queue until they time out. The memory parameter of `{argon2}` could also make the process use too much memory. So before verifying, the server reads the cost parameters, and **if one is over the limit, it does not compute and rejects the login as a wrong password**. There are default limits even if you do not write this section. All keys are optional positive integers (0 is a load error) and take effect on a SIGHUP reload:

| Key | Default | What it limits |
| --- | --- | --- |
| `pbkdf2_iterations` | 5000000 | Iterations of `{PBKDF2}`, `{PBKDF2-SHA1}`, `{PBKDF2-SHA256}`, `{PBKDF2-SHA512}`, `{X-PBKDF2}` |
| `crypt_rounds` | 1000000 | `rounds` of `$5$`, `$6$` and `$sha1$` in `{crypt}`, and the rounds of extended DES (starting with `_`) |
| `bcrypt_cost` | 15 | Cost of bcrypt in `{crypt}` (`$2a$`, `$2b$`, `$2y$`); each increase of 1 doubles the work |
| `argon2_memory_kib` | 262144 | Memory of `{argon2}` (KiB, 256 MiB by default) |
| `argon2_iterations` | 10 | Iterations of `{argon2}` |
| `argon2_lanes` | 16 | Parallelism of `{argon2}` |

- **Going over a limit is an ordinary wrong password:** it counts in `failed` of `rradius_password_checks_total` and in `[auth.lockout]`, and is not "backend unavailable". The server logs one WARN per account per process. It names the account, the format and the parameter that was exceeded (for example `[auth.password_limits].pbkdf2_iterations`), and does not include the stored value or the password. Formats with a fixed cost, such as DES, `$1$`, `{SHA}` and `{SSHA}`, are not limited.
- **When you upgrade:** existing hashes with a cost above the default limit are rejected after the upgrade. When you see this WARN, check that the hash is legitimate, then raise the matching key.
- **Measured (Apple M4 Max, release build, one run, no contention):** values exactly at the default limit take about 0.29 / 0.37 / 0.81 s for PBKDF2-SHA1 / 256 / 512, about 0.18 / 0.15 s for `$5$` / `$6$`, about 1.3 s for bcrypt cost 15, and about 0.94 s for argon2 (256 MiB, t=10, p=16), with a peak process memory of about 266 MiB. A slower CPU is several times slower.
- **Multiply the argon2 memory limit by the number of CPUs:** the number of checks at the same time equals the number of CPUs, so in the worst case the memory used is about `argon2_memory_kib` × the number of CPUs (about 16 GiB with 64 cores and the default). On a machine with little memory, lower `argon2_memory_kib`, and make sure your argon2 hashes do not exceed it.
- **The defaults are higher than common recommendations:** the common recommendation is about 600,000 to 1,300,000 iterations for PBKDF2 and bcrypt cost 10 to 13, so ordinary deployments are not affected.

**See what is still in use with the statistics:** `rradius_password_checks_total{scheme="...",result="ok|failed"}` counts every login that is checked against a stored password (PAP, CHAP, MS-CHAP, MS-CHAPv2), by the format of the stored value and the result. The values of `scheme` are fixed: `clear`, `none` (plaintext with no prefix), `crypt`, `md5`, `smd5`, `sha`, `ssha`, `sha224`, `ssha224`, `sha256`, `ssha256`, `sha384`, `ssha384`, `sha512`, `ssha512`, `pbkdf2-sha1` (`{PBKDF2}` counts here too), `pbkdf2-sha256`, `pbkdf2-sha512`, `x-pbkdf2`, `ns-mta-md5`, `nthash`, `argon2` and `unknown` (an unrecognized prefix). The two results of every format exist from startup, and those that have not happened are 0. No prefix written in the database ever becomes a label. `failed` is not only a mistyped password: CHAP can compare only plaintext (`{clear}` or no prefix), MS-CHAP can compare only plaintext or `{nthash}`, and both always fail against one-way hashes such as `{crypt}` and `{ssha}`; a cost parameter over the limit (see below) also counts. So when `failed` grows for a format, first work out which protocol the logins use and whether they are really wrong passwords; do not assume the data is bad. Only when the format is `unknown` and the result is always `failed` is the imported value usually the problem; see the hashes with no prefix above. The `bind` mode of LDAP, `netlogon`, Lua handlers and certificate logins do not compare against a stored password and are not counted. Empty values, hashes with no prefix and timeouts are not counted either.

These statistics see only accounts that log in; old accounts that never log in do not appear. To take stock of the formats of all the accounts in the database, use a query (replace `users` and `password` with your table and column):

```sql
SELECT CASE WHEN password LIKE '{%}%' THEN lower(substr(password, 1, instr(password, '}')))
            ELSE 'none' END AS scheme,
       count(*) AS accounts
FROM users
GROUP BY scheme
ORDER BY accounts DESC, scheme;
```

This query has been run on SQLite: a value with no prefix (including `{broken`, which lacks the `}`) counts as `none`, and the prefix is case-insensitive, so `{SSHA}` and `{ssha}` count together. Function names may differ in other databases.

**How to phase one out:** first take stock, then look at the statistics. A weak format is not phased out until the stock count is 0. If the stock count shows accounts but the statistics show no successful login for a long time, those accounts are unused; you can ask their owners to change the password, or remove them after a deadline. For accounts still in use, change them to a new format with `user passwd` (see the [manual, chapter 07](manual/07-users-and-chains.md#migrate-password-hashes-from-an-old-system)). rradius does not convert on login, and there is no setting to turn a format off.

### 5.1 MS-CHAPv2

The `sql` handler, the `attribute` mode of the `ldap` handler and the `netlogon` handler (section 3.7, AD accounts) support MS-CHAPv2 (RFC 2759). On success they reply with `MS-CHAP2-Success` and `MS-MPPE-Send-Key` and `MS-MPPE-Recv-Key`; on failure they reply with `MS-CHAP-Error` carrying `E=691 R=0`.

- **It works only with plaintext, `{clear}` or `{nthash}`.** All other formats are rejected, because the protocol needs the NT hash. For AD accounts, use the `netlogon` handler, and the DC does the verification.
- **MS-CHAPv2 itself is weak:** a captured exchange can be cracked offline, and a leaked NT hash is the same as a leaked password. Use it only inside an encrypted tunnel, and do not use it if something else will do.
- **User name:** verification uses only the part after the last backslash (`alice` for `BIGCO\alice`). The account lookup uses the full `User-Name` by default. To ignore the domain, set `strip_realm` (see section 3.6) or write it in the query yourself. Normalization affects only the account lookup; verification still uses the name as received.
- **Not supported:** the `bind` mode of the `ldap` handler, the top-level script and the `lua` handler. EAP-TLS uses no password; for MS-CHAPv2 inside PEAP and TTLS, see section 2.12. For MS-CHAPv1, see section 5.2.

### 5.2 MS-CHAPv1

PPTP VPNs and old dial-up devices still use MS-CHAPv1 (RFC 2433). Both the RADIUS `MS-CHAP-Challenge` (8 bytes) with `MS-CHAP-Response` (50 bytes: identifier, flags, LM response, NT response) and the TACACS+ MSCHAP authentication (`authen_type` 5, section 4.6) are supported. The `sql` handler and the `attribute` mode of the `ldap` handler verify it, and the storage formats are the same as for MS-CHAPv2 (plaintext, `{clear}` or `{nthash}`). **It is not accepted by default:** write `[auth] allow_mschapv1 = true` only for devices that cannot do without it (see "Turning off MS-CHAPv1" below).

> **Risk: the password hash of MS-CHAPv1 uses only DES (56 bits), so the NT hash can be recovered almost instantly from a captured challenge and response, whatever the strength of the password; a leaked NT hash is the same as a leaked password. Windows also turns it off by default.** Use it only on a management network or with old devices that cannot be upgraded, and do not use it if MS-CHAPv2, PEAP or EAP-TLS will do.

- **Only the NT response is checked.** A request is always rejected when bit 0 of the flags is not set (it asks for the LM response) or when it has only the LM response (RADIUS treats it as having no credentials, and TACACS+ returns FAIL). The LM response is ignored, and there is no `{lmhash}` storage format.
- **The MPPE key is only 128 bits.** On success the server returns `MS-CHAP-MPPE-Keys` (8 bytes of zeros as the LM-Key, followed by a 16-byte NT-Key, which is the MD4 of the NT hash, RFC 3079 section 2.3; encrypted the way User-Password is, per RFC 2548 section 2.4.1, so it is 32 bytes in the packet) and `MS-MPPE-Encryption-Types` (2, allowing only 128 bits). **40/56-bit MPPE is not supported**, so it succeeds only when a PPTP device can negotiate 128 bits; the LM-Key is zero and has not been verified with real hardware.
- **On failure** the Access-Reject carries `MS-CHAP-Error` (`E=691 R=0`) and no `MS-CHAP2-Success` (version 1 has no server proof).
- **Handlers that cannot verify it:** the `bind` mode of the `ldap` handler (the directory sees only the password) rejects it. The `netlogon` handler handles only MS-CHAPv2 and skips MS-CHAPv1, so only a chain that ends there rejects it. The top-level script and the `lua` handler do not see the response and do not verify for the script; what the script can see are the attributes of the request.
- **Not supported:** changing the password (`MS-CHAP-CPW`), MS-CHAPv1 inside EAP (PEAP, TTLS) and the LM response. When a request carries `User-Password`, `CHAP-Password` or `MS-CHAP2-Response` as well, the order is PAP, CHAP, MS-CHAPv2, and MS-CHAPv1 comes last.
- **Length of the RADIUS User-Password:** per RFC 2865 section 5.2, the length must be 16 to 128 bytes and a multiple of 16. An Access-Request that does not fit is answered with Access-Reject before decryption; no handler or script runs, it does not count in `[auth.lockout]`, the handler in the authentication record is `length`, and one rate-limited WARN is logged (the client and the length, not the password). The value is fixed and there is no setting. **Upgrade note:** a User-Password longer than 128 bytes, or not padded to a multiple of 16, used to be accepted and is now rejected; first check the authentication records for `handler="length"`.
- **Turning off MS-CHAPv1 (`[auth] allow_mschapv1`):** an optional boolean, `false` by default: leaving out the key is the same as writing `false`. When it is not written or is `false`, the auth chain refuses MS-CHAPv1 logins before any handler (RADIUS `MS-CHAP-Response`, TACACS+ MSCHAP START): RADIUS returns Access-Reject with `MS-CHAP-Error` (`E=691 R=0`), TACACS+ returns FAIL, and the handler in the authentication record is `mschapv1_refused`. **It does not query the database or the directory, and it does not count toward the failures of `[auth.lockout]` or take one of its slots** (it is not a password guess), so an old device that keeps retrying does not lock the account. Every refusal logs one INFO line (the client, the protocol and the account, not the challenge or the response). PAP, CHAP, MS-CHAPv2 and EAP are not affected, and the change takes effect on a SIGHUP reload. **Upgrade note:** this default used to be `true`. Old devices that still send MS-CHAPv1 all fail to log in after the upgrade. `rradius_password_checks_total` is split only by storage format and cannot tell which protocol a login used, so to find them, look at the lines with `handler="mschapv1_refused"` in the authentication records or `MS-CHAPv1 refused` in the log, and write `allow_mschapv1 = true` only after you have confirmed that you cannot do without it. When there is no `[auth]`, an `MS-CHAP-Response` handled by the Lua script itself is outside this switch. The `MS-CHAP-MPPE-Keys` returned when MS-CHAPv1 succeeds are derived from the NT hash and are no stronger than it; this is set by the protocol, and the switch is the only mitigation.
- **Dictionary:** `MS-CHAP-MPPE-Keys` is an attribute with `encrypt=1`, and `set_attr` now supports `encrypt=1` (section 4.5); an `octets[N]` attribute with a fixed length can also be set with `set_attr`, and the value must be exactly N bytes.

## 6. Example: a VPN gateway on LDAP, other devices on SQL

```toml
dictionary = "/usr/share/rradius/dictionary"

[lua]
script = "policy.lua"

[cluster]
data_dir = "/var/lib/rradius"
secret_raft_file = "raft.secret"
secret_api_file = "api.secret"

[auth]
[[auth.handlers]]
type = "sql"
user_query = "SELECT password FROM users WHERE username = ?"
reply_query = "SELECT attribute, value FROM user_reply WHERE username = ?"

[auth.chains.vpn]
policy = "all_accept"
[[auth.chains.vpn.handlers]]
type = "ldap"
urls = ["ldaps://dc1.example.com"]
bind_dn = "cn=radius,ou=services,dc=example,dc=com"
bind_password_file = "ldap.secret"
base_dn = "ou=people,dc=example,dc=com"
filter = "(uid={user})"
[[auth.chains.vpn.handlers]]
type = "lua"
script = "vpn_hours.lua"     # for example, return 'accept' only during working hours

[radius]
[[radius.clients]]
name = "switches"
address = "10.0.0.0/16"
secret_file = "switches.secret"

[[radius.clients]]
name = "vpn-gw"
address = "10.1.0.1"
secret_file = "vpn-gw.secret"
auth = "vpn"
```

## 7. Interoperability

The following combinations have been tested with the interoperability tests. These tests are kept with the enterprise features in the rradius-enterprise repository, and the CI of the core does not run them.

| Item | Peer |
| --- | --- |
| UDP authentication, accounting, Status-Server | `radclient` from FreeRADIUS 3.2.10 |
| Extended attributes, `long-extended` fragmentation, EVS, TLV, WiMAX vendor format, receiving and sending | `radclient` from FreeRADIUS 3.2.10. It does not send `IPv6-6rd-*` and shows only the number when it receives it, so for this one only the content of the response was verified |
| TACACS+ ASCII and PAP login | Python `tacacs_plus` 2.6 |
| EAP-TLS (TLS 1.2, 1.3), rejecting another CA, a revoked certificate and a mismatched name | `eapol_test` from wpa_supplicant |
| PEAP-MSCHAPv2: plaintext and `{nthash}` of `sql`, the `attribute` mode of `ldap`, a wrong password, a name that does not exist, an anonymous outer name, rejecting TLS 1.3 | `eapol_test` from wpa_supplicant |
| EAP-TTLS: inner PAP (plaintext and `{argon2}` of `sql`, the `bind` mode of `ldap`) and inner MS-CHAPv2 (plaintext and `{nthash}`), a wrong password, a name that does not exist, an anonymous outer name, rejecting TLS 1.3 | `eapol_test` from wpa_supplicant |
| Linux host login: right and wrong passwords, a name that does not exist, an Access-Challenge answered right and wrong, a wrong shared secret | `pam_radius` from FreeRADIUS (commit `36cf87d` of 4.0.0) with `pamtester`, with its `require_message_authenticator` turned on |

**Not tested:** any real NAS, router or switch. Implementations may differ between vendors, so test once on real hardware before going live. The ASCII flow of `tacacs_plus` carries the account in the START, so GETUSER is covered only by the project's own tests.

**Host login:** to let a Linux host log in through rradius, use `pam_radius` from FreeRADIUS; rradius does not provide its own PAM or NSS module. When you set it up, note:

- **Turn on `require_message_authenticator`.** It is an option of `pam_radius` and is off by default; if you do not turn it on, it does not require the response to carry this attribute.
- **It works over UDP only,** and the password is hidden with the shared secret. Use a long random secret, and use it only on a trusted network.
- **It does authentication only and does not provide accounts.** `pam_radius` does not look up local accounts, and the tests did not create accounts either; but programs such as `sshd` must first be able to find the user, so prepare the accounts separately. RADIUS has no way to look up users, so an NSS module cannot help here.
- **Write the port explicitly when `/etc/services` has no `radius`,** for example `172.30.0.10:1812 secret 3`, or you get `Authentication service cannot retrieve authentication info`. Minimal container images often lack this file.

The interoperability tests reached only the PAM layer with `pamtester`, and `sshd`, `sudo` or `login` were not tested.

**Fuzz testing:** `rradius-server/fuzz/` uses `cargo-fuzz` to test RADIUS packets, TACACS+ packets, EAP packets, the dictionary and the text of address attributes. `.github/workflows/fuzz.yml` runs every target for 300 seconds each week, starting from the seeds in `seeds/`. The targets are compiled and run only once a week, so after you change a fuzz target, start the Fuzz workflow by hand on the branch before merging. It needs the nightly toolchain. To run it locally:

```bash
rustup toolchain install nightly-2026-11-05 --profile minimal
```

```bash
cargo install cargo-fuzz --version 0.13.2 --locked
```

```bash
cd rradius-server/fuzz && mkdir -p corpus/radius_packet && cargo +nightly-2026-11-05 fuzz run radius_packet corpus/radius_packet seeds/radius_packet -- -max_total_time=120
```

When it finds a problem, the input that caused the crash is saved in `fuzz/artifacts/<target>/`. After you fix it, add it to `fuzz/seeds/<target>/` and CI checks it from then on.

## 8. Troubleshooting

The table below matches the message itself. Each line also has a time and a level in front of it, and in the `json` format the message is in the `message` field (see section 2.10).

| Symptom | Possible cause |
| --- | --- |
| Status code 2 at startup | A configuration error; the error message points to the file and the field |
| ``moved to `[lua].script` `` | An old configuration file; move the top-level `script` into `[lua]` |
| `cannot compile script '<file>'` | A syntax error in the script |
| `script exceeded the execution limit of N ms` | The script ran too long; check the loops, or raise `[lua].max_execution_ms` |
| `memory error: not enough memory` | The VM went over its memory limit; check whether the script keeps a lot of data, or raise `[lua].max_memory_mb` |
| `sql_query timed out` / `sql_execute timed out` | The database is too slow or the cluster has no majority of nodes; raise `query_timeout_ms` or check the cluster |
| `sql_execute: non-deterministic ...` | A write uses a time or random function; use `now_unix()` instead |
| `Lua script did not accept accounting ...` | The accounting script did not return `'accept'`, and the device retransmits; make sure it returns `'accept'` when the record is stored |
| `... failed: the chain did not accept the inner name` | A wrong PEAP or TTLS password, or the user was not found. For the inner MS-CHAPv2 of PEAP and TTLS, the cause may also be an `ldap` handler in `bind` mode; inner PAP of TTLS can use `bind` |
| `... failed: the chain accepted the inner name without checking a password` | The chain accepted, but no handler compared a password, for example when there is only a `lua` handler. For inner PAP of TTLS, `sql`, `ldap` or a `lua` handler with `verifies_password = true` must accept |
| `... failed: this client has no chain to check a password with` | PEAP and TTLS need a chain, and this client has none |
| `... failed: an MS-CHAP challenge other than the one the tunnel exports` | The inner MS-CHAPv2 of TTLS used a challenge other than the one the tunnel exports; this may be an incompatible client, or a relay attack |
| `... failed: an unrecognized mandatory inner AVP` | The inner method of the TTLS client is not PAP or MS-CHAPv2, for example inner EAP or CHAP |
| `EAP conversation with client ... failed` | Look at the reason that follows: the certificate, a `State` that cannot be found (common when switching nodes), a TLS version or cipher suite mismatch (section 2.12) |
| `EAP conversation with client ... refused` | The certificate passed, but the name does not match, or the chain or the script did not accept. With LDAP, make sure `certificate_filter` is set; with SQL, make sure the first column has a value or `certificate_query` is set |
| `accounting spool: request from client ... not written` | The spool is full or the disk write failed; this request gets no reply, and the device retransmits it; check the disk and `max_bytes` (section 2.11) |
| `accounting spool: request from client ... moved to rejected.log` | The request was given up on during replay, and the reason follows; the file is not cleaned up automatically (section 2.11) |
| `accounting spool: cannot read`, `cannot record progress` | Reading the spool file or writing the progress failed; check the disk and permissions. It retries later and may replay the same request |
| `accounting spool: removed ... octets of an unfinished write` | An unfinished write was cut off at startup; the content is kept in `truncated.log` if there was any |
| `accounting spool: could not remove an unfinished write` | It could not be cut off at startup; that whole file is renamed to `.corrupt` later |
| `accounting spool: ... damaged files set aside` | Damaged files were renamed to `.corrupt`, and the requests inside are not replayed |
| `rradius_acct_spool_bytes` never goes down | The database has not recovered, or the script still fails; look at the log messages that start with `Lua script` |
| The device keeps retransmitting accounting packets | The accounting script fails or does not return `'accept'`; look at the log messages that start with `Lua script` |
| `io`, `os.execute`, `require`, `loadstring`, `getfenv`, `setfenv` fail in a script | The sandbox does not provide these functions |
| `lacking a Message-Authenticator` | The device did not send this attribute; upgrade the device, or set `require_message_authenticator` of that client to `no` or `auto` (see section 2.6) |
| RADIUS gives no response | The source address is not in `[[radius.clients]]`, the shared secret does not match, the request has no Message-Authenticator, or the limit on requests in progress has been reached |
| `RADIUS ... overloaded: discarded N packets` | `max_auth_inflight` or `max_acct_inflight` has been reached; usually first find out why handling slowed down (LDAP, the database), and only then consider raising the limit |
| `TACACS+ overloaded: closed N connections` | `max_connections` has been reached |
| `TACACS+ connection from <address> closed: it already holds max_connections_per_source = N connections` | The connections open from this source address have reached `max_connections_per_source`; at most one line per second, and when there are several, the line ends with "(N since the last line)". When a device opens connections and does not close them, lower `idle_timeout_ms`; when the address is behind NAT or a jump host, raise this value |
| `cluster was not ready to apply migrations` | The cluster was not ready within `migrations_wait_ms`, a SQL file ran too long, or the local replica did not catch up; start the other nodes, or raise this value. When a node keeps exiting with status code 1, this value is usually smaller than the run time of the longest SQL file |
| `migration file '...' ...` | See section 2.5: the file name, the number, an applied file that was modified, or a SQL error |
| `reload failed, keeping the running configuration: ...` | The reload failed and the service stays as it was; fix the problem the message points to and send SIGHUP again |
| `these need a restart to apply: ...` | The listed settings were changed but not applied; see section 2.9; it is listed on every reload until you restart |
| Ending with `(N since the last line)` | RADIUS packets from unknown sources and TACACS+ connections, which anyone can trigger, so each kind is limited to one line per second. The line names only the source of the current one, and N is the total since the previous line; for other sources, look at the metrics or capture packets |
| `accepting a connection failed N times` | Usually the file descriptors have run out; the process retries automatically and does not need a restart. `rradiusd tuning` calculates the recommended limit and how to set it from the configuration (section 8.1), or lower `max_connections` (1024 by default) |
| `auth chain: rejecting a request from client ...` | The name is empty after processing, or does not match `allowed`; see section 3.6 |
| `script ... : ...` | A log line the script wrote with `log()`, not an error |
| `RADIUS response attribute error: ...` | The value of a reply attribute does not match the dictionary type, for example when `reply_query` returns `2001:db8::1/64` as a prefix. That authentication is rejected even if the password is correct; fix the value in the table |
| `rradius_auth: authentication ... decision="reject"` | The authentication record. `handler` is the handler that made the decision; `none` means all were skipped, `no_password_check` means no one compared a password in `all_accept`, and `normalization` means the name was rejected |
| `auth chain: rejecting a request from client ...: all_accept without checking a password` | None of the `sql` and `ldap` handlers in `all_accept` accepted, and only the script said yes, or the script returned `challenge` before they accepted. Usually the name is not in the table. The same applies to certificates; see section 3.2 |
| `stopping: after N ms, ... they are cut off` | At stop, requests were still unfinished after waiting `drain_ms`, and those requests get no response; usually the script, LDAP or the database is too slow. To wait longer, raise `[shutdown] drain_ms`, and raise the grace period with it (section 2.16) |
| At stop, `/readyz` is 503 `stopping` but still responds | Normal: this is phase 1, `ready_delay_ms`, which lets the load balancer remove this node first (section 2.16) |
| `Raft node has not been initialized` | The node has not joined the cluster yet, and it stops after it joins; when it keeps appearing, check whether the other nodes are started |
| `rradius_log_dropped_lines_total` keeps increasing | The log output cannot keep up or writing fails, and log lines are lost; check where stderr goes, or lower `level` or turn off `auth` |
| A TACACS+ device login keeps getting ERROR | The packets of one session are in the wrong order, more than 8 logins are unfinished on the connection, or a login took longer than 60 seconds |
| All LDAP authentications are rejected | Check the log messages that start with `auth ldap handler:`: a connection or certificate error, a service account login failure, a search failure |

### 8.1 Operating system tuning `tuning`

`rradiusd tuning` lists which system limits to raise, to what values and how to set them, according to the operating system it runs on (Linux, macOS, FreeBSD) and the configuration file.

```bash
rradiusd tuning --config /etc/rradius/rradius.toml
rradiusd tuning            # reads the default configuration file; with none, it estimates from the default settings
```

- **It only reads and changes nothing.** It needs no root, starts no cluster node and opens no port. For each item it lists the current value, the recommended value, the reason, a command that takes effect at once (for example `sysctl -w`) and the file to write so that it survives a reboot, and the administrator runs them. An item whose current value already reaches the recommended value is marked `ok` with no command; a value that cannot be read is marked `unknown` with the reason.
- **First see whether packets are really being dropped.** The report starts with the number of UDP packets dropped since boot because the receive buffer was full (`RcvbufErrors` in Linux `/proc/net/snmp`; `netstat -s -p udp` on macOS and FreeBSD); on Linux it also lists the number dropped before they reached the socket (`/proc/net/softnet_stat`). When it is 0, the receive buffer does not need tuning now, and the recommendations below are for higher load; when it is not 0, adjust and watch whether it keeps increasing.
- **What it checks:**

  | Item | Linux | macOS | FreeBSD |
  | --- | --- | --- | --- |
  | UDP receive buffer (with `[radius]`) | `recv_buffer_bytes` not set: `net.core.rmem_default`, `net.core.rmem_max`; set: `net.core.rmem_max` and `[radius] recv_buffer_bytes` | Not set: `kern.ipc.maxsockbuf` (raise it first), `net.inet.udp.recvspace`; set: `kern.ipc.maxsockbuf` and `[radius] recv_buffer_bytes` | Same as macOS |
  | File descriptors | `RLIMIT_NOFILE` of the process | Same as Linux, plus `kern.maxfilesperproc` | Same as macOS |
  | TCP accept queue (with TACACS+ or RadSec) | `net.core.somaxconn` | `kern.ipc.somaxconn` | `kern.ipc.soacceptqueue` |
  | NIC receive queue | `net.core.netdev_max_backlog`, recommended only when softnet drops packets; not recommended when the number of softnet drops cannot be read | None | None |
  | Configuration file | `[radius] sockets`, `[runtime] pin_threads` | `sockets` can only be 1 | `sockets` can only be 1; `[runtime] pin_threads` |

- **When `recv_buffer_bytes` is not set, on Linux raise `rmem_default`, not only `rmem_max`.** In that case rradius does not set `SO_RCVBUF`, and each socket gets `rmem_default`, but that also affects the UDP sockets of every program on the machine. **We recommend setting `[radius] recv_buffer_bytes` instead (section 2.3):** after that, only the upper limit `rmem_max` has to be at least the configured value (at least twice that for `kern.ipc.maxsockbuf` on macOS and FreeBSD), and the report no longer lists the system default but one item that compares `recv_buffer_bytes` with the calculated recommendation; when it is smaller than the recommendation, change the configuration file and restart.
- **How the recommended values are calculated:** the need for file descriptors is 256 (an estimate for the database files, logs, the cluster and the connection pool) plus `max_connections` of TACACS+, `read_connections`, 2 for each cluster node, and 2 times `max_connections` of RadSec in rradius-enterprise; the recommended limit is twice the need, and at least 65536. The receive buffer of each UDP socket is `max_auth_inflight` divided among the `sockets` sockets, counting 4096 bytes per packet, at least 4 MiB, rounded to a whole MiB. The TCP accept queue is at least 1024, the same as the length the program asks for.
- **These recommended values have not been measured;** they are a starting point: after you apply them, confirm with the drop count above.
- **FreeBSD is not a released platform:** the recommendations there are written from the official documentation and have not been tested on FreeBSD, and the start of the report says so.
- When there is `--config` but the file cannot be read or has an error, it exits with status 2 and prints the reason, the same as `check`.

## 9. Performance measurements

`rradius-bench/` is a stand-alone load-test tool for RADIUS and TACACS+, used to measure the throughput and latency of the server. It is not in the Docker image, so you build it yourself.

The numbers already measured for each feature are written in the matching configuration sections: the three-node cluster, read connections and snapshots in section 2.2; multiple receive sockets, core pinning and accounting writes in section 2.4; the authentication record in section 2.10; the accounting spool in section 2.11; and the comparison of TACACS+ authorize groups with scripts in section 2.3.

**Use test secrets only.** `--secret` appears in the process list, and the secrets and accounts of the example configurations are written in the files, and they accept every login, so do not use them against a production environment or put them on a production server.

### Build

```bash
cargo build --release --locked --manifest-path rradius-bench/Cargo.toml
```

```bash
cargo build --release --locked --manifest-path rradius-server/Cargo.toml
```

### Example configurations

`rradius-server/benchmarks/` holds the server configurations for measuring. They all bind only `127.0.0.1`, the secret is `bench-secret`, and the metrics port is 9812. The tool only sends requests, and you start the server separately. The EAP-AKA' and RadSec configurations (`aka.toml`, `radsec.toml`) are in `benchmarks/` of rradius-enterprise.

| Configuration | What the accounting script does |
| --- | --- |
| `accept.toml` | Writes nothing and always accepts |
| `acct-sql.toml` | Writes one database row each time |
| `acct-cpu.toml` | Pure computation each time; about 30 ms when run alone, and on another machine adjust the loop count in `acct-cpu.lua` |
| `eap.toml` | Always accepts accounting; EAP-TLS, PEAP and TTLS use the account `bench` (password `bench`), see "EAP logins" below |
| `tacacs.toml` | Opens only TACACS+, port 4949, and always accepts authentication, authorization and accounting, see "TACACS+" below |

Start the server in the `rradius-server/benchmarks/` directory. Its data goes in `data/` of that directory (which is gitignored):

```bash
cd rradius-server/benchmarks && ../../target/release/rradiusd serve --config accept.toml
```

Stop the server before you switch to another configuration. The commands below run in another terminal in the project root.

To measure the maximum throughput, use `accept.toml`. The latency of this measurement is mostly queueing, so do not use it to look at latency:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load pap,concurrency=64,window=1
```

The five rows of the [accounting load table](#24-overload-and-retransmission) follow in order. No accounting load, with `accept.toml`:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load pap,rate=100
```

One database row each time, concurrency 256 and 4096, with `acct-sql.toml`:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=256 --load pap,rate=100
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=4096 --load pap,rate=100
```

About 30 ms of pure computation each time, concurrency 256 and 4096, with `acct-cpu.toml`. This load queues for a long time, so lengthen both the warm-up and the timeout:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=256 --load pap,rate=100 --warmup 15 --duration 20 --timeout 60
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load acct-start,concurrency=4096 --load pap,rate=100 --warmup 15 --duration 20 --timeout 60
```

### EAP logins

The `eap-tls`, `peap` (inner EAP-MSCHAPv2), `ttls` (inner PAP) and `ttls-mschapv2` (inner MS-CHAPv2) loads run complete EAP logins: one login counts as one request, counted from the first Access-Request to the Access-Accept or Access-Reject, and `rounds` in the report is the average number of Access-Requests per login. The MS-MPPE keys carried by the Access-Accept must equal the keys exported from TLS to count as `accept`; if they differ it counts as `invalid`, so everything counted is a login that really completed.

- **Options.** `--cert-file` and `--key-file` are the certificate used for `eap-tls` logins; `--tls-version` is `1.2` (default) or `1.3`, and `1.3` works only with `eap-tls`; `--eap-fragment` is the upper limit of the TLS data the tool sends in each fragment (1000 octets by default). EAP always carries Message-Authenticator and cannot be combined with `--no-message-authenticator`.
- **The server certificate is not checked by default.** The targets of load tests mostly use certificates they made themselves, so by default any server certificate is accepted (the signature of the handshake is still checked); the certificate is checked only when you give `--ca-file`, and the name is `--tls-name` (default `--server`). Do not use this default against a production environment.
- **When the server offers another method,** the tool replies once with a Nak asking for its own method, which adds a round trip. `eap.toml` offers EAP-TLS first, so `rounds` of PEAP and TTLS are each one more.
- **The tool also does the TLS handshake,** and `eap-tls` in particular signs with the private key every time. When a `tool_saturated` warning appears, limit the server to fewer CPUs or give the tool more threads; when `concurrency` is small, lower `window` as well, or there is only one socket and the tool uses only one thread (for example `concurrency=64,window=8,threads=7`).

First, in the project root, make the certificates that `eap.toml` needs (`eap-certs/` is gitignored):

```bash
mkdir -p rradius-server/benchmarks/eap-certs && cd rradius-server/benchmarks/eap-certs
openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj /CN=bench-ca -keyout ca.key -out ca.pem
openssl req -newkey rsa:2048 -nodes -subj /CN=localhost -keyout server.key -out server.csr
printf 'subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile server.ext -out server.pem
openssl req -newkey rsa:2048 -nodes -subj /CN=bench -keyout client.key -out client.csr
printf 'subjectAltName=DNS:bench\nextendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 -extfile client.ext -out client.pem
cd ../../..
```

The name `bench` of the client certificate must equal the name used to log in (`match_identity`). Then start the server with `eap.toml` in `rradius-server/benchmarks/`, and run the three loads one by one from the project root:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load eap-tls,concurrency=64,window=8 --cert-file rradius-server/benchmarks/eap-certs/client.pem --key-file rradius-server/benchmarks/eap-certs/client.key
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load peap,concurrency=64,window=8
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --metrics-url http://127.0.0.1:9812/metrics --load ttls,concurrency=64,window=8
```

Measured on 2026-11-08 on Linux (arm64) in Docker on the same Apple M4 Max, with the server limited to CPUs 0–6 and the tool to 7–13 (`threads=7`), `eap.toml`, TLS 1.2, two runs each. First with RSA 2048 certificates (the commands above):

| Method | Round trips per login | Concurrency 64, `window=8` (logins/s) | Concurrency 512 (logins/s) | Server CPU per login |
| --- | --- | --- | --- | --- |
| EAP-TLS | 6 | 8,410–8,660, p99 13 ms | 9,100–9,380, p99 90–92 ms, 21–41 timeouts per run | about 690–740 µs |
| PEAP | 10 (including one Nak) | 8,770–8,900, p99 12.6–12.9 ms | 8,890–9,090, p99 81–84 ms, 17 timeouts in one run | about 730–780 µs |
| TTLS | 6 (including one Nak) | 10,630–10,710, p99 12 ms | 10,880–10,930, p99 74–75 ms | about 630–660 µs |

With the same conditions but ECDSA P-256 certificates (both server and client changed), concurrency 64, `window=8`, run one after the other with RSA in the same container:

| Method | RSA 2048 (logins/s) | ECDSA P-256 (logins/s) | Server CPU per login, RSA → ECDSA | Round trips per login, RSA → ECDSA |
| --- | --- | --- | --- | --- |
| EAP-TLS | 7,230–8,590, p99 13–22 ms | 24,080–24,730, p99 5.3–5.5 ms | about 700–790 → 265–267 µs | 6 → 4 |
| PEAP | 7,850–8,620, p99 13–16 ms | 18,320–18,500, p99 6.0–6.1 ms | about 776–777 → 350–352 µs | 10 → 9 |
| TTLS | 9,780–10,140, p99 13–14 ms | 28,940–29,340, p99 4.2–4.3 ms | about 644–656 → 219–220 µs | 6 → 5 |

- **The server CPU is used up:** from concurrency 64 the 7 CPUs are almost full (56–69 CPU seconds used every 10 seconds), so about 9,000 to 11,000 logins per second is the limit with RSA 2048, and more concurrency only adds latency and timeouts.
- **One EAP login uses far more CPU than PAP:** with RSA 2048 one login takes about 0.6–0.8 ms, while a PAP that always accepts takes about 18 µs in the same environment (measured in section 2.13 of the rradius-enterprise usage reference). The difference is that every login is a full TLS handshake.
- **Most of it is the RSA signature:** with ECDSA P-256 the CPU per login falls to about a third and the throughput is about 2 to 3 times higher; the certificates are smaller, so the server's turn has fewer fragments, which saves one round trip. PEAP is still about 130 µs more than TTLS, possibly because of the hashing of the inner MS-CHAPv2 and the extra round trips, which has not been confirmed.
- **Commands for ECDSA certificates:** replace every `-newkey rsa:2048` above with `-newkey ec -pkeyopt ec_paramgen_curve:prime256v1`. Use OpenSSL 3: rradius cannot read the EC private keys generated by the LibreSSL built into macOS (`failed to parse private key`).
- **Memory of the tool:** about 15 MB at concurrency 512.
- The numbers are good only for comparison; real devices also differ in wireless, retransmission and the number of users.

### UDP

The UDP receive loop also works the way RadSec does: after it receives a packet, it first tries to receive the next one without waiting. If the next one is already there, it hands this packet to a new task, so the requests of one socket are spread across several threads. If not, it handles this one in the receive loop first and sends the response directly without waiting, and hands it to a new task only when it has to wait for the database, LDAP, HTTP or an upstream. The slot is held until the response is sent, the receive loop does not wait for the send (when it cannot send, it hands it to a new task), and overload discarding and retransmission detection are unchanged.

Measured on 2026-11-09 in the same environment (server CPUs 0–6, tool 7–13, `radsec.toml`, accounting through a script that writes one database row, PEAP with `eap.toml`), compared with the version before the change (`main` `5dc1c95`) in the same measurement, two runs each, with no timeouts:

| Load | Before (logins or requests/s) | After (logins or requests/s) | Server CPU per request |
| --- | --- | --- | --- |
| `pap,concurrency=64,window=64` (1 source socket) | 188,200–196,500 | 230,900–238,700 | about 21.5 → 18–18.5 µs |
| `pap,concurrency=64,window=8` (8 sockets) | 237,500–257,700 | 265,300–281,000 | about 24 → 20–21 µs |
| `pap,concurrency=256,window=4` (64 sockets) | 245,600–271,100 | 281,000–282,200 | about 23–24 → 20.3–20.4 µs |
| `acct-start,concurrency=64,window=8`, through the database | 68,100–69,500 | 76,300–77,100 | — |
| `peap,concurrency=64,window=8` | 8,990–9,000 | 9,270–9,370 | about 772 → 738–740 µs |

Always handling in the receive loop was not adopted: the packets of one NAS all land on the same socket (Linux assigns by source address and port) and would crowd onto one core.

- **A script that uses a lot of CPU and never has to wait holds the receive loop:** new packets of the same socket stay in the system receive buffer until it finishes; when the buffer is full the system drops them, and they are not counted in `rradius_radius_discarded_total{reason="overload"}`. This happens only when no other packet is waiting at that moment, and the same is true of RadSec (see above).
- If the handling inside the receive loop panics, only that request gets no response (one error line is logged), and the receive loop continues as usual.

### TACACS+

The `tacacs-pap`, `tacacs-ascii`, `tacacs-author` and `tacacs-acct` loads send TACACS+ (RFC 8907) to `--tacacs-port` (49 by default), and the packets are encrypted with `--secret`.

| Load | What it sends | Result |
| --- | --- | --- |
| `tacacs-pap` | Authentication START, PAP, with `--user` (or `--users`) and `--password` | PASS counts as `accept`, FAIL as `reject` |
| `tacacs-ascii` | Authentication START, ASCII, with no account; replies with the account after GETUSER and the password after GETPASS | The same; three round trips count as one login, and `rounds` is 3 |
| `tacacs-author` | Authorization REQUEST: `service=shell`, `cmd=show`, `cmd-arg=version` | PASS_ADD or PASS_REPL counts as `accept`, FAIL as `reject` |
| `tacacs-acct` | Accounting START record | SUCCESS counts as `accounting`, ERROR (the script did not accept) as `reject` |

- **One request at a time per connection.** rradius handles one connection in order, and sending more on the same connection only queues them, so `window` of a TACACS+ load is fixed at 1, and any other value is an invalid parameter; the number of connections equals `concurrency`, so for example `concurrency=64` is like 64 devices that each open one connection.
- **`--tacacs-connect keep` (default):** all connections are made before the load is sent and are then reused; if they cannot connect before the measurement, nothing is sent and the exit code is 3. When the server closes a connection, the request waiting for a response counts as `socket_errors`, the tool reconnects, and the number is recorded in `reconnects`.
- **`--tacacs-connect per-request`:** every request opens a new connection, which the tool closes after the last reply, and the latency includes setting up the connection. A failure to connect counts as `socket_errors`.
- **No retransmission.** A request that does not complete within `--timeout` counts as `timeouts` (for `tacacs-ascii` it is the three round trips of the whole login added together), and that connection is closed (`keep` reconnects), because a late reply would be taken for the reply of the next request. `--retries` does not apply to TACACS+ loads.
- **The reply must match to count.** The sequence number and the session id must match, and the lengths of the decrypted fields must match the packet length, or it counts as `invalid`. When the shared secret is wrong the decrypted result is garbage, and this is how it is caught; standard error prints `lengths: ...` once.
- **`concurrency` must not exceed `max_connections` of the server** (1024 by default) **or `max_connections_per_source`** (64 by default; all the tool's connections come from the same address). The server completes the TCP connection first and then closes the extra ones at once, so the tool cannot see it before the measurement, and both modes show it as `socket_errors`; `keep` also keeps reconnecting (`reconnects` increases, and a connection closed before it has received any reply is retried only every 100 ms). `tacacs.toml` raises both to 8192.
- **`per-request` leaves TIME_WAIT on the tool's side.** A connection closed by the tool stays on the tool's side for a while (about 60 seconds on Linux) before the port is released. On Linux the default usable ports are about 28,000 (`net.ipv4.ip_local_port_range` is 32768–60999), so if you keep opening more than about 470 new connections per second, the ports run out and you get `socket_errors` for failing to connect. `net.ipv4.tcp_tw_reuse` on Linux is 2 by default, which reuses TIME_WAIT ports only on loopback, so the limit does not apply when the tool and the server are on the same machine, and you meet it only when connecting to another machine. To measure at a higher rate, widen the range (for example `sysctl -w net.ipv4.ip_local_port_range="1024 65535"`) and set `net.ipv4.tcp_tw_reuse` to 1, or spread over several source addresses with `--bind`; `timewait` in `ss -s` shows how many there are now. On macOS the range is 49152–65535 (about 16,000 ports), TIME_WAIT is about 30 seconds, the limit is about 540 connections per second, and past that the error is `Can't assign requested address`.

Start the server with `tacacs.toml` in `rradius-server/benchmarks/`, and run the four loads one by one from the project root:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-pap,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-ascii,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-author,concurrency=64
```

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --load tacacs-acct,concurrency=64
```

A new connection for each request. When you connect to another machine or are on macOS, use a fixed rate below the TIME_WAIT limit above; a fixed concurrency opens connections as fast as it can and uses up the ports within seconds:

```bash
target/release/rradius-bench run --server 127.0.0.1 --secret bench-secret --tacacs-port 4949 --tacacs-connect per-request --load tacacs-pap,rate=300
```

Measured on 2026-11-08 on Linux (arm64) in OrbStack on the same Apple M4 Max, with the server limited to CPUs 0–6 and the tool to 7–13 (`threads=7`), `tacacs.toml`, three runs per row, all with no timeouts, `invalid`, socket errors or reconnects:

| Load | Connections | Requests/s | p50 / p99 | Server CPU per request |
| --- | --- | --- | --- | --- |
| `tacacs-pap,concurrency=64` | 64, `keep` | 313,500–323,700 | 0.18–0.19 / 0.54–0.56 ms | about 16 µs |
| `tacacs-ascii,concurrency=64` | 64, `keep` | 140,800–142,400 logins | 0.42 / 1.07–1.10 ms | about 31 µs (one login) |
| `tacacs-author,concurrency=64` | 64, `keep` | 336,200–371,500 | 0.16–0.17 / 0.46–0.58 ms | about 13–14 µs |
| `tacacs-acct,concurrency=64` | 64, `keep` | 372,500–385,600 | 0.15–0.16 / 0.46–0.48 ms | about 14 µs |
| `tacacs-pap,concurrency=512` | 512, `keep` | 413,200–430,500 | 1.06–1.10 / 3.4–3.5 ms | about 16 µs |
| `tacacs-pap,concurrency=64` | one per request, `per-request` | 16,600–21,300 | 0.97–1.09 / 13–36 ms | about 38–42 µs |

At a fixed 400 per second, the latency of `keep` is p50 0.89–0.99 ms and p99 2.8–4.0 ms, and for `per-request` it is p50 1.24–1.27 ms and p99 2.7–2.9 ms.

- **With persistent connections it is faster than RADIUS:** in the same environment, PAP over UDP is 246,800–265,600 per second at about 22–23 µs per request (the same measurement as RadSec; see the usage reference of rradius-enterprise, section 9, "RadSec"), and PAP over TACACS+ is about 16 µs per request. Authorization and accounting are a little less than PAP. The reason for either of these has not been confirmed.
- **An ASCII login is three round trips,** so one login uses about twice the CPU of PAP and has less than half the throughput.
- **A new connection per request is much more expensive:** the throughput is about 6% of `keep`, and the server spends about 22–26 µs more per request, on accepting and closing connections. The tool's threads were also busy 81–93% (two runs had the `tool_saturated` warning), so part of this number is the limit of the tool. During the measurement TIME_WAIT built up to the whole port range (28,232 ports); because it is loopback, they were reused through the default 2 of `tcp_tw_reuse`, and no errors appeared.
- **At low rates, p50 of `per-request` is about 0.3 ms more,** which is the round trip of setting up the connection. Real devices mostly keep a connection or open one every so often, depending on the device's configuration.

### Fixed concurrency and fixed rate

The format of `--load` is `KIND[,concurrency=N][,rate=N][,window=N][,threads=N][,alive=N]`. It can be repeated, and each load uses its own threads, so background pressure does not make the threads that measure logins queue. `KIND` can be `pap`, `chap`, `chap-authenticator`, `mschapv2`, `acct-start`, `acct-alive`, `acct-stop`, `acct-on`, `acct-off`, `status-auth`, `status-acct`, `session`, the EAP kinds `eap-tls`, `peap`, `ttls`, `ttls-mschapv2` and `aka-prime` (see "EAP logins" above, and the usage reference of rradius-enterprise, section 9, "EAP-AKA'"), and the TACACS+ kinds `tacacs-pap`, `tacacs-ascii`, `tacacs-author` and `tacacs-acct` (see "TACACS+" above).

- **Fixed concurrency (`concurrency=N`) measures throughput.** The next request is sent only after a response arrives, so the latency is mostly queueing and is not suitable for describing latency.
- **Fixed rate (`rate=N`) measures latency.** Requests are sent on a schedule without waiting for responses, and the latency is counted from the scheduled time, so when the server stalls, the later requests are counted too.

### Reading the report

- The Authenticator of every response is verified, and one that fails counts as `invalid` and never as a success.
- There is no retransmission by default, and a lost packet counts as `timeouts`. `--retries` turns retransmission on.
- Exit codes: 0 all succeeded, 1 some were rejected or got an Access-Challenge, 2 some timed out, failed verification or had a socket error, 3 a wrong argument or configuration.
- The warnings below make `trustworthy` false, and the numbers of this run cannot be taken as the capability of the server:

| Warning | Meaning |
| --- | --- |
| `tool_saturated` | The workers of the tool were busy more than 90%, so what was measured is the limit of the tool |
| `rate_not_met` | The fixed rate was not sent on schedule, usually because the CPU was taken away or the requested rate is more than the tool or the server can handle. When fewer than 1% of the scheduled requests were sent (the requests in the measuring window are sent only after the late schedule has been cleared, so `sent` in the report is 0), the message adds "cannot keep up with this rate": lower `rate`, or raise `concurrency` and `threads`. The exit code is not affected |
| `duplicate_path` | The server took the retransmission cache, and the script did not run |

- The warnings below do not affect `trustworthy`, and you must judge them yourself. A `trustworthy` of true also does not mean the server dropped no packets:

| Warning | Meaning |
| --- | --- |
| `server_discarded_<reason>` | The server dropped packets, for example when over a quota |
| `metrics_unavailable` | `--metrics-url` cannot be read, so it is also not known whether the retransmission cache was used or packets were dropped |

`--metrics-url` compares the counters of the server before and after the measurement. The server must have `[metrics]` turned on in its configuration.

### Limits of the tool itself

```bash
target/release/rradius-bench calibrate
```

`calibrate` runs four combinations against the responder built into the program, and measures the limit of the tool itself and the latency floor of this machine. When the throughput of the server is close to this limit, or the latency is close to this floor, measure on another machine.

### Limits of measuring on one machine

When the tool and the server are on the same machine they compete for CPU, so the numbers are good only for comparing before and after on the same machine and cannot be taken as the capability of a real network. In particular, in a scenario where the accounting script uses the whole CPU, the login latency includes the scheduling of both the tool and the server. To get numbers for a real network, put the tool on another machine and set the source address with `--bind`.

**Numbers measured at different times differ by 5–8%.** On 2026-11-09 the main loads were measured again with `main` `dd48f9c` (EAP, TACACS+ and EAP-AKA' all agree with the sections above), and the PAP of UDP and RadSec was 5–8% lower than the numbers in the earlier sections. The version at the merge of PR #86 and `dd48f9c` were run interleaved, three rounds each, in the same container, and the two showed no difference (UDP 267,000–280,000 against 274,000–279,000; RadSec with 7 connections 279,000–282,000 against 284,000–289,000; with 64 connections 274,000–275,000 against 270,000–274,000 per second), so it was the condition of that container, not a slower program. To compare two versions, run them interleaved in the same execution, and do not compare numbers from different times.
