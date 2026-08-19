//
// Copyright (c) 2026 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

//! Experimental Multipath QUIC support for the noq backend.
//!
//! Configured through `transport.link.quic.multipath` (flattened into the
//! endpoint config by the configurator in `utils.rs`). See the PoC spec
//! sections 8-10 for the configuration model: the connecting side lists its
//! paths (first entry = the primary/handshake path's local side, later
//! entries are opened as additional paths after the handshake), the
//! listening side only enables the negotiation.

use std::{net::IpAddr, net::SocketAddr, str::FromStr, time::Duration};

use futures::StreamExt;
// This module is compiled only under the quic_noq feature, so it names noq
// directly (the per-file `backend` aliases in the sibling modules are local
// to those files).
use noq as backend;
use zenoh_protocol::core::endpoint::Config;
use zenoh_result::{bail, zerror, ZResult};

/// Endpoint config keys (flattened from transport.link.quic.multipath).
pub const MULTIPATH: &str = "multipath";
/// Path entries joined with '|'.
pub const MULTIPATH_PATHS: &str = "multipath_paths";
pub const MULTIPATH_MAX_PATHS: &str = "multipath_max_paths";
pub const MULTIPATH_KEEP_ALIVE_MS: &str = "multipath_keep_alive_ms";
pub const MULTIPATH_IDLE_TIMEOUT_MS: &str = "multipath_idle_timeout_ms";

pub const DEFAULT_MAX_CONCURRENT_PATHS: u32 = 4;
pub const DEFAULT_KEEP_ALIVE_MS: u64 = 1000;
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 3000;

/// The local side of one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSpec {
    /// A concrete local IP (bound directly; no special privileges).
    Ip(IpAddr),
    /// A network interface name (pinned with SO_BINDTODEVICE; needs
    /// CAP_NET_RAW, typically root).
    Iface(String),
}

/// One path entry: `local:<ip>[@<remote>]` or `iface:<name>[@<remote>]`.
/// A missing remote means "use the primary remote from the locator".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSpec {
    pub local: LocalSpec,
    pub remote: Option<SocketAddr>,
}

impl FromStr for PathSpec {
    type Err = zenoh_result::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, remote) = match s.split_once('@') {
            Some((head, remote)) => (
                head,
                Some(
                    remote
                        .parse()
                        .map_err(|e| zerror!("invalid remote addr in path spec '{s}': {e}"))?,
                ),
            ),
            None => (s, None),
        };
        let local = match head.split_once(':') {
            Some(("local", ip)) => LocalSpec::Ip(
                ip.parse()
                    .map_err(|e| zerror!("invalid local ip in path spec '{s}': {e}"))?,
            ),
            Some(("iface", name)) if !name.is_empty() => LocalSpec::Iface(name.to_string()),
            _ => bail!("path spec '{s}' must start with 'local:<ip>' or 'iface:<name>'"),
        };
        Ok(PathSpec { local, remote })
    }
}

/// Resolves a [`LocalSpec`] to a concrete local IP (IPv4 for now; the PoC
/// test environment and the vehicle deployment are IPv4).
pub fn resolve_local_ip(spec: &LocalSpec) -> ZResult<IpAddr> {
    match spec {
        LocalSpec::Ip(ip) => Ok(*ip),
        LocalSpec::Iface(name) => zenoh_util::net::get_ipv4_ipaddrs(Some(name), true)
            .into_iter()
            .next()
            .ok_or_else(|| zerror!("interface '{name}' has no usable IPv4 address").into()),
    }
}

/// Parsed multipath settings for one endpoint.
#[derive(Debug, Clone)]
pub struct MultipathConfig {
    pub max_concurrent_paths: u32,
    /// First entry = the primary (handshake) path's local side; later entries
    /// are opened as additional paths. Empty on the listening side.
    pub paths: Vec<PathSpec>,
    pub keep_alive_interval: Duration,
    pub max_idle_timeout: Duration,
}

