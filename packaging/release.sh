#!/usr/bin/env bash
# Builds release archives of rradius-server: the program, its dictionaries, the examples of the user
# manual, the documentation and the licenses of the crates it is built from, laid out as in the
# repository so that every relative path in them still holds. Everything packaged comes from one
# commit, through `git archive`.
#
#   packaging/release.sh [--target macos-aarch64|linux-x86_64|linux-aarch64]... [--out <dir>] [--commit <rev>]
#
# Each archive is rradius-<version>-<platform>.tar.gz. Without --target it builds for this
# machine. Linux builds use the Dockerfile's build stage. It needs cargo-about
# and cargo-deny, at the versions below.
set -euo pipefail

usage() {
    echo "usage: packaging/release.sh [--target macos-aarch64|linux-x86_64|linux-aarch64]... [--out <dir>] [--commit <rev>]" >&2
    exit 2
}

# The tools that check the licenses of the crates and list them, at the versions their
# configurations (deny.toml, about.toml, about.hbs) are written for.
cargo_about_version=0.9.3
cargo_deny_version=0.20.2

fail() {
    echo "release.sh: $*" >&2
    exit 1
}

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out="$root/dist"
commit=HEAD
targets=()
program=rradius-server
while [ $# -gt 0 ]; do
    case "$1" in
        --target) [ $# -ge 2 ] || usage; targets+=("$2"); shift 2 ;;
        --out) [ $# -ge 2 ] || usage; out="$2"; shift 2 ;;
        --commit) [ $# -ge 2 ] || usage; commit="$2"; shift 2 ;;
        *) usage ;;
    esac
done

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) host=macos-aarch64 ;;
    Linux-x86_64) host=linux-x86_64 ;;
    Linux-aarch64 | Linux-arm64) host=linux-aarch64 ;;
    *) host=unknown ;;
esac
[ ${#targets[@]} -gt 0 ] || targets=("$host")
for target in "${targets[@]}"; do
    case "$target" in
        macos-aarch64) [ "$host" = macos-aarch64 ] || fail "macos-aarch64 can only be built on an arm64 Mac" ;;
        linux-x86_64 | linux-aarch64) command -v docker >/dev/null || fail "$target is built in Docker, and docker is not installed" ;;
        *) fail "unknown target '$target'" ;;
    esac
done

for tool in about deny; do
    case "$tool" in
        about) wanted=$cargo_about_version install="cargo install --locked --features cli cargo-about@$cargo_about_version" ;;
        deny) wanted=$cargo_deny_version install="cargo install --locked cargo-deny@$cargo_deny_version" ;;
    esac
    found=$(cargo "$tool" --version 2>/dev/null | sed -n 's/^cargo-[a-z]* //p') || true
    [ "$found" = "$wanted" ] || fail "cargo-$tool $wanted is needed${found:+, not $found}; install it with: $install"
done

hash=$(git -C "$root" rev-parse --verify "$commit^{commit}") || fail "no commit '$commit'"
# A program's version, from its Cargo.toml in the commit packaged.
version_of() {
    local version
    version=$(git -C "$root" show "$hash:$1/Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
    [ -n "$version" ] || fail "cannot read the version from $1/Cargo.toml"
    echo "$version"
}
version_of "$program" >/dev/null
if [ "$commit" = HEAD ] && [ -n "$(git -C "$root" status --porcelain)" ]; then
    echo "release.sh: warning: the working tree has uncommitted changes; packaging only ${hash:0:12}" >&2
fi

mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
# Whatever ends the run, nothing half made stays behind.
trap 'rm -rf "$work"; rm -f "$out"/*.tar.gz.partial' EXIT
mkdir "$work/src"
git -C "$root" archive "$hash" | tar -x -C "$work/src"

# Every crate under a license deny.toml allows, before anything is built. Then the sources of
# the crates of every platform, which the license lists are read from.
# The exception deny.toml makes for the fuzz targets is not met here.
(cd "$work/src" && cargo deny --locked check --allow license-exception-not-encountered licenses) || fail "a crate's license is not one deny.toml allows"
(cd "$work/src" && cargo fetch --locked) || fail "cannot fetch the crates"

# Where a program's binary for a target goes once built.
binary_of() { echo "$work/bin-$1-$2/$1"; }

# The Rust target of a platform.
triple_of() {
    case "$1" in
        macos-aarch64) echo aarch64-apple-darwin ;;
        linux-x86_64) echo x86_64-unknown-linux-gnu ;;
        linux-aarch64) echo aarch64-unknown-linux-gnu ;;
    esac
}

