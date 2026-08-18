# Zenoh over Multipath QUIC (noq) — PoC

Experimental replacement of Zenoh's Quinn-based QUIC backend with
[noq](https://github.com/n0-computer/noq) (a Quinn fork implementing
draft-ietf-quic-multipath), demonstrating that a single Zenoh session keeps
delivering messages over one QUIC connection with two network paths when one
path goes down. Measured results: [RESULTS.md](RESULTS.md).

## Architecture

```
Zenoh application (z_pub / z_sub — unchanged)
        |
zenoh-link-quic (unchanged, thin layer)
        |
zenoh-link-commons::quic  -- backend alias: quinn (default) | noq (quic_noq)
        |                    + quic/multipath.rs (config, open_path, path-event logs)
noq Endpoint
        |
noq::MultiSocket  -- one UDP socket per NIC (SO_BINDTODEVICE),
        |            dispatch by Transmit::src_ip   [client side only]
Multipath QUIC (draft-ietf-quic-multipath)
   /            \
path A          path B
```

The listener keeps a single wildcard socket (paths are distinguished by their
remote 4-tuple). The socket-ownership decision (why per-NIC sockets on the
client, modeled after mqvpn, with iroh's `Transports` as the implementation
reference) is recorded in the workspace note
`docs/notes/2026-08-19-socket-ownership-investigation.md`.

## What was changed

Zenoh (this fork, branch `feat/noq-mpquic-poc`):

- `Cargo.toml`, `zenoh/`, `io/zenoh-transport/`, `io/zenoh-link/`,
  `io/zenoh-links/zenoh-link-quic/`, `io/zenoh-link-commons/` manifests:
  feature chain `transport_quic_noq` (+ `transport_quic_noq_qlog`) down to
  `zenoh-link-commons/quic_noq`, with noq as a local path dependency.
- `io/zenoh-link-commons/src/quic/{unicast,utils,plaintext}.rs`: all quinn
  references routed through a `backend` alias; API divergences absorbed in
  cfg pairs (crypto traits: `PathId` in packet protection, by-value
  `ConnectionId`, `&self` config traits; per-path address accessors cached on
  `QuicConnection`).
- `io/zenoh-link-commons/src/quic/multipath.rs` (new): config parsing,
  additional-path opening with teardown guard, path lifecycle logging.
- `commons/zenoh-config`: `transport.link.quic.multipath` section
  (documented in `DEFAULT_CONFIG.json5`).

noq (fork, branch `feat/mpquic-poc`):

- `noq/src/multisocket.rs` (new, feature `multisocket`): generic bundling of
  per-interface sockets behind `AsyncUdpSocket`, `Transmit::src_ip` dispatch,
  member send errors absorbed (a downed NIC must not kill the connection).
- `noq/examples/multipath.rs` (new): standalone two-path demo/repro binary
  (both single-socket and `--ifaces` MultiSocket modes).

## How the integration works

- Compile-time backend selection; the default build still uses quinn
  byte-for-byte (verified against the upstream test suite). quinn stays in
  the dependency graph when noq is enabled (cargo features are additive) but
  has zero call sites.
