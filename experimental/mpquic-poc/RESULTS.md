# MPQUIC PoC — E2E and failover results

Run date: 2026-08-18 (UTC timestamps). Environment: Linux netns topology from
`netns.sh` (+ `netem`: path0 RTT 10ms/100Mbps, path1 RTT 50ms/30Mbps) inside a
privileged container (`run-in-docker.sh`). Binaries: zenoh `feat/noq-mpquic-poc`
@ e3f1bdf5 (examples built with `zenoh/transport_quic_noq`), noq `feat/mpquic-poc`
@ 46d24f46. Full logs: workspace `logs/e2e-{pub,sub}.log`.

## Multipath establishment (Testing Order step 7)

- z_pub held exactly **two UDP sockets**, one per NIC (`ss -aunp` in `zc`):
  `0.0.0.0%c0:41849` and `0.0.0.0%c1:50013` — MultiSocket + SO_BINDTODEVICE.
- One QUIC connection (`connection=105224521387760`) with two paths:

```
19:46:49.101 connection=105224521387760 path=PathId(0) side=client local=? remote=10.10.0.2:7447 state=active (primary)
19:46:49.122 connection=105224521387760 path=PathId(1) side=client local=10.20.0.1 remote=10.20.0.2:7447 state=created
19:46:49.174 connection=105224521387760 path=PathId(1) side=client state=active
19:46:49.174 connection=105224521387760 path=PathId(1) side=client local=10.20.0.1 remote=10.20.0.2:7447 state=validated
```

(`local=?` on the primary: noq reports no local IP for a client's handshake
path; the socket is nevertheless the c0-pinned member.)

## Failure test (Testing Order steps 8-9, spec section 10)

`fail-primary` (c0 down) at **19:47:01.068** while z_pub published 1 msg/s.

| spec section 10 measurement | value |
|---|---|
| time path failure detected | 19:47:04.150 → **3.08 s** after fail (per-path idle timeout 3000 ms as configured) |
| last message on the failed path (sub arrival) | seq 10 @ 19:47:00.154 |
| first successful message after failover (sub arrival) | seq 11 @ 19:47:04.345 (burst: 11-14 delivered within 4 ms — QUIC retransmission drained the outage backlog over path 1) |
| Zenoh message gap | **0 lost** (sequence 0..33 complete; delivery stalled 4.19 s, no loss) |
| connection ID / session changed | **no** (single connection id throughout; exactly one `state=active (primary)` line; no re-establishment) |

Failure-side events:

```
19:47:04.150 connection=105224521387760 path=PathId(0) side=client state=failed reason=TimedOut
19:47:04.319 connection=105224521387760 path=PathId(0) side=client state=discarded
```

## Recovery observation

`recover-primary` (c0 up) at 19:47:16.074: no new path events — an abandoned
path is not re-opened automatically (noq default; dynamic re-open is future
work). Traffic continued on path 1 until shutdown; both processes exited on
SIGINT without panics.

## Standalone noq ladder (Testing Order steps 6a/6b, for reference)

- 6a (noq default socket, two real local IPs): second path validated
  (`PathId(1)`, local 10.20.0.1 → 10.20.0.2:4433); failover: `Abandoned
  { TimedOut }` client-side, `RemoteAbandoned` server-side, echoes continued
  with zero failures.
- 6b (MultiSocket, `--ifaces c0,c1`): same results with two device-bound
  sockets; interface-down send errors were absorbed by the multi-socket layer
  (connection survived).
