#!/bin/sh
# Fails unless the Release workflow (release.yml) has passed for a commit, as
# packages/tarball/release.sh needs before it packs a release.
#
# release.yml runs every workflow of the repository for one commit: the jobs that otherwise wait
# for the weekly run or for a hand start (Docker, fuzzing, the advisory check, the load generator)
# run in it too. Start it for the commit that is to be released:
#
#   gh workflow run release.yml --ref <branch or tag>
#
# and wait for it to pass. The commit must be one GitHub has: a commit that was never pushed has
# no run. Needs the `gh` command, logged in to a user who can read the repository's runs.
#
# usage: .github/scripts/require-release-run.sh <commit>
set -eu

[ $# -eq 1 ] || { echo "usage: $0 <commit>" >&2; exit 2; }
commit=$1
command -v gh >/dev/null 2>&1 || { echo "require-release-run.sh: the gh command is needed to look up the runs of release.yml" >&2; exit 1; }
gh_failed() {
    echo "require-release-run.sh: gh could not look up the runs of release.yml in ${repo}: $1" >&2
    echo "Check that gh is signed in and can read the repository: gh auth status" >&2
    echo "(For a build that is not a release, release.sh takes --no-ci-check.)" >&2
    exit 1
}
dir=$(dirname "$0")
remote=$(git -C "$dir" remote get-url origin)
repo=$(printf '%s\n' "$remote" | sed -E 's#^(https://github\.com/|git@github\.com:|ssh://git@github\.com/)##; s#\.git$##')
err=$(mktemp)
trap 'rm -f "$err"' EXIT
# A gh that fails (not signed in, offline, no access) is not the same as a commit without a run.
if ! passed=$(gh run list --repo "$repo" --workflow release.yml --commit "$commit" --status success --json databaseId --jq length 2>"$err"); then
    gh_failed "$(head -n 1 "$err")"
fi
case "$passed" in
    "" | *[!0-9]*) gh_failed "unexpected output: $passed" ;;
esac
if [ "$passed" -lt 1 ]; then
    echo "require-release-run.sh: no passed run of release.yml for ${commit} in ${repo}." >&2
    echo "Start one on the commit to release and wait for it: gh workflow run release.yml --ref <branch or tag>" >&2
    echo "(For a build that is not a release, release.sh takes --no-ci-check.)" >&2
    exit 1
fi
echo "release.yml passed for ${commit}"