- Multipath is negotiated only when BOTH sides set
  `transport.link.quic.multipath.enabled` (listener: flag only; connector:
  also `paths`, first entry = the primary/handshake path's local side).
- Path lifecycle is logged as
  `connection=<id> path=<id> side=<role> ... state=created|active|validated|failed|discarded`
  (target `zenoh_link_commons`, enable with `RUST_LOG=zenoh_link_commons=debug`).
  noq emits no `Established` event for PathId 0; the integration logs the
  primary explicitly at connect/accept time.
- With `transport_quic_noq_qlog` and `QLOGDIR=<dir>`, per-connection qlog
  files (`zenoh-mpquic-*-{client,server}.qlog`, multipath-aware schema) are
  emitted.

## Running: single path (localhost)

```bash
./experimental/mpquic-poc/gen-certs.sh
cargo build -p zenoh-examples --features zenoh/transport_quic_noq --example z_sub --example z_pub

./target/debug/examples/z_sub -l 'quic/127.0.0.1:7447' --no-multicast-scouting \
  --cfg 'transport/link/tls/listen_certificate:"experimental/mpquic-poc/certs/server.pem"' \
  --cfg 'transport/link/tls/listen_private_key:"experimental/mpquic-poc/certs/server-key.pem"'

./target/debug/examples/z_pub -e 'quic/localhost:7447' --no-multicast-scouting \
  --cfg 'transport/link/tls/root_ca_certificate:"experimental/mpquic-poc/certs/ca.pem"'
```

## Running: multipath (2-path netns)

Topology (see `netns.sh`): `zc` (client, c0=10.10.0.1, c1=10.20.0.1) and `zs`
(server, s0=10.10.0.2, s1=10.20.0.2), veth pairs per path. Root is required
for netns + SO_BINDTODEVICE; without host sudo, `run-in-docker.sh` runs
everything in a privileged container (topology is per-container, so run one
scenario per invocation).

```bash
cargo build -p zenoh-examples --features zenoh/transport_quic_noq --examples
cd experimental/mpquic-poc
./run-in-docker.sh bash -c '
  zenoh/experimental/mpquic-poc/netns.sh up
  zenoh/experimental/mpquic-poc/netns.sh netem          # optional path asymmetry
  zenoh/experimental/mpquic-poc/run-sub.sh > /tmp/sub.log 2>&1 &
  sleep 3
  zenoh/experimental/mpquic-poc/run-pub.sh > /tmp/pub.log 2>&1 &
  sleep 10
  grep -E "state=" /tmp/pub.log
'
```

Expected: one connection id with `path=PathId(0) ... state=active (primary)`
and `path=PathId(1) ... state=created` → `state=validated`; `ss -aunp` inside
`zc` shows two device-bound sockets (`0.0.0.0%c0`, `0.0.0.0%c1`).

## Failure demo

With the multipath scenario running, `netns.sh fail-primary` takes c0 down
(timestamped); `netns.sh recover-primary` brings it back. Expected (measured
values in [RESULTS.md](RESULTS.md)): `state=failed reason=TimedOut` for
PathId(0) within the configured idle timeout (3 s default), delivery
continues on PathId(1), no message loss (QUIC retransmission drains the
outage backlog), connection id unchanged (no session re-establishment).

## Known limitations

- Paths can only be opened by the connecting side (noq: server-side
  `open_path` is not allowed).
- Path local sides are IPs only (no local port; noq's `FourTuple` carries no
  local port), and `iface:` pinning is Linux/Android + IPv4 + root
  (CAP_NET_RAW) only.
- The endpoint-level `#iface=`, `#bind=` and `#dscp=` parameters conflict
  with multipath and are rejected.
- The socket set is fixed at connect time: no dynamic interface add/remove
  (no netlink monitoring), no automatic re-open of an abandoned path after
  the interface recovers, no handshake-path rotation (mqvpn-style dynamics
  are follow-up work).
- A secondary path that resolves at connect time but whose member socket was
  skipped cannot be opened later; a member socket that breaks persistently
  degrades into per-path idle timeouts rather than a fail-fast error.
- `quic/...` datagram links and `udp/...?rel=1` links compile against the
  noq backend too (the quic module is shared) and the multipath config
  reaches datagram links via the shared configurator — both are unvalidated
  in this PoC.
- GitHub CI cannot build these branches (the noq path dependency points
  outside the repository); a git dependency or an adjacent checkout step
  would be needed.
- The 1.75-compat build (`zenoh-pinned-deps-1-75`) does not cover the noq
  feature (noq's MSRV is 1.88, edition 2024).

## Scheduler: current state and future integration points

For the roadmap `ROS 2 QoS → Zenoh priority → noq scheduler → MPQUIC path
selection` (spec section 13/20). Findings from noq's source (branch
`feat/mpquic-poc`):

- **Where selection happens:** there is no scheduler function. Paths are
  iterated in ascending `PathId` order in `Connection::poll_transmit`
  (`noq-proto/src/connection/mod.rs:1023`, loop at `:1075-1110`); the first
  path that may send and is not congestion-blocked wins
  (eligibility: `scheduling_info`, `mod.rs:1154`; congestion veto:
  `path_congestion_check`, `mod.rs:1939`). The code states the policy
  itself: "Currently it chooses the lowest path that is not congestion
  blocked" (`mod.rs:7337-7339`).
- **Abstraction:** none — `PathSchedulingInfo` and `scheduling_info` are
  private; the only config knob is `max_concurrent_multipath_paths`.
- **Pluggability:** applications can influence selection only through
  `PathStatus` (`Available`/`Backup`, a strict fallback: Backup carries app
  data only when no validated Available path exists) via
  `Path::set_status` / `open_path`'s initial status.
- **Metadata at selection time:** per-path state only (validated, status,
  remote CIDs); RTT/cwnd exist on `PathData` but are not consulted for the
  choice, and the frame/stream being sent is not visible — stream data is
  drawn from a connection-global priority queue *after* the path is fixed
  (`write_stream_frames`, `streams/state.rs:522`, takes no `PathId`).
- **Insertion seams for future work:**
  1. make `scheduling_info` delegate to a `PathScheduler` trait, configured
     via a `TransportConfig` factory mirroring
     `congestion_controller_factory` (`config/transport.rs:58,:346`);
  2. replace the ascending-`PathId` walk (`mod.rs:1075-1110`) with a
     scheduler-supplied ordering (enough for lowest-RTT/weighted policies);
  3. for per-stream steering (the ROS 2 QoS chain), `write_stream_frames`
     must take the chosen `PathId`/a stream filter and the pending-stream
     queue must be partitioned per path class — retransmissions are
     currently re-queued globally (`mod.rs:3496-3499`), so path pinning
     would need to cover that too.
