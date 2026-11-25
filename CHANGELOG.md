# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

The first release.

### Added

- **RADIUS:** authentication, accounting and Status-Server; PAP, CHAP, MS-CHAPv2, EAP-TLS, PEAP-MSCHAPv2 and EAP-TTLS; extended and vendor-specific attributes; the dictionaries in `share/`.
- **Executable name:** the program is `rradiusd`, in this edition and in the enterprise edition, so that moving from one to the other replaces one file. `rradiusd --version` prints `(community)` or `(enterprise)` after the version. The crates, the container image and the release archives keep their names.
- **Cluster recovery:** the cluster's node-to-node connections close after 10 seconds without an acknowledgement (Linux `TCP_USER_TIMEOUT`), so a follower cut off from the others and reconnected resumes in about 5 to 7 seconds instead of 15 to 47, without tuning `net.ipv4.tcp_retries2`.
- **Minimal `config init`:** `rradiusd config init` prints a working configuration of about 30 lines (the default dictionary path, `include`, `[lua]`, a single-node `[cluster]`, `[radius]` with one example client, and `[metrics]`); `config init --full` prints the previous commented reference of every setting and its default. Both take `--lang en|zh-TW`.
- **Default dictionary:** `dictionary` can be left out of the configuration: the dictionary is then read from `<prefix>/share/rradius/dictionary`, or `/usr/share/rradius/dictionary` without a build-time `RRADIUS_PREFIX`; `--version` prints it on a third line.
- **Traffic bytes:** `rradius_traffic_bytes_total{protocol,direction}` counts the bytes of RADIUS (UDP) and TACACS+ packets received and written, and `status` shows them as `In bytes` and `Out bytes`. They are protocol packets, without the headers of IP, UDP, TCP and TLS, and without RadSec and proxy traffic. `status` shows them in B, KiB, MiB, GiB and TiB (1024 each); `--json` and the metrics keep plain bytes.
- **Port block lookup (CGNAT):** the accounting log keeps each RFC 8045 `IP-Port-Range` as its own object in an `IP-Port-Range` list, next to the flat keys it always had, and `rradiusd accounting find --ip <address> --port <port> --at <time>` reads the log files and lists who held that public address and port at that time, with the attributes the report has and which time (`Event-Timestamp` or the received time) it relied on. A `Stop` of the same session ends its blocks, which RFC 8045 does not say and the output marks. A `--file` whose name ends in `.gz` is decompressed (several gzip members in a row included; standard input is not), it keeps nothing itself and sets no retention period. Not tried against a real CGN or BNG.
- **TACACS+:** authentication, authorization (permit and deny rules by device group) and accounting.
- **Account sources:** built-in tables with many password hash formats, LDAP and Active Directory, HTTP APIs and other databases, which can be chained.
- **Policies:** authentication and accounting logic in Luau scripts and SQL.
- **Clustering:** a built-in hiqlite cluster (SQLite replicated with Raft) of three or more nodes that keeps serving when one node is down; nodes can be added and removed while running; the configuration is reloaded with SIGHUP.
- **Operations:** Prometheus metrics, structured logs, `check` to validate a configuration, `status` (one node or the whole cluster; `--watch`, `--json`), and commands for users, sessions, queries and the cluster.
- **`status --tui`:** a full-screen view of `status`, one panel at a time, with a table of the items and a line chart of the last 60 seconds of the selected rate; in a cluster, `[` and `]` choose all the members added up or one member, and `p` pauses. It is part of every build.
- **Documentation:** the user manual in `docs/manual.zh-TW/` (a guide by topic with deployment scenarios, and reference appendices for the configuration, commands, metrics and Lua API) and the usage guide in `docs/usage.md`. The scenario examples in `rradius-server/examples/scenarios/` are started by automated tests.
- **Packaging and deployment:** a Docker image and Compose files, Kubernetes and Synology examples, Prometheus alert rules, and packaging for deb, rpm, FreeBSD and tarball releases.

### Known limits

- The byte counters count protocol packets only, without the headers of IP, UDP, TCP and TLS, and in this edition only RADIUS and TACACS+ traffic and CoA/Disconnect datagrams; RadSec and proxy traffic are counted by the enterprise edition.
- The release has not been tried on physical NAS, router or switch hardware. The manual marks the steps that were not tried on the outside system they involve (for example Windows and Active Directory, Samba, Synology DSM and `snmp_notifier`), and the numbers carried over from earlier measurements as not measured again.
