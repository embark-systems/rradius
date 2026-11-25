# rradius

An AAA server for RADIUS and TACACS+. The authentication and accounting logic is written in Luau scripts and SQL, and the data lives in a built-in hiqlite cluster. The Traditional Chinese version of this file is [README.zh-TW.md](README.zh-TW.md).

## Contents

| Path | Contents |
| --- | --- |
| `bin/rradiusd` | The server, which is also the administration command (`check`, `user`) |
| `share/` | The RADIUS dictionaries; `dictionary` in the configuration file points to `share/dictionary` |
| `rradius-server/examples/scenarios/` | The configuration, table migrations and Lua scripts of each scenario of the user manual; you can start them as they are |
| `rradius-server/examples/` | Other example configurations and scripts |
| `docs/manual/` | The user manual (English); start with `docs/manual/README.md` |
| `docs/manual.zh-TW/` | The user manual (Traditional Chinese); start with `docs/manual.zh-TW/README.md` |
| `docs/usage.md` | The reference of every setting and feature (English) |
| `docs/usage.zh-TW.md` | The reference of every setting and feature (Traditional Chinese) |
| `VERSION` | The version and the commit it was built from; a Linux build has one more line, `glibc <version>`, the oldest glibc it needs |

The directory layout is the same as the source repository, so the relative paths in the example configurations and the documents work as they are.

The enterprise features (RadSec, forwarding by realm, Disconnect and change of authorization (CoA), dynamic clients, EAP-AKA', OCSP) need rradius-enterprise, whose archive also carries the documents and scenarios of those features.

## Quick start

```bash
cd rradius-server
../bin/rradiusd check --config examples/scenarios/quick-start/rradius.toml
../bin/rradiusd serve --config examples/scenarios/quick-start/rradius.toml
```

The user manual assumes the executable is on your `PATH`; its first step is `export PATH="$PWD/target/release:$PATH"` (in the root of the source repository). With this archive, run `export PATH="$PWD/bin:$PATH"` in the unpacked root instead; everything else is the same. See `docs/manual/03-quick-start.md`.

- The secret files of the examples (`*.secret`) hold sample values; always replace them in production.
- A running example creates directories such as `data/` in its own directory; to start over, stop the server and delete them.
- Forming a cluster does not need CA certificates (`ca-certificates`) to be installed. To use LDAP over TLS or to call HTTPS from a script with `http_request`, the host needs the system CA certificates, or you set `ca_file` in the configuration.
- In production, put the dictionaries in a fixed place (for example `/usr/share/rradius`) and point `dictionary` in the configuration file there.
