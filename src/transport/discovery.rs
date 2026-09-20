use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
use crate::protocol::bcudp::xml::{C2mQ, UdpXml};
use crate::Error;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const RESEND_INTERVAL: Duration = Duration::from_millis(500);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Sends a request to `dest` on a fixed `RESEND_INTERVAL` cadence and
/// inspects every reply via `matcher` until it returns `Some` or
/// `OVERALL_TIMEOUT` elapses.
///
/// Resending is driven by its own timer (not by "did we just get an
/// unrelated packet"): an earlier version resent on every loop iteration,
/// including ones where a packet arrived but didn't match — against the
/// real Reolink relays, several concurrent queries sharing one socket
/// turned that into a resend storm (each query's misses fired resends for
/// every other query's replies too) that looked exactly like a network
/// hang from the outside.
///
/// `build_request` is called fresh for every send (not just once) so each
/// attempt carries its own `tid`. Investigating a run where a query
/// stalled for the full 15s on one attempt and then succeeded in 30ms on
/// the very next (same socket, same destination, same UID — see
/// `probe_specific_relay`, 2026-09-13) showed the only real difference
/// between the two was the random tid baked into the request: a fixed
/// `request_bytes` made every one of the ~30 resends in a 15s attempt an
/// identical, and possibly equally-unlucky, roll. Rebuilding per send
/// turns each attempt into ~30 independent rolls instead of one.
async fn send_and_await<T>(
    socket: &UdpSocket,
    dest: SocketAddr,
    mut build_request: impl FnMut() -> Vec<u8>,
    mut matcher: impl FnMut(UdpDiscovery, SocketAddr) -> Option<crate::Result<T>>,
) -> crate::Result<T> {
    let mut resend = tokio::time::interval(RESEND_INTERVAL);
    resend.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 2048];
    timeout(OVERALL_TIMEOUT, async {
        loop {
            tokio::select! {
                _ = resend.tick() => {
                    socket.send_to(&build_request(), dest).await?;
                }
                recv = socket.recv_from(&mut buf) => {
                    let (n, from) = recv?;
                    // A packet we can't parse (unmodeled message type,
                    // garbage, or meant for a different in-flight query
                    // sharing this socket) should not abort the whole
                    // wait — only a genuine timeout or a matched reply
                    // should end it. Silently skipping used to be a hard
                    // error here, which meant an unrelated stray packet
                    // could kill an otherwise-still-live wait.
                    let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else {
                        continue;
                    };
                    if let Some(result) = matcher(disc, from) {
                        return result;
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| Error::P2pNegotiationTimeout)?
}

#[derive(Debug, Clone, Copy)]
pub struct UidLookupResult {
    pub reg: SocketAddr,
    pub relay: SocketAddr,
}

/// Asks one P2P relay server what it knows about `uid`. Retries the send
/// every `RESEND_INTERVAL` (UDP has no delivery guarantee) until a reply
/// arrives or `OVERALL_TIMEOUT` elapses.
pub async fn resolve_uid(
    socket: &UdpSocket,
    uid: &str,
    relay_addr: SocketAddr,
) -> crate::Result<UidLookupResult> {
    let build_request = || {
        write_bcudp(&BcUdp::Discovery(UdpDiscovery {
            tid: rand::random::<u32>().max(1),
            payload: UdpXml::C2mQ(C2mQ {
                uid: uid.to_string(),
                ver: 3,
                os: "WIN".to_string(),
            }),
        }))
    };
    send_and_await(socket, relay_addr, build_request, |disc, from| {
        // Real relays reply with their own tid, not ours, so we match by
        // source address instead.
        if from != relay_addr {
            return None;
        }
        let UdpXml::M2cQr(reply) = disc.payload else {
            return None;
        };
        let reg = reply.reg?;
        let relay = reply.relay?;
        let reg_ip = reg.ip.parse().ok()?;
        let relay_ip = relay.ip.parse().ok()?;
        Some(Ok(UidLookupResult {
            reg: SocketAddr::new(reg_ip, reg.port),
            relay: SocketAddr::new(relay_ip, relay.port),
        }))
    })
    .await
    .map_err(|_| Error::UidResolutionFailed { uid: uid.to_string() })
}

/// Queries every address in `relay_addrs` for `uid` concurrently, each on
/// its *own freshly bound socket*, and returns as soon as any one replies
/// with real data.
///
/// A single reader broadcasting to all N addresses from one shared socket
/// was tried and ruled out on 2026-09-13: N concurrent [`resolve_uid`]
/// calls sharing one socket lose replies outright (a reply meant for task
/// A can be dequeued by task B's `recv_from`, found not to match B's
/// expected source address, and discarded — gone, not delayed), and a
/// single-reader dispatcher fixing that still saw zero replies arrive
/// (confirmed via `tcpdump`) when fanning out to all ~11 addresses from
/// one local port.
///
/// Giving every candidate its own socket removes the shared-reader race.
/// It does *not* fully explain the observed flakiness, though: a later
/// same-socket, same-destination, same-UID repeat query stalled the full
/// [`OVERALL_TIMEOUT`] on one attempt and then succeeded in ~30ms on the
/// very next, which points at something tid-dependent in the request
/// rather than purely at address fan-out — see `send_and_await`'s doc
/// comment for the per-send tid fix. Treat this function as a
/// last-known-good mitigation, not a confirmed root-cause fix.
async fn resolve_uid_broadcast(
    uid: &str,
    relay_addrs: &[SocketAddr],
) -> crate::Result<UidLookupResult> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;

    let mut queries: FuturesUnordered<_> = relay_addrs
        .iter()
        .copied()
        .map(|addr| async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            resolve_uid(&socket, uid, addr).await
        })
        .collect();
    while let Some(result) = queries.next().await {
        if let Ok(result) = result {
            return Ok(result);
        }
    }
    Err(Error::UidResolutionFailed { uid: uid.to_string() })
}

#[derive(Debug, Clone, Copy)]
pub struct RegisterResult {
    pub sid: u32,
    pub dev: Option<SocketAddr>,
    /// The device's externally NAT-mapped address — a third connect
    /// candidate alongside `dev` (LAN-local) and `relay`. See
    /// `R2cCr::dmap`'s doc comment for how this was found.
    pub map: Option<SocketAddr>,
    pub relay: Option<SocketAddr>,
}

/// Tells the register server our address for `uid` and asks it for the
/// device's own address (if it knows one) or a relay to use instead.
pub async fn register(
    socket: &UdpSocket,
    uid: &str,
    client_id: i32,
    local_port: u16,
    lookup: &UidLookupResult,
) -> crate::Result<RegisterResult> {
    // `cli.ip` needs our real outbound-routing local IP, not a placeholder.
    // `socket` itself is bound wildcard (0.0.0.0:0, see
    // `bind_udp_socket_with_large_rcvbuf`) and deliberately left unconnected
    // — it receives from several different peers over its lifetime, matched
    // manually against each reply's source address — so we can't
    // `.connect()`/`.local_addr()` it directly without permanently pinning
    // it to one peer. Instead, open a throwaway socket, connect *it* to the
    // same address this C2R_C request is about to be sent to (`lookup.reg`),
    // and read back the local IP the OS would route through — the standard
    // "connect to learn your outbound IP" trick — then drop it.
    let local_ip = {
        let probe = UdpSocket::bind("0.0.0.0:0").await?;
        probe.connect(lookup.reg).await?;
        probe.local_addr()?.ip()
    };

    let build_request = || {
        write_bcudp(&BcUdp::Discovery(UdpDiscovery {
            tid: rand::random::<u32>().max(1),
            payload: UdpXml::C2rC(crate::protocol::bcudp::xml::C2rC {
                uid: uid.to_string(),
                cli: crate::protocol::bcudp::xml::IpPort {
                    ip: local_ip.to_string(),
                    port: local_port,
                },
                relay: crate::protocol::bcudp::xml::IpPort {
                    ip: lookup.relay.ip().to_string(),
                    port: lookup.relay.port(),
                },
                cid: client_id,
                debug: false,
                family: 4,
                os: "WIN".to_string(),
                revision: Some(3),
            }),
        }))
    };
    send_and_await(socket, lookup.reg, build_request, |disc, from| {
        if from != lookup.reg {
            return None;
        }
        match disc.payload {
            UdpXml::R2cCr(reply) => {
                if reply.rsp != 0 {
                    return Some(Err(Error::ProtocolError(format!(
                        "register rejected with rsp={}",
                        reply.rsp
                    ))));
                }
                let sid = reply.sid?;
                Some(Ok(RegisterResult {
                    sid,
                    dev: reply
                        .dev
                        .and_then(|d| SocketAddr::new(d.ip.parse().ok()?, d.port).into()),
                    map: reply
                        .dmap
                        .and_then(|d| SocketAddr::new(d.ip.parse().ok()?, d.port).into()),
                    relay: reply
                        .relay
                        .and_then(|d| SocketAddr::new(d.ip.parse().ok()?, d.port).into()),
                }))
            }
            // The real register server sends this first (within ~40ms) for
            // every request, direct or not, then follows up ~100ms later
            // with an R2C_C_R for the *same* session that carries the
            // actual relay endpoint to use — confirmed against a real
            // capture, 2026-09-13, across three different UIDs, all
            // showing the identical two-reply pattern. `dev`/`dmap` here
            // can't be trusted as the final word (R2C_C_R repeats the same
            // `dev` alongside the relay info this message never carries),
            // so treat this one as a keepalive and keep waiting for the
            // real answer instead of returning early on it.
            UdpXml::R2cT(_) => None,
            _ => None,
        }
    })
    .await
}

#[derive(Debug, Clone)]
pub struct PeerHandle {
    pub addr: SocketAddr,
    pub local_connection_id: i32,
    pub remote_connection_id: i32,
    /// The login nonce, when the connection handshake delivered one (only
    /// the relay path's `D2C_CFM` does — see `D2cCfm::nc`). `login()` uses
    /// this instead of performing the legacy nonce-exchange BC message when
    /// present.
    pub nonce: Option<String>,
    /// Whether this connection is direct (device address) rather than via
    /// relay. The direct path needs a periodic `C2D_HB` keepalive or real
    /// hardware never processes any BC data sent to it — see
    /// `BcConnection::spawn_direct_keepalive`.
    pub is_direct: bool,
}

/// Client-initiated direct connect to the device's own address (`dev_addr`,
/// from `RegisterResult::dev`).
///
/// Earlier versions of this function sent `C2D_C`/waited for `D2C_C_R` —
/// the pure-LAN-discovery message pair used when a device has no prior
/// `sid` (see [`C2dC`](crate::protocol::bcudp::xml::C2dC)/[`D2cCr`](crate::protocol::bcudp::xml::D2cCr)).
/// That elicited a reply from the real NVR (so P2P "connect" looked
/// successful) but every later BC message — including `C2D_HB` and the
/// login request — then went completely unanswered. Both `bairelay`
/// (`connection/discovery.rs::client_initiated_dev`, read-only reference)
/// and `nodelink-js` (`bcudp/BcUdpStream.ts::p2pClientInitiated`, same)
/// instead send `C2D_T` with the `sid` from `register()` and
/// `conn: "local"` — the *same* message pair `connect_relay` already uses
/// with `conn: "relay"` — and, after the device confirms, send a
/// `C2R_CFM{conn: "local"}` back to the register server, exactly as
/// `connect_relay` does for `conn: "relay"`. `C2D_C` is apparently a
/// separate, session-less legacy path the device also answers but never
/// treats as "connected" for a UID/`sid`-registered client.
pub async fn connect_direct(
    socket: &UdpSocket,
    register_addr: SocketAddr,
    dev_addr: SocketAddr,
    register: &RegisterResult,
    client_id: i32,
) -> crate::Result<PeerHandle> {
    let build_request = || {
        write_bcudp(&BcUdp::Discovery(UdpDiscovery {
            tid: rand::random::<u32>().max(1),
            payload: UdpXml::C2dT(crate::protocol::bcudp::xml::C2dT {
                sid: register.sid,
                conn: "local".to_string(),
                cid: client_id,
                mtu: 1350,
                pl: String::new(),
                lver: 3,
            }),
        }))
    };
    let peer = send_and_await(socket, dev_addr, build_request, |disc, from| {
        match disc.payload {
            UdpXml::D2cCfm(reply) => {
                // sid+cid are the correlation signal here, not tid.
                if reply.rsp != 0 || reply.cid != client_id || reply.sid != register.sid {
                    return None;
                }
                Some(Ok(PeerHandle {
                    addr: from,
                    local_connection_id: client_id,
                    remote_connection_id: reply.did,
                    // Per bairelay: conn="local" never carries the sigV3
                    // nonce (relay-only, cloud-account gated).
                    nonce: None,
                    is_direct: true,
                }))
            }
            // The register server's way of saying "give up waiting on this
            // session" — observed sent ~13s after a real device never
            // answered a direct-connect attempt. Failing immediately
            // instead of waiting out the rest of OVERALL_TIMEOUT lets
            // connect_by_uid fall back to the relay path much sooner.
            UdpXml::R2cDisc(_) => Some(Err(Error::P2pNegotiationTimeout)),
            _ => None,
        }
    })
    .await?;

    // Confirm the local/direct path to the register server — mirrors
    // connect_relay's post-connect C2R_CFM, just with conn="local". Not yet
    // verified against real hardware whether this confirm is what unblocks
    // the device (vs. the C2D_T/D2C_CFM pair itself); sending it is free
    // and matches both reference implementations exactly.
    let confirm = BcUdp::Discovery(UdpDiscovery {
        tid: rand::random::<u32>().max(1),
        payload: UdpXml::C2rCfm(crate::protocol::bcudp::xml::C2rCfm {
            sid: register.sid,
            conn: "local".to_string(),
            rsp: 0,
            cid: client_id,
            did: peer.remote_connection_id,
        }),
    });
    socket
        .send_to(&write_bcudp(&confirm), register_addr)
        .await?;

    Ok(peer)
}

/// `register_addr` is where the final C2R_CFM confirmation is sent — in
/// production this is `lookup.reg` from Task 12/13; tests may point it at a
/// throwaway socket since the client doesn't wait for a reply to it.
///
/// `relay_addr` is the caller's choice, not `register.relay` alone: the
/// register server's R2C_T reply variant (the direct-only path — no relay
/// offer at all, `register.relay` always `None`) still needs a relay to
/// fall back to when the `dev` address it gave turns out to be
/// unreachable (e.g. a private LAN address from a device on a different
/// network — confirmed against a real NVR on 2026-09-13). The original
/// M2C_Q_R discovery reply (`UidLookupResult::relay`) is always available
/// for that, so callers should pass `register.relay.unwrap_or(lookup.relay)`.
pub async fn connect_relay(
    socket: &UdpSocket,
    register_addr: SocketAddr,
    relay_addr: SocketAddr,
    register: &RegisterResult,
    client_id: i32,
) -> crate::Result<PeerHandle> {
    let build_request = || {
        write_bcudp(&BcUdp::Discovery(UdpDiscovery {
            tid: rand::random::<u32>().max(1),
            payload: UdpXml::C2dT(crate::protocol::bcudp::xml::C2dT {
                sid: register.sid,
                conn: "relay".to_string(),
                cid: client_id,
                mtu: 1350,
                pl: String::new(),
                lver: 3,
            }),
        }))
    };
    let peer = send_and_await(socket, relay_addr, build_request, |disc, from| {
        let UdpXml::D2cCfm(reply) = disc.payload else {
            return None;
        };
        // sid+cid are the correlation signal here, not tid.
        if reply.rsp != 0 || reply.cid != client_id || reply.sid != register.sid {
            return None;
        }
        Some(Ok(PeerHandle {
            addr: from,
            local_connection_id: client_id,
            remote_connection_id: reply.did,
            nonce: reply.nc.clone(),
            is_direct: false,
        }))
    })
    .await?;

    // Confirm the relay path to the register server (fire-and-forget: the
    // register server never replies to this). Real captured traffic shows
    // the official client sending this exact message 3 times over ~1s
    // instead of once — tried reproducing that (2026-09-13) on the theory
    // that a single lost packet here was causing the device to tear the
    // relay session down, but a real-hardware retest with the retry in
    // place still hit the same disconnect (see `.plans/reolink-linux-project.md`,
    // bug #13) while adding ~1s to every connect for no measured benefit,
    // so reverted back to a single send.
    let confirm = BcUdp::Discovery(UdpDiscovery {
        tid: rand::random::<u32>().max(1),
        payload: UdpXml::C2rCfm(crate::protocol::bcudp::xml::C2rCfm {
            sid: register.sid,
            conn: "relay".to_string(),
            rsp: 0,
            cid: client_id,
            did: peer.remote_connection_id,
        }),
    });
    socket
        .send_to(&write_bcudp(&confirm), register_addr)
        .await?;

    Ok(peer)
}

/// Sends `C2D_T` to every candidate we have — `register.dev` (LAN-local,
/// `conn: "local"`), `register.map` (NAT-external, `conn: "map"`), and
/// `relay_addr` (`conn: "relay"`) — all at once on the same socket, and
/// returns as soon as any of them answers with a matching `D2C_CFM`.
///
/// This mirrors the official Windows client exactly, not the old
/// serial/conditional strategy `connect_direct`+`connect_relay` were
/// combined under in `connect_by_uid`. A real capture of the official
/// client connecting over VPN (2026-09-16) showed it firing all three
/// `C2D_T` within ~300us of receiving `R2C_C_R` — not trying local first
/// and only falling back to relay on failure. With VPN on, the LAN-local
/// candidate answered first (~80ms) and won; with VPN off (so the LAN
/// address is genuinely unreachable), local's `D2C_CFM` never arrived and
/// relay won instead (~100ms) — a real race, decided purely by whichever
/// candidate answers first, not a priority order.
///
/// Confirms whichever path won back to the register server via
/// `C2R_CFM{conn: <winner>}`, using the reply's own `conn` — never
/// hardcoded, since any candidate can win.
///
/// `relay_addr` mirrors `connect_relay`'s own parameter: callers should
/// pass `register.relay.unwrap_or(lookup.relay)` so a register reply with
/// no relay offer still has one to race against.
pub async fn connect_race(
    socket: &UdpSocket,
    register_addr: SocketAddr,
    relay_addr: SocketAddr,
    register: &RegisterResult,
    client_id: i32,
) -> crate::Result<PeerHandle> {
    let candidates: Vec<(&'static str, SocketAddr)> = [
        register.dev.map(|a| ("local", a)),
        register.map.map(|a| ("map", a)),
        Some(("relay", relay_addr)),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut resend = tokio::time::interval(RESEND_INTERVAL);
    resend.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 2048];

    let (peer, winning_conn): (PeerHandle, String) = timeout(OVERALL_TIMEOUT, async move {
        loop {
            tokio::select! {
                _ = resend.tick() => {
                    for (conn, addr) in &candidates {
                        let request = write_bcudp(&BcUdp::Discovery(UdpDiscovery {
                            tid: rand::random::<u32>().max(1),
                            payload: UdpXml::C2dT(crate::protocol::bcudp::xml::C2dT {
                                sid: register.sid,
                                conn: conn.to_string(),
                                cid: client_id,
                                mtu: 1350,
                                pl: String::new(),
                                lver: 3,
                            }),
                        }));
                        let _ = socket.send_to(&request, *addr).await;
                    }
                }
                recv = socket.recv_from(&mut buf) => {
                    let (n, from) = recv?;
                    let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else {
                        continue;
                    };
                    let UdpXml::D2cCfm(reply) = disc.payload else {
                        continue;
                    };
                    // sid+cid are the correlation signal here, not tid.
                    if reply.rsp != 0 || reply.cid != client_id || reply.sid != register.sid {
                        continue;
                    }
                    let peer = PeerHandle {
                        addr: from,
                        local_connection_id: client_id,
                        remote_connection_id: reply.did,
                        // Present on every path, including "local" — see
                        // the correction on `D2cCfm::nc`'s doc comment.
                        nonce: reply.nc.clone(),
                        is_direct: reply.conn != "relay",
                    };
                    return Ok::<_, crate::Error>((peer, reply.conn.clone()));
                }
            }
        }
    })
    .await
    .map_err(|_| Error::P2pNegotiationTimeout)??;

    let confirm = BcUdp::Discovery(UdpDiscovery {
        tid: rand::random::<u32>().max(1),
        payload: UdpXml::C2rCfm(crate::protocol::bcudp::xml::C2rCfm {
            sid: register.sid,
            conn: winning_conn,
            rsp: 0,
            cid: client_id,
            did: peer.remote_connection_id,
        }),
    });
    socket
        .send_to(&write_bcudp(&confirm), register_addr)
        .await?;

    Ok(peer)
}

const P2P_RELAY_HOSTNAMES: [&str; 12] = [
    "p2p.reolink.com",
    "p2p1.reolink.com",
    "p2p2.reolink.com",
    "p2p3.reolink.com",
    "p2p4.reolink.com",
    "p2p5.reolink.com",
    "p2p6.reolink.com",
    "p2p7.reolink.com",
    "p2p8.reolink.com",
    "p2p9.reolink.com",
    "p2p10.reolink.com",
    "p2p11.reolink.com",
];

/// Shared prefix of `connect_by_uid`: resolve the relay hostnames, broadcast
/// the UID lookup, and register our address. Returns everything both the
/// relay-preferring and direct-preferring connect strategies need.
async fn resolve_and_register(
    socket: &UdpSocket,
    uid: &str,
) -> crate::Result<(UidLookupResult, RegisterResult, i32)> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;
    use std::net::ToSocketAddrs;

    // `to_socket_addrs()` is a blocking syscall with no timeout of its own,
    // so a slow or blocked resolver (common behind a VPN) can otherwise
    // hang the whole connect attempt indefinitely. Resolve every hostname
    // off the async executor via spawn_blocking, concurrently, each bounded
    // by DNS_TIMEOUT.
    const DNS_TIMEOUT: Duration = Duration::from_secs(3);
    let mut dns_lookups: FuturesUnordered<_> = P2P_RELAY_HOSTNAMES
        .iter()
        .map(|&host| async move {
            let resolved =
                tokio::task::spawn_blocking(move || format!("{host}:9999").to_socket_addrs().ok());
            tokio::time::timeout(DNS_TIMEOUT, resolved)
                .await
                .ok()
                .and_then(|joined| joined.ok())
                .flatten()
        })
        .collect();
    let mut relay_addrs = Vec::new();
    while let Some(resolved) = dns_lookups.next().await {
        if let Some(addrs) = resolved {
            // resolve_uid_broadcast binds each candidate socket to
            // "0.0.0.0:0", which cannot send to an AAAA address, so drop
            // IPv6 candidates here rather than failing on them later.
            relay_addrs.extend(addrs.filter(|a| a.is_ipv4()));
        }
    }
    relay_addrs.sort();
    relay_addrs.dedup();
    if relay_addrs.is_empty() {
        return Err(Error::UidResolutionFailed { uid: uid.to_string() });
    }

    // Query every resolved relay concurrently, each on its own socket — see
    // resolve_uid_broadcast's doc comment for why a shared socket doesn't work.
    let lookup = resolve_uid_broadcast(uid, &relay_addrs).await?;

    let local_port = socket.local_addr()?.port();
    let client_id = rand::random::<i32>().abs().max(1);
    let register_result = register(socket, uid, client_id, local_port, &lookup).await?;
    Ok((lookup, register_result, client_id))
}

/// Resolves `uid` against whichever relay hostname answers first, registers
/// our address, then races every connect candidate the register server
/// gave us (LAN-local, NAT-external "map", and relay) via `connect_race`.
///
/// **Corrected 2026-09-16.** This function used to skip the direct
/// attempt entirely whenever register also handed back a relay address,
/// on the theory (from a 2026-09-13 capture) that the real Windows client
/// never bothers with direct once relay is available. A fresh capture of
/// that same client connecting over VPN disproves it: `R2C_C_R` carries a
/// relay address in every real reply we've seen, yet the client still
/// fires `C2D_T{conn: "local"}` (and `"map"`) at the same instant as
/// `C2D_T{conn: "relay"}` and just uses whichever answers first — see
/// `connect_race`'s doc comment for the full evidence. The old logic
/// meant our client could never reach a direct/local connection under
/// real conditions (register always seems to offer a relay as a
/// fallback), which is very likely why a real throughput comparison
/// (2026-09-16) found the official client sustaining the camera's full
/// ~8.8Mbit/s encoder rate on both direct and relay paths while ours,
/// always forced onto relay, managed only ~3.5Mbit/s there.
pub async fn connect_by_uid(socket: &UdpSocket, uid: &str) -> crate::Result<PeerHandle> {
    let (lookup, register_result, client_id) = resolve_and_register(socket, uid).await?;
    let relay_addr = register_result.relay.unwrap_or(lookup.relay);
    connect_race(socket, lookup.reg, relay_addr, &register_result, client_id).await
}

/// Diagnostic-only, not used by `connect_by_uid`: identical, but tries
/// `connect_direct` first whenever register gave us a device address at
/// all — regardless of whether a relay is also available — instead of only
/// as a last resort when no relay exists. Exists to test, against real
/// hardware whose relay-path login keeps failing, whether the direct path
/// (where the legacy nonce exchange is the documented mechanism, per
/// neolink) succeeds where relay does not. Delete once that question is
/// answered.
pub async fn connect_by_uid_prefer_direct(socket: &UdpSocket, uid: &str) -> crate::Result<PeerHandle> {
    let (lookup, register_result, client_id) = resolve_and_register(socket, uid).await?;

    if let Some(dev_addr) = register_result.dev {
        if let Ok(peer) =
            connect_direct(socket, lookup.reg, dev_addr, &register_result, client_id).await
        {
            return Ok(peer);
        }
    }

    let relay_addr = register_result.relay.unwrap_or(lookup.relay);
    connect_relay(socket, lookup.reg, relay_addr, &register_result, client_id).await
}

/// A real device that IS reachable directly responds to `C2D_T` within
/// about a second (matches `connect_by_uid_prefer_direct`'s own observed
/// timing); a device that ISN'T only tells us so via the register server's
/// `R2C_DISC`, sent roughly `OVERALL_TIMEOUT` after the attempt starts (see
/// `connect_direct`'s own doc comment). Bounding the attempt at this much
/// shorter timeout instead turns "device unreachable directly" into a
/// prompt fall-through to relay rather than a multi-second hang that reads,
/// to a user watching a static "Connecting..." label, as the app simply
/// not connecting — confirmed against real hardware 2026-09-14 (VPN off,
/// so the device really isn't reachable directly) via
/// `ReolinkClient::connect_by_uid_prefer_tcp`, the only caller.
const OPPORTUNISTIC_DIRECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Like `connect_by_uid_prefer_direct`, but bounds the opportunistic direct
/// attempt at `OPPORTUNISTIC_DIRECT_TIMEOUT` instead of letting it run the
/// full `OVERALL_TIMEOUT` before falling back to relay. Used by
/// `ReolinkClient::connect_by_uid_prefer_tcp` to attempt a direct (and
/// from there, TCP) session without a long hang on devices that turn out
/// not to be reachable directly.
pub async fn connect_by_uid_prefer_direct_bounded(
    socket: &UdpSocket,
    uid: &str,
) -> crate::Result<PeerHandle> {
    let (lookup, register_result, client_id) = resolve_and_register(socket, uid).await?;

    if let Some(dev_addr) = register_result.dev {
        if let Ok(Ok(peer)) = timeout(
            OPPORTUNISTIC_DIRECT_TIMEOUT,
            connect_direct(socket, lookup.reg, dev_addr, &register_result, client_id),
        )
        .await
        {
            return Ok(peer);
        }
    }

    let relay_addr = register_result.relay.unwrap_or(lookup.relay);
    connect_relay(socket, lookup.reg, relay_addr, &register_result, client_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bcudp::model::BcUdp;
    use crate::protocol::bcudp::xml::{IpPort, UdpXml};
    use tokio::net::UdpSocket;

    #[tokio::test]
    #[ignore = "hits the real Reolink relay network; run manually with --ignored"]
    async fn resolve_uid_broadcast_finds_the_real_relay_among_all_candidates() {
        use std::net::ToSocketAddrs;
        let addrs: Vec<SocketAddr> = P2P_RELAY_HOSTNAMES
            .iter()
            .filter_map(|h| format!("{h}:9999").to_socket_addrs().ok()?.next())
            .collect();
        assert!(!addrs.is_empty(), "failed to resolve any relay hostname");
        let result = resolve_uid_broadcast("9527000EXAMPLE01", &addrs).await;
        assert!(result.is_ok(), "expected the real relay to answer: {result:?}");
    }

    #[tokio::test]
    async fn resolve_uid_talks_to_a_fake_relay() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let fake_relay = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = relay.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected a discovery packet");
            };
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::M2cQr(crate::protocol::bcudp::xml::M2cQr {
                    reg: Some(IpPort { ip: "127.0.0.1".to_string(), port: 12345 }),
                    relay: Some(IpPort { ip: "127.0.0.1".to_string(), port: 12346 }),
                }),
            });
            relay
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });

        let result = resolve_uid(&client, "9527000EXAMPLE01", relay_addr)
            .await
            .unwrap();
        assert_eq!(result.reg.port(), 12345);
        assert_eq!(result.relay.port(), 12346);
        fake_relay.await.unwrap();
    }

    #[tokio::test]
    async fn broadcast_finds_the_one_relay_with_real_data_among_several_silent_ones() {
        // Regression test for the concurrency bug found on 2026-09-13: N
        // independent resolve_uid calls sharing one socket can lose a
        // reply to another task's read. resolve_uid_broadcast must use a
        // single reader instead, so it has to find the one relay that
        // actually answers even with several others in the candidate list
        // that never respond at all.
        let silent_relay_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_a_addr = silent_relay_a.local_addr().unwrap();
        let silent_relay_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_b_addr = silent_relay_b.local_addr().unwrap();

        let real_relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let real_relay_addr = real_relay.local_addr().unwrap();
        let fake_real_relay = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = real_relay.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected a discovery packet");
            };
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::M2cQr(crate::protocol::bcudp::xml::M2cQr {
                    reg: Some(IpPort { ip: "127.0.0.1".to_string(), port: 22345 }),
                    relay: Some(IpPort { ip: "127.0.0.1".to_string(), port: 22346 }),
                }),
            });
            real_relay
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });

        let relay_addrs = [silent_a_addr, silent_b_addr, real_relay_addr];
        let result = resolve_uid_broadcast("9527000EXAMPLE01", &relay_addrs)
            .await
            .unwrap();
        assert_eq!(result.reg.port(), 22345);
        assert_eq!(result.relay.port(), 22346);
        fake_real_relay.await.unwrap();
        drop(silent_relay_a);
        drop(silent_relay_b);
    }

    #[tokio::test]
    async fn register_talks_to_a_fake_register_server() {
        let register_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register_server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let fake_register = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = register_server.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected a discovery packet");
            };
            let UdpXml::C2rC(req) = disc.payload else {
                panic!("expected C2R_C");
            };
            assert_eq!(req.cid, 1);
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::R2cCr(crate::protocol::bcudp::xml::R2cCr {
                    dev: Some(IpPort { ip: "127.0.0.1".to_string(), port: 9001 }),
                    dmap: None,
                    relay: None,
                    nat: "NULL".to_string(),
                    sid: Some(555),
                    rsp: 0,
                }),
            });
            register_server
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });

        let lookup = UidLookupResult {
            reg: register_addr,
            relay: "127.0.0.1:1".parse().unwrap(),
        };
        let result = register(&client, "9527000EXAMPLE01", 1, 40000, &lookup)
            .await
            .unwrap();
        assert_eq!(result.sid, 555);
        assert_eq!(result.dev.unwrap().port(), 9001);
        fake_register.await.unwrap();
    }

    #[tokio::test]
    async fn register_waits_past_an_r2c_t_keepalive_for_the_real_r2c_c_r() {
        // Regression test for a real bug found 2026-09-13 via packet
        // capture: the real register server sends R2C_T for *every*
        // request (not just direct-only ones) within ~40ms, then follows
        // up ~100ms later with an R2C_C_R for the same session that
        // carries the actual relay endpoint. The original code returned
        // on whichever arrived first, which was always R2C_T, so
        // `RegisterResult::relay` was always None even when a real relay
        // was available and required (none of the user's 3 real NVRs
        // have port forwarding, so direct connect never works for them).
        let register_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register_server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let fake_register = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = register_server.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected a discovery packet");
            };
            let UdpXml::C2rC(_) = disc.payload else {
                panic!("expected C2R_C");
            };

            let r2c_t = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::R2cT(crate::protocol::bcudp::xml::R2cT {
                    dmap: Some(IpPort { ip: "203.0.113.1".to_string(), port: 9001 }),
                    dev: Some(IpPort { ip: "10.0.0.5".to_string(), port: 9001 }),
                    cid: 1,
                    sid: 555,
                }),
            });
            register_server
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&r2c_t), from)
                .await
                .unwrap();

            let r2c_c_r = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::R2cCr(crate::protocol::bcudp::xml::R2cCr {
                    dev: Some(IpPort { ip: "10.0.0.5".to_string(), port: 9001 }),
                    dmap: None,
                    relay: Some(IpPort { ip: "203.0.113.1".to_string(), port: 52238 }),
                    nat: "NULL".to_string(),
                    sid: Some(555),
                    rsp: 0,
                }),
            });
            register_server
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&r2c_c_r), from)
                .await
                .unwrap();
        });

        let lookup = UidLookupResult {
            reg: register_addr,
            relay: "127.0.0.1:1".parse().unwrap(),
        };
        let result = register(&client, "9527000EXAMPLE01", 1, 40000, &lookup)
            .await
            .unwrap();
        assert_eq!(result.sid, 555);
        assert_eq!(result.relay.unwrap().port(), 52238);
        fake_register.await.unwrap();
    }

    #[tokio::test]
    async fn connect_direct_succeeds_against_a_fake_device() {
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let device_addr = device.local_addr().unwrap();
        let register_stub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register_stub.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let fake_device = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2dT(req) = disc.payload else {
                panic!("expected C2D_T");
            };
            assert_eq!(req.conn, "local");
            assert_eq!(req.sid, 555);
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::D2cCfm(crate::protocol::bcudp::xml::D2cCfm {
                    sid: req.sid,
                    conn: "local".to_string(),
                    rsp: 0,
                    cid: req.cid,
                    did: 99,
                    nc: None,
                    pl: String::new(),
                    lver: 3,
                }),
            });
            device
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });
        let drain_register_cfm = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, _) = register_stub.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2rCfm(cfm) = disc.payload else {
                panic!("expected C2R_CFM");
            };
            assert_eq!(cfm.conn, "local");
            assert_eq!(cfm.did, 99);
        });

        let register_result = RegisterResult {
            sid: 555,
            dev: Some(device_addr),
            map: None,
            relay: None,
        };
        let peer = connect_direct(&client, register_addr, device_addr, &register_result, 1)
            .await
            .unwrap();
        assert_eq!(peer.addr, device_addr);
        assert_eq!(peer.local_connection_id, 1);
        assert_eq!(peer.remote_connection_id, 99);
        assert!(peer.is_direct);
        fake_device.await.unwrap();
        drain_register_cfm.await.unwrap();
    }

    #[tokio::test]
    async fn connect_relay_succeeds_against_a_fake_relay() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let register_stub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register_stub.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let fake_relay = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = relay.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2dT(req) = disc.payload else {
                panic!("expected C2D_T");
            };
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::D2cCfm(crate::protocol::bcudp::xml::D2cCfm {
                    sid: req.sid,
                    conn: "relay".to_string(),
                    rsp: 0,
                    cid: req.cid,
                    did: 77,
                    nc: Some("123456".to_string()),
                    pl: String::new(),
                    lver: 3,
                }),
            });
            relay
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });
        let drain_register_cfm = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let _ = register_stub.recv_from(&mut buf).await;
        });

        let register_result = RegisterResult {
            sid: 555,
            dev: None,
            map: None,
            relay: Some(relay_addr),
        };
        let peer = connect_relay(&client, register_addr, relay_addr, &register_result, 1)
            .await
            .unwrap();
        assert_eq!(peer.addr, relay_addr);
        assert_eq!(peer.remote_connection_id, 77);
        assert_eq!(peer.nonce, Some("123456".to_string()));
        fake_relay.await.unwrap();
        drain_register_cfm.await.unwrap();
    }

    #[tokio::test]
    async fn connect_race_picks_whichever_candidate_answers_first_and_confirms_it() {
        // Mirrors the real VPN-off capture (2026-09-16): "local" receives
        // its C2D_T but never answers (unreachable device address), while
        // relay answers immediately and should win the race — with
        // C2R_CFM naming the actual winner, not a hardcoded path.
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dev_addr = dev.local_addr().unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let register_stub = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let register_addr = register_stub.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let silent_dev = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, _from) = dev.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2dT(req) = disc.payload else {
                panic!("expected C2D_T");
            };
            assert_eq!(req.conn, "local");
        });

        let fake_relay = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = relay.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2dT(req) = disc.payload else {
                panic!("expected C2D_T");
            };
            assert_eq!(req.conn, "relay");
            let reply = BcUdp::Discovery(crate::protocol::bcudp::model::UdpDiscovery {
                tid: disc.tid,
                payload: UdpXml::D2cCfm(crate::protocol::bcudp::xml::D2cCfm {
                    sid: req.sid,
                    conn: "relay".to_string(),
                    rsp: 0,
                    cid: req.cid,
                    did: 88,
                    nc: Some("654321".to_string()),
                    pl: String::new(),
                    lver: 3,
                }),
            });
            relay
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });

        let confirm_check = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, _from) = register_stub.recv_from(&mut buf).await.unwrap();
            let (BcUdp::Discovery(disc), _) =
                crate::protocol::bcudp::codec::read_bcudp(&buf[..n]).unwrap().unwrap()
            else {
                panic!("expected discovery");
            };
            let UdpXml::C2rCfm(cfm) = disc.payload else {
                panic!("expected C2R_CFM");
            };
            assert_eq!(cfm.conn, "relay");
            assert_eq!(cfm.did, 88);
        });

        let register_result = RegisterResult {
            sid: 555,
            dev: Some(dev_addr),
            map: None,
            relay: Some(relay_addr),
        };
        let peer = connect_race(&client, register_addr, relay_addr, &register_result, 1)
            .await
            .unwrap();
        assert_eq!(peer.addr, relay_addr);
        assert_eq!(peer.remote_connection_id, 88);
        assert_eq!(peer.nonce, Some("654321".to_string()));
        assert!(!peer.is_direct);

        fake_relay.await.unwrap();
        confirm_check.await.unwrap();
        silent_dev.await.unwrap();
    }
}
