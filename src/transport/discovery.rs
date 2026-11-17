use crate::protocol::bcudp::codec::{read_bcudp, write_bcudp};
use crate::protocol::bcudp::model::{BcUdp, UdpDiscovery};
use crate::protocol::bcudp::xml::{C2dC, C2mQ, ClientList, UdpXml};
use crate::Error;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::{sleep_until, timeout, Instant};

const RESEND_INTERVAL: Duration = Duration::from_millis(500);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(15);

tokio::task_local! {
    /// The race of `ReolinkClient::connect_and_login_by_uid` this task is running, for `race_note`: its number in
    /// the negotiation debug lines and when it started.
    pub(crate) static DEBUG_RACE: (u32, std::time::Instant);
}

/// One negotiation debug line about connecting by UID (see `crate::diag::line`): under the race's number and the
/// time since it started when called inside one. The lines carry numbers, booleans and fixed words only (never an
/// address, a UID or a device's text): they are pasted into public reports.
pub(crate) fn race_note(text: std::fmt::Arguments) {
    if !crate::diag::on() {
        return;
    }
    match DEBUG_RACE.try_with(|race| *race) {
        Ok((race, started)) => {
            let ms = started.elapsed().as_millis();
            crate::diag::line(format_args!("[race {race} +{ms} ms] NEGOTIATION race: {text}"))
        }
        Err(_) => crate::diag::line(format_args!("NEGOTIATION race: {text}")),
    }
}

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
    /// The answer's `debug`, when it is a number: echoed in `C2R_DISC` (see `abandon`).
    pub debug: Option<u32>,
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
            debug: reply.debug.and_then(|debug| debug.trim().parse().ok()),
        }))
    })
    .await
    .map_err(|_| Error::UidResolutionFailed)
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
///
/// Returns the answer and the address it came from.
async fn resolve_uid_broadcast(
    uid: &str,
    relay_addrs: &[SocketAddr],
) -> crate::Result<(UidLookupResult, SocketAddr)> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;

    let mut queries: FuturesUnordered<_> = relay_addrs
        .iter()
        .copied()
        .map(|addr| async move {
            let socket = UdpSocket::bind("0.0.0.0:0").await?;
            resolve_uid(&socket, uid, addr).await.map(|result| (result, addr))
        })
        .collect();
    while let Some(result) = queries.next().await {
        if let Ok(result) = result {
            return Ok(result);
        }
    }
    Err(Error::UidResolutionFailed)
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
    register_fresh(socket, uid, client_id, local_port, lookup, None).await
}

