#!/usr/bin/env bash
# One-command Phase B demo (spec section 23): a ROS 2 topic surviving a
# network-path failure over one Multipath QUIC connection.
#
#   ros2 talker --CycloneDDS-- bridge ==MPQUIC(2 paths)== bridge --CycloneDDS-- ros2 listener
#      (netns zc, domain 0)                                  (netns zs, domain 1)
#
# Everything runs inside one privileged container (netns topology from
# netns.sh); the zenoh-bridge-ros2dds binary is built on the host against
# the local fork and bind-mounted in. Usage:
#   ./ros-demo.sh            # full run: establish -> fail -> recover -> summary
# Requires: docker, the mpquic-ros-demo:local image (Dockerfile.ros), certs
# (gen-certs.sh), and a release-or-debug bridge binary at
# ../../zenoh-plugin-ros2dds/target/debug/zenoh-bridge-ros2dds.
set -euo pipefail
cd "$(dirname "$0")"
WS="$(cd ../../.. && pwd)"

BRIDGE=zenoh-plugin-ros2dds/target/debug/zenoh-bridge-ros2dds
[ -f "$WS/$BRIDGE" ] || { echo "bridge binary not found: $WS/$BRIDGE" >&2; exit 1; }
[ -f certs/server.pem ] || { echo "run ./gen-certs.sh first" >&2; exit 1; }

IMAGE="${MPQUIC_ROS_IMAGE:-mpquic-ros-demo:local}"

docker run --rm --privileged -v "$WS":/ws -w /ws "$IMAGE" bash -c '
set -euo pipefail
D=zenoh/experimental/mpquic-poc
LOG=/tmp/rosdemo; mkdir -p $LOG

say() { printf "\n\033[1m== %s ==\033[0m\n" "$*"; }

# Pin DDS to loopback: the demo fails the c0 interface, and CycloneDDS must
# not have picked it for the LOCAL talker<->bridge / listener<->bridge hop.
export CYCLONEDDS_URI="<CycloneDDS><Domain><General><Interfaces><NetworkInterface name=\"lo\"/></Interfaces><AllowMulticast>false</AllowMulticast></General><Discovery><ParticipantIndex>auto</ParticipantIndex><Peers><Peer address=\"127.0.0.1\"/></Peers></Discovery></Domain></CycloneDDS>"
now_ms() { date -u +%s%3N; }

say "1/6 building the 2-path netns topology"
$D/netns.sh up
$D/netns.sh netem

say "2/6 starting the listener side (netns zs: bridge + ros2 listener, domain 1)"
cd /ws/zenoh   # cert paths in the json5 files are zenoh-repo-root relative
ip netns exec zs env CYCLONEDDS_URI="$CYCLONEDDS_URI" RUST_LOG=zenoh_link_commons=debug,zenoh=info \
  /ws/zenoh-plugin-ros2dds/target/debug/zenoh-bridge-ros2dds \
  -c experimental/mpquic-poc/server.json5 -d 1 > $LOG/bridge-zs.log 2>&1 &
cd /ws
sleep 3
ip netns exec zs env CYCLONEDDS_URI="$CYCLONEDDS_URI" bash -c "source /opt/ros/jazzy/setup.bash && \
  ROS_DOMAIN_ID=1 RMW_IMPLEMENTATION=rmw_cyclonedds_cpp \
  exec ros2 run demo_nodes_cpp listener" > $LOG/listener.log 2>&1 &

say "3/6 starting the talker side (netns zc: bridge + ros2 talker, domain 0)"
cd /ws/zenoh   # cert paths in the json5 files are zenoh-repo-root relative
ip netns exec zc env CYCLONEDDS_URI="$CYCLONEDDS_URI" RUST_LOG=zenoh_link_commons=debug,zenoh=info \
  /ws/zenoh-plugin-ros2dds/target/debug/zenoh-bridge-ros2dds \
  -c experimental/mpquic-poc/client.json5 -d 0 > $LOG/bridge-zc.log 2>&1 &
