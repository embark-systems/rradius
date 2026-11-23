# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

#### Changed

- **Breaking:** the subcommand that starts the service is now `serve`: `rradius-server serve [--config <path>]` (and `rradius-enterprise serve`). The name `server` is no longer accepted: it prints the usage and exits with status 2. Change the `ExecStart` of a systemd unit, the `command` of a Compose file, the `CMD` of a Dockerfile, the `args` of a Kubernetes container and the command of a FreeBSD rc script when you upgrade.

### 0.1.0

The first release.

#### Added

- **RADIUS:** authentication, accounting and Status-Server; PAP, CHAP, MS-CHAPv2, EAP-TLS, PEAP-MSCHAPv2 and EAP-TTLS; extended and vendor-specific attributes; the dictionaries in `share/`.
- **TACACS+:** authentication, authorization (permit and deny rules by device group) and accounting.
- **Account sources:** built-in tables with many password hash formats, LDAP and Active Directory, HTTP APIs and other databases, which can be chained.
- **Policies:** authentication and accounting logic in Luau scripts and SQL.
- **Clustering:** a built-in hiqlite cluster (SQLite replicated with Raft) of three or more nodes that keeps serving when one node is down; nodes can be added and removed while running; the configuration is reloaded with SIGHUP.
- **Operations:** Prometheus metrics, structured logs, `check` to validate a configuration, `status` (one node or the whole cluster; `--watch`, `--json`), and commands for users, sessions, queries and the cluster.
- **`status --tui`:** a full-screen view of `status`, one panel at a time. It is part of every build.
- **Documentation:** the user manual in `docs/manual/` (a guide by topic with deployment scenarios, and reference appendices for the configuration, commands, metrics and Lua API) and the usage guide in `docs/usage.md`. The scenario examples in `rradius-server/examples/scenarios/` are started by automated tests.
- **Packaging and deployment:** a Docker image and Compose files, Kubernetes and Synology examples, Prometheus alert rules, and packaging for deb, rpm, FreeBSD and tarball releases.

#### Known limits

- There are no metrics for bytes in and out, and none that count TACACS+ replies by status.
- The release has not been tried on physical NAS, router or switch hardware. The manual marks the steps that were not tried on the outside system they involve (for example Windows and Active Directory, Samba, Synology DSM and `snmp_notifier`), and the numbers carried over from earlier measurements as not measured again.
