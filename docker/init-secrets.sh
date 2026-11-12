#!/bin/sh
# Creates the secrets compose.yaml needs, keeping any that already exist. The directory
# stays private; the files are world-readable because the container's user is not the
# host's, and Compose mounts them as they are.
set -eu
dir="$(dirname "$0")/secrets"
mkdir -p "$dir"
chmod 700 "$dir"
for name in raft api radius; do
    file="$dir/$name.secret"
    if [ ! -e "$file" ]; then
        openssl rand -hex 32 > "$file"
        chmod 644 "$file"
        echo "created $file"
    fi
done
