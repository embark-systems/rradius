#!/bin/sh
# Builds the rpm from the committed source: packages/rpm/build.sh [--out <dir>] [--commit <rev>] [--no-ci-check]
# The Rust toolchain (1.99 or newer, as the Dockerfile uses), cmake, gcc-c++, make, rpm-build and
# systemd-rpm-macros must be installed; the build downloads the crates (packages/README.md).
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
        *) echo "usage: packages/rpm/build.sh [--out <dir>] [--commit <rev>] [--no-ci-check]" >&2; exit 2 ;;
    esac
done
"$root/packages/common/check-release.sh" "$commit" $ci_flag
# The version and the spec are those of the commit, as the source is.
spec_text=$(git -C "$root" show "$commit:packages/rpm/rradius.spec")
version=$(printf '%s\n' "$spec_text" | sed -n 's/^Version:[[:space:]]*//p')
top=$(mktemp -d)
trap 'rm -rf "$top"' EXIT
mkdir -p "$top/SOURCES"
printf '%s\n' "$spec_text" > "$top/rradius.spec"
git -C "$root" archive --prefix="rradius-$version/" -o "$top/SOURCES/rradius-$version.tar.gz" "$commit"
rpmbuild -bb --define "_topdir $top" "$top/rradius.spec"
mkdir -p "$out"
cp "$top"/RPMS/*/rradius-*.rpm "$out/"
ls "$out"/rradius-*.rpm
