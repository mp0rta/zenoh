#!/usr/bin/env bash
# Runs a command in a privileged container with the whole workspace mounted,
# so the netns topology can be exercised without host sudo (root inside the
# container may create namespaces/veth/tc). The container is ephemeral: the
# topology built by netns.sh vanishes with it, so run one full scenario per
# invocation.
#
# usage: ./run-in-docker.sh bash -c '<commands, workspace-root relative>'
# The image needs iproute2/ping and a glibc compatible with the host-built
# binaries; override with MPQUIC_DOCKER_IMAGE.
set -euo pipefail
WS="$(cd "$(dirname "$0")/../../.." && pwd)" # -> workspace root (parent of zenoh/, noq/)
IMAGE="${MPQUIC_DOCKER_IMAGE:-mqvpn-bench:latest}"
exec docker run --rm --privileged -v "$WS":/ws -w /ws "$IMAGE" "$@"