/// `register`, skipping a reply that gives the `retired` session's sid: a new attempt waits within the same bound
/// for a fresh session (every retry of the official app got a new sid in our captures).
async fn register_fresh(
    socket: &UdpSocket,
    uid: &str,
    client_id: i32,
    local_port: u16,
    lookup: &UidLookupResult,
    retired: Option<u32>,
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
                race_note(format_args!(
                    "register reply R2C_C_R: rsp={} dev={} dmap={} relay={}",
                    reply.rsp,
                    reply.dev.is_some(),
                    reply.dmap.is_some(),
                    reply.relay.is_some()
                ));
                if reply.rsp != 0 {
                    return Some(Err(Error::RegisterRejected { rsp: reply.rsp }));
                }
                let sid = reply.sid?;
                if retired == Some(sid) {
                    race_note(format_args!("register reply R2C_C_R: the retired session's sid, still waiting"));
                    return None;
                }
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
            UdpXml::R2cT(reply) => {
                // It has no `rsp` of its own.
                race_note(format_args!(
                    "register reply R2C_T: dev={} dmap={}",
                    reply.dev.is_some(),
                    reply.dmap.is_some()
                ));
                None
            }
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
/// login request — then went completely unanswered. This function instead
/// sends `C2D_T` with the `sid` from `register()` and `conn: "local"` — the
/// *same* message pair `connect_relay` already uses with `conn: "relay"` —
/// and, after the device confirms, sends a `C2R_CFM{conn: "local"}` back to
/// the register server, exactly as `connect_relay` does for `conn: "relay"`.
/// `C2D_C` is apparently a separate, session-less legacy path the device
/// also answers but never treats as "connected" for a UID/`sid`-registered
/// client. `lan_connect` uses it for LAN discovery.
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
                    // The direct path leaves the nonce to the legacy login
                    // exchange (see `ReolinkClient::login`), even though a
                    // captured direct `D2C_CFM` carries `nc` too (see `D2cCfm::nc`).
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
    // the device (vs. the C2D_T/D2C_CFM pair itself); sending it is free.
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
/// production this is `lookup.reg`; tests may point it at a
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
    // place still hit the same disconnect while adding ~1s to every connect for no measured benefit,
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
/// relay won instead (~100ms).
///
/// The first `D2C_CFM` wins at once when no better path was offered (local
/// before map before relay). After a map or relay one, a better path still
/// gets `BETTER_PATH_GRACE` to confirm, and the best confirmed one wins: in a
/// capture of ours (2026-11-07) the relay confirmed 10 ms before local, and
/// the device answered our login on its LAN path, which the session pinned to
/// the relay dropped.
///
/// Confirms whichever path won back to the register server via
/// `C2R_CFM{conn: <winner>}`, using the reply's own `conn` — never
/// hardcoded, since any candidate can win.
///
/// `relay_addr` mirrors `connect_relay`'s own parameter: callers should
/// pass `register.relay.unwrap_or(lookup.relay)` so a register reply with
/// no relay offer still has one to race against.
///
/// Returns the peer and which path won, by its name only ("local",
/// "NAT-mapped" or "relay"; never an address), for the negotiation debug
/// output.
pub async fn connect_race(
    socket: &UdpSocket,
    register_addr: SocketAddr,
    relay_addr: SocketAddr,
    register: &RegisterResult,
    client_id: i32,
) -> crate::Result<(PeerHandle, &'static str)> {
    let deadline = Instant::now() + OVERALL_TIMEOUT;
    let (peer, conn, _) = connect_race_until(socket, register_addr, relay_addr, register, client_id, deadline).await?;
    Ok((peer, path_name(&conn)))
}

/// `connect_race` with its limit at `deadline`, giving the winning path as its `conn` and when the first
/// `D2C_CFM` was accepted. Ends with `P2pNegotiationTimeout` when no `D2C_CFM` came by `deadline`.
pub(crate) async fn connect_race_until(
    socket: &UdpSocket,
    register_addr: SocketAddr,
    relay_addr: SocketAddr,
    register: &RegisterResult,
    client_id: i32,
    deadline: Instant,
) -> crate::Result<(PeerHandle, String, Instant)> {
    let candidates: Vec<(&'static str, SocketAddr)> = [
        register.dev.map(|a| ("local", a)),
        register.map.map(|a| ("map", a)),
        Some(("relay", relay_addr)),
    ]
    .into_iter()
    .flatten()
    .collect();

    let best_possible = candidates.iter().map(|(conn, _)| path_rank(conn)).min().unwrap_or(2);

    let mut resend = tokio::time::interval(RESEND_INTERVAL);
    resend.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 2048];

    let (peer, winning_conn, confirmed_at): (PeerHandle, String, Instant) = async move {
        let mut best: Option<(PeerHandle, String)> = None;
        let mut confirmed_at: Option<Instant> = None;
        // A CFM confirmed shortly before the limit still wins: the grace never runs past it.
        let mut deadline = deadline;
        loop {
            tokio::select! {
                _ = sleep_until(deadline) => break,
                _ = resend.tick() => {
                    // A confirmed candidate is not asked again while a better one has its grace.
                    let confirmed = best.as_ref().map(|(_, conn)| conn.as_str());
                    for (conn, addr) in candidates.iter().filter(|(conn, _)| Some(*conn) != confirmed) {
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
                    // Tells a device that gave its nonce with the handshake from one that left it to the login.
                    race_note(format_args!(
                        "D2C_CFM on the {} path: lver={} nonce present={}",
                        path_name(&reply.conn),
                        reply.lver,
                        reply.nc.is_some()
                    ));
                    let peer = PeerHandle {
                        addr: from,
                        local_connection_id: client_id,
                        remote_connection_id: reply.did,
                        // Present on every path, including "local" — see
                        // the correction on `D2cCfm::nc`'s doc comment.
                        nonce: reply.nc.clone(),
                        is_direct: reply.conn != "relay",
                    };
                    confirmed_at.get_or_insert_with(Instant::now);
                    // The device answers on its LAN path once that path confirms (our captures), so a later
                    // local D2C_CFM must win over an earlier relay one.
                    if best.as_ref().is_none_or(|(_, conn)| path_rank(&reply.conn) < path_rank(conn)) {
                        best = Some((peer, reply.conn));
                    }
                    if best.as_ref().is_some_and(|(_, conn)| path_rank(conn) <= best_possible) {
                        break;
                    }
                    // shortcut: fixed grace; if a local D2C_CFM is seen later than this after the relay's, make
                    // it relative to the relay's own CFM time.
                    deadline = deadline.min(Instant::now() + BETTER_PATH_GRACE);
                }
            }
        }
        let (peer, conn) = best.ok_or(Error::P2pNegotiationTimeout)?;
        Ok::<_, Error>((peer, conn, confirmed_at.unwrap_or_else(Instant::now)))
    }
    .await?;

    let confirm = BcUdp::Discovery(UdpDiscovery {
        tid: rand::random::<u32>().max(1),
        payload: UdpXml::C2rCfm(crate::protocol::bcudp::xml::C2rCfm {
            sid: register.sid,
            conn: winning_conn.clone(),
            rsp: 0,
            cid: client_id,
            did: peer.remote_connection_id,
        }),
    });
    socket
        .send_to(&write_bcudp(&confirm), register_addr)
        .await?;

    Ok((peer, winning_conn, confirmed_at))
}

/// How long `connect_race` waits, after a map or relay `D2C_CFM`, for a
/// better path to confirm. The relay confirmed 10-12 ms before local in our
/// captures, and local took 74-111 ms over a VPN.
const BETTER_PATH_GRACE: Duration = Duration::from_millis(150);

/// A `D2C_CFM`'s `conn` as the negotiation debug output names the path: one of four fixed words, never the
/// device's own text.
pub(crate) fn path_name(conn: &str) -> &'static str {
    match conn {
        "local" => "local",
        "map" => "NAT-mapped",
        "relay" => "relay",
        _ => "other",
    }
}

