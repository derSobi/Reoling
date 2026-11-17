//! Finding Reolink devices on the local network, the way the official app
//! does in our captures: a four-byte request broadcast to UDP port 2000 of
//! the subnet, which each camera answers to UDP port 3000 of the asker with
//! its model and UID in clear text. The request names no device, and the
//! answer is not authenticated: it says what a host on the network claims.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// The port the devices listen on for the request.
pub const REQUEST_PORT: u16 = 2000;
/// The port the devices answer to, whatever port the request came from.
pub const REPLY_PORT: u16 = 3000;
/// The whole request, the same in every one of the official app's.
const REQUEST: [u8; 4] = [0xaa, 0xaa, 0x00, 0x00];
/// The official app asks about every 11 s; a dialog that is open for a moment asks more often.
const ASK_EVERY: Duration = Duration::from_secs(4);
/// How long one wait for an answer lasts, and so how soon a search that is no longer wanted ends.
const TICK: Duration = Duration::from_millis(250);
/// How many devices one search reports. The answers are not authenticated, and a host that invents UIDs must
/// not fill the list without end; no home or small office has this many.
const MAX_FOUND: usize = 32;
/// How long the search waits for the port the devices answer to: a search that has just been told to end, such
/// as that of a dialog closed and opened again, holds it for up to `TICK` more.
const PORT_WAIT: Duration = Duration::from_millis(750);

/// A device that answered the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// As the device gave it. Shown to the user, never logged.
    pub uid: String,
    /// The model word, when the answer has one.
    pub model: Option<String>,
    /// Where the answer came from.
    pub addr: IpAddr,
}

/// Reads a device's answer. In the two answers of our captures (388 bytes, zero-filled) the bytes `aa aa` stand
/// at 104, the model word at 132 and the UID at 228, each text ended by a zero byte. An answer without the
/// marker or without a UID of the usual form is not a device's.
fn parse_answer(payload: &[u8], from: IpAddr) -> Option<Found> {
    if payload.get(104..106)? != [0xaa, 0xaa] {
        return None;
    }
    let uid = text_at(payload, 228)?;
    if uid.len() != 16 || !uid.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
        return None;
    }
    let model = text_at(payload, 132).filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_graphic() || b == b' '));
    Some(Found { uid: uid.to_string(), model: model.map(str::to_string), addr: from })
}

/// The text that starts at `at`: up to its zero byte, 32 bytes or the end of `payload`, whichever comes first.
fn text_at(payload: &[u8], at: usize) -> Option<&str> {
    let field = payload.get(at..)?;
    let field = &field[..field.len().min(32)];
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).ok()
}

/// The broadcast address of every network this machine is directly on, from the kernel's route table
/// (`/proc/net/route`): the official app sends the request there, not to 255.255.255.255.
// shortcut: reads the table's numbers as on a little-endian machine, which every platform the app is built for
// is; parse by byte order if that changes.
fn broadcast_addresses(route_table: &str) -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    for line in route_table.lines() {
        // Iface, Destination, Gateway, Flags, RefCnt, Use, Metric, Mask.
        let fields: Vec<&str> = line.split_whitespace().collect();
        let number = |at: usize| fields.get(at).and_then(|text| u32::from_str_radix(text, 16).ok());
        let (Some(destination), Some(gateway), Some(flags), Some(mask)) = (number(1), number(2), number(3), number(7))
        else {
            continue;
        };
        // A network we are on ourselves: the route is up, rejects nothing and has no gateway; not the loopback,
        // not a single host.
        let (up, reject) = (flags & 0x1 != 0, flags & 0x200 != 0);
        if !up || reject || gateway != 0 || mask == 0 || mask == u32::MAX || fields[0] == "lo" {
            continue;
        }
        let (destination, mask) = (destination.to_le_bytes(), mask.to_le_bytes());
        let broadcast = Ipv4Addr::from([0, 1, 2, 3].map(|i| destination[i] | !mask[i]));
        if !found.contains(&broadcast) {
            found.push(broadcast);
        }
    }
    found
}

