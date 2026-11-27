# Packages

The Traditional Chinese version of this file is [README.zh-TW.md](README.zh-TW.md).

Packaging files for installing on a host. Each package is built from committed source, and the build downloads crates, so it does **not** meet the "offline build" requirement of the official Debian, Fedora and FreeBSD repositories. The purpose here is to build the packages yourself and publish them from your own repository. For the release archives, see [`tarball/`](tarball/README.md).

| Directory | Contents | Output |
| --- | --- | --- |
| `common/` | Files shared by all three package types: the systemd service, the logrotate configuration, and the post-install notes `README.packages` | |
| `deb/` | The `debian/` directory and `build.sh` | `rradius_<version>_<arch>.deb` (amd64, arm64) |
| `rpm/` | `rradius.spec` and `build.sh` | `rradius-<version>-1.<dist>.<arch>.rpm` (x86_64, aarch64) |
| `freebsd/` | The `net/rradius` port and `generate-crates.py` | FreeBSD ports |

## What the installation looks like

| | Linux (deb, rpm) | FreeBSD |
| --- | --- | --- |
| Executable | `/usr/bin/rradiusd` | `/usr/local/bin/rradiusd` |
| Dictionaries | `/usr/share/rradius/` | `/usr/local/share/rradius/` |
| Configuration | `/etc/rradius/` (default `rradius.toml`) | `/usr/local/etc/rradius/` |
| Data | `/var/lib/rradius` | `/var/db/rradius` |
| Logs | `/var/log/rradius` | `/var/log/rradius` |
| Documentation, examples | `/usr/share/doc/rradius/` | `/usr/local/share/doc/rradius/` |
| Service | systemd `rradius.service` | `rc.d/rradius` |
| User | system user `rradius` | user `rradius` |

The core and enterprise executables have the same name (`rradiusd`) and share the same configuration, data directory, service and user, so install only one of them on a machine. Run `rradiusd --version` to see which edition you have: the core prints `(community)` and the enterprise edition prints `(enterprise)`. If the enterprise edition later ships a deb or rpm too, both packages install `/usr/bin/rradiusd`, so the enterprise package must declare a conflict with the core (deb uses `Conflicts` and `Replaces`, rpm uses `Conflicts` and `Obsoletes`); otherwise the package manager reports a file conflict.

The default configuration file path is fixed at build time (`RRADIUS_PREFIX`, see section 1 of the [usage reference](../docs/usage.md)): the Linux packages leave it unset, so it is `/etc/rradius/rradius.toml`; FreeBSD sets it to `/usr/local`.

The packages **do not create a configuration file and do not enable the service**: the deb neither enables nor starts it; the rpm follows the distribution's default (Rocky and similar usually do not enable it); and the service does not start while the configuration file is missing (`ConditionPathExists`). After installing, follow [`common/README.packages`](common/README.packages) (on the host it is `/usr/share/doc/rradius/README.packages`): run `rradiusd config init` to generate a starter configuration, change the dictionary path, run `check`, and start the service. Removing the package keeps the data, logs and user.

## Building

The glibc version a package depends on follows the build host. The deb is built with `deb/docker-build.sh` in a Debian 12 container (amd64 or arm64; the other architecture runs under emulation, which is slow but gives the same result), and it depends on glibc 2.34 or later, so it installs on Debian 12 and 13 and on Ubuntu 22.04 and later. The rpm works the same way: to install on an older distribution, build on that distribution.

All packages need Rust 1.99 or later (the same as the `Dockerfile`), cmake, g++ and make. Rust is newer than what the distributions ship, so the packages do not list cargo as a build dependency; provide it with rustup or the `rust` image. The build environment variables are the same as in the `Dockerfile`: `LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG`, plus `JEMALLOC_SYS_WITH_LG_PAGE=16` on arm64; `debian/rules`, `rradius.spec` and the port already set them. FreeBSD also needs `ac_cv_func__malloc_thread_cleanup=no` and `ac_cv_func__pthread_mutex_init_calloc_cb=no`, which the port and the repository's `.cargo/config.toml` set: an `rradiusd` built without them refuses to start, and a `target` directory built before needs `cargo clean -p tikv-jemalloc-sys` first (for a release build, `cargo clean --release -p tikv-jemalloc-sys`).

```bash
# deb: also needs debhelper and dpkg-dev
packages/deb/build.sh --out dist

# deb, built in a Debian 12 container (needs only docker)
packages/deb/docker-build.sh --platform linux/amd64 --out dist

# rpm: also needs rpm-build and systemd-rpm-macros
packages/rpm/build.sh --out dist
```

Both scripts take the committed content with `git archive` (`--commit <rev>` selects which commit), so uncommitted changes do not go into the package.

