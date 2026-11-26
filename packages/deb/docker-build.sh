#!/bin/sh
# Builds the deb in a Debian 12 container, for linux/amd64 or linux/arm64, from the committed
# source: packages/deb/docker-build.sh [--platform linux/amd64|linux/arm64] [--out <dir>] [--commit <rev>] [--no-ci-check]
#
# Debian 12 has the oldest glibc (2.36) of the releases still in use, and a deb depends on the
# glibc it was built against, so a deb built here installs on Debian 12 and 13 and on Ubuntu 22.04
# and later. The rust image supplies the toolchain (1.99, as the Dockerfile uses). A platform other
# than the machine's runs under emulation: slow, but the same build. The crates are cached in a
# Docker volume per platform.
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$root/dist"
commit=HEAD
platform=
ci_flag=
while [ $# -gt 0 ]; do
    case "$1" in
        --platform) platform=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --commit) commit=$2; shift 2 ;;
        --no-ci-check) ci_flag=--no-ci-check; shift ;;
        *) echo "usage: packages/deb/docker-build.sh [--platform linux/amd64|linux/arm64] [--out <dir>] [--commit <rev>] [--no-ci-check]" >&2; exit 2 ;;
    esac
done
case "$platform" in
    "" | linux/amd64 | linux/arm64) ;;
    *) echo "docker-build.sh: platform must be linux/amd64 or linux/arm64" >&2; exit 2 ;;
esac
"$root/packages/common/check-release.sh" "$commit" $ci_flag
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src" "$out"
git -C "$root" archive "$commit" | tar -x -C "$work/src"
cache=rradius-deb-cargo-$(echo "${platform:-native}" | tr / -)
# shellcheck disable=SC2086
docker run --rm ${platform:+--platform $platform} \
    -v "$cache:/usr/local/cargo/registry" \
    -v "$work/src:/src:ro" -v "$out:/out" \
    rust:1.99-bookworm sh -c '
        set -eu
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends cmake g++ make debhelper dpkg-dev fakeroot
        mkdir /build && cp -r /src /build/rradius && cd /build/rradius
        cp -r packages/deb/debian debian && cp packages/common/rradius.service debian/rradius.service
        dpkg-buildpackage -b -us -uc -d
        cp /build/rradius_*.deb /out/
    '
ls "$out"/rradius_*.deb
