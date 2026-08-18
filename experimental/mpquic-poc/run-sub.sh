#!/usr/bin/env bash
# server/subscriber side, run inside netns zs (as root: host sudo or the
# privileged container via run-in-docker.sh).
set -euo pipefail
cd "$(dirname "$0")/../.." # -> zenoh repo root
# z_sub's println! output carries no timestamps; prefix one per line
# (needed for the spec section 10 measurements). UTC to match tracing logs.
ip netns exec zs env RUST_LOG=zenoh_link_commons=debug,zenoh=info \
  ./target/debug/examples/z_sub -c experimental/mpquic-poc/server.json5 2>&1 |
  while IFS= read -r line; do printf '%s %s\n' "$(date -u +%T.%3N)" "$line"; done
