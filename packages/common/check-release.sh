#!/bin/sh
# Checks a commit before a deb or an rpm is built from it:
#
#   - unless --no-ci-check is given, the Release workflow (release.yml) has passed for the commit
#     (.github/scripts/require-release-run.sh, the check packages/tarball/release.sh makes too);
#   - the version of rradius-server/Cargo.toml is also the version of packages/deb/debian/changelog,
#     packages/rpm/rradius.spec and packages/freebsd/net/rradius/Makefile. --no-ci-check does not
#     skip this: a package with the wrong version is wrong whether or not it is a release.
#
# Everything is read from the commit, not from the working tree, as `git archive <commit>` is what
# the packages are built from. A working tree with uncommitted changes gets a warning when the
# commit is HEAD.
#
# usage: packages/common/check-release.sh <commit> [--no-ci-check]
set -eu

usage() {
    echo "usage: packages/common/check-release.sh <commit> [--no-ci-check]" >&2
    exit 2
}
fail() {
    echo "check-release.sh: $*" >&2
    exit 1
}

commit=
ci_check=yes
while [ $# -gt 0 ]; do
    case "$1" in
        --no-ci-check) ci_check=no; shift ;;
        -*) usage ;;
        *) [ -z "$commit" ] || usage; commit=$1; shift ;;
    esac
done
[ -n "$commit" ] || usage

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
hash=$(git -C "$root" rev-parse --verify "$commit^{commit}" 2>/dev/null) || fail "no commit '$commit'"

if [ "$ci_check" = yes ]; then
    "$root/.github/scripts/require-release-run.sh" "$hash" || exit 1
fi

# The text of a file in the commit; empty when the file is not there.
show() {
    git -C "$root" show "$hash:$1" 2>/dev/null || true
}
cargo_file=rradius-server/Cargo.toml
changelog_file=packages/deb/debian/changelog
spec_file=packages/rpm/rradius.spec
makefile=packages/freebsd/net/rradius/Makefile
cargo=$(show "$cargo_file" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
changelog=$(show "$changelog_file" | sed -n '1s/^[^ ]* (\([^)]*\)).*$/\1/p')
spec=$(show "$spec_file" | sed -n 's/^Version:[[:space:]]*//p' | head -n 1)
bsd=$(show "$makefile" | sed -n 's/^DISTVERSION=[[:space:]]*//p' | head -n 1)

[ -n "$cargo" ] || fail "cannot read the version from $cargo_file"
bad=
for pair in "$changelog_file=$changelog" "$spec_file=$spec" "$makefile=$bsd"; do
    file=${pair%%=*}
    version=${pair#*=}
    [ -n "$version" ] || fail "cannot read the version from $file"
    if [ "$version" != "$cargo" ]; then
        echo "check-release.sh: $file: $version (Cargo.toml: $cargo)" >&2
        bad=yes
    fi
done
[ -z "$bad" ] || exit 1

if [ "$commit" = HEAD ] && [ -n "$(git -C "$root" status --porcelain)" ]; then
    echo "check-release.sh: warning: the working tree has uncommitted changes; packaging only $(echo "$hash" | cut -c1-12)" >&2
fi
echo "release check passed for $(echo "$hash" | cut -c1-12), version $cargo"
