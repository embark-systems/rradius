Name:           rradius
Version:        0.1.0
Release:        1%{?dist}
Summary:        RADIUS and TACACS+ server with Luau scripts and a built-in cluster database
License:        BSD-2-Clause AND CC-BY-4.0
URL:            https://github.com/embark-systems/rradius
Source0:        rradius-%{version}.tar.gz
ExclusiveArch:  x86_64 aarch64

# cargo is not a build requirement: the program needs a Rust newer than the distributions
# ship (the Dockerfile builds with 1.99), so it comes from rustup or the rust image.
BuildRequires:  cmake
BuildRequires:  gcc-c++
BuildRequires:  make
BuildRequires:  systemd-rpm-macros
Requires(pre): shadow-utils
%{?systemd_requires}

# Rust binaries carry no separate debug information the tools can split.
%global debug_package %{nil}
%global _build_id_links none

%description
rradius answers RADIUS and TACACS+ requests. Authentication and accounting logic is written in
Luau scripts and SQL; the data lives in an embedded hiqlite cluster, so a cluster needs no
external database.

The package installs the program, the RADIUS dictionaries and the documentation. It does not
create a configuration or start the service; see %{_docdir}/%{name}/README.packages.

%prep
%autosetup -n rradius-%{version}

%build
# As the Dockerfile builds: the query planner stability guarantee of the bundled SQLite, and
# a jemalloc that runs on 4, 16 and 64 KiB page kernels on aarch64.
export LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG
%ifarch aarch64
export JEMALLOC_SYS_WITH_LG_PAGE=16
%endif
cargo build --release --locked -p rradius-server

%install
install -D -m 0755 target/release/rradius-server %{buildroot}%{_bindir}/rradius-server
install -d %{buildroot}%{_datadir}/rradius
cp -a share/. %{buildroot}%{_datadir}/rradius/
install -d %{buildroot}%{_docdir}/%{name}
cp -a docs/manual %{buildroot}%{_docdir}/%{name}/manual
install -m 0644 docs/usage.md %{buildroot}%{_docdir}/%{name}/usage.md
cp -a rradius-server/examples %{buildroot}%{_docdir}/%{name}/examples
install -m 0644 packages/common/README.packages %{buildroot}%{_docdir}/%{name}/README.packages
install -D -m 0644 packages/common/rradius.service %{buildroot}%{_unitdir}/rradius.service
install -D -m 0644 packages/common/rradius.logrotate %{buildroot}%{_sysconfdir}/logrotate.d/rradius
install -d %{buildroot}%{_sysconfdir}/rradius
install -d -m 0750 %{buildroot}%{_sharedstatedir}/rradius %{buildroot}%{_localstatedir}/log/rradius

%pre
getent group rradius >/dev/null || groupadd -r rradius
getent passwd rradius >/dev/null || \
    useradd -r -g rradius -d %{_sharedstatedir}/rradius -s /sbin/nologin \
        -c "rradius RADIUS and TACACS+ server" rradius
exit 0

%post
%systemd_post rradius.service

%preun
%systemd_preun rradius.service

%postun
%systemd_postun_with_restart rradius.service

%files
%license LICENSE
%{_bindir}/rradius-server
%{_datadir}/rradius
%{_docdir}/%{name}
%{_unitdir}/rradius.service
%config(noreplace) %{_sysconfdir}/logrotate.d/rradius
%dir %{_sysconfdir}/rradius
%attr(0750,rradius,rradius) %dir %{_sharedstatedir}/rradius
%attr(0750,rradius,rradius) %dir %{_localstatedir}/log/rradius

%changelog
* Mon Nov 16 2026 Vanilla Hsu <v@embark.systems> - 0.1.0-1
- Package of rradius 0.1.0.