/// Which confirmed path `connect_race` prefers: lower is better.
fn path_rank(conn: &str) -> u8 {
    match conn {
        "local" => 0,
        "map" => 1,
        _ => 2,
    }
}

/// The P2P servers asked about a UID, all in parallel. Exactly the set the
/// official Windows client resolves and queries in our captures
/// (2026-09-13): `p2p1`..`p2p16`
/// except `p2p4` and `p2p5`, and not the bare `p2p.reolink.com`.
const P2P_RELAY_HOSTNAMES: [&str; 14] = [
    "p2p1.reolink.com",
    "p2p2.reolink.com",
    "p2p3.reolink.com",
    "p2p6.reolink.com",
    "p2p7.reolink.com",
    "p2p8.reolink.com",
    "p2p9.reolink.com",
    "p2p10.reolink.com",
    "p2p11.reolink.com",
    "p2p12.reolink.com",
    "p2p13.reolink.com",
    "p2p14.reolink.com",
    "p2p15.reolink.com",
    "p2p16.reolink.com",
];

/// Shared prefix of `connect_by_uid` and `connect_by_uid_prefer_direct`: `register_attempt` with a new client id.
/// Returns everything both connect strategies need.
pub(crate) async fn resolve_and_register(
    socket: &UdpSocket,
    uid: &str,
) -> crate::Result<(UidLookupResult, RegisterResult, i32)> {
    let attempt = register_attempt(socket, uid, new_client_id(), None).await?;
    Ok((attempt.lookup, attempt.register, attempt.client_id))
}

/// A random client id, positive (`abs` of `i32::MIN` would overflow).
pub(crate) fn new_client_id() -> i32 {
    ((rand::random::<u32>() >> 1) as i32).max(1)
}

