#!/bin/sh
# Builds the rpm from the committed source: packages/rpm/build.sh [--out <dir>] [--commit <rev>]
# The Rust toolchain (1.99 or newer, as the Dockerfile uses), cmake, gcc-c++, make, rpm-build and
# systemd-rpm-macros must be installed; the build downloads the crates (packages/README.md).
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$root/dist"
commit=HEAD
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out=$2; shift 2 ;;
        --commit) commit=$2; shift 2 ;;
        *) echo "usage: packages/rpm/build.sh [--out <dir>] [--commit <rev>]" >&2; exit 2 ;;
    esac
done
version=$(sed -n 's/^Version:[[:space:]]*//p' "$root/packages/rpm/rradius.spec")
top=$(mktemp -d)
trap 'rm -rf "$top"' EXIT
mkdir -p "$top/SOURCES"
git -C "$root" archive --prefix="rradius-$version/" -o "$top/SOURCES/rradius-$version.tar.gz" "$commit"
rpmbuild -bb --define "_topdir $top" "$root/packages/rpm/rradius.spec"
mkdir -p "$out"
cp "$top"/RPMS/*/rradius-*.rpm "$out/"
ls "$out"/rradius-*.rpm