# Writes to `dest` the licenses of the crates `program` is built from on `target`: those
# cargo-about lists, then the NOTICE files the crates carry, which it does not.
notices() {
    local program=$1 target=$2 dest=$3 triple listing dir file
    triple=$(triple_of "$target")
    listing="$work/crates-$program-$target.json"
    (cd "$work/src" && cargo about generate --frozen --fail -c about.toml -m "$program/Cargo.toml" --target "$triple" -o "$dest" about.hbs) || fail "cannot list the licenses of $program for $target (is every license deny.toml allows also in about.toml?)"
    (cd "$work/src" && cargo about generate --frozen -c about.toml -m "$program/Cargo.toml" --target "$triple" --format json -o "$listing") || fail "cannot list the crates of $program for $target"
    printf '## NOTICE files\n\nThe NOTICE files of the crates above that carry one. The other crates carry none.\n\n' >>"$dest"
    # A crate appears once for each license it is listed under.
    grep -o '"manifest_path":"[^"]*"' "$listing" | sed 's/^"manifest_path":"//; s/"$//' | sort -u | while read -r manifest; do
        dir=$(dirname "$manifest")
        for file in "$dir"/NOTICE*; do
            [ -f "$file" ] || continue
            printf '## NOTICE of %s\n\n````text\n' "$(basename "$dir")"
            cat "$file"
            printf '\n````\n\n'
        done
    done >>"$dest"
}

build() {
    local program=$1 target=$2 dest
    dest=$(binary_of "$program" "$target")
    mkdir -p "$(dirname "$dest")"
    case "$target" in
        macos-aarch64)
            # A target directory of its own, kept between runs, so a release does not rebuild
            # every dependency.
            # Under the repository's dist/, which git ignores, wherever the archives go. The SQLite
            # flag is the one .cargo/config.toml sets, given here too because the configuration
            # does not override a value already in the environment.
            (cd "$work/src/rradius-server" && LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG CARGO_TARGET_DIR="$root/dist/.target/$target" cargo build --release --locked -p "$program")
            cp "$root/dist/.target/$target/release/$program" "$dest"
            ;;
        linux-x86_64 | linux-aarch64)
            local platform=linux/amd64 image="rradius-release-build:$program-$target" container
            [ "$target" = linux-aarch64 ] && platform=linux/arm64
            docker buildx build --platform "$platform" --target build --load -t "$image" "$work/src"
            container=$(docker create --platform "$platform" "$image")
            docker cp "$container:/usr/local/bin/$program" "$dest"
            docker rm "$container" >/dev/null
            ;;
    esac
    chmod 755 "$dest"
}

