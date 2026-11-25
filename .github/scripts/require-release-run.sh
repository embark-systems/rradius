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
dir=$(dirname "$0")
remote=$(git -C "$dir" remote get-url origin)
repo=$(printf '%s\n' "$remote" | sed -E 's#^(https://github\.com/|git@github\.com:|ssh://git@github\.com/)##; s#\.git$##')
passed=$(gh run list --repo "$repo" --workflow release.yml --commit "$commit" --status success --json databaseId --jq length 2>/dev/null) || passed=0
if [ "${passed:-0}" -lt 1 ]; then
    echo "require-release-run.sh: no passed run of release.yml for ${commit} in ${repo}." >&2
    echo "Start one on the commit to release and wait for it: gh workflow run release.yml --ref <branch or tag>" >&2
    echo "(For a build that is not a release, release.sh takes --no-ci-check.)" >&2
    exit 1
fi
echo "release.yml passed for ${commit}"