cd /ws
sleep 3
ip netns exec zc env CYCLONEDDS_URI="$CYCLONEDDS_URI" bash -c "source /opt/ros/jazzy/setup.bash && \
  ROS_DOMAIN_ID=0 RMW_IMPLEMENTATION=rmw_cyclonedds_cpp \
  exec ros2 run demo_nodes_cpp talker" > $LOG/talker.log 2>&1 &

say "4/6 waiting for the topic to flow end-to-end over MPQUIC"
for i in $(seq 1 30); do
  grep -q "I heard" $LOG/listener.log 2>/dev/null && break
  sleep 1
done
grep -q "I heard" $LOG/listener.log || { echo "FAIL: listener never received /chatter"; for f in $LOG/*.log; do echo "--- $f"; tail -n 5 "$f"; done; cp $LOG/*.log /ws/logs/ 2>/dev/null; exit 1; }
grep -m1 "state=validated" $LOG/bridge-zc.log >/dev/null || { echo "FAIL: second path not validated"; exit 1; }
echo "topic flowing; 2 paths validated on one connection:"
grep -E "state=(active \(primary\)|validated)" $LOG/bridge-zc.log | sed "s/\x1b\[[0-9;]*m//g" | grep -oE "connection=[0-9]+ path=PathId\([0-9]\).*state=[a-z]+( \(primary\))?" | head -2
sleep 5

say "5/6 failing the primary path (c0 down) while the topic is flowing"
LAST_BEFORE=$(grep "I heard" $LOG/listener.log | tail -1 | grep -oE "Hello World: [0-9]+" | grep -oE "[0-9]+")
T_FAIL=$(now_ms)
ip -n zc link set c0 down
# wait for the out-of-band detection in the bridge log
DETECT_MS=""
for i in $(seq 1 100); do
  if grep -q "state=failed" $LOG/bridge-zc.log; then
    T_DETECT=$(now_ms); DETECT_MS=$((T_DETECT - T_FAIL)); break
  fi
  sleep 0.05
done
sleep 10   # let the topic keep flowing on the surviving path
ip -n zc link set c0 up

say "6/6 teardown and summary"
kill %4 %3 %2 %1 2>/dev/null || true
sleep 1

TOTAL=$(grep -c "I heard" $LOG/listener.log || echo 0)
LAST_AFTER=$(grep "I heard" $LOG/listener.log | tail -1 | grep -oE "Hello World: [0-9]+" | grep -oE "[0-9]+")
# gap check across the whole run (talker counts monotonically from 1)
GAPS=$(grep -oE "Hello World: [0-9]+" $LOG/listener.log | grep -oE "[0-9]+" | awk "NR>1 && \$1 != prev+1 { print prev\"->\"\$1 } { prev=\$1 }")
FAIL_REASON=$(grep -m1 "state=failed" $LOG/bridge-zc.log | sed "s/\x1b\[[0-9;]*m//g" | grep -oE "reason=[A-Za-z]+" || true)
RECONNECTS=$(grep -c "state=active (primary)" $LOG/bridge-zc.log)

echo ""
echo "----------------------------------------------------------"
echo " ROS 2 topic over Multipath QUIC - failover demo summary"
echo "----------------------------------------------------------"
echo " path failure detection : ${DETECT_MS:-NOT DETECTED} ms (${FAIL_REASON:-n/a})"
echo " messages received      : $TOTAL (last seq before fail: ${LAST_BEFORE:-?}, final: ${LAST_AFTER:-?})"
echo " sequence gaps          : ${GAPS:-none}"
echo " QUIC connections used  : $RECONNECTS (1 = no reconnect, failover was seamless)"
echo "----------------------------------------------------------"
[ -n "$DETECT_MS" ] || exit 1
[ "$RECONNECTS" = "1" ] || { echo "FAIL: session reconnected"; exit 1; }
cp $LOG/*.log /ws/logs/ 2>/dev/null || true
'
echo "logs copied to workspace logs/ (bridge-zc, bridge-zs, talker, listener)"
