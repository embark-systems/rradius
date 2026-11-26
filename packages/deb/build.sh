#!/bin/sh
# Builds the deb from the committed source: packages/deb/build.sh [--out <dir>] [--commit <rev>] [--no-ci-check]
# The Rust toolchain (1.99 or newer, as the Dockerfile uses), cmake, g++, make, debhelper and
# dpkg-dev must be installed; the build downloads the crates. Run it in a container of the rust
# image when the machine has no such toolchain (packages/README.md).
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$root/dist"
commit=HEAD
ci_flag=
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out=$2; shift 2 ;;
        --commit) commit=$2; shift 2 ;;
        --no-ci-check) ci_flag=--no-ci-check; shift ;;
        *) echo "usage: packages/deb/build.sh [--out <dir>] [--commit <rev>] [--no-ci-check]" >&2; exit 2 ;;
    esac
done
"$root/packages/common/check-release.sh" "$commit" $ci_flag
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/rradius-src"
git -C "$root" archive "$commit" | tar -x -C "$work/rradius-src"
cp -r "$work/rradius-src/packages/deb/debian" "$work/rradius-src/debian"
cp "$work/rradius-src/packages/common/rradius.service" "$work/rradius-src/debian/rradius.service"
(cd "$work/rradius-src" && dpkg-buildpackage -b -us -uc -d)
mkdir -p "$out"
cp "$work"/rradius_*.deb "$out/"
ls "$out"/rradius_*.deb