/// One accepted registration of a UID on `socket`: what its P2P attempt needs, and what `register_again` and
/// `abandon` take from it. The times are those `C2R_DISC` reports.
#[derive(Debug, Clone)]
pub(crate) struct Registration {
    pub lookup: UidLookupResult,
    pub register: RegisterResult,
    pub client_id: i32,
    /// The lookup server whose answer was accepted: the one the official app asks again (our captures).
    pub lookup_server: SocketAddr,
    /// When the attempt began, and when the accepted lookup answer came.
    pub started: Instant,
    pub answered: Instant,
    /// When the first `C2R_C` went out.
    pub registered: Instant,
}

/// The P2P part of `ReolinkClient::connect_and_login_by_uid` up to a registration: resolve the relay hostnames,
/// broadcast the UID lookup, and register our address under `client_id`, never taking the `retired` sid.
pub(crate) async fn register_attempt(
    socket: &UdpSocket,
    uid: &str,
    client_id: i32,
    retired: Option<u32>,
) -> crate::Result<Registration> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;
    use std::net::ToSocketAddrs;
    let started = Instant::now();

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
            let addrs = tokio::time::timeout(DNS_TIMEOUT, resolved)
                .await
                .ok()
                .and_then(|joined| joined.ok())
                .flatten();
            (host, addrs)
        })
        .collect();
    let mut relay_addrs = Vec::new();
    // Which name of our list each address came from, for the debug line below: filled only when it prints.
    let debug = crate::diag::on();
    let mut hosts: Vec<(SocketAddr, &'static str)> = Vec::new();
    while let Some((host, resolved)) = dns_lookups.next().await {
        if let Some(addrs) = resolved {
            // resolve_uid_broadcast binds each candidate socket to
            // "0.0.0.0:0", which cannot send to an AAAA address, so drop
            // IPv6 candidates here rather than failing on them later.
            for addr in addrs.filter(|a| a.is_ipv4()) {
                relay_addrs.push(addr);
                if debug {
                    hosts.push((addr, host));
                }
            }
        }
    }
    relay_addrs.sort();
    relay_addrs.dedup();
    if relay_addrs.is_empty() {
        return Err(Error::UidResolutionFailed);
    }

    // Query every resolved relay concurrently, each on its own socket — see
    // resolve_uid_broadcast's doc comment for why a shared socket doesn't work.
    let (lookup, answered_by) = resolve_uid_broadcast(uid, &relay_addrs).await?;
    let answered = Instant::now();
    // The name is one of `P2P_RELAY_HOSTNAMES`, the first that resolved to the address: never the address itself.
    let host = hosts.iter().find(|(addr, _)| *addr == answered_by).map_or("unknown", |(_, host)| *host);
    race_note(format_args!("lookup answered by {host}, {} server address(es) asked", relay_addrs.len()));
    register_on(socket, uid, client_id, lookup, answered_by, (started, answered), retired).await
}

/// `register_fresh` under `client_id` after the lookup `lookup_server` answered; `times` are when the attempt
/// began and when that answer came.
async fn register_on(
    socket: &UdpSocket,
    uid: &str,
    client_id: i32,
    lookup: UidLookupResult,
    lookup_server: SocketAddr,
    (started, answered): (Instant, Instant),
    retired: Option<u32>,
) -> crate::Result<Registration> {
    let local_port = socket.local_addr()?.port();
    let registered = Instant::now();
    let register = register_fresh(socket, uid, client_id, local_port, &lookup, retired).await?;
    Ok(Registration { lookup, register, client_id, lookup_server, started, answered, registered })
}

/// The next attempt after `previous` was abandoned, as the official app makes it (our captures): one lookup from
/// the new `socket` to the server whose answer `previous` accepted, then a registration under the next client id.
/// When that server gives no usable answer, the full lookup of `register_attempt` instead (our own fallback, never
/// seen in the captures).
pub(crate) async fn register_again(
    socket: &UdpSocket,
    uid: &str,
    previous: &Registration,
) -> crate::Result<Registration> {
    // The official app's retries count the client id up by one.
    let client_id = previous.client_id.checked_add(1).unwrap_or(1);
    let retired = Some(previous.register.sid);
    let started = Instant::now();
    match resolve_uid(socket, uid, previous.lookup_server).await {
        Ok(lookup) => {
            let times = (started, Instant::now());
            register_on(socket, uid, client_id, lookup, previous.lookup_server, times, retired).await
        }
        Err(_) => {
            race_note(format_args!("the lookup server asked again did not answer, asking every server"));
            register_attempt(socket, uid, client_id, retired).await
        }
    }
}

