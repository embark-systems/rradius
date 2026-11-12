# rradius

English | [繁體中文](README.zh_TW.md)

An AAA server for RADIUS and TACACS+, written in Rust. Authentication and accounting logic is written in Luau scripts and SQL, and data lives in a built-in hiqlite cluster (SQLite replicated with Raft), so no separate database is needed.

- **RADIUS:** authentication, accounting, Status-Server; PAP, CHAP, MS-CHAPv2, EAP-TLS, PEAP-MSCHAPv2, EAP-TTLS; extended and vendor-specific attributes.
- **TACACS+:** authentication, authorization (permit and deny rules by device group), accounting.
- **Account sources:** built-in tables (many password hash formats), LDAP / Active Directory, HTTP APIs, which can be chained.
- **Clustering:** three or more nodes form a cluster that keeps serving when any one node is down; send SIGHUP to reload a changed configuration.
- **Operations:** Prometheus metrics, structured logs, `--check` to validate the configuration, admin commands.

## Building and running

Requires Rust (stable).

```bash
cargo build --release
cd rradius-server
../target/release/rradius-server --config examples/scenarios/quick-start/rradius.toml
```

Docker works too: run `docker/init-secrets.sh` first, then `docker compose up --build` (single node) or use `compose.cluster.yaml` (three nodes); see [section 1.1 of the usage guide](docs/usage.md#11-docker). Release archives are made with `packaging/release.sh`; see [`packaging/README.md`](packaging/README.md).

## Documentation

The documentation is written in Traditional Chinese.

- [User manual](docs/manual/README.md): step-by-step guides by deployment scenario. Each scenario comes with a configuration you can start as is, in [`rradius-server/examples/scenarios/`](rradius-server/examples/scenarios/), covered by automated tests.
- [Usage guide](docs/usage.md): reference for every setting and feature.

## Repository layout

| Directory | Contents |
| --- | --- |
| `rradius-core/` | The server library: protocols, authentication chain, Lua scripts, cluster, configuration |
| `rradius-server/` | The `rradius-server` program, examples and tests |
| `rradius-bench/` | The load generator `rradius-bench` |
| `share/` | RADIUS dictionaries |

Tests: `cargo test --workspace --locked`.

## Enterprise edition

RadSec, realm-based proxying and roaming, Disconnect / CoA, NAS clients managed from a table, a Wi-Fi password per user, EAP-AKA', OCSP, EAP fast reconnect, and TACACS+ over TLS are supported by rradius-enterprise, provided separately. When rradius-server meets these settings, it says that they need rradius-enterprise.

## License

BSD 2-Clause; see [`LICENSE`](LICENSE). The RADIUS dictionaries in `share/` are under their own license; see [`share/LICENSE`](share/LICENSE) and [`share/NOTICE`](share/NOTICE).
