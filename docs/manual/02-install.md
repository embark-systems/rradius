# 02 Installation

> After this chapter you can:
> - Pick an installation method for your environment: source, deb, rpm, FreeBSD port, release tarball, Docker, Docker Compose or Synology
> - Know where the program, dictionary, configuration, data and logs are after installation
> - Write a first configuration file that passes `check`, and start the service
> - Use `--version` to confirm the version and the default configuration file path

Prerequisites: you have read [chapter 01](01-overview.md) and know the roles of the configuration file, the data directory and the dictionary.

In this chapter:

- [Choosing an installation method](#choosing-an-installation-method)
- [Where files are after installation](#where-files-are-after-installation)
- [Building from source](#building-from-source)
- [deb package (Debian, Ubuntu)](#deb-package-debian-ubuntu)
- [rpm package (RHEL family)](#rpm-package-rhel-family)
- [FreeBSD port](#freebsd-port)
- [Release tarball](#release-tarball)
- [Docker image](#docker-image)
- [Docker Compose](#docker-compose)
- [Synology NAS](#synology-nas)
- [First start after a package install](#first-start-after-a-package-install)
- [Checking the version and default configuration path](#checking-the-version-and-default-configuration-path)
- [Upgrading and removing](#upgrading-and-removing)
- [Common mistakes](#common-mistakes)
- [Related chapters](#related-chapters)

## Choosing an installation method

The repository provides build scripts and definition files for each installation method. In this manual, every method assumes you **build from source yourself** and then install the result on your machine:

| Method | Good for | Build script or definition file | Verification status in this manual |
| --- | --- | --- | --- |
| Source | Development, testing, following the exercises in this manual | `cargo` | Verified |
| deb | Debian 12 and 13, Ubuntu 22.04 and later | `packages/deb/` | Not verified on real hardware |
| rpm | RHEL family (for example Rocky, AlmaLinux) | `packages/rpm/` | Not verified on real hardware |
| FreeBSD port | FreeBSD | `packages/freebsd/net/rradius/` | rc.d script verified on real hardware, build not verified |
| Release tarball | You do not want a package manager, or you want to try it on macOS | `packages/tarball/release.sh` | Not verified on real hardware |
| Docker | Container environments | `Dockerfile` | Verified |
| Docker Compose | Single-host container deployment | `compose.yaml`, `docker/` | Verified |
| Synology | Container Manager on a Synology NAS | `deploy/synology/compose.yaml` | Not verified on real hardware |

The cluster Compose file (`compose.cluster.yaml`) and the Kubernetes manifests (`deploy/kubernetes/`) are described in [chapter 16](16-cluster.md).

All paths in this chapter are relative to the root of the rradius source repository.

> **Note:** The load-testing program `rradius-bench` is not in any package, tarball or Docker image. Chapter 03 uses it to test logins and chapter 19 uses it for load tests. To use it, build it as described in [Building from source](#building-from-source) and put it on a machine that can reach the server.

## Where files are after installation

The locations for each installation method are summarized below. The sources are `packages/deb/debian/rules`, `packages/rpm/rradius.spec`, `packages/freebsd/net/rradius/` and `Dockerfile`:

| | deb, rpm | FreeBSD port | Docker image |
| --- | --- | --- | --- |
| Executable | `/usr/bin/rradiusd` | `/usr/local/bin/rradiusd` | `/usr/local/bin/rradiusd` |
| Dictionary | `/usr/share/rradius/` | `/usr/local/share/rradius/` | `/usr/share/rradius/` |
| Configuration | `/etc/rradius/` (`conf.d/` is empty) | `/usr/local/etc/rradius/` (`conf.d/` is empty) | `/etc/rradius/`, you mount it in yourself |
| Default configuration file | `/etc/rradius/rradius.toml` | `/usr/local/etc/rradius/rradius.toml` | `/etc/rradius/rradius.toml` |
| Data | `/var/lib/rradius` (0750) | `/var/db/rradius` (0750) | `/var/lib/rradius` (VOLUME) |
| Log directory | `/var/log/rradius` (0750) | `/var/log/rradius` (0750) | None, logs go to standard error |
| Documents and examples | `/usr/share/doc/rradius/` | `/usr/local/share/doc/rradius/` (DOCS option) | None |
| Service | systemd `rradius.service` | `rc.d` `rradius` | The image `CMD` |
| Runs as | System user `rradius` | User `rradius` | User `rradius` (uid 10001) |

Notes:

- **The default configuration file path is fixed at build time.** If the environment variable `RRADIUS_PREFIX=<prefix>` is set when you build, the default path is `<prefix>/etc/rradius/rradius.toml`. If it is unset, empty or `/`, the path is `/etc/rradius/rradius.toml`. Environment variables at run time cannot change it. When the configuration file has no `dictionary`, the dictionary it reads follows the same prefix: `<prefix>/share/rradius/dictionary`, or `/usr/share/rradius/dictionary` when the prefix is unset. The FreeBSD port builds with `RRADIUS_PREFIX=/usr/local`; the other methods do not set it. `--version` shows the actual values (see [Checking the version](#checking-the-version-and-default-configuration-path)).
- **Packages do not create a configuration file and do not enable the service.** After installation you generate the configuration file, check it and then enable the service. The steps are in [First start after a package install](#first-start-after-a-package-install).
- **Logs go to standard error.** Under systemd the journal collects them. `/var/log/rradius` is for the accounting log file (`[radius.accounting_log]`, chapter 13).

## Building from source

Whichever method you end up using, it is worth doing this section once: the exercises in this manual use programs built from source.

### Required tools

- The Rust toolchain (`cargo`). The required version is newer than what Linux distributions ship, so install it with rustup. For the exact version, see the version used by `FROM rust:…` on the first line of `Dockerfile`.
- cmake, a C++ compiler (g++ or clang) and make.
- git.
- The build downloads dependencies from crates.io, so you need network access.

### Build

```console
$ git clone https://github.com/embark-systems/rradius.git
$ cd rradius
$ cargo build --locked --release -p rradius-server -p rradius-bench
…
    Finished `release` profile [optimized] target(s) in …
```

The output is in:

- `target/release/rradiusd`: the server and the management commands
- `target/release/rradius-bench`: the load-testing and login-testing tool

Notes:

- **Always add `--locked`.** It makes cargo build with the versions recorded in the repository's `Cargo.lock` and not upgrade dependencies on its own.
- The `.cargo/config.toml` in the repository sets `LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG`, a compile option the built-in SQLite needs. You do not need to set it when you build inside the repository.
- On arm64 Linux, the `Dockerfile`, deb, rpm and the FreeBSD port also set `JEMALLOC_SYS_WITH_LG_PAGE=16`, so the program runs on kernels with 4, 16 and 64 KiB pages. If you build on arm64 Linux yourself and move the program to another machine, set it too.

After the build, check that the program runs:

```console
$ ./target/release/rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
```

The text in parentheses on the first line of `--version` is the edition: `(community)` for the core edition and `(enterprise)` for the enterprise edition. Both editions' executables are named `rradiusd` and share the same configuration, data directory, service and user, so install only one of them on a machine. To keep both side by side for comparison, put them in different directories and run them by full path.

To try it with the example scenarios right away, continue with [chapter 03](03-quick-start.md).

To use the program you built from source as a production service, copy `target/release/rradiusd` to `/usr/local/bin`, copy the dictionary directory `share/` to a fixed location (for example `/usr/share/rradius`), and write your own service file. `packages/common/rradius.service` is a ready-made systemd unit you can use as a reference. An easier way, though, is to build a deb or an rpm.

## deb package (Debian, Ubuntu)

> **Not verified on real hardware:** This section was checked against the build scripts in `packages/deb/` and the contents of the `debian/` directory. The package was not rebuilt or installed while this manual was written.

### Build

There are two ways to build. Both build from **committed** source (using `git archive`), so uncommitted changes do not go into the package.

To build in a container, you only need docker on the machine:

```console
$ packages/deb/docker-build.sh --platform linux/amd64 --out dist
```

- `--platform` can be `linux/amd64` or `linux/arm64`. When it is not the local architecture, the build runs under emulation, which is slower but gives the same result.
- The container is Debian 12. The package depends on the build environment's glibc; a package built on Debian 12 also installs on newer distributions.
- `--commit <rev>` selects which commit to build. The default is `HEAD`.

To build directly on the local machine, you need the Rust toolchain, cmake, g++, make, debhelper and dpkg-dev:

```console
$ packages/deb/build.sh --out dist
```

The output is `dist/rradius_<version>_<architecture>.deb`, where the architecture is amd64 or arm64.

### Install

```console
$ sudo apt install ./dist/rradius_0.1.0_amd64.deb
```

What the package does:

- Installs `/usr/bin/rradiusd`, the dictionary in `/usr/share/rradius/`, and documents and examples in `/usr/share/doc/rradius/`.
- Creates the system user and group `rradius`, and creates `/var/lib/rradius` and `/var/log/rradius`, owned by `rradius` with mode 0750.
- Creates an empty `/etc/rradius/` and `/etc/rradius/conf.d/`.
- Installs the systemd unit `rradius.service` and the logrotate configuration `/etc/logrotate.d/rradius`.
- **Does not enable or start the service.**

Next, follow [First start after a package install](#first-start-after-a-package-install).

## rpm package (RHEL family)

> **Not verified on real hardware:** This section was checked against `packages/rpm/rradius.spec` and `packages/rpm/build.sh`. The package was not rebuilt or installed while this manual was written.

### Build

You need the Rust toolchain, cmake, gcc-c++, make, rpm-build and systemd-rpm-macros:

```console
$ packages/rpm/build.sh --out dist
```

The output is `dist/rradius-<version>-1.<dist>.<architecture>.rpm`, where the architecture is x86_64 or aarch64. The package depends on the build host's glibc, so build it on the distribution (or in a container of the distribution) you want to install it on.

### Install

```console
$ sudo dnf install ./dist/rradius-0.1.0-1.el9.x86_64.rpm
```

The installed files are the same as for deb: `/usr/bin/rradiusd`, `/usr/share/rradius/`, `/usr/share/doc/rradius/`, `/etc/rradius/` (with an empty `conf.d/`), `/var/lib/rradius` and `/var/log/rradius` (0750, owned by `rradius`), the systemd unit and the logrotate configuration.

Whether the service is enabled automatically depends on the distribution's systemd preset (the spec uses `%systemd_post`). RHEL-family distributions usually do not enable it. Either way, the service does not start when the configuration file does not exist; the reason is in the next section.

Next, follow [First start after a package install](#first-start-after-a-package-install).

## FreeBSD port

> **Partly verified on real hardware:** This section was checked against the Makefile, `pkg-plist` and rc.d script in `packages/freebsd/net/rradius/`. The rc.d script was started as the `rradius` user on FreeBSD 15.2-STABLE and ran stably. The port build and `pkg` installation were not tested.

### Build

1. Copy `packages/freebsd/net/rradius` to `net/rradius` in the ports tree.
2. Add `rradius` to `UIDs` and `GIDs` in the ports tree. The port uses `USERS` and `GROUPS` to create this user.
3. In the port directory, run `make makesum` to generate `distinfo`, then `make package`.

The port fetches the source for the tag `v<version>` from GitHub, so that version must already be tagged.

### After installation

- The executable is `/usr/local/bin/rradiusd` and the dictionary is `/usr/local/share/rradius/`.
- The data directory is `/var/db/rradius` and the log directory is `/var/log/rradius`, owned by `rradius`, 0750.
- An empty `/usr/local/etc/rradius/` and `conf.d/`.
- The default configuration file is `/usr/local/etc/rradius/rradius.toml`.
- With the DOCS option on, documents and examples are in `/usr/local/share/doc/rradius/`.

### rc.d service

The rc.d script `rradius` reads these `rc.conf` variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `rradius_enable` | `NO` | Set to `YES` to start the service |
| `rradius_config` | `/usr/local/etc/rradius/rradius.toml` | Configuration file |
| `rradius_user` | `rradius` | User to run as |

The service runs through `daemon(8)`. When the program exits, it restarts automatically after 2 seconds, and output goes to syslog (tag `rradius`). When `rradius_user` is set, rc.subr already runs the whole command as that user with `su -m`, so the script does not add `-u` to `daemon` (a non-root `daemon -u` fails at initgroups). The PID file is in `/var/run/rradius/`, which the script creates at start, owned by `rradius_user`. `service rradius reload` sends SIGHUP to the program, which reloads the configuration (chapter 04).

```console
# sysrc rradius_enable=YES
# service rradius start
```

> **Note:** The port's program runs as the `rradius` user and cannot bind ports below 1024. To serve TACACS+ on its standard port 49, use `mac_portacl(4)`, set `net.inet.ip.portrange.reservedhigh` to 0, or use a port above 1024. RADIUS ports 1812 and 1813 are not affected.

You can have newsyslog rotate the accounting log file. The template is `/usr/local/etc/newsyslog.conf.d/rradius.conf.sample`; it takes effect once you remove `.sample`.

## Release tarball

> **Not verified on real hardware:** This section was checked against `packages/tarball/release.sh`. A tarball was not generated or unpacked while this manual was written.

Generate the tarball with `packages/tarball/release.sh`:

```console
$ packages/tarball/release.sh --target linux-x86_64 --out dist
```

- `--target` can be `macos-aarch64`, `linux-x86_64` or `linux-aarch64`, and you can repeat it to give several. If you do not give it, the script builds for the local platform.
- `macos-aarch64` can only be built on an arm64 Mac. The Linux builds are made in Docker using the build stage of the `Dockerfile`.
- The script needs specific versions of `cargo-about` and `cargo-deny`, which it uses to check and list the licenses of dependencies. When the versions do not match, the script prints the install commands to run.
- Before it builds anything, the script checks with `gh` that the Release workflow (`release.yml`, which runs every other workflow for one commit) has passed for the commit. Start it with `gh workflow run release.yml --ref <ref>`. For a build that is not a release, such as one from a fork, add `--no-ci-check`.
- The output is `dist/rradius-<version>-<platform>.tar.gz` and the matching `.sha256`. Before packaging finishes, the script unpacks the tarball and checks the example scenarios with the program inside; if a check fails, no output is produced.

The directory layout in the tarball is the same as in the source repository, so the relative paths in the example configurations still work:

| Path | Contents |
| --- | --- |
| `bin/rradiusd` | The server and the management commands |
| `share/` | The dictionary, plus the dictionary's `LICENSE` and `NOTICE` |
| `rradius-server/examples/` | Example configurations, scripts and scenarios |
| `docs/` | Documentation |
| `LICENSE`, `THIRD-PARTY-LICENSES.md` | Licenses of rradius and its dependencies |
| `VERSION` | The version and the commit used for the build; the Linux build has an extra line `glibc <version>`, the minimum glibc it needs |

The tarball has no service file and does not create users or directories. If you use it as a production service, prepare all of these yourself.

## Docker image

The `Dockerfile` in the repository root has two stages: it builds with `cargo build --release --locked -p rradius-server` in a Rust image, then puts the program and the dictionary into a Debian slim image.

```console
$ docker build -t rradius-server .
…
$ docker run --rm rradius-server --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
```

What is in the image:

| Item | Value |
| --- | --- |
| Executable | `/usr/local/bin/rradiusd`, which is the `ENTRYPOINT` |
| Default command | `CMD ["serve", "--config", "/etc/rradius/rradius.toml"]` |
| Dictionary | `/usr/share/rradius/` |
| Runs as | `rradius` (uid 10001), not root |
| Data | `VOLUME /var/lib/rradius`, owned by `rradius` |
| EXPOSE | `1812/udp`, `1813/udp`, `49/tcp`, `8100/tcp`, `8200/tcp` |
| Other | `ca-certificates` is installed; LDAP over TLS uses the system CA certificates |

Because the `ENTRYPOINT` is `rradiusd`, whatever follows `docker run` is a subcommand, for example `docker run --rm rradius-server --version`.

The image has no configuration file. You must mount `/etc/rradius/rradius.toml` from outside. The easiest way is to use Compose, described in the next section.

> **Note:** EXPOSE lists the cluster ports 8100 and 8200, but without `[[cluster.nodes]]` and without `cluster.listen`, these two ports bind only to `127.0.0.1` and cannot be reached from outside the container. A single node does not need these ports open to the outside.

## Docker Compose

`compose.yaml` runs one node. The configuration is in `docker/config/`, and the secret files are generated by `docker/init-secrets.sh`.

### Steps

```console
$ sh docker/init-secrets.sh
created docker/secrets/raft.secret
created docker/secrets/api.secret
created docker/secrets/radius.secret
$ docker compose up --build -d
…
 Container …-rradius-1 Started
```

`init-secrets.sh` uses `openssl rand -hex 32` to generate three files: the cluster's Raft password, the API password, and the shared secret for RADIUS clients. Files that already exist are not overwritten, so it is safe to run again.

Check the logs to confirm the start succeeded:

```console
$ docker compose logs
…
rradius-1  | … INFO rradius_core::cli: cluster node 1 started
rradius-1  | … INFO rradius_core::cli: cluster node 1 has caught up with the cluster
rradius-1  | … INFO rradius_core::cli: Servers started. Press Ctrl+C to stop.
rradius-1  | … INFO rradius_core::radius: RADIUS Server listening on 0.0.0.0:1812 and 0.0.0.0:1813
…
rradius-1  | … INFO rradius_core::metrics: Metrics listening on 127.0.0.1:9812
```

Check the status inside the container:

```console
$ docker compose exec rradius rradiusd status --config /etc/rradius/rradius.toml
… WARN rradius_core::common::seal: cluster.secret_raft_file: '/run/secrets/raft' has mode 644, which others can reach or its group can write; make it 0600 or 0640
…
rradius status of 127.0.0.1:9812 over 1.0 s

Node
  Ready                     ok
  Leader                    1
…
```

> **Note:** The `has mode 644` warning here is expected. `init-secrets.sh` deliberately makes the secret files readable by everyone, because users inside the container are not users on the host and Compose does not change permissions when it mounts the files. What protects the secrets is the `docker/secrets/` directory itself (mode 700).

To stop:

```console
$ docker compose down
```

Adding `-v` also deletes the data volume, and the accounts and accounting data go with it.

### What this Compose file does

Settings in `compose.yaml`:

- Only `1812/udp` and `1813/udp` are published. The TACACS+ and metrics ports are not published to the host.
- The configuration directory `./docker/config` is mounted read-only at `/etc/rradius`. Data is in a volume named `data`, mounted at `/var/lib/rradius`.
- The three secret files are mounted as Compose secrets at `/run/secrets/raft`, `/run/secrets/api` and `/run/secrets/radius`.
- The root file system is read-only (`read_only: true`), `/tmp` is a tmpfs, all capabilities are dropped, and `no-new-privileges` is set. The server writes only to the data volume and `/tmp`, so this is enough.
- `stop_grace_period: 30s`: gives the database enough time to finish when stopping. Docker's default of 10 seconds may not be enough.
- `restart: unless-stopped`.

`docker/config/` contains:

| File | Contents |
| --- | --- |
| `rradius.toml` | The dictionary points to `/usr/share/rradius/dictionary`; the data directory is `/var/lib/rradius`; the sql handler queries the `users` table; one client for each of the three private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`); the metrics port is `127.0.0.1:9812` |
| `auth.lua` | Handles accounting only: writes each accounting request to the `accounting` table |
| `migrations/1_schema.sql` | Creates the two tables `users` and `accounting` |
| `cluster/` | Configuration for a three-node cluster, see chapter 16 |

> **Warning:** The default clients cover three entire private ranges, because the source addresses Docker gives containers fall in these ranges. Before production use, change them to the addresses or ranges where your NAS actually is (chapter 05).

### Adding accounts

This configuration has no `[accounts]`, so `user add` cannot be used. The approach suggested in the comments of `1_schema.sql` is to add a migration file with a higher number, for example `docker/config/migrations/2_users.sql`, containing `INSERT INTO users …`. For the format of the password column, see [chapter 07](07-users-and-chains.md).

You can also add `[accounts]` to `rradius.toml` as described in chapter 07, then manage accounts with `docker compose exec rradius rradiusd user add …`.

> **Note:** A migration file that has already been applied must not be changed. To change a table structure or add data, always add a file with the next number.

## Synology NAS

> **Not verified on real hardware:** This section was checked only against `deploy/synology/compose.yaml` and has not been run on a Synology NAS. The way to move the image to the NAS is this manual's recommendation; the repository has no ready-made script for it.

`deploy/synology/compose.yaml` lets Synology's Container Manager run one node as a "project". It is almost the same as the `compose.yaml` in the previous section, with two differences:

- **The image is not on a registry.** `image: rradius-server:latest` must be built beforehand on another machine with `docker build -t rradius-server .` and then imported into the NAS. For example, on the build machine run `docker save rradius-server:latest -o rradius-server.tar`, then import the file on the image page of Container Manager. Build the image for the same architecture as the NAS: x86_64 or arm64.
- **The secret files are mounted directly as files.** The three secret files are mounted read-only with ordinary bind mounts at `/run/secrets/`, without Compose secrets, so the Compose version on the NAS does not matter.

Lay out the directories on the NAS like this:

```text
/volume1/docker/rradius/compose.yaml   deploy/synology/compose.yaml
/volume1/docker/rradius/config/        a copy of docker/config/ (edit rradius.toml here)
/volume1/docker/rradius/secrets/       raft.secret, api.secret, radius.secret
```

You can generate the secret files on the build machine with `sh docker/init-secrets.sh` and then copy the three files in `docker/secrets/` over.

Data uses the named volume `data`, which Docker makes writable for the container's user (uid 10001). If you use a folder created in File Station instead, the container's user usually has no write permission.

### Pre-checks

| Item | Notes |
| --- | --- |
| Model | It must be able to install Container Manager (called Docker in DSM 6 through 7.1). It must show up in a search in Synology's Package Center; some entry-level models do not have it. Check your model on Synology's website |
| CPU architecture | Build a `linux/amd64` image for x86_64 models and `linux/arm64` for ARM models; the image must have the same architecture as the NAS |
| Memory | Memory use of a single node was not measured on DSM. If the same NAS runs many other services, check the available memory first |
| Network | The NAS needs a fixed address (a DHCP reservation or a manual setting), and your devices' RADIUS server address must point to it |
| Single point of failure | RADIUS is the entry to the network: when the NAS reboots or DSM updates, nobody can connect. If you can, use the three nodes on different machines from [chapter 16](16-cluster.md) instead of relying on one machine. Three containers on the same NAS are no more reliable than one |

### Building the image on another computer

The NAS CPU is slow and Rust takes a long time to compile, so build the image on another computer with Docker and move it over. `--platform` must match the NAS architecture:

```console
$ docker buildx build --platform linux/amd64 -t rradius-server:latest --load .
$ docker save rradius-server:latest | gzip > rradius-server.tar.gz
```

For an ARM NAS, replace `linux/amd64` with `linux/arm64`. When the `Dockerfile` builds on arm64, it fixes jemalloc's page size at 64 KiB, so the same arm64 image also runs on kernels with 4 KiB and 16 KiB pages. Building `linux/amd64` on an Apple Silicon computer uses emulation and is very slow.

### Steps on DSM

1. **Put the folders in place:** With File Station or SSH, create `/volume1/docker/rradius/` (the location of the `docker` shared folder depends on your setup), and put `compose.yaml`, `config/` and `secrets/` in it.
2. **Import the image:** Container Manager → "Image" → "Add" → "Add from file", and choose `rradius-server.tar.gz` (a plain `.tar` from `docker save` without gzip, as in the previous section, also works). After the import the name is `rradius-server:latest`, the same as `image` in `compose.yaml`.
3. **Edit the configuration:** Edit `config/rradius.toml`.
   - `[[radius.clients]]` accepts the three private ranges `10.0.0.0/8`, `172.16.0.0/12` and `192.168.0.0/16` by default (see the warning in [Docker Compose](#docker-compose)). Narrow it to your devices' addresses or ranges, and give each client a `name` so the logs and metrics can tell the devices apart.
   - If you want accounts, add `2_users.sql` to `config/migrations/` **before the first start** (see [Adding accounts](#adding-accounts)); a migration file that has been applied must not be changed.
   - Keep `[metrics]` bound to `127.0.0.1` only. Do not change it to an externally reachable address (it has no authentication).
4. **Create the project and start it:** Container Manager → "Project" → "Create", with the project name `rradius`, the path `/volume1/docker/rradius`, and the source "Use existing docker-compose.yml". Start it when done. At the first start, the server creates the database in the data volume and applies the migrations. When the project's "Log" page shows `RADIUS Server listening on`, it accepts requests.
5. **Open the firewall:** If the firewall under DSM "Control Panel" → "Security" → "Firewall" is enabled, allow your devices' addresses to reach UDP 1812 and 1813.
6. **Configure the devices:** Set the RADIUS server to the NAS address, the authentication port to 1812, the accounting port to 1813, and the shared secret to the contents of `secrets/radius.secret`.
7. **Try it once:** From another computer on the LAN that has `rradius-bench` ([Building from source](#building-from-source)):

   ```console
   $ rradius-bench run --server <NAS address> --auth-port 1812 --secret "$(cat radius.secret)" \
       --user <account> --password <password> --load pap,concurrency=1 --requests 1 | grep 'pap:'
   ```

   `accept 1` means it works; `reject 1` means the server answered and only the account or password is wrong. If there is no response, see the troubleshooting below.

> **Not verified on real hardware:** The DSM screen names and the order of operations above are written from Synology's documentation and the general behavior of Docker. They have not been run on real hardware.

### Troubleshooting on Synology

| Symptom | Cause and fix |
| --- | --- |
| The device gets no response, and the log shows no request | The firewall is blocking it, or the device has the wrong NAS address. On the NAS, use `sudo tcpdump -ni any udp port 1812` (if the tool is available) to see whether packets arrive |
| The log has `RADIUS packet from unknown client <address> discarded` | The source address is not in `[[radius.clients]]`. Look at the address in the log. Whether Docker keeps the original source address when it translates addresses for external packets under bridge networking was not verified on DSM. If every address shown is a Docker internal address (for example `172.17.0.1`), see the next row |
| All devices have the same source address | Remove `ports:` from `compose.yaml` and use `network_mode: host` instead, so the container uses the NAS network directly. Then the NAS's own services must not use 1812 or 1813 |
| It exits right after starting, with a permission error | The data directory was replaced with a folder created in File Station, and uid 10001 cannot write to it. Use the example's named volume, or change the owner of that folder to uid 10001 |
| The log says `Read-only file system` | The example `compose.yaml` makes the root file system read-only (`read_only: true`), and only the data volume and `/tmp` are writable. If the configuration has paths that point elsewhere (for example the accounting spool or the accounting log file), mount a volume for them, or point them under `/var/lib/rradius` |
| The project did not come up after the NAS rebooted | `restart: unless-stopped` brings the container up after Docker starts. If the package has not finished starting after the NAS reboot, wait a while and look again; if it still has not come up, start the project by hand |

When there is a configuration error (for example `migration file … changed after it was applied`), the container exits immediately, and the reason is in the project's log; see [chapter 18](18-operations.md).

### Why there is no Synology package

The repository has only `deploy/synology/compose.yaml`. It has no Synology package (SPK) definition and no registry that publishes the image. The reasons:

- A package format needs a separate build for each architecture and DSM version, and there is no signing or distribution channel.
- It has not been verified whether the user permissions a package uses can bind a low port such as TACACS+ port 49.
- A Docker image is a single image and behaves the same as on other platforms. A native package would first need DSM's glibc version, non-root port binding and the ARM page size verified.
- There is no public registry, so the image is moved with `docker save`. When there is a registry in the future, change `image` in `compose.yaml` to its address and the NAS will pull it by itself.
- The secret files use a bind mount and not Compose `secrets`, because the Docker Compose version on a NAS does not always support them.

### What is verified and what is not

`deploy/synology/compose.yaml` has `read_only`, `tmpfs`, `cap_drop` and `no-new-privileges`, and the same settings are in the `compose.yaml` in the repository root. This manual did not actually start anything with `deploy/synology/compose.yaml`. None of these are verified: the screens and behavior of DSM's Container Manager and the older Docker package, support on each model, the DSM firewall, whether address translation keeps the source address, Hyper Backup, and automatic start after a NAS reboot.

## First start after a package install

After you install the deb, the rpm or the FreeBSD port, you must create the configuration file yourself. The steps below are for Linux. On FreeBSD, replace `/etc/rradius` with `/usr/local/etc/rradius`, the dictionary with `/usr/local/share/rradius/dictionary`, and the data directory with `/var/db/rradius`, and start the service with the commands in the rc.d section above.

> **Not verified on real hardware:** The configuration below was verified locally with `check` (with the paths changed to a temporary directory), but the service was not started on a Linux machine with the package installed.

### 1. Generate the starter configuration

```console
# rradiusd config init > /etc/rradius/rradius.toml
```

`config init` prints a draft you can use as it is. The cluster password and the shared secret of the example client are randomly generated. `dictionary` is set to the default dictionary location of this version (`/usr/share/rradius/dictionary` for a package install). There is no `[[cluster.nodes]]`, so it is a single node. It has `include = ["conf.d/*.toml"]`, so the `.toml` files in `conf.d/` are read in as well. It has one `[metrics]`. With `--lang zh-TW`, the comments are in Chinese. The output is about 30 lines; to see every setting and its default value, use `config init --full`.

### 2. Change three things

The service can write only to `/var/lib/rradius` and `/var/log/rradius` (the unit sets `ProtectSystem=strict`), and relative paths in the configuration file are based on the directory the configuration file is in. So the draft's `data_dir = "data"` becomes `/etc/rradius/data`, which the service cannot write to. Change it to:

```toml
# /etc/rradius/rradius.toml
# … (other settings omitted)
[cluster]
data_dir = "/var/lib/rradius"
# Single machine: bind the cluster ports to localhost only
listen = "127.0.0.1"
```

- Cluster: the draft has no `[[cluster.nodes]]`; it is a single node listening on `127.0.0.1`, so you do not need to set `listen`. Add `[[cluster.nodes]]` and set `listen` only when you build a cluster; see chapter 16.
- The draft's `[lua]` points to `auth.lua`. You must create this file yourself, at `/etc/rradius/auth.lua`. For how to write scripts, see chapter 10. If you only want the configuration to pass the check for now, you can write one line, `return "reject"`.
- Replace the example client `127.0.0.1` with your NAS (chapter 05).

If you later use the accounting spool (`dir` in `[radius.accounting_spool]`), put it under `/var/lib/rradius`. Put the accounting log file (`file` in `[radius.accounting_log]`) under `/var/log/rradius`.

### 3. Permissions of the configuration files

The configuration file contains passwords, so let only the `rradius` group read it:

```console
# chown root:rradius /etc/rradius/rradius.toml /etc/rradius/auth.lua
# chmod 640 /etc/rradius/rradius.toml /etc/rradius/auth.lua
```

Files in `conf.d/` follow the same rule: only `rradius` can read them and only root can write them. An included file can change who can log in, and the server warns about files that the group or others can write to.

### 4. Check, then start

```console
# rradiusd check
configuration ok
# systemctl enable --now rradius
```

Without `--config`, `check` reads the default configuration file `/etc/rradius/rradius.toml`. On success it prints `configuration ok` and exits with code 0; on an error it exits with code 2.

### Key points of the systemd unit

`rradius.service` (the source is `packages/common/rradius.service`):

- `ExecStart=/usr/bin/rradiusd serve`: reads the default configuration file.
- `ConditionPathExists=/etc/rradius/rradius.toml`: when the configuration file does not exist, the service does not start.
- Runs as the `rradius` user. `AmbientCapabilities=CAP_NET_BIND_SERVICE` lets it bind the TACACS+ port 49.
- `Restart=on-failure`, `RestartSec=2`: when the program exits abnormally, it restarts after two seconds.
- `TimeoutStopSec=60`: gives a cluster node enough time to stop.
- `ExecReload` sends SIGHUP: `systemctl reload rradius` reloads the configuration and reopens the accounting log file (chapter 04).
- `LimitNOFILE=65536`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`.

The logrotate configuration `/etc/logrotate.d/rradius` rotates `/var/log/rradius/*.jsonl` every day, keeps 30 copies, and sends SIGHUP after rotation so the program opens a new file. It does not use `copytruncate`, because that would lose data.

## Checking the version and default configuration path

```console
$ rradiusd --version
rradiusd 0.1.0 (community)
default config: /etc/rradius/rradius.toml
default dictionary: /usr/share/rradius/dictionary
```

- The first line is the program name and version.
- The second line is the configuration file read when there is no `--config`. For a program built by the FreeBSD port, this is `/usr/local/etc/rradius/rradius.toml`.
- The third line is the dictionary read when the configuration file has no `dictionary`. For a program built by the FreeBSD port, this is `/usr/local/share/rradius/dictionary`. The package installs the dictionary here, so you do not need to write it.

`--version` does not read the configuration file, does not open any port and does not create any directory, so it is always safe to run.

## Upgrading and removing

> **Not verified on real hardware:** This section is based on the deb `rules` and `postrm` and the rpm spec. No upgrade or removal was actually done.

- **Upgrade:** The package does not overwrite your edited `/etc/rradius/rradius.toml` and does not touch `conf.d/`. If the service is running, both deb and rpm restart it after the upgrade.
- **Removal:** The data (`/var/lib/rradius`), the logs (`/var/log/rradius`) and the `rradius` user are kept, because they hold the cluster database and the accounting data.
- **deb purge:** It deletes only `/etc/rradius/conf.d` and `/etc/rradius`, and only when they are empty directories.
- **Docker:** Replace the image and recreate the container; the data is in the volume. `docker compose down -v` deletes the volume.

## Common mistakes

**Symptom:** `rradiusd check` prints:

```text
/etc/rradius/rradius.toml: cannot read configuration: No such file or directory (os error 2)
```

**Cause:** The default configuration file does not exist. The package does not create it for you.
**Fix:** Generate it with `config init` as in [First start after a package install](#first-start-after-a-package-install), or give another file with `--config`. When you start with `serve`, the error message has an extra line below it: `(the default configuration; give --config <path> for another)`.

**Symptom:** `systemctl start rradius` shows no error, but the service is not running.
**Cause:** `/etc/rradius/rradius.toml` does not exist, so the unit's `ConditionPathExists` is not met and the service is skipped.
**Fix:** Create the configuration file, and start the service after `check` passes.

**Symptom:** `check` prints `/usr/share/rradius/dictionary: cannot read dictionary: No such file or directory (os error 2)`.
**Cause:** You are not using a package or Docker, and the dictionary is not at the default location from `config init`.
**Fix:** Change `dictionary` to where the dictionary really is (`share/dictionary` in the source and in the tarball), or put the dictionary at the default dictionary path and delete the `dictionary` line.

**Symptom:** `check` prints `cannot read script 'auth.lua': No such file or directory (os error 2)`.
**Cause:** The `config init` draft points to `auth.lua`, and you must create this file yourself.
**Fix:** Create `auth.lua` next to the configuration file (chapter 10).

**Symptom:** `check` passes, but `serve` prints `cannot start cluster node 1: …` and exits with code 2.
**Cause:** The data directory is not writable. The most common cause is keeping the draft's `data_dir = "data"`, which becomes `/etc/rradius/data` in a package install. `check` does not try to write to the data directory, so it cannot catch this.
**Fix:** Change `data_dir` to `/var/lib/rradius` (`/var/db/rradius` on FreeBSD), and make sure the owner is `rradius`.

**Symptom:** `check` or another command shows the warning `cluster.listen is not set, so the cluster's Raft and API ports are on all interfaces …`.
**Cause:** The configuration has `[[cluster.nodes]]` but `cluster.listen` is not set (the `config init` draft has no `[[cluster.nodes]]`, so it does not show this).
**Fix:** On a single machine, set `listen = "127.0.0.1"`, or remove `[[cluster.nodes]]`. For clusters, see chapter 16.

**Symptom:** In a Compose deployment, command output keeps showing `has mode 644, which others can reach or its group can write`.
**Cause:** `init-secrets.sh` deliberately sets the secret files to 644; see the explanation in [Docker Compose](#docker-compose).
**Fix:** In a Compose deployment you can ignore it. When you are not using Compose, change the secret files to 0600 or 0640.

**Symptom:** On FreeBSD, TACACS+ is not listening on port 49.
**Cause:** The port runs as an ordinary user and cannot bind ports below 1024.
**Fix:** See the note in [FreeBSD port](#freebsd-port).

## Related chapters

- [01 Getting to know rradius](01-overview.md)
- [03 First start and first login](03-quick-start.md)
- [04 Configuration file basics](04-configuration.md)
- [05 RADIUS service and NAS clients](05-radius-clients.md)
- [10 Writing policies in Lua](10-lua-policies.md)
- [16 Clusters and high availability](16-cluster.md)
- [17 Monitoring, status and logs](17-monitoring.md)
- [20 Security hardening](20-security.md)
