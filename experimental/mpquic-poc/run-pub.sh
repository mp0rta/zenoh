#!/usr/bin/env bash
# client/publisher side, run inside netns zc (as root: host sudo or the
# privileged container via run-in-docker.sh).
set -euo pipefail
cd "$(dirname "$0")/../.." # -> zenoh repo root
exec ip netns exec zc env RUST_LOG=zenoh_link_commons=debug,zenoh=info \
  ./target/debug/examples/z_pub -c experimental/mpquic-poc/client.json5