/// Searches until `event` returns false: asks every `ASK_EVERY`, calls `event(Some(device))` once for each
/// device that answers, up to `MAX_FOUND` of them, and `event(None)` about every `TICK` in between. Fails when
/// the port the devices answer to stays taken for `PORT_WAIT`, as it does while the official app runs on the
/// same machine.
pub fn search(event: impl FnMut(Option<Found>) -> bool) -> std::io::Result<()> {
    let table = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    let mut targets: Vec<SocketAddr> =
        broadcast_addresses(&table).into_iter().map(|ip| SocketAddr::from((ip, REQUEST_PORT))).collect();
    if targets.is_empty() {
        targets.push(SocketAddr::from((Ipv4Addr::BROADCAST, REQUEST_PORT)));
    }
    search_on(REPLY_PORT, &targets, ASK_EVERY, event)
}

/// `search` with the port to listen on, the addresses to ask and how often.
fn search_on(
    reply_port: u16,
    targets: &[SocketAddr],
    ask_every: Duration,
    mut event: impl FnMut(Option<Found>) -> bool,
) -> std::io::Result<()> {
    let waited = Instant::now();
    let listener = loop {
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, reply_port)) {
            Ok(listener) => break listener,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && waited.elapsed() < PORT_WAIT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    };
    listener.set_read_timeout(Some(TICK))?;
    // The request leaves from a port of its own, as the official app's does.
    let asker = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    asker.set_broadcast(true)?;
    let mut seen = std::collections::HashSet::new();
    let mut ask_at = Instant::now();
    let mut buf = [0u8; 1024];
    loop {
        if Instant::now() >= ask_at {
            for target in targets {
                // A network that cannot be reached now does not end the search on the others.
                let _ = asker.send_to(&REQUEST, target);
            }
            ask_at = Instant::now() + ask_every;
        }
        let device = match listener.recv_from(&mut buf) {
            Ok((n, from)) => parse_answer(&buf[..n], from.ip())
                .filter(|found| seen.len() < MAX_FOUND && seen.insert(found.uid.clone())),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => None,
            Err(e) => return Err(e),
        };
        if !event(device) {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer laid out as the two of our captures are, with made-up values.
    fn answer(model: &str, uid: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 388];
        bytes[80..86].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x01]);
        bytes[104..106].copy_from_slice(&[0xaa, 0xaa]);
        bytes[108..117].copy_from_slice(b"192.0.2.9");
        bytes[128..130].copy_from_slice(&9000u16.to_le_bytes());
        bytes[132..132 + model.len()].copy_from_slice(model.as_bytes());
        bytes[164..181].copy_from_slice(b"02:00:00:00:00:01");
        bytes[228..228 + uid.len()].copy_from_slice(uid.as_bytes());
        bytes
    }

    fn from() -> IpAddr {
        IpAddr::from([192, 0, 2, 9])
    }

    #[test]
    fn an_answer_gives_the_model_the_uid_and_where_it_came_from() {
        let found = parse_answer(&answer("RLC-520A", "9527000EXAMPLE01"), from()).unwrap();
        assert_eq!(found.uid, "9527000EXAMPLE01");
        assert_eq!(found.model.as_deref(), Some("RLC-520A"));
        assert_eq!(found.addr, from());
        // A device that names no model is still found.
        assert_eq!(parse_answer(&answer("", "9527000EXAMPLE01"), from()).unwrap().model, None);
    }

    #[test]
    fn what_is_not_a_devices_answer_is_ignored() {
        let good = answer("RLC-520A", "9527000EXAMPLE01");
        for short in [0, 4, 105, 243] {
            assert_eq!(parse_answer(&good[..short], from()), None, "{short} bytes");
        }
        let mut no_marker = good.clone();
        no_marker[104] = 0;
        assert_eq!(parse_answer(&no_marker, from()), None);
        // Our own request, heard back on a network that echoes broadcasts.
        assert_eq!(parse_answer(&REQUEST, from()), None);
        for uid in ["", "short", "9527000example01", "9527000EXAMPLE0\u{e9}", "9527000 EXAMPLE1", "<uid>9527000EXA"] {
            assert_eq!(parse_answer(&answer("RLC-520A", uid), from()), None, "{uid:?}");
        }
        // A model that is not plain text is left out, the device is still found.
        let mut odd_model = good.clone();
        odd_model[132] = 0xff;
        assert_eq!(parse_answer(&odd_model, from()).unwrap().model, None);
        // A UID that runs to the end of the answer without its zero byte is read to the end, not past it.
        let mut bytes = good[..244].to_vec();
        bytes[228..244].copy_from_slice(b"9527000EXAMPLE01");
        assert_eq!(parse_answer(&bytes, from()).unwrap().uid, "9527000EXAMPLE01");
    }

    #[test]
    fn the_request_goes_to_the_broadcast_address_of_each_network_we_are_on() {
        // As the kernel prints it: destination, gateway and mask as hexadecimal numbers.
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eth0\t00000000\t010200C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                     eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
                     eth1\t0000FEA9\t00000000\t0001\t0\t0\t1000\t0000FFFF\t0\t0\t0\n\
                     eth2\t006433C6\t00000000\t0001\t0\t0\t0\tFFFFFFFF\t0\t0\t0\n\
                     eth3\t000200C0\t00000000\t0000\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                     eth4\t006433C6\t00000000\t0201\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                     lo\t0000007F\t00000000\t0001\t0\t0\t0\t000000FF\t0\t0\t0\n";
        let expected = [Ipv4Addr::new(192, 0, 2, 255), Ipv4Addr::new(169, 254, 255, 255)];
        assert_eq!(broadcast_addresses(table), expected);
        assert_eq!(broadcast_addresses(""), Vec::<Ipv4Addr>::new());
        let unreadable = "eth0\tzz\t00000000\t0001\t0\t0\t0\tGG\n\neth0 short\n";
        assert_eq!(broadcast_addresses(unreadable), Vec::<Ipv4Addr>::new());
    }

    /// A fake camera: answers each request it hears with `reply`, to `reply_port` of the asker.
    fn camera(reply: Vec<u8>, reply_port: u16) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                if buf[..n] == REQUEST {
                    let _ = socket.send_to(&reply, (from.ip(), reply_port));
                }
            }
        });
        addr
    }

    /// A port nothing listens on, for the search to listen on.
    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn a_search_reports_each_device_once_and_ends_when_told() {
        let reply_port = free_port();
        let first = camera(answer("RLC-520A", "9527000EXAMPLE01"), reply_port);
        let second = camera(answer("RLC-1224A", "9527000EXAMPLE02"), reply_port);
        let started = Instant::now();
        let mut found = Vec::new();
        // Long enough for several requests, which the cameras answer each time.
        let (ask_every, wanted) = (Duration::from_millis(200), Duration::from_millis(900));
        search_on(reply_port, &[first, second], ask_every, |device| {
            found.extend(device);
            started.elapsed() < wanted
        })
        .unwrap();
        assert!(started.elapsed() < wanted + Duration::from_secs(1), "the search went on after it was told to end");
        found.sort_by(|a, b| a.uid.cmp(&b.uid));
        let uids: Vec<&str> = found.iter().map(|f| f.uid.as_str()).collect();
        assert_eq!(uids, ["9527000EXAMPLE01", "9527000EXAMPLE02"]);
        assert_eq!(found[1].model.as_deref(), Some("RLC-1224A"));
    }

    #[test]
    fn a_search_fails_when_the_answer_port_stays_taken() {
        let taken = UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let started = Instant::now();
        let result = search_on(port, &[], ASK_EVERY, |_| true);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
        assert!(started.elapsed() < PORT_WAIT + Duration::from_secs(1));
    }

    /// A search that was just told to end still holds the port for a moment; the next one waits for it.
    #[test]
    fn a_search_waits_a_moment_for_the_answer_port() {
        let taken = UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(taken);
        });
        assert!(search_on(port, &[], ASK_EVERY, |_| false).is_ok());
    }

    /// A host that answers with one made-up UID after another fills the list only so far.
    #[test]
    fn a_search_reports_no_more_than_its_limit_of_devices() {
        let reply_port = free_port();
        let liar = UdpSocket::bind("127.0.0.1:0").unwrap();
        let liar_addr = liar.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let Ok((_, from)) = liar.recv_from(&mut buf) else { return };
            for n in 0..MAX_FOUND * 3 {
                let _ = liar.send_to(&answer("RLC-520A", &format!("9527000EXAMPL{n:03}")), (from.ip(), reply_port));
            }
        });
        let (started, mut found) = (Instant::now(), 0);
        search_on(reply_port, &[liar_addr], ASK_EVERY, |device| {
            found += usize::from(device.is_some());
            started.elapsed() < Duration::from_millis(800)
        })
        .unwrap();
        assert_eq!(found, MAX_FOUND);
    }
}
