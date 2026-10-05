#!/bin/bash
# Runs a command in the udnf build container (.devcontainer/Containerfile), e.g.:
#   scripts/dev.sh cargo build
#   scripts/dev.sh cargo test
# The repository is mounted at the same path, so paths and target/ are shared with the host.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
image=localhost/udnf-dev:latest
containerfile=$repo/.devcontainer/Containerfile

# Rebuild the image when the Containerfile is newer than it.
built=$(podman image inspect -f '{{.Created.Unix}}' "$image" 2>/dev/null || echo 0)
if (( $(stat -c %Y "$containerfile") > built )); then
    podman build -q -t "$image" -f "$containerfile" "$repo/.devcontainer" >&2
fi

tty=()
[[ -t 0 && -t 1 ]] && tty=(-it)
exec podman run --rm "${tty[@]}" \
    --userns=keep-id --security-opt label=disable \
    -v "$repo:$repo" -w "$PWD" \
    -e HOME=/tmp -e CARGO_HOME="$repo/target/cargo-home" \
    "$image" "$@"