impl MultipathConfig {
    /// Returns `Ok(None)` when multipath is not enabled for this endpoint.
    pub fn from_endpoint_config(epconf: &Config) -> ZResult<Option<Self>> {
        if epconf.get(MULTIPATH) != Some("true") {
            return Ok(None);
        }
        let paths = epconf
            .get(MULTIPATH_PATHS)
            .map(|s| {
                s.split('|')
                    .map(PathSpec::from_str)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        // An empty path list is the legal listener-side form (negotiation
        // only). On the connecting side the first entry pins the primary
        // path's local side, so its remote comes from the locator.
        if let Some(first) = paths.first() {
            if first.remote.is_some() {
                bail!(
                    "the first path entry is the primary path; its remote comes from the \
                     locator and must not be specified"
                );
            }
        }
        let parse_u64 = |key: &str, default: u64| -> ZResult<u64> {
            match epconf.get(key) {
                Some(s) => s.parse().map_err(|e| zerror!("bad {key}: {e}").into()),
                None => Ok(default),
            }
        };
        let max_concurrent_paths: u32 = match epconf.get(MULTIPATH_MAX_PATHS) {
            Some(s) => s
                .parse()
                .map_err(|e| zerror!("bad {MULTIPATH_MAX_PATHS}: {e}"))?,
            None => DEFAULT_MAX_CONCURRENT_PATHS,
        };
        if max_concurrent_paths == 0 {
            bail!("{MULTIPATH_MAX_PATHS} must be >= 1 (0 silently disables the negotiation)");
        }
        Ok(Some(MultipathConfig {
            max_concurrent_paths,
            paths,
            keep_alive_interval: Duration::from_millis(parse_u64(
                MULTIPATH_KEEP_ALIVE_MS,
                DEFAULT_KEEP_ALIVE_MS,
            )?),
            max_idle_timeout: Duration::from_millis(parse_u64(
                MULTIPATH_IDLE_TIMEOUT_MS,
                DEFAULT_IDLE_TIMEOUT_MS,
            )?),
        }))
    }
}

/// Spawns a task that logs path lifecycle events for this connection
/// (PoC spec section 12; see experimental/mpquic-poc/README.md for where the
/// spec lives). noq never emits `Established` for the primary
/// path (PathId 0, established by the handshake itself), so callers log that
/// one explicitly at connection setup.
pub fn spawn_path_event_logger(conn: backend::Connection, side: &'static str) {
    let events = conn.path_events();
    let conn_id = conn.stable_id();
    zenoh_runtime::ZRuntime::Acceptor.spawn(async move {
        let mut events = std::pin::pin!(events);
        while let Some(event) = events.next().await {
            match event {
                Ok(backend::PathEvent::Established { id, .. }) => {
                    tracing::info!("connection={conn_id} path={id:?} side={side} state=active");
                }
                Ok(backend::PathEvent::Abandoned { id, reason, .. }) => {
                    tracing::warn!(
                        "connection={conn_id} path={id:?} side={side} state=failed reason={reason:?}"
                    );
                }
                Ok(backend::PathEvent::Discarded { id, path_stats, .. }) => {
                    tracing::info!("connection={conn_id} path={id:?} side={side} state=discarded");
                    tracing::debug!(
                        "connection={conn_id} path={id:?} side={side} final stats={path_stats:?}"
                    );
                }
                Ok(other) => {
                    tracing::debug!("connection={conn_id} side={side} path_event={other:?}");
                }
                Err(lagged) => {
                    tracing::warn!(
                        "connection={conn_id} side={side} path_events lagged: {lagged:?}"
                    );
                }
            }
        }
    });
}

/// Opens the configured additional paths on an established client connection
/// (PoC spec section 9). Failures are logged, not fatal: the session
/// continues on the primary path.
pub async fn open_additional_paths(
    conn: &backend::Connection,
    config: &MultipathConfig,
    primary_remote: SocketAddr,
) {
    if !conn.is_multipath_enabled() {
        tracing::warn!(
            "connection={} multipath was requested but not negotiated (is the listener's \
             transport.link.quic.multipath.enabled set?)",
            conn.stable_id()
        );
        return;
    }
    if config.paths.is_empty() {
        tracing::info!(
            "connection={} side=client multipath negotiated without configured paths \
             (negotiation-only; no additional paths will be opened)",
            conn.stable_id()
        );
        return;
    }
    // paths[0] is the primary path, already established by the handshake.
    for spec in config.paths.iter().skip(1) {
        let local = match resolve_local_ip(&spec.local) {
            Ok(ip) => ip,
            Err(e) => {
                tracing::warn!("skipping path spec {spec:?}: {e}");
                continue;
            }
        };
        let remote = spec.remote.unwrap_or(primary_remote);
        let tuple = backend::FourTuple::new(remote, Some(local));
        let mut attempts = 0u32;
        let mut created_logged = false;
        loop {
            let open = conn.open_path(tuple, backend::PathStatus::Available);
            // OpenPath::path_id() is available before the future resolves
            // (None on immediate rejection). After a RemoteCidsExhausted
            // retry the established PathId may differ from this created
            // line (each open_path call allocates anew) — harmless.
            if !created_logged {
                if let Some(id) = open.path_id() {
                    tracing::info!(
                        "connection={} path={id:?} side=client local={local} remote={remote} \
                         state=created",
                        conn.stable_id()
                    );
                    created_logged = true;
                }
            }
            // Guard the await against connection teardown. The noq fork now
            // resolves pending OpenPath futures on terminate; this guard is
            // kept as defense in depth (upstream noq without that fix would
            // otherwise leak the future, this task and the connection state).
            let opened = tokio::select! {
                res = open => res,
                _ = conn.on_closed() => {
                    tracing::info!(
                        "connection={} side=client connection closed while opening additional \
                         paths; stopping",
                        conn.stable_id()
                    );
                    return;
                }
            };
            match opened {
                Ok(path) => {
                    tracing::info!(
                        "connection={} path={:?} side=client local={local} remote={remote} \
                         state=validated",
                        conn.stable_id(),
                        path.id()
                    );
                    break;
                }
                // Right after the handshake the peer may not have issued
                // enough CIDs for a new path yet; retry the transient error
                // (same as noq's own tests).
                Err(backend::PathError::RemoteCidsExhausted) if attempts < 50 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => {
                    tracing::warn!(
                        "connection={} path=? side=client local={local} remote={remote} \
                         state=failed reason={e}",
                        conn.stable_id()
                    );
                    break;
                }
            }
        }
    }
}

/// The out-of-band verdict passed to noq when the local interface set
/// changes: a path is recoverable iff its local IP is still present on an
/// up interface. Paths whose local IP noq has not learned yet are treated
/// as recoverable (the in-band timers remain the safety net for them).
#[derive(Debug)]
struct NetmonHint {
    conn_id: usize,
    up_ips: std::collections::HashSet<IpAddr>,
}

impl backend::NetworkChangeHint for NetmonHint {
    fn is_path_recoverable(&self, path_id: backend::PathId, tuple: backend::FourTuple) -> bool {
        let recoverable = match tuple.local_ip() {
            Some(ip) => self.up_ips.contains(&ip.to_canonical()),
            None => true,
        };
        tracing::info!(
            "connection={} path={path_id:?} netmon verdict: local={:?} recoverable={recoverable}",
            self.conn_id,
            tuple.local_ip()
        );
        recoverable
    }
}

/// The set of local IPs currently assigned to *up* interfaces. Raw
/// per-interface address lists are unusable here: `ip link set <dev> down`
/// keeps the IPv4 addresses assigned (only routes are flushed), so only the
/// up-filtered `local_addresses` set reflects usability.
fn up_ips(state: &netwatch::netmon::State) -> std::collections::HashSet<IpAddr> {
    state
        .local_addresses
        .regular
        .iter()
        .chain(state.local_addresses.loopback.iter())
        .map(|ip| ip.to_canonical())
        .collect()
}

/// Spawns the out-of-band interface monitor for one client connection
/// (PoC spec section 22): on interface loss, the paths whose local IPs
/// vanished are reported unrecoverable through noq's
/// `Endpoint::handle_network_change`, which abandons them within
/// milliseconds instead of waiting for the in-band idle timeout (the
/// timeout stays armed as the safety net). The task owns the monitor for
/// its whole life and ends with the connection.
pub fn spawn_network_monitor(
    endpoint: backend::Endpoint,
    conn: backend::Connection,
    iface_map: Vec<(String, IpAddr)>,
) {
    let conn_id = conn.stable_id();
    zenoh_runtime::ZRuntime::Acceptor.spawn(async move {
        use n0_watcher::Watcher;
        let monitor = match netwatch::netmon::Monitor::new().await {
            Ok(monitor) => monitor,
            Err(e) => {
                tracing::warn!(
                    "connection={conn_id} netmon unavailable ({e}); path failures will be \
                     detected by the in-band idle timeout only"
                );
                return;
            }
        };
        let mut watcher = monitor.interface_state();
        let mut known_up = up_ips(&watcher.get());
        tracing::debug!(
            "connection={conn_id} netmon: watching {:?}, currently up: {known_up:?}",
            iface_map
        );
        let on_closed = conn.on_closed();
        // OnClosed holds only a weak handle by design; dropping our strong
        // clone here keeps noq's implicit-close semantics intact (a monitor
        // holding the connection alive would defeat it, and would leak the
        // connection on link-construction failure paths that never call
        // close explicitly).
        drop(conn);
        tokio::pin!(on_closed);
        loop {
            tokio::select! {
                updated = watcher.updated() => {
                    let Ok(state) = updated else {
                        tracing::warn!(
                            "connection={conn_id} netmon watcher closed; falling back to \
                             in-band path timeouts"
                        );
                        break;
                    };
                    let up = up_ips(&state);
                    let mut lost_tracked = false;
                    for (name, ip) in &iface_map {
                        let ip = ip.to_canonical();
                        match (known_up.contains(&ip), up.contains(&ip)) {
                            (true, false) => {
                                lost_tracked = true;
                                tracing::warn!(
                                    "connection={conn_id} netmon: interface {name} lost {ip}; \
                                     marking its paths unrecoverable"
                                );
                            }
                            (false, true) => {
                                tracing::info!(
                                    "connection={conn_id} netmon: interface {name} regained {ip} \
                                     (paths are not re-opened automatically)"
                                );
                            }
                            _ => {}
                        }
                    }
                    if lost_tracked {
                        endpoint.handle_network_change(Some(std::sync::Arc::new(NetmonHint {
                            conn_id,
                            up_ips: up.clone(),
                        })));
                    }
                    known_up = up;
                }
                _ = &mut on_closed => break,
            }
        }
        // The monitor is dropped here (its actor aborts on drop); keeping it
        // owned until the loop ends is required for updates to keep flowing.
        drop(monitor);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local_with_remote() {
        let p: PathSpec = "local:10.20.0.1@10.20.0.2:7447".parse().unwrap();
        assert_eq!(p.local, LocalSpec::Ip("10.20.0.1".parse().unwrap()));
        assert_eq!(p.remote, Some("10.20.0.2:7447".parse().unwrap()));
    }

    #[test]
    fn parse_iface_without_remote() {
        let p: PathSpec = "iface:wwan0".parse().unwrap();
        assert_eq!(p.local, LocalSpec::Iface("wwan0".to_string()));
        assert_eq!(p.remote, None);
    }

    #[test]
    fn parse_iface_with_remote() {
        // the exact form the demo's client.json5 ships
        let p: PathSpec = "iface:c1@10.20.0.2:7447".parse().unwrap();
        assert_eq!(p.local, LocalSpec::Iface("c1".to_string()));
        assert_eq!(p.remote, Some("10.20.0.2:7447".parse().unwrap()));
    }

    #[test]
    fn reject_garbage() {
        assert!("10.20.0.1".parse::<PathSpec>().is_err());
        assert!("local:".parse::<PathSpec>().is_err());
        assert!("iface:".parse::<PathSpec>().is_err());
        assert!("local:10.20.0.1@nonsense".parse::<PathSpec>().is_err());
    }

    fn epconf_test(config: &str, f: impl FnOnce(&Config)) {
        use core::str::FromStr;
        let s = if config.is_empty() {
            "quic/10.10.0.2:7447".to_string()
        } else {
            format!("quic/10.10.0.2:7447#{config}")
        };
        let endpoint = zenoh_protocol::core::EndPoint::from_str(&s).unwrap();
        f(&endpoint.config());
    }

    #[test]
    fn config_disabled_when_key_absent() {
        epconf_test("", |c| {
            assert!(MultipathConfig::from_endpoint_config(c).unwrap().is_none());
        });
    }

    #[test]
    fn config_full() {
        epconf_test(
            "multipath=true;multipath_paths=iface:c0|iface:c1@10.20.0.2:7447;\
             multipath_max_paths=8;multipath_keep_alive_ms=500;multipath_idle_timeout_ms=2000",
            |c| {
                let mp = MultipathConfig::from_endpoint_config(c).unwrap().unwrap();
                assert_eq!(mp.max_concurrent_paths, 8);
                assert_eq!(mp.paths.len(), 2);
                assert_eq!(mp.paths[0].local, LocalSpec::Iface("c0".into()));
                assert_eq!(mp.paths[0].remote, None);
                assert_eq!(mp.paths[1].remote, Some("10.20.0.2:7447".parse().unwrap()));
                assert_eq!(mp.keep_alive_interval, Duration::from_millis(500));
                assert_eq!(mp.max_idle_timeout, Duration::from_millis(2000));
            },
        );
    }

    #[test]
    fn config_listener_form_has_empty_paths() {
        epconf_test("multipath=true;multipath_max_paths=4", |c| {
            let mp = MultipathConfig::from_endpoint_config(c).unwrap().unwrap();
            assert!(mp.paths.is_empty());
            assert_eq!(mp.max_concurrent_paths, 4);
        });
    }

    #[test]
    fn config_rejects_zero_max_paths() {
        epconf_test("multipath=true;multipath_max_paths=0", |c| {
            assert!(MultipathConfig::from_endpoint_config(c).is_err());
        });
    }

    #[test]
    fn config_rejects_remote_on_primary() {
        epconf_test(
            "multipath=true;multipath_paths=iface:c0@10.10.0.2:7447",
            |c| {
                assert!(MultipathConfig::from_endpoint_config(c).is_err());
            },
        );
    }
}