/// How many abandon messages of each kind `abandon` sends, `gap` apart: three, 0.5 s apart, in our captures.
const ABANDON_ROUNDS: u32 = 3;

/// The `debug` the official app put in every `C2R_DISC` of our captures, for a lookup answer that gave none.
const DISC_DEBUG: u32 = 251658240;

/// Abandons the silent attempt `attempt` on its own `socket`, as the official app does (our captures): in each of
/// `ABANDON_ROUNDS` rounds `gap` apart, `C2D_DISC` to the path that confirmed the session, if one did, then
/// `C2R_DISC` to the register server. Each message keeps one tid and payload through its rounds; no reply is
/// awaited and no BC logout is sent (the attempt never logged in). `confirmed` is the confirmed peer, its `conn`
/// and when its first `D2C_CFM` came; `sent` is the BC data the session sent its peer (`C2R_DISC`'s `send`);
/// `c2d_t_from` is when the first `C2D_T` went out.
pub(crate) async fn abandon(
    socket: &UdpSocket,
    uid: &str,
    attempt: &Registration,
    confirmed: Option<(&PeerHandle, &str, Instant)>,
    sent: crate::transport::connection::SentData,
    c2d_t_from: Instant,
    gap: Duration,
) {
    use crate::protocol::bcudp::xml::{C2dDisc, C2rDisc, DiscCounters, DiscTime};
    let now = Instant::now();
    let ms = |from: Instant, to: Instant| {
        i64::try_from(to.saturating_duration_since(from).as_millis()).unwrap_or(i64::MAX)
    };
    let tid = || rand::random::<u32>().max(1);
    // Nothing is received from a silent peer in our captures, hence -1.
    let recv = DiscCounters { lc: -1, rc: -1, ..Default::default() };
    let disc = C2rDisc {
        uid: uid.to_string(),
        sid: attempt.register.sid,
        time: DiscTime {
            query: ms(attempt.started, attempt.answered),
            setup: confirmed.map_or(0, |(_, _, at)| ms(attempt.started, at)),
            conn: ms(confirmed.map_or(c2d_t_from, |(_, _, at)| at), now),
        },
        conn: confirmed.map_or("unknown", |(_, conn, _)| conn).to_string(),
        rsp: if confirmed.is_some() { 0 } else { -1 },
        cid: attempt.client_id,
        send: DiscCounters {
            cnt: sent.copies,
            cntr: sent.resent,
            size: sent.bytes,
            sizer: sent.resent_bytes,
            lc: 0,
            rc: if confirmed.is_some() { 0 } else { -1 },
            ..Default::default()
        },
        recv,
        debug: attempt.lookup.debug.unwrap_or(DISC_DEBUG),
    };
    let to_register = write_bcudp(&BcUdp::Discovery(UdpDiscovery { tid: tid(), payload: UdpXml::C2rDisc(disc) }));
    let to_peer = confirmed.map(|(peer, _, _)| {
        let disc = C2dDisc { cid: peer.local_connection_id, did: peer.remote_connection_id };
        (write_bcudp(&BcUdp::Discovery(UdpDiscovery { tid: tid(), payload: UdpXml::C2dDisc(disc) })), peer.addr)
    });
    for round in 0..ABANDON_ROUNDS {
        // Each gap from the round before, so a late round never sends the next one at once.
        if round > 0 {
            tokio::time::sleep(gap).await;
        }
        if let Some((bytes, addr)) = &to_peer {
            let _ = socket.send_to(bytes, *addr).await;
        }
        let _ = socket.send_to(&to_register, attempt.lookup.reg).await;
    }
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
    connect_by_uid_with_path(socket, uid).await.map(|(peer, _)| peer)
}

