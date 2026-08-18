#!/usr/bin/env bash
# 2-path netns topology for the MPQUIC PoC (spec section 11).
#   zc (client)                     zs (server)
#   c0 10.10.0.1 ---- veth ---- s0 10.10.0.2
#   c1 10.20.0.1 ---- veth ---- s1 10.20.0.2
# usage: sudo ./netns.sh up|down|netem|fail-primary|recover-primary
set -euo pipefail

up() {
  ip netns add zc
  ip netns add zs
  ip link add c0 type veth peer name s0
  ip link add c1 type veth peer name s1
  ip link set c0 netns zc; ip link set s0 netns zs
  ip link set c1 netns zc; ip link set s1 netns zs
  ip -n zc addr add 10.10.0.1/24 dev c0
  ip -n zs addr add 10.10.0.2/24 dev s0
  ip -n zc addr add 10.20.0.1/24 dev c1
  ip -n zs addr add 10.20.0.2/24 dev s1
  for ns in zc zs; do
    ip -n $ns link set lo up
  done
  ip -n zc link set c0 up; ip -n zs link set s0 up
  ip -n zc link set c1 up; ip -n zs link set s1 up
  ip netns exec zc ping -c1 -W1 10.10.0.2 >/dev/null
  ip netns exec zc ping -c1 -W1 10.20.0.2 >/dev/null
  echo "netns up: both paths reachable"
}

netem() {
  # Path 0: RTT 10ms / 100Mbps, Path 1: RTT 50ms / 30Mbps (spec section 11)
  # netem delay is per direction (applied on both egresses): 5ms x2 = RTT 10ms,
  # 25ms x2 = RTT 50ms. Do NOT "fix" these to 10ms/25ms x2 -- that doubles the RTT.
  ip netns exec zc tc qdisc replace dev c0 root netem delay 5ms rate 100mbit
  ip netns exec zs tc qdisc replace dev s0 root netem delay 5ms rate 100mbit
  ip netns exec zc tc qdisc replace dev c1 root netem delay 25ms rate 30mbit
  ip netns exec zs tc qdisc replace dev s1 root netem delay 25ms rate 30mbit
  echo "netem applied"
}

fail_primary() {
  # -u: tracing (pub-side logs) timestamps default to UTC; keep comparisons in UTC
  date -u +"%T.%3N fail-primary: taking c0 down"
  ip -n zc link set c0 down
}

recover_primary() {
  date -u +"%T.%3N recover-primary: bringing c0 up"
  ip -n zc link set c0 up
}

down() {
  ip netns del zc 2>/dev/null || true
  ip netns del zs 2>/dev/null || true
  echo "netns removed"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  netem) netem ;;
  fail-primary) fail_primary ;;
  recover-primary) recover_primary ;;
  *) echo "usage: $0 up|down|netem|fail-primary|recover-primary"; exit 1 ;;
esac