# Unpacks `archive` and checks it with its own binary, here or in a container of its platform.
self_check() {
    local program=$1 target=$2 archive=$3 name=$4 version=$5 dir="$work/check-$1-$2"
    mkdir -p "$dir"
    tar -xzf "$archive" -C "$dir"
    # Run with the unpacked directory as $1. It checks every configuration of the scenarios the
    # archive carries but those whose directory has a make-certs.sh or setup.sh, which make
    # certificates or keys first: at least their nine files of seven scenarios.
    # Its license list names only the crates it is built from (hickory-resolver is not one) and
    # the NOTICE files.
    local roots='rradius-server/examples/scenarios' at_least=9 own_crates="if grep -q '^- hickory-resolver ' THIRD-PARTY-LICENSES.md; then exit 1; fi" own_license="test -s LICENSE
test ! -e rradius-enterprise"
    local script="set -e
cd \"\$1\"
test \"\$(bin/$program --version)\" = \"$program $version\"
checked=0
for root in $roots; do
    seen=0
    for config in \$root/*/*.toml; do
        [ -e \"\$config\" ] || continue
        seen=1
        scenario=\$(dirname \"\$config\")
        if [ -e \"\$scenario/make-certs.sh\" ] || [ -e \"\$scenario/setup.sh\" ]; then continue; fi
        bin/$program --check --config \"\$config\" >/dev/null || { echo \"\$config failed --check\" >&2; exit 1; }
        checked=\$((checked + 1))
    done
    test \$seen = 1 || { echo \"no scenarios in \$root\" >&2; exit 1; }
done
test \$checked -ge $at_least
test ! -e rradius-server/examples/scenarios/quick-start/data
grep -q '^- hiqlite ' THIRD-PARTY-LICENSES.md
grep -q '^## NOTICE files' THIRD-PARTY-LICENSES.md
$own_crates
$own_license"
    case "$target" in
        "$host")
            sh -c "$script" sh "$dir/$name"
            ;;
        linux-x86_64 | linux-aarch64)
            local platform=linux/amd64 base image="rradius-release-check:$target"
            [ "$target" = linux-aarch64 ] && platform=linux/arm64
            # The system the Docker image runs on: its base and the CA certificates it installs,
            # which scenarios with [lua.http] need to pass --check.
            base=$(sed -n 's/^FROM \(debian:[^ ]*\)$/\1/p' "$work/src/Dockerfile" | tail -n 1)
            [ -n "$base" ] || fail "cannot find the Debian image the Dockerfile runs on"
            printf 'FROM %s\nRUN apt-get update \\\n    && apt-get install --yes --no-install-recommends ca-certificates \\\n    && rm -rf /var/lib/apt/lists/*\n' "$base" |
                docker buildx build --platform "$platform" --load -t "$image" - || fail "cannot make the image that checks the $target archive"
            docker run --rm --platform "$platform" -v "$dir:/check" "$image" sh -c "$script" sh "/check/$name"
            ;;
        *)
            echo "release.sh: $target cannot be run here; not checked" >&2
            ;;
    esac
}

package() {
    local program=$1 target=$2 version name
    version=$(version_of "$program")
    name="rradius-$version-$target"
    local stage="$work/stage-$program-$target/$name"
    mkdir -p "$stage/bin"
    cp "$(binary_of "$program" "$target")" "$stage/bin/$program"
    local docs='rradius-server/examples docs/usage.md docs/manual share/dictionary* share/LICENSE share/NOTICE'
    # $docs holds a glob, expanded inside $work/src.
    # shellcheck disable=SC2086
    (cd "$work/src" && tar -cf - $docs) | tar -xf - -C "$stage"
    cp "$work/notices-$program-$target.md" "$stage/THIRD-PARTY-LICENSES.md"
    cp "$work/src/LICENSE" "$stage/LICENSE"
    cp "$work/src/packaging/README.md" "$stage/README.md"
    printf '%s %s\ncommit %s\n' "$program" "$version" "$hash" >"$stage/VERSION"
    if [ "${target#linux-}" != "$target" ]; then
        # The newest glibc symbol version the binary asks for is the oldest glibc it runs on.
        local glibc
        glibc=$(strings "$stage/bin/$program" | grep -o '^GLIBC_[0-9][0-9.]*$' | sed 's/^GLIBC_//' | sort -t . -k 1,1n -k 2,2n -k 3,3n | tail -n 1)
        [ -n "$glibc" ] || fail "cannot find which glibc the $target binary needs"
        printf 'glibc %s\n' "$glibc" >>"$stage/VERSION"
        printf '\n## 系統需求\n\n這個 Linux 版需要 glibc %s 以上。\n' "$glibc" >>"$stage/README.md"
    fi

    # SQLite keeps its compile options as strings in the binary; without this one some queries
    # are planned again on every read (see docs/usage.md section 1).
    strings "$stage/bin/$program" | grep -x ENABLE_QPSG >/dev/null || fail "the $target binary's SQLite was built without SQLITE_ENABLE_QPSG"

    local archive="$out/$name.tar.gz"
    rm -f "$archive" "$archive.sha256"
    local flags=()
    # macOS's tar would add its own metadata, which other tars warn about.
    if tar --version 2>/dev/null | grep -q bsdtar; then flags=(--no-mac-metadata --uid 0 --gid 0 --uname root --gname root); else flags=(--owner 0 --group 0); fi
    (cd "$work/stage-$program-$target" && COPYFILE_DISABLE=1 tar -czf "$archive.partial" "${flags[@]}" "$name")
    if ! self_check "$program" "$target" "$archive.partial" "$name" "$version"; then
        rm -f "$archive.partial"
        fail "the $program $target archive failed its check"
    fi
    mv "$archive.partial" "$archive"
    local sum
    if command -v shasum >/dev/null; then sum=(shasum -a 256); else sum=(sha256sum); fi
    (cd "$out" && "${sum[@]}" "$name.tar.gz" >"$name.tar.gz.sha256")
    echo "$archive"
}

# Every license list first: a crate whose license about.toml does not accept stops the run
# before anything is built.
for target in "${targets[@]}"; do
    notices "$program" "$target" "$work/notices-$program-$target.md"
done
for target in "${targets[@]}"; do
    build "$program" "$target"
    package "$program" "$target"
done