/// `connect_by_uid`, also returning which path won ("local", "NAT-mapped"
/// or "relay", by name only), for the negotiation debug output.
pub async fn connect_by_uid_with_path(socket: &UdpSocket, uid: &str) -> crate::Result<(PeerHandle, &'static str)> {
    let (lookup, register_result, client_id) = resolve_and_register(socket, uid).await?;
    let relay_addr = register_result.relay.unwrap_or(lookup.relay);
    connect_race(socket, lookup.reg, relay_addr, &register_result, client_id).await
}

/// Diagnostic-only, not used by `connect_by_uid`: identical, but tries
/// `connect_direct` first whenever register gave us a device address at
/// all — regardless of whether a relay is also available — instead of only
/// as a last resort when no relay exists. Exists to test, against real
/// hardware whose relay-path login keeps failing, whether the direct path
/// (where the nonce comes from the legacy nonce exchange) succeeds where
/// relay does not.
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

/// Where `lan_connect` asks: the broadcast address of the LAN, port 2018.
// shortcut: 255.255.255.255 leaves on one interface only; add per-interface directed broadcasts if a multi-homed
// user needs them.
pub const LAN_DISCOVERY: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 2018));

/// Asks the LAN for the device `uid` with `C2D_C` sent to `to`
/// (`LAN_DISCOVERY` in the app), as the official app does alongside the
/// server lookup (our captures): one request, one tid, resent on
/// `send_and_await`'s cadence. The first `D2C_C_R` that echoes our cid gives
/// the device's LAN address and its id for a session-less direct session.
/// `D2C_C_R` names no UID: the answer proves only that a host on our LAN
/// answered for it.
pub async fn lan_connect(socket: &UdpSocket, uid: &str, to: SocketAddr) -> crate::Result<PeerHandle> {
    let client_id = rand::random::<i32>().abs().max(1);
    let request = write_bcudp(&BcUdp::Discovery(UdpDiscovery {
        tid: rand::random::<u32>().max(1),
        payload: UdpXml::C2dC(C2dC {
            uid: uid.to_string(),
            // The device answers to this port.
            cli: ClientList { port: u32::from(socket.local_addr()?.port()) },
            cid: client_id,
            mtu: 1350,
            debug: 0,
            os: "WIN".to_string(),
            pl: String::new(),
            lver: 3,
        }),
    }));
    // Counted for the debug line: the broadcasts sent, and the answers that were not ours.
    let (mut sent, mut ignored) = (0u32, 0u32);
    let build_request = || {
        sent += 1;
        request.clone()
    };
    let result = send_and_await(socket, to, build_request, |disc, from| {
        let UdpXml::D2cCr(reply) = disc.payload else {
            return None;
        };
        if reply.rsp != 0 || reply.cid != client_id {
            ignored += 1;
            return None;
        }
        Some(Ok(PeerHandle {
            addr: from,
            local_connection_id: client_id,
            remote_connection_id: reply.did,
            // The legacy nonce exchange gives it at login.
            nonce: None,
            is_direct: true,
        }))
    })
    .await;
    race_note(format_args!(
        "LAN discovery: {sent} broadcast(s) sent, answered={}, {ignored} answer(s) with another cid or rsp ignored",
        result.is_ok()
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::bcudp::model::BcUdp;
    use crate::protocol::bcudp::xml::{IpPort, UdpXml};
    use tokio::net::UdpSocket;
    use tokio::task::JoinHandle;

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
                    debug: None,
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
                    debug: None,
                }),
            });
            real_relay
                .send_to(&crate::protocol::bcudp::codec::write_bcudp(&reply), from)
                .await
                .unwrap();
        });

        let relay_addrs = [silent_a_addr, silent_b_addr, real_relay_addr];
        let (result, answered_by) = resolve_uid_broadcast("9527000EXAMPLE01", &relay_addrs)
            .await
            .unwrap();
        assert_eq!(answered_by, real_relay_addr);
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
            debug: None,
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
            debug: None,
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
        let start = Instant::now();
        let (peer, path) = connect_race(&client, register_addr, relay_addr, &register_result, 1)
            .await
            .unwrap();
        // The silent local candidate had its grace before the relay won.
        let elapsed = start.elapsed();
        assert!(elapsed >= BETTER_PATH_GRACE && elapsed < Duration::from_secs(2), "{elapsed:?}");
        assert_eq!(path, "relay");
        assert_eq!(peer.addr, relay_addr);
        assert_eq!(peer.remote_connection_id, 88);
        assert_eq!(peer.nonce, Some("654321".to_string()));
        assert!(!peer.is_direct);

        fake_relay.await.unwrap();
        confirm_check.await.unwrap();
        silent_dev.await.unwrap();
    }

    /// A connect candidate that answers the first `C2D_T` it gets, `delay`
    /// after this call, with a `D2C_CFM` of `conn` and `did`.
    fn confirming_candidate(socket: UdpSocket, conn: &'static str, did: i32, delay: Duration) -> JoinHandle<()> {
        // From the call, not the receipt: under a paused clock a spawned task may read the request a tick late.
        let answer_at = Instant::now() + delay;
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { panic!("expected discovery") };
            let UdpXml::C2dT(req) = disc.payload else { panic!("expected C2D_T") };
            assert_eq!(req.conn, conn);
            sleep_until(answer_at).await;
            let cfm = crate::protocol::bcudp::xml::D2cCfm {
                sid: req.sid,
                conn: conn.to_string(),
                rsp: 0,
                cid: req.cid,
                did,
                nc: None,
                pl: String::new(),
                lver: 3,
            };
            let reply = BcUdp::Discovery(UdpDiscovery { tid: disc.tid, payload: UdpXml::D2cCfm(cfm) });
            socket.send_to(&write_bcudp(&reply), from).await.unwrap();
        })
    }

    /// The path the register server is told: the first `C2R_CFM`'s conn and did.
    async fn confirmed_path(register: &UdpSocket) -> (String, i32) {
        let mut buf = [0u8; 2048];
        let (n, _) = register.recv_from(&mut buf).await.unwrap();
        let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { panic!("expected discovery") };
        let UdpXml::C2rCfm(cfm) = disc.payload else { panic!("expected C2R_CFM") };
        (cfm.conn, cfm.did)
    }

    /// The 2026-11-07 failure: the relay confirms first, local 20 ms later.
    /// Local wins, and the register server is told so.
    #[tokio::test]
    async fn connect_race_waits_briefly_for_local_after_a_relay_cfm() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = confirming_candidate(dev, "local", 99, Duration::from_millis(20));
        let via_relay = confirming_candidate(relay, "relay", 88, Duration::ZERO);
        let offer = RegisterResult { sid: 555, dev: Some(dev_addr), map: None, relay: Some(relay_addr) };
        let (peer, path) = connect_race(&client, register.local_addr().unwrap(), relay_addr, &offer, 1).await.unwrap();
        assert_eq!((path, peer.addr, peer.remote_connection_id, peer.is_direct), ("local", dev_addr, 99, true));
        assert_eq!(confirmed_path(&register).await, ("local".to_string(), 99));
        local.await.unwrap();
        via_relay.await.unwrap();
    }

    /// A relay CFM just before the overall limit still wins, though the
    /// silent local candidate's grace would run past that limit.
    #[tokio::test(start_paused = true)]
    async fn connect_race_keeps_a_cfm_that_comes_just_before_the_limit() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let via_relay = confirming_candidate(relay, "relay", 88, OVERALL_TIMEOUT - Duration::from_millis(100));
        let offer = RegisterResult { sid: 555, dev: Some(dev_addr), map: None, relay: Some(relay_addr) };
        let (peer, path) = connect_race(&client, register.local_addr().unwrap(), relay_addr, &offer, 1).await.unwrap();
        assert_eq!((path, peer.addr), ("relay", relay_addr));
        assert_eq!(confirmed_path(&register).await, ("relay".to_string(), 88));
        via_relay.await.unwrap();
        drop(dev);
    }

    /// With only a relay offered there is nothing better to wait for.
    #[tokio::test]
    async fn connect_race_does_not_wait_without_a_better_candidate() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let via_relay = confirming_candidate(relay, "relay", 88, Duration::ZERO);
        let offer = RegisterResult { sid: 555, dev: None, map: None, relay: Some(relay_addr) };
        let start = Instant::now();
        let (_, path) = connect_race(&client, register.local_addr().unwrap(), relay_addr, &offer, 1).await.unwrap();
        assert!(start.elapsed() < BETTER_PATH_GRACE, "{:?}", start.elapsed());
        assert_eq!(path, "relay");
        via_relay.await.unwrap();
    }

    /// The Home Hub's usual case: local confirms first and wins at once.
    #[tokio::test]
    async fn connect_race_returns_at_once_when_local_answers_first() {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (dev_addr, relay_addr) = (dev.local_addr().unwrap(), relay.local_addr().unwrap());
        let register = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = confirming_candidate(dev, "local", 99, Duration::ZERO);
        let offer = RegisterResult { sid: 555, dev: Some(dev_addr), map: None, relay: Some(relay_addr) };
        let start = Instant::now();
        let (peer, path) = connect_race(&client, register.local_addr().unwrap(), relay_addr, &offer, 1).await.unwrap();
        assert!(start.elapsed() < BETTER_PATH_GRACE, "{:?}", start.elapsed());
        assert_eq!((path, peer.addr), ("local", dev_addr));
        assert_eq!(confirmed_path(&register).await, ("local".to_string(), 99));
        local.await.unwrap();
        drop(relay);
    }

    /// The next `C2D_C` a fake device gets: the request, its tid and where it came from.
    async fn lan_request(device: &UdpSocket) -> (C2dC, u32, SocketAddr) {
        let mut buf = [0u8; 2048];
        let (n, from) = device.recv_from(&mut buf).await.unwrap();
        let Ok(Some((BcUdp::Discovery(disc), _))) = read_bcudp(&buf[..n]) else { panic!("expected discovery") };
        let UdpXml::C2dC(request) = disc.payload else { panic!("expected C2D_C") };
        (request, disc.tid, from)
    }

    async fn send_d2c_c_r(device: &UdpSocket, to: SocketAddr, rsp: i32, cid: i32, did: i32) {
        let reply = crate::protocol::bcudp::xml::D2cCr { rsp, cid, did };
        let reply = BcUdp::Discovery(UdpDiscovery { tid: 1, payload: UdpXml::D2cCr(reply) });
        device.send_to(&write_bcudp(&reply), to).await.unwrap();
    }

    /// The request has the captured fields and keeps its tid on a resend;
    /// the device's answer gives its address and id.
    #[tokio::test]
    async fn lan_connect_finds_the_device_that_answers_c2d_c() {
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let device_addr = device.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = u32::from(client.local_addr().unwrap().port());
        let fake = tokio::spawn(async move {
            let (first, first_tid, _) = lan_request(&device).await;
            let (again, tid, from) = lan_request(&device).await;
            assert_eq!((first, first_tid), (again.clone(), tid), "the resend is the same request");
            let fields = (again.uid.as_str(), again.cli.port, again.mtu, again.debug, again.os.as_str(), again.lver);
            assert_eq!(fields, ("9527000EXAMPLE01", client_port, 1350, 0, "WIN", 3));
            send_d2c_c_r(&device, from, 0, again.cid, 7).await;
        });
        let peer = lan_connect(&client, "9527000EXAMPLE01", device_addr).await.unwrap();
        assert_eq!((peer.addr, peer.remote_connection_id, peer.nonce, peer.is_direct), (device_addr, 7, None, true));
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn lan_connect_ignores_an_answer_with_another_cid_or_rsp() {
        let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let device_addr = device.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake = tokio::spawn(async move {
            let (request, _, from) = lan_request(&device).await;
            send_d2c_c_r(&device, from, 0, request.cid.wrapping_add(1), 1).await;
            send_d2c_c_r(&device, from, -1, request.cid, 2).await;
            send_d2c_c_r(&device, from, 0, request.cid, 3).await;
        });
        let peer = lan_connect(&client, "9527000EXAMPLE01", device_addr).await.unwrap();
        assert_eq!(peer.remote_connection_id, 3);
        fake.await.unwrap();
    }
}