Before building, each script runs `common/check-release.sh` on that commit: the Release workflow must have passed for the commit (the same check as `tarball/release.sh`, which needs `gh` signed in), and the version in `rradius-server/Cargo.toml` must be the version in `deb/debian/changelog`, `rpm/rradius.spec` and `freebsd/net/rradius/Makefile`. A mismatch names each file and stops the build. `--no-ci-check` skips only the workflow check (for a build in a container without `gh`, or of a commit that is not on GitHub); the version check always runs.

FreeBSD: put `freebsd/net/rradius` under `net/` in the ports tree, add `rradius` to the `UIDs` and `GIDs` files of the ports tree (the port creates the user with `USERS` and `GROUPS`), run `make makesum` to generate `distinfo`, then `make package`. `Makefile.crates` is generated from `Cargo.lock` by `freebsd/generate-crates.py`; rerun it whenever `Cargo.lock` changes (`--check` only checks whether it is out of date). The port fetches the GitHub tag `v<version>`, so the tag must exist before you publish.

## What has been verified

| | Status |
| --- | --- |
| deb | Built with `docker-build.sh` in a Debian 12 container for amd64 (emulated) and arm64, depending on glibc 2.34 or later; both were installed in a Debian 12 container: contents, user and directories, the default configuration file path `/etc/rradius/rradius.toml`, `config init` runs, permissions 0750. Earlier it was also built in an Ubuntu 24.04 arm64 container, and removal (user kept) and the service not enabling itself were verified. **Verified on 2026-11-18 on a Debian 12 arm64 machine with systemd (OrbStack):** after installation the service was neither enabled nor started, `conf.d/` was present, `check` passed after `config init`, `systemctl enable --now` started it and it served requests, `status` was normal, files in `conf.d/` were read into `config show`, `restart` stopped it cleanly, after an upgrade (`--reinstall`) the service was still running (see the fix in the next row), removal kept the configuration, data and user, and `purge` removed only the empty directories. The same set of checks was also run on a Debian 12 amd64 machine (OrbStack, x86_64) against the amd64 deb, with the same results |
| deb upgrade | Found on 2026-11-18: `dh_installsystemd --no-start` stops the running service on upgrade and does not start it again, so after `apt install --reinstall` the service was "enabled but stopped". After changing it to use only `--no-enable` (the fix is in `debian/rules`), a fresh install still neither enables nor starts the service, and after an upgrade the service restarts and keeps serving |
| rpm | Built in a Rocky Linux 9 (aarch64) container; contents, installation (`--nodeps`, since the container has no systemd) and removal (user kept) were checked. **Built and verified on 2026-11-18 on an AlmaLinux 9 aarch64 machine with systemd (OrbStack):** `rpmbuild` built it with Rust 1.99, after `dnf install` the service was neither enabled nor started, `config init`, `check` and `systemctl enable --now` worked and the service served requests, `status` was normal, `dnf reinstall` restarted the service (the process ID changed and it kept running), and `dnf remove` stopped the service and kept the user, configuration and data, and the empty `conf.d/` was removed with the package. SELinux is Disabled on that machine, so the same set of checks was also run on a full AlmaLinux 9 aarch64 virtual machine (SELinux enforcing, targeted policy, started with QEMU): installation, startup, `status`, restart on upgrade and removal all passed, with no AVC denial records; the package ships no SELinux policy, so the process runs as `unconfined_service_t` (unconfined, not protected by policy). The x86_64 rpm was also built on an AlmaLinux 9 x86_64 machine (OrbStack) and given the same set of checks (installation, startup, `status`, restart on upgrade, removal), with the same results |
| `conf.d/` | The definitions of all three packages create an empty `/etc/rradius/conf.d` (`etc/rradius/conf.d` on FreeBSD): `deb/debian/rules` and `rradius.postrm`, the `%dir` in `rpm/rradius.spec`, and the `@dir` in `pkg-plist`; the explanation is in `common/README.packages` and the FreeBSD `README.packages`. **Only the definition files were checked; the packages were not rebuilt and the installation was not verified** |
| Service file | There is only one copy, `common/rradius.service`; `deb/build.sh` copies it into `debian/` at build time. |
| Service file verification | `systemd-analyze verify` reports no errors; the service was not actually started |
| FreeBSD port | **Not verified on FreeBSD.** There is no `distinfo` (generate it on FreeBSD with `make makesum`), and `make`, `portlint` and `poudriere` have not been run; the rc.d script passed only `sh -n`; the number of entries in `Makefile.crates` matches `Cargo.lock` |
| Version and updates | The version comes from `rradius-server/Cargo.toml`; when you change it, also update `deb/debian/changelog`, `rpm/rradius.spec` and `freebsd/net/rradius/Makefile`; `common/check-release.sh` checks it when a deb or rpm is built |
