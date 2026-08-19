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

## Acceptance criteria (spec_draft.md section 19)

| # | Criterion | Evidence |
|---|---|---|
| 1 | Zenoh builds with noq instead of Quinn behind an experimental backend feature | `cargo build --features transport_quic_noq` green throughout; default build unchanged (same warning set and test results as `main`) |
| 2 | Zenoh pub/sub works in noq single-path mode | localhost z_pub/z_sub smoke (9 msgs, clean SIGINT exit); 26 upstream QUIC transport tests pass on the noq backend matching the quinn baseline (openclose 6, transport 19, multilink 1) |
| 3 | At least 2 usable paths inside ONE noq QUIC connection | E2E: `connection=105224521387760` with `PathId(0)` (primary) and `PathId(1)` `created→validated`; two device-bound client sockets |
| 4 | Zenoh pub/sub works over that MPQUIC connection | 34 messages delivered over the 2-path connection (this file, above) |
| 5 | Zenoh session survives one network path going down | seq continued to 33 after c0 down; no session re-establishment (single connection id, one primary-active line) |
| 6 | Message delivery continues on the surviving path | seq 11.. delivered over PathId(1); **0 messages lost** (retransmission drained the 4.19 s stall) |
| 7 | Reproducible via Linux commands/scripts | `netns.sh`, `gen-certs.sh`, `run-{sub,pub}.sh`, `run-in-docker.sh` (sudo-less), all committed |
| 8 | Path creation/failure visible in logs/qlog | `state=created/active/validated/failed/discarded` lines (above); qlog files verified with `transport_quic_noq_qlog` + `QLOGDIR` (`zenoh-mpquic-*-{client,server}.qlog`) |
| 9 | No changes to ROS 2 or zenoh-bridge-ros2dds | diff vs `main` touches only manifests, `io/zenoh-link-commons/src/quic/`, `commons/zenoh-config`, `DEFAULT_CONFIG.json5` and `experimental/mpquic-poc/` |
| 10 | Custom scheduling left for a later phase | not implemented; noq's current policy and the insertion seams are documented in README.md |

## Detection-latency follow-up (2026-08-19)

The 3.08 s detection is exactly the configured per-path `max_idle_timeout`
(3000 ms): this PoC detects path failure **in-band only** (noq's
`PathTimer::PathIdle`, floored at 3×PTO per RFC 9000 §10.1 guidance,
`noq-proto/src/connection/mod.rs:883-896`).

Attempting to tighten the in-band timers destabilized the idle secondary
path — it timed out *without any failure being injected*:

| keep-alive / idle (ms) | outcome |
|---|---|
| 1000 / 3000 (default) | stable; detection 3.08 s |
| 300 / 1000 | PathId(1) `TimedOut` ~1.45 s after validation, no failure injected |
| 200 / 600 | PathId(1) `TimedOut` ~0.95 s after validation, no failure injected |

So sub-second in-band detection is not reachable by configuration alone with
current noq: the per-path keep-alive fails to hold an idle (non-scheduled)
path at sub-second intervals (suspects: the 3×PTO idle floor vs a fresh
path's initial RTT estimate, and `ping_path(path_id).ok()` discarding errors
at `mod.rs:2550-2552`). Worth an upstream issue with this reproduction.
A second upstream-note candidate found during final review: `open_path`
called after the connection has terminated still inserts an undrained
watch sender (no fail-fast on `state.error`), reopening the pending-forever
window the fork's terminate fix closed — Zenoh is protected by its
`on_closed` select guard.

Comparison with mqvpn (which detects the same event in milliseconds): mqvpn's
speed does not come from QUIC timers at all. It watches netlink
(`RTM_NEWLINK` with `IFLA_OPERSTATE` down / `IFF_UP` cleared, `RTM_DELLINK`)
and removes the path immediately on the kernel event
(`src/platform/linux/netlink_mon.c`, `remove_path_by_index`); its *in-band*
fallback is far slower than ours (xquic ping every 15 s, idle timeout 120 s).
The equivalent hook exists in noq (`Endpoint::handle_network_change` +
`NetworkChangeHint`, used by iroh's netmon) — wiring a netlink monitor to it
is the future-work item already scoped out in the spec (section 3), and would
give ms-class detection for local interface failures; in-band timers would
remain the safety net for remote/mid-path failures.

## Standalone noq ladder (Testing Order steps 6a/6b, for reference)

- 6a (noq default socket, two real local IPs): second path validated
  (`PathId(1)`, local 10.20.0.1 → 10.20.0.2:4433); failover: `Abandoned
  { TimedOut }` client-side, `RemoteAbandoned` server-side, echoes continued
  with zero failures.
- 6b (MultiSocket, `--ifaces c0,c1`): same results with two device-bound
  sockets; interface-down send errors were absorbed by the multi-socket layer
  (connection survived).
