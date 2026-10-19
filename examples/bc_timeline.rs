//! Manual diagnostic tool, not part of the app and never run by it: turns a
//! capture of a client talking Baichuan (BC) to a device — over TCP
//! (port 9000 by default) and/or over BCUDP, the P2P transport (local,
//! NAT-mapped or relayed) — into a timeline with one JSON object per BC
//! message (JSONL), so the messages can be matched against GUI actions by
//! host time. It decodes with the exact same code Reoling uses to talk to
//! real devices (`read_bc`, `read_bcudp`, the BCUDP XML cipher and CRC,
//! `UdpXml`, `aes_key_from_password`).
//!
//! What it does:
//! - copies the capture into a private directory under /tmp (`tshark` is
//!   confined by an AppArmor profile that only lets it read files there) and
//!   deletes the copy afterwards;
//! - TCP: has `tshark` list every TCP segment to/from the port (no
//!   reassembly by `tshark`), then reassembles each connection, each
//!   direction on its own, by sequence number, dropping retransmitted bytes;
//!   every byte keeps the capture time of the earliest segment that carried
//!   it;
//! - UDP: has `tshark` list every UDP datagram that starts with a BCUDP
//!   magic (Data, Ack, or a P2P discovery/handshake packet). A *UDP session*
//!   is the Data/Ack traffic between one app endpoint and one peer endpoint
//!   (the device, its NAT mapping, or a relay) with one pair of connection
//!   ids. Which side is the app comes from the P2P packets between the two
//!   (`C2x_*` are sent by the app) whose `cid`/`did` are the session's own
//!   connection ids in those roles (a relay also forwards handshake packets
//!   of other connections to the same port), else from which host sent `C2x_*`
//!   anywhere in the capture, else from which side starts with a login
//!   request, else (a guess, said so in the summary) from which side sends
//!   less. Each direction's Data payloads are put in packet-id order; a
//!   retransmitted id keeps its first capture (time and bytes). A missing id
//!   can't be sized, so the stream gets a *break* there: a message across a
//!   break is reported truncated and framing restarts at the break with the
//!   strict header search;
//! - splits each direction into BC messages with Reoling's own header rules
//!   (`read_header`, `wire_len`): a header is 24 bytes for classes 0x6414
//!   and 0x0000 and for recorded-media data (playback, download), whose
//!   class and response code hold the playback's sequence number instead,
//!   20 bytes otherwise; and, per connection/session,
//!   takes the nonce from its own legacy-login reply to derive its AES key;
//!   a UDP session without one (a client may skip that exchange when the
//!   handshake delivered a nonce) uses the `nc` of the `D2C_CFM` whose
//!   `cid`/`did` match the session's connection ids, with AES assumed (as
//!   Reoling itself does) and checked against the login hash.
//!
//! Each line holds: `t` (pcap epoch time of the segment/datagram that
//! carried the message's first byte), `t_local` (the same instant as
//! `tshark` prints it in host local time), `frame`, `t_last`/`frame_last`
//! (the packet that carried the last byte), `tcp_stream`, `conn`, `dir`
//! (`app->dev` or `dev->app`), `off` (byte offset in that direction's
//! stream), the header fields as Reoling's `BcHeader` names them (`msg_id`,
//! `msg_name` when Reoling has a name for it, `channel_id`, `stream_type`,
//! `msg_num`, `response_code`, `class`, `body_len`, `ext_len` = payload
//! offset), `payload_len`, `seq` (only on recorded-media data: the
//! playback sequence number, `class << 16 | response_code`, which then are
//! not a class and a code), and `payload`: `xml` (then `xml`, `xml_ok`, `tags`
//! hold the decrypted text, whether it parses as well-formed XML, and the
//! top-level element names), `binary` (video/audio/image data: only its
//! length is given, never its content), `none`, `encrypted` (no key: header
//! only), `missing` (with `truncated`: bytes not in the capture) or `not-xml`
//! (decrypted, but not XML: a wrong password shows up as many of these).
//! `ext` holds the decrypted extension XML when there is one, and
//! `ext_channel_id` its `channelId` (the camera the message is about).
//! Values of secret-looking elements (`password`, `uid`, `token`, the login
//! hashes, ...) are replaced by `(redacted)`.
//!
//! Lines of a UDP session have, instead of `tcp_stream`: `transport`
//! (`"udp"`), `udp_session` (the session number of the summary and of the
//! flow file), then `conn` (`app->peer`), `peer` (the peer's address),
//! `path` (`local`, `map` or `relay` from the handshake, or null) and
//! `packet_id`/`packet_id_last` (the Data packets that carried the first and
//! the last byte). TCP lines are exactly as before this tool knew UDP: they
//! have no `transport` field (absent means TCP).
//!
//! Note: in the captured sessions the client leaves `msg_num` at 0 and
//! counts its requests in the two header bytes Reoling calls `channel_id`
//! (low) and `stream_type` (high): seen in captures counting up past 255 into
//! `stream_type`, with every reply echoing both. `req_id` is those two bytes
//! as one number, so `(tcp_stream, msg_id, req_id)` pairs a request with
//! its reply (what happens after 65535 hasn't been seen). Pushed messages
//! the app never asked for come with `req_id` 0 (channel list, ...) or with
//! the `req_id` of the subscribe request that started them (alarm events).
//!
//! Lines are sorted by `t`. A summary per connection and per UDP session
//! goes to stderr.
//!
//! UDP transport records (the "flow" file, JSONL, sorted by `t`), written to
//! `--flow-out FILE` or, with `--out X.jsonl`, to `X.udpflow.jsonl`, each
//! with a `kind`:
//! - `session`: one per UDP session: endpoints, `path`, connection ids
//!   (`conn_id_app_to_dev` is the device's id, `did`; `conn_id_dev_to_app`
//!   the app's, `cid`), how the sides were decided, packet counts;
//! - `ack`: one per Ack packet: `dir` (the way the ack travels: `app->dev`
//!   acknowledges the device's Data), `connection_id`, `group_id`,
//!   `packet_id` (cumulative; 4294967295 = nothing acknowledged yet),
//!   `rate` (the fourth header word: the payload bytes per second the ack's
//!   sender says it received over the last whole second), the trailing
//!   bitmap's `bitmap_len`, `bitmap_ones` (and `bitmap` itself when short:
//!   one digit per packet id after `packet_id`, 1 = already received), plus
//!   what the capture shows: `data_sent_max` (the highest Data packet id the
//!   other side had sent by then), `unacked` (`data_sent_max - packet_id`)
//!   and `rx_bytes_1s` (Data payload bytes, retransmissions included,
//!   captured travelling towards the ack's sender in the second up to it —
//!   the number to compare `rate` with);
//! - `data_1s`: per session, direction and second since the session's
//!   first packet: Data `packets`, `new` (first copies), `dups`, `bytes`,
//!   `max_packet_id`, and `acks_back`/`ack_rate_last` (the acks the receiver
//!   sent that second, and the last `rate` among them);
//! - `p2p`: one per P2P discovery/handshake packet (`C2M_Q`, `R2C_C_R`,
//!   `C2D_T`, `D2C_CFM`, heartbeats, broadcasts, ...): decrypted with the
//!   fixed UDP XML key, `msg` (the element under `<P2P>`), `reoling_type`
//!   (Reoling's `UdpXml` variant, or null when Reoling doesn't model it),
//!   `crc_ok`, the ids it carries (`sid`, `cid`, `did`, `conn`, `rsp`) and
//!   the XML itself (`uid` redacted). `udp_session` links it to a session
//!   with the same two endpoints, when there is one.
//!
//! Passwords: the default is read from `--password-file FILE` (at most one
//! such option: a second is an error, not a replacement), else from the
//! file named by REOLING_CAPTURE_PASSWORD_FILE (only a trailing newline is
//! stripped), else from the REOLING_CAPTURE_PASSWORD environment variable.
//! `--password-file IP=FILE` (repeatable) gives the device at that IP its
//! own password, e.g. an NVR next to a Hub in the same capture; a UDP
//! session matches it by its peer's IP or by the device addresses the P2P
//! register server announced for it (`dev`/`dmap`), so a relayed session can
//! still be matched by the device's own IP. When the login hash in the app's
//! `LoginUser` request can be checked and the device's own password doesn't
//! match it, the other configured passwords are tried and the summary says
//! which file matched. A device's file that doesn't exist (yet) is only a
//! warning. Passwords are never printed: only file names are. With no
//! password (or with `--header-only`), the timeline still lists every
//! message with its header; the login exchange itself is BCEncrypt and
//! decodes without a password. This tool makes no network connection.
//!
//! ```bash
//! REOLING_CAPTURE_PASSWORD_FILE=~/.config/reoling/capture-password \
//!   cargo run --release --example bc_timeline -- capture.pcapng --out /tmp/timeline.jsonl
//! cargo run --release --example bc_timeline -- capture.pcapng \
//!   --password-file ~/.config/reoling/capture-password \
//!   --password-file 172.17.3.6=$HOME/.config/reoling/capture-password-nvr --out /tmp/timeline.jsonl
//! cargo run --release --example bc_timeline -- capture.pcapng --header-only > /tmp/headers.jsonl
//! ```
//!
//! Options: `--port N` (TCP only; default 9000), `--device IP` (only that
//! device: TCP packets to/from it, UDP sessions whose peer or announced
//! device address it is; default: everything), `--transport tcp|udp|both`
//! (default both; `tcp` gives exactly what this tool gave before it knew
//! UDP), `--out FILE` (default stdout), `--flow-out FILE`,
//! `--password-file [IP=]FILE`, `--header-only`.

use md5::{Digest, Md5};
use reoling::protocol::bc::codec::read_bc;
use reoling::protocol::bc::header_codec::{read_header, wire_len};
use reoling::protocol::bc::model::*;
use reoling::protocol::bc::xml::{BcXml, Extension};
use reoling::protocol::bcudp::codec::read_bcudp;
use reoling::protocol::bcudp::crc::calc_crc;
use reoling::protocol::bcudp::model::{BcUdp, MAGIC_HEADER_UDP_ACK, MAGIC_HEADER_UDP_DATA, MAGIC_HEADER_UDP_NEGO};
use reoling::protocol::bcudp::xml::{IpPort, UdpXml};
use reoling::protocol::bcudp::xml_crypto;
use reoling::protocol::crypto::{aes_key_from_password, EncryptionProtocol};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BC_MAGIC: [u8; 4] = MAGIC_HEADER.to_le_bytes();
/// No real BC message comes anywhere near this; a larger `body_len` means
/// the bytes at hand aren't a header.
const MAX_BODY_LEN: u32 = 64 << 20;

const APP_TO_DEV: usize = 0;
const DEV_TO_APP: usize = 1;
const DIR_NAMES: [&str; 2] = ["app->dev", "dev->app"];

/// An ack bitmap up to this long is written out in the flow file.
const BITMAP_SHOWN_MAX: usize = 64;

/// Names for the message ids Reoling itself knows (its own constants).
const MSG_NAMES: &[(u32, &str)] = &[
    (MSG_ID_LOGIN, "login"),
    (MSG_ID_LOGOUT, "logout"),
    (MSG_ID_VIDEO, "video"),
    (MSG_ID_VIDEO_STOP, "video_stop"),
    (MSG_ID_TALK_ABILITY, "talk_ability"),
    (MSG_ID_TALK_STOP, "talk_stop"),
    (MSG_ID_PTZ, "ptz"),
    (MSG_ID_PTZ_PRESET, "ptz_preset"),
    (MSG_ID_OSD, "osd"),
    (MSG_ID_GET_COMPRESSION, "get_compression"),
    (MSG_ID_SET_COMPRESSION, "set_compression"),
    (MSG_ID_ABILITY_SUPPORT, "ability_support"),
    (MSG_ID_VERSION, "version"),
    (MSG_ID_SNAP, "snap"),
    (MSG_ID_CHANNEL_INFO, "channel_info"),
    (MSG_ID_STREAM_INFO, "stream_info"),
    (MSG_ID_GET_PTZ_PRESET, "get_ptz_preset"),
    (MSG_ID_SUBSCRIBE, "subscribe"),
    (MSG_ID_SUPPORT, "support"),
    (MSG_ID_TALK_CONFIG, "talk_config"),
    (MSG_ID_TALK_DATA, "talk_data"),
    (MSG_ID_PLAY_SIREN, "play_siren"),
    (MSG_ID_SPOTLIGHT, "spotlight"),
    (MSG_ID_GET_ZOOM_FOCUS, "get_zoom_focus"),
    (MSG_ID_SET_ZOOM_FOCUS, "set_zoom_focus"),
    (MSG_ID_CHANNEL_VERSION, "channel_version"),
    (MSG_ID_PTZ_GUARD, "ptz_guard"),
    (MSG_ID_GET_PTZ_GUARD, "get_ptz_guard"),
    (MSG_ID_PTZ_CALIBRATE, "ptz_calibrate"),
    (MSG_ID_IMAGE_UPLOAD, "image_upload"),
    (MSG_ID_IMAGE_FILE, "image_file"),
    (MSG_ID_CHANNEL_ENCODE_TABLE, "channel_encode_table"),
];

fn main() {
    if let Err(e) = run() {
        eprintln!("bc_timeline: {e}");
        std::process::exit(1);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TransportSel {
    Tcp,
    Udp,
    Both,
}

struct Options {
    pcap: PathBuf,
    port: u16,
    device: Option<String>,
    out: Option<PathBuf>,
    flow_out: Option<PathBuf>,
    header_only: bool,
    transport: TransportSel,
    /// `--password-file FILE`: the default password, instead of the
    /// environment.
    default_password_file: Option<PathBuf>,
    /// `--password-file IP=FILE`, in the order given.
    device_password_files: Vec<(IpAddr, PathBuf)>,
}

const USAGE: &str = "usage: bc_timeline <capture.pcap|.pcapng> [--port 9000] [--device IP] [--transport tcp|udp|both]\n\
     \x20      [--out FILE] [--flow-out FILE] [--password-file [IP=]FILE]... [--header-only]\n\
     (default password from --password-file FILE, else from the file named by REOLING_CAPTURE_PASSWORD_FILE,\n\
     else from REOLING_CAPTURE_PASSWORD; FILE alone may be given once; --password-file IP=FILE, repeatable,\n\
     gives the device at IP its own;\n\
     with no password, only headers and the login exchange are decoded)";

fn parse_args() -> Result<Options, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut pcap = None;
    let mut opts = Options {
        pcap: PathBuf::new(),
        port: 9000,
        device: None,
        out: None,
        flow_out: None,
        header_only: false,
        transport: TransportSel::Both,
        default_password_file: None,
        device_password_files: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                opts.port = args.next().and_then(|p| p.parse().ok()).ok_or("--port needs a number")?;
            }
            "--device" => {
                let ip = args.next().ok_or("--device needs an IP address")?;
                if ip.is_empty() || !ip.chars().all(|c| c.is_ascii_hexdigit() || c == '.' || c == ':') {
                    return Err(format!("--device: {ip:?} is not an IP address"));
                }
                opts.device = Some(ip);
            }
            "--transport" => {
                opts.transport = match args.next().as_deref() {
                    Some("tcp") => TransportSel::Tcp,
                    Some("udp") => TransportSel::Udp,
                    Some("both") => TransportSel::Both,
                    _ => return Err("--transport needs tcp, udp or both".to_string()),
                };
            }
            "--out" => opts.out = Some(args.next().ok_or("--out needs a file name")?.into()),
            "--flow-out" => opts.flow_out = Some(args.next().ok_or("--flow-out needs a file name")?.into()),
            "--password-file" => {
                let spec = args.next().ok_or("--password-file needs [IP=]FILE")?;
                match spec.split_once('=').and_then(|(ip, path)| Some((ip.parse::<IpAddr>().ok()?, path))) {
                    Some((_, "")) => return Err(format!("--password-file {spec}: no file name after '='")),
                    Some((ip, path)) => opts.device_password_files.push((ip, PathBuf::from(path))),
                    None if opts.default_password_file.is_some() => {
                        return Err(format!(
                            "--password-file {spec}: a default password file was already given; \
                             give each further one as --password-file IP=FILE"
                        ))
                    }
                    None => opts.default_password_file = Some(PathBuf::from(spec)),
                }
            }
            "--header-only" => opts.header_only = true,
            "-h" | "--help" => return Err(USAGE.to_string()),
            other if other.starts_with("--") => return Err(format!("unknown option {other}\n{USAGE}")),
            other if pcap.is_none() => pcap = Some(PathBuf::from(other)),
            _ => return Err(USAGE.to_string()),
        }
    }
    opts.pcap = pcap.ok_or(USAGE)?;
    Ok(opts)
}

/// A password and where it came from. Only `label` is ever printed.
struct Secret {
    label: String,
    password: String,
}

#[derive(Default)]
struct Passwords {
    default: Option<Secret>,
    per_device: Vec<(IpAddr, Secret)>,
}

impl Passwords {
    fn count(&self) -> usize {
        self.default.iter().count() + self.per_device.len()
    }

    /// For a device known by `ips` (best first): its own password (the first
    /// `--password-file IP=` whose IP is among them, else the default), and
    /// every other distinct configured password.
    fn pick(&self, ips: &[IpAddr]) -> (Option<&Secret>, Vec<&Secret>) {
        let primary = ips
            .iter()
            .find_map(|ip| self.per_device.iter().find(|(dev, _)| dev == ip).map(|(_, s)| s))
            .or(self.default.as_ref());
        let mut others: Vec<&Secret> = Vec::new();
        for s in self.default.iter().chain(self.per_device.iter().map(|(_, s)| s)) {
            let seen = primary.is_some_and(|p| p.password == s.password) || others.iter().any(|o| o.password == s.password);
            if !seen {
                others.push(s);
            }
        }
        (primary, others)
    }
}

/// The password in `path` (only a trailing newline is stripped). Never
/// printed, logged or written anywhere by this tool.
fn read_password_file(path: &Path) -> Result<Secret, std::io::Error> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            eprintln!("warning: {} is readable by other users (chmod 600 it)", path.display());
        }
    }
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not UTF-8 text"))?;
    let password = text.strip_suffix('\n').map(|t| t.strip_suffix('\r').unwrap_or(t)).unwrap_or(&text);
    if password.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty"));
    }
    Ok(Secret { label: path.display().to_string(), password: password.to_string() })
}

fn password_file_error(path: &Path, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::InvalidData if e.to_string() == "empty" => format!("the password file {} is empty", path.display()),
        std::io::ErrorKind::InvalidData => format!("the password file {} is not UTF-8 text", path.display()),
        kind => format!("could not read the password file {}: {}", path.display(), kind),
    }
}

/// The passwords, if any are available.
fn load_passwords(opts: &Options) -> Result<Passwords, String> {
    if opts.header_only {
        return Ok(Passwords::default());
    }
    let default_file = opts
        .default_password_file
        .clone()
        .or_else(|| std::env::var_os("REOLING_CAPTURE_PASSWORD_FILE").map(PathBuf::from));
    let default = if let Some(path) = default_file {
        Some(read_password_file(&path).map_err(|e| password_file_error(&path, &e))?)
    } else if let Ok(password) = std::env::var("REOLING_CAPTURE_PASSWORD") {
        // Same trimming as `decrypt_bc_stream` always did for this variable.
        Some(Secret { label: "REOLING_CAPTURE_PASSWORD".to_string(), password: password.trim_end().to_string() })
    } else {
        None
    };
    let mut per_device = Vec::new();
    for (ip, path) in &opts.device_password_files {
        match read_password_file(path) {
            Ok(secret) => per_device.push((*ip, secret)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("warning: the password file {} for {ip} does not exist: ignored", path.display());
            }
            Err(e) => return Err(password_file_error(path, &e)),
        }
    }
    Ok(Passwords { default, per_device })
}

fn run() -> Result<(), String> {
    let opts = parse_args()?;
    let passwords = load_passwords(&opts)?;
    if passwords.default.is_none() {
        if passwords.per_device.is_empty() {
            eprintln!("no password given: header-only mode (plus the BCEncrypt login exchange)");
        } else {
            eprintln!("no default password: devices without their own --password-file are decoded header-only (plus the BCEncrypt login exchange)");
        }
    }
    let want_tcp = opts.transport != TransportSel::Udp;
    let want_udp = opts.transport != TransportSel::Tcp;

    let (segments, datagrams) = {
        let tmp = PrivateTmpDir::create()?;
        let copy = tmp.0.join("capture.pcapng");
        std::fs::copy(&opts.pcap, &copy).map_err(|e| format!("could not copy {}: {e}", opts.pcap.display()))?;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        let segments = if want_tcp { read_segments(&copy, opts.port, opts.device.as_deref())? } else { Vec::new() };
        let datagrams = if want_udp { read_datagrams(&copy, opts.transport == TransportSel::Udp)? } else { Vec::new() };
        (segments, datagrams)
        // `tmp` (and the copy in it) is removed here.
    };

    let connections = group_connections(segments, opts.port);
    let udp = build_udp(datagrams, opts.device.as_deref());
    if connections.is_empty() && udp.packets.is_empty() {
        return Err(match opts.transport {
            TransportSel::Tcp => format!("no TCP segments to or from port {} in {}", opts.port, opts.pcap.display()),
            TransportSel::Udp => format!("no BCUDP packets in {}", opts.pcap.display()),
            TransportSel::Both => {
                format!("no TCP segments to or from port {} and no BCUDP packets in {}", opts.port, opts.pcap.display())
            }
        });
    }

    let mut records: Vec<Record> = Vec::new();
    for (tcp_stream, conn) in &connections {
        records.extend(decode_connection(*tcp_stream, conn, &passwords));
    }
    let tcp_messages = records.len();
    if !udp.packets.is_empty() {
        print_udp_overview(&udp);
        for session in &udp.sessions {
            records.extend(decode_udp_session(session, &udp, &passwords));
        }
    }
    let udp_messages = records.len() - tcp_messages;
    records.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.frame.cmp(&b.frame)).then(a.order.cmp(&b.order)));

    let mut out: Box<dyn std::io::Write> = match &opts.out {
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(path).map_err(|e| format!("could not create {}: {e}", path.display()))?,
        )),
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    };
    for record in &records {
        writeln!(out, "{}", record.json).map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;
    if let Some(path) = &opts.out {
        if udp_messages > 0 {
            eprintln!(
                "wrote {} messages ({tcp_messages} over TCP, {udp_messages} over UDP) to {}",
                records.len(),
                path.display()
            );
        } else {
            eprintln!("wrote {} messages to {}", records.len(), path.display());
        }
    }

    if !udp.packets.is_empty() {
        let flow_path = opts.flow_out.clone().or_else(|| opts.out.as_deref().map(flow_path_for));
        match flow_path {
            Some(path) => {
                let flow = flow_records(&udp);
                let mut file = std::io::BufWriter::new(
                    std::fs::File::create(&path).map_err(|e| format!("could not create {}: {e}", path.display()))?,
                );
                for record in &flow {
                    writeln!(file, "{}", record.json).map_err(|e| e.to_string())?;
                }
                file.flush().map_err(|e| e.to_string())?;
                eprintln!(
                    "wrote {} UDP transport records ({} sessions) to {}",
                    flow.len(),
                    udp.sessions.len(),
                    path.display()
                );
            }
            None => eprintln!("UDP ack/flow records not written: give --flow-out FILE (or --out FILE)"),
        }
    }
    Ok(())
}

/// `X.jsonl` -> `X.udpflow.jsonl`, anything else -> `<it>.udpflow.jsonl`.
fn flow_path_for(out: &Path) -> PathBuf {
    let text = out.as_os_str().to_string_lossy();
    match text.strip_suffix(".jsonl") {
        Some(stem) => PathBuf::from(format!("{stem}.udpflow.jsonl")),
        None => PathBuf::from(format!("{text}.udpflow.jsonl")),
    }
}

/// A directory only this user can read, under /tmp (where `tshark` may
/// read), removed with everything in it when dropped.
struct PrivateTmpDir(PathBuf);

impl PrivateTmpDir {
    fn create() -> Result<Self, String> {
        let base = match std::env::var("TMPDIR") {
            Ok(dir) if dir == "/tmp" || dir.starts_with("/tmp/") => PathBuf::from(dir),
            _ => PathBuf::from("/tmp"),
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("bc_timeline.{}.{nanos}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for PrivateTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One TCP segment as `tshark` lists it — or, for UDP, one Data packet
/// (`seq` is then its packet id, `tcp_stream` and `syn` unused).
struct Segment {
    frame: u64,
    t: f64,
    t_text: String,
    t_local: String,
    tcp_stream: u64,
    src: String,
    sport: u16,
    dst: String,
    dport: u16,
    seq: u32,
    syn: bool,
    payload: Vec<u8>,
}

/// How a `tshark` run ended.
struct TsharkRun {
    parsed: usize,
    bad_lines: usize,
    /// Exit status and stderr, when it failed.
    failure: Option<String>,
}

/// Runs `tshark -T fields` over `pcap`, handing each output line to
/// `on_line` (which says whether the line parsed).
fn run_tshark(
    pcap: &Path,
    options: &[&str],
    filter: &str,
    fields: &[&str],
    mut on_line: impl FnMut(&str) -> bool,
) -> Result<TsharkRun, String> {
    let mut cmd = Command::new("tshark");
    cmd.arg("-n").arg("-r").arg(pcap);
    cmd.args(options);
    cmd.args(["-Y", filter, "-T", "fields"]);
    cmd.args(["-E", "separator=/t", "-E", "occurrence=f", "-E", "header=n"]);
    for field in fields {
        cmd.args(["-e", field]);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run tshark: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });

    let mut run = TsharkRun { parsed: 0, bad_lines: 0, failure: None };
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("reading tshark output: {e}"))?;
        if on_line(&line) {
            run.parsed += 1;
        } else {
            run.bad_lines += 1;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr_text = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        run.failure = Some(format!("{status}: {}", stderr_text.trim()));
    }
    Ok(run)
}

fn read_segments(pcap: &Path, port: u16, device: Option<&str>) -> Result<Vec<Segment>, String> {
    // Data segments and SYNs only (the SYN gives each direction's initial
    // sequence number); ICMP errors quoting a TCP header are left out.
    let mut filter = format!("tcp.port == {port} && (tcp.len > 0 || tcp.flags.syn == 1) && !icmp && !icmpv6");
    if let Some(ip) = device {
        let field = if ip.contains(':') { "ipv6.addr" } else { "ip.addr" };
        let _ = write!(filter, " && {field} == {ip}");
    }
    let fields = [
        "frame.number",
        "frame.time_epoch",
        "frame.time",
        "tcp.stream",
        "ip.src",
        "ipv6.src",
        "tcp.srcport",
        "ip.dst",
        "ipv6.dst",
        "tcp.dstport",
        "tcp.seq_raw",
        "tcp.flags.syn",
        "tcp.payload",
    ];
    let mut segments = Vec::new();
    let run = run_tshark(pcap, &["-o", "tcp.desegment_tcp_streams:FALSE"], &filter, &fields, |line| {
        match parse_segment(line) {
            Some(segment) => {
                segments.push(segment);
                true
            }
            None => false,
        }
    })?;
    if let Some(failure) = run.failure {
        if segments.is_empty() {
            return Err(format!("tshark failed ({failure})"));
        }
        // E.g. a ring-buffer file still being written ends mid-packet.
        eprintln!("warning: tshark exited with {failure}");
    }
    if run.bad_lines > 0 {
        eprintln!("warning: skipped {} tshark lines that did not parse", run.bad_lines);
    }
    Ok(segments)
}

fn parse_segment(line: &str) -> Option<Segment> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 13 {
        return None;
    }
    let pick = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { a.to_string() };
    let payload_hex: String = f[12].chars().filter(|c| *c != ':').collect();
    Some(Segment {
        frame: f[0].parse().ok()?,
        t: f[1].parse().ok()?,
        t_text: f[1].to_string(),
        t_local: f[2].to_string(),
        tcp_stream: f[3].parse().ok()?,
        src: pick(f[4], f[5]),
        sport: f[6].parse().ok()?,
        dst: pick(f[7], f[8]),
        dport: f[9].parse().ok()?,
        seq: f[10].parse().ok()?,
        syn: matches!(f[11], "1" | "True" | "true"),
        payload: hex_decode(&payload_hex)?,
    })
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// One TCP connection (one `tcp.stream`): its two directions.
struct Connection {
    app: String,
    dev: String,
    dev_ip: String,
    dirs: [Direction; 2],
}

#[derive(Default)]
struct Direction {
    syn_seq: Option<u32>,
    segments: Vec<Segment>,
}

fn endpoint(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

fn group_connections(segments: Vec<Segment>, port: u16) -> BTreeMap<u64, Connection> {
    let mut connections: BTreeMap<u64, Connection> = BTreeMap::new();
    for segment in segments {
        let dir = if segment.dport == port { APP_TO_DEV } else { DEV_TO_APP };
        let conn = connections.entry(segment.tcp_stream).or_insert_with(|| {
            let (app, dev, dev_ip) = if dir == APP_TO_DEV {
                (endpoint(&segment.src, segment.sport), endpoint(&segment.dst, segment.dport), segment.dst.clone())
            } else {
                (endpoint(&segment.dst, segment.dport), endpoint(&segment.src, segment.sport), segment.src.clone())
            };
            Connection { app, dev, dev_ip, dirs: Default::default() }
        });
        let direction = &mut conn.dirs[dir];
        if segment.syn && direction.syn_seq.is_none() {
            direction.syn_seq = Some(segment.seq);
        }
        if !segment.payload.is_empty() {
            direction.segments.push(segment);
        }
    }
    connections
}

/// Where a segment's bytes landed in the reassembled stream.
struct Placed {
    off: usize,
    len: usize,
    seg: usize,
}

/// Packet-level facts about one direction of a UDP session.
#[derive(Default, Debug, PartialEq)]
struct UdpStreamStats {
    /// Data packets captured, retransmissions included.
    packets: usize,
    /// Distinct packet ids.
    unique: usize,
    /// Copies of an id already captured (dropped).
    duplicates: usize,
    /// Of those, copies whose bytes differ from the first one.
    conflicting: usize,
    /// Ids between the first and the last one never captured.
    missing_ids: u64,
    first_id: u32,
    last_id: u32,
    /// First copies captured after a higher id had been.
    out_of_order: usize,
}

/// One direction of a connection, reassembled.
struct Reassembled<'a> {
    data: Vec<u8>,
    /// Byte ranges no captured segment covered (filled with zeros in `data`).
    holes: Vec<(usize, usize)>,
    /// UDP only: offsets where packet ids are missing, i.e. where an
    /// unknown number of bytes was lost (sorted). Offset 0 is one when the
    /// first captured id isn't 0.
    breaks: Vec<usize>,
    /// Every segment (retransmissions too), sorted by `off`. For UDP only
    /// the copy kept of each packet id.
    placed: Vec<Placed>,
    max_len: usize,
    segments: &'a [Segment],
    retransmitted_bytes: usize,
    udp: Option<UdpStreamStats>,
}

impl<'a> Reassembled<'a> {
    fn new(direction: &'a Direction) -> Self {
        let segments = &direction.segments[..];
        let base = match (direction.syn_seq, segments.first()) {
            (Some(syn), _) => syn.wrapping_add(1),
            (None, Some(first)) => first.seq,
            (None, None) => 0,
        };
        // Relative to the first data byte, allowing for wrap-around and for
        // a segment captured before the "first" one (no SYN, reordering).
        let rel: Vec<i64> = segments.iter().map(|s| s.seq.wrapping_sub(base) as i32 as i64).collect();
        let start = rel.iter().copied().min().unwrap_or(0).min(0);
        let mut placed: Vec<Placed> = segments
            .iter()
            .zip(&rel)
            .enumerate()
            .map(|(seg, (s, r))| Placed { off: (r - start) as usize, len: s.payload.len(), seg })
            .collect();
        placed.sort_by_key(|p| (p.off, segments[p.seg].frame));

        let mut data = Vec::new();
        let mut holes = Vec::new();
        let mut retransmitted_bytes = 0;
        for p in &placed {
            let end = p.off + p.len;
            if end <= data.len() {
                retransmitted_bytes += p.len;
                continue;
            }
            if p.off > data.len() {
                holes.push((data.len(), p.off));
                data.resize(p.off, 0);
            }
            let skip = data.len() - p.off;
            retransmitted_bytes += skip;
            data.extend_from_slice(&segments[p.seg].payload[skip..]);
        }
        let max_len = placed.iter().map(|p| p.len).max().unwrap_or(0);
        Self { data, holes, breaks: Vec::new(), placed, max_len, segments, retransmitted_bytes, udp: None }
    }

    /// One direction of a UDP session from its Data packets (`seq` = packet
    /// id, in capture order): payloads in packet-id order, each id once (its
    /// first capture), a break wherever ids are missing.
    fn from_packets(segments: &'a [Segment]) -> Self {
        let mut order: Vec<usize> = (0..segments.len()).collect();
        order.sort_by_key(|&i| (segments[i].seq, segments[i].frame));
        let mut stats = UdpStreamStats { packets: segments.len(), ..Default::default() };
        let mut data = Vec::new();
        let mut breaks = Vec::new();
        let mut placed = Vec::new();
        let mut retransmitted_bytes = 0;
        let mut kept: Option<usize> = None;
        for i in order {
            let s = &segments[i];
            if let Some(k) = kept {
                let prev = segments[k].seq;
                if s.seq == prev {
                    stats.duplicates += 1;
                    retransmitted_bytes += s.payload.len();
                    if s.payload != segments[k].payload {
                        stats.conflicting += 1;
                    }
                    continue;
                }
                if s.seq != prev.wrapping_add(1) {
                    breaks.push(data.len());
                    stats.missing_ids += u64::from(s.seq.wrapping_sub(prev).wrapping_sub(1));
                }
            } else {
                stats.first_id = s.seq;
                if s.seq != 0 {
                    breaks.push(0);
                }
            }
            placed.push(Placed { off: data.len(), len: s.payload.len(), seg: i });
            data.extend_from_slice(&s.payload);
            stats.unique += 1;
            stats.last_id = s.seq;
            kept = Some(i);
        }
        let mut seen = HashSet::new();
        let mut highest: Option<u32> = None;
        for s in segments {
            if seen.insert(s.seq) && highest.is_some_and(|h| s.seq < h) {
                stats.out_of_order += 1;
            }
            highest = highest.max(Some(s.seq));
        }
        let max_len = placed.iter().map(|p| p.len).max().unwrap_or(0);
        Self { data, holes: Vec::new(), breaks, placed, max_len, segments, retransmitted_bytes, udp: Some(stats) }
    }

    /// The earliest captured segment that carried byte `off`.
    fn first_seen(&self, off: usize) -> Option<&'a Segment> {
        let end = self.placed.partition_point(|p| p.off <= off);
        self.placed[..end]
            .iter()
            .rev()
            .take_while(|p| p.off + self.max_len > off)
            .filter(|p| p.off + p.len > off)
            .map(|p| &self.segments[p.seg])
            .min_by(|a, b| a.t.total_cmp(&b.t).then(a.frame.cmp(&b.frame)))
    }

    /// Whether `start..end` misses bytes: overlaps a hole, or has a break
    /// strictly inside.
    fn overlaps_hole(&self, start: usize, end: usize) -> bool {
        self.holes.iter().any(|&(a, b)| a < end && start < b) || self.break_inside(start, end).is_some()
    }

    fn hole_at(&self, pos: usize) -> Option<usize> {
        self.holes.iter().find(|&&(a, b)| a <= pos && pos < b).map(|&(_, b)| b)
    }

    /// The first break strictly inside `start..end`.
    fn break_inside(&self, start: usize, end: usize) -> Option<usize> {
        let i = self.breaks.partition_point(|&b| b <= start);
        self.breaks.get(i).copied().filter(|&b| b < end)
    }

    fn is_break(&self, pos: usize) -> bool {
        self.breaks.binary_search(&pos).is_ok()
    }
}

/// A header found at `pos`, with the message's full length on the wire —
/// both from Reoling's own `read_header` and `wire_len` (the 24-byte
/// recorded-media header and the 4 stray bytes after a class 0xffff reply
/// included), and cross-checked against `read_bc` whenever a message is
/// decoded.
struct Framed {
    header: BcHeader,
    total: usize,
}

/// Whether a header is of a kind real devices send: a class with a fixed
/// meaning, or recorded-media data whose class and code hold a playback
/// sequence number. Anything else is counted as an unknown class.
fn known_layout(header: &BcHeader) -> bool {
    is_fixed_class(header.class) || header.replay_seq().is_some()
}

fn frame_at(data: &[u8], strict: bool) -> Option<Framed> {
    if data.len() < 4 || data[..4] != BC_MAGIC {
        return None;
    }
    let (header, header_len) = read_header(data).ok()??;
    if header.body_len > MAX_BODY_LEN {
        return None;
    }
    if header.payload_offset.is_some_and(|o| o > header.body_len) {
        return None;
    }
    // While searching for the next message after lost bytes, only accept
    // headers of a known kind, so a stray magic inside video data isn't
    // taken for a header.
    if strict && !known_layout(&header) {
        return None;
    }
    Some(Framed { total: wire_len(&header, header_len), header })
}

/// The next offset from `from` on where a plausible header starts (strict
/// framing, not inside a hole), or the end of the stream.
fn next_header(stream: &Reassembled, from: usize) -> usize {
    (from..stream.data.len().saturating_sub(3))
        .find(|&p| {
            stream.data[p..p + 4] == BC_MAGIC && stream.hole_at(p).is_none() && frame_at(&stream.data[p..], true).is_some()
        })
        .unwrap_or(stream.data.len())
}

/// Calls `f(pos, framed, end)` for every complete message of `stream`,
/// framed exactly as `decode_session` frames them, until `f` returns true.
fn walk_messages(stream: &Reassembled, mut f: impl FnMut(usize, &Framed, usize) -> bool) {
    let mut pos = 0usize;
    let mut resync = false;
    while pos < stream.data.len() {
        if let Some(hole_end) = stream.hole_at(pos) {
            pos = hole_end;
            resync = true;
            continue;
        }
        if stream.is_break(pos) {
            resync = true;
        }
        let Some(framed) = frame_at(&stream.data[pos..], resync) else {
            pos = next_header(stream, pos + 1);
            resync = true;
            continue;
        };
        let end = pos + framed.total;
        if let Some(lost_at) = stream.break_inside(pos, end) {
            pos = lost_at;
            resync = true;
            continue;
        }
        if end > stream.data.len() || stream.overlaps_hole(pos, end) {
            pos = end;
            resync = true;
            continue;
        }
        resync = false;
        if f(pos, &framed, end) {
            return;
        }
        pos = end;
    }
}

/// Header bytes 12-13 (`channel_id`, `stream_type`) as one little-endian
/// number: the request counter of a client that leaves `msg_num` at 0,
/// echoed by each reply.
fn req_id(h: &BcHeader) -> u16 {
    u16::from_le_bytes([h.channel_id, h.stream_type])
}

/// How the payloads of one connection can be decoded.
enum Keying {
    /// The device chose an encryption we can apply (none, BCEncrypt, or
    /// AES with the password given).
    Known(EncryptionProtocol),
    /// AES, but no password: only the login exchange (BCEncrypt) decodes.
    NoKey,
    /// This connection's login isn't in the capture (it started earlier).
    NoNonce,
}

struct LoginFacts {
    nonce: String,
    /// Low byte of the device's `0xddXX` login reply: the encryption it
    /// picked (0 none, 1 BCEncrypt, anything else AES).
    enc_byte: u8,
}

/// The nonce from this connection's own legacy-login reply (`msg_id` 1,
/// `response_code` 0xddXX). `read_bc` decodes that reply from its header
/// alone, whatever `enc` is passed.
fn find_login(stream: &Reassembled) -> Option<LoginFacts> {
    let mut pos = 0;
    while let Some(framed) = frame_at(&stream.data[pos..], false) {
        let end = pos + framed.total;
        if end > stream.data.len() || stream.overlaps_hole(pos, end) {
            return None;
        }
        let h = &framed.header;
        if h.msg_id == MSG_ID_LOGIN && h.response_code >> 8 == 0xdd {
            let (bc, _) = read_bc(&stream.data[pos..end], &EncryptionProtocol::Unencrypted, &mut HashSet::new()).ok()??;
            let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = bc.body else { return None };
            let nonce = BcXml::from_bytes(&payload).ok()?.encryption?.nonce;
            return Some(LoginFacts { nonce, enc_byte: (h.response_code & 0xff) as u8 });
        }
        pos = end;
    }
    None
}

/// The hash in the app's first `LoginUser` request (the `<password>`
/// element), decoded with `enc` (the login exchange's encryption).
fn find_login_hash(stream: &Reassembled, enc: &EncryptionProtocol) -> Option<String> {
    let mut found = None;
    walk_messages(stream, |pos, framed, end| {
        if framed.header.msg_id != MSG_ID_LOGIN {
            return false;
        }
        let Ok(Some((bc, _))) = read_bc(&stream.data[pos..end], enc, &mut HashSet::new()) else { return false };
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = &bc.body else { return false };
        match as_xml_text(payload).and_then(|xml| element_text(xml, "password")) {
            Some(hash) => {
                found = Some(hash.to_string());
                true
            }
            None => false,
        }
    });
    found
}

/// Reoling's own modern-login hash (see `md5_hex_truncated` in
/// src/client.rs): uppercase hex MD5, first 31 characters.
fn login_hash(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02X}")).collect();
    hex[..31].to_string()
}

/// Which password to decode with: the device's own (`primary`) unless the
/// login hash can be checked and another configured one matches it instead.
/// The flag says the choice is one of `others`.
fn choose_password<'p>(
    primary: Option<&'p Secret>,
    others: &[&'p Secret],
    nonce: Option<&str>,
    hash: Option<&str>,
) -> (Option<&'p Secret>, bool) {
    if let (Some(nonce), Some(hash)) = (nonce, hash) {
        let matches = |s: &Secret| login_hash(&format!("{}{nonce}", s.password)) == hash;
        if primary.is_some_and(matches) {
            return (primary, false);
        }
        if let Some(other) = others.iter().find(|s| matches(s)) {
            return (Some(*other), true);
        }
    }
    (primary, false)
}

/// The text of the first `<tag>...</tag>` leaf element in `xml`.
fn element_text<'x>(xml: &'x str, tag: &str) -> Option<&'x str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(&xml[start..end])
}

struct Record {
    t: f64,
    frame: u64,
    /// Tie-break for messages that share a packet: connection, direction,
    /// offset.
    order: (u64, usize, usize),
    json: String,
}

#[derive(Default)]
struct DirStats {
    messages: usize,
    xml: usize,
    xml_bad: usize,
    binary: usize,
    not_xml: BTreeMap<u32, usize>,
    encrypted: usize,
    truncated: usize,
    resyncs: usize,
    skipped_bytes: usize,
    framing_mismatch: usize,
    unknown_class: usize,
}

/// What a decoded connection is, for its lines and its summary.
enum Ident {
    Tcp { tcp_stream: u64 },
    Udp { session: usize, peer: String, path: Option<String> },
}

/// Everything `decode_session` needs about one TCP connection or UDP session.
struct SessionInput<'a, 'p> {
    ident: Ident,
    conn_name: String,
    /// Printed first in the summary.
    header: Vec<String>,
    streams: [Reassembled<'a>; 2],
    /// UDP: the `nc` from the session's `D2C_CFM`.
    handshake_nonce: Option<String>,
    /// The device's addresses, for `--password-file IP=`.
    device_ips: Vec<IpAddr>,
    passwords: &'p Passwords,
}

fn decode_connection(tcp_stream: u64, conn: &Connection, passwords: &Passwords) -> Vec<Record> {
    let streams = [Reassembled::new(&conn.dirs[APP_TO_DEV]), Reassembled::new(&conn.dirs[DEV_TO_APP])];
    let conn_name = format!("{}->{}", conn.app, conn.dev);
    if streams.iter().all(|s| s.data.is_empty()) {
        eprintln!("connection tcp.stream={tcp_stream} {conn_name}: no payload (refused or never used)");
        return Vec::new();
    }
    let all = || conn.dirs.iter().flat_map(|d| d.segments.iter());
    let earliest = all().min_by(|a, b| a.t.total_cmp(&b.t)).map_or("", |s| s.t_local.as_str());
    let latest = all().max_by(|a, b| a.t.total_cmp(&b.t)).map_or("", |s| s.t_local.as_str());
    let header = vec![format!("connection tcp.stream={tcp_stream} {conn_name}  [{earliest} .. {latest}]")];
    decode_session(SessionInput {
        ident: Ident::Tcp { tcp_stream },
        conn_name,
        header,
        streams,
        handshake_nonce: None,
        device_ips: conn.dev_ip.parse().ok().into_iter().collect(),
        passwords,
    })
}

fn decode_session(input: SessionInput) -> Vec<Record> {
    let SessionInput { ident, conn_name, header, streams, handshake_nonce, device_ips, passwords } = input;
    let is_udp = matches!(ident, Ident::Udp { .. });
    let login = find_login(&streams[DEV_TO_APP]);
    // The nonce, and the encryption the device picked in its legacy-login
    // reply (None: no such reply, AES assumed).
    let nonce: Option<(String, Option<u8>)> = match (&login, handshake_nonce) {
        (Some(l), _) => Some((l.nonce.clone(), Some(l.enc_byte))),
        (None, Some(nc)) => Some((nc, None)),
        (None, None) => None,
    };
    // Login messages are BCEncrypt (or plain) whatever was negotiated.
    let login_enc = match &nonce {
        Some((_, Some(0))) => EncryptionProtocol::Unencrypted,
        _ => EncryptionProtocol::BcEncrypt,
    };
    let login_user_hash = find_login_hash(&streams[APP_TO_DEV], &login_enc);
    let (primary, others) = passwords.pick(&device_ips);
    let (chosen, chosen_from_others) =
        choose_password(primary, &others, nonce.as_ref().map(|n| n.0.as_str()), login_user_hash.as_deref());
    let keying = match (&nonce, chosen) {
        (None, _) => Keying::NoNonce,
        (Some((_, Some(0))), _) => Keying::Known(EncryptionProtocol::Unencrypted),
        (Some((_, Some(1))), _) => Keying::Known(EncryptionProtocol::BcEncrypt),
        (Some((n, _)), Some(s)) => Keying::Known(EncryptionProtocol::Aes { key: aes_key_from_password(&s.password, n) }),
        (Some(_), None) => Keying::NoKey,
    };
    let order_base = match &ident {
        Ident::Tcp { tcp_stream } => *tcp_stream,
        Ident::Udp { session, .. } => (1u64 << 40) + *session as u64,
    };
    let mut records = Vec::new();
    let mut stats: [DirStats; 2] = Default::default();
    let mut login_reply_code: Option<u16> = None;
    let mut firmware: Vec<String> = Vec::new();

    for dir in [APP_TO_DEV, DEV_TO_APP] {
        let stream = &streams[dir];
        let st = &mut stats[dir];
        // Per connection, per direction, per msg_id and per `req_id`:
        // `read_bc` keys its binary-mode set by msg_num only, and the
        // captured client leaves msg_num at 0 on every message, so without
        // this a video stream would put every later message in binary mode
        // (and the two directions would mix too). (Recorded media decide
        // binary mode per message inside `read_bc` and never touch it.)
        let mut bin_modes: HashMap<(u32, u16), HashSet<u16>> = HashMap::new();
        let mut pos = 0usize;
        let mut resync = false;
        while pos < stream.data.len() {
            if let Some(hole_end) = stream.hole_at(pos) {
                pos = hole_end;
                resync = true;
                continue;
            }
            if stream.is_break(pos) {
                // UDP: packets are missing right before this byte.
                resync = true;
            }
            let Some(framed) = frame_at(&stream.data[pos..], resync) else {
                // Not at a header: lost bytes, or a stream that starts
                // mid-message. Look for the next plausible header.
                let next = next_header(stream, pos + 1);
                st.skipped_bytes += next - pos;
                st.resyncs += 1;
                pos = next;
                resync = true;
                continue;
            };
            let h = &framed.header;
            if !known_layout(h) {
                st.unknown_class += 1;
            }
            let end = pos + framed.total;
            // UDP: a message across missing packets can't be complete, and
            // its length says nothing about where the next one starts (the
            // lost bytes can't be counted): framing restarts at the break.
            let lost_at = stream.break_inside(pos, end);
            let Some(first) = stream.first_seen(pos) else {
                // Can't happen: every byte outside a hole came from a segment.
                pos += 1;
                resync = true;
                continue;
            };
            let last_byte = lost_at.unwrap_or(end.min(stream.data.len())) - 1;
            let last = stream.first_seen(last_byte).unwrap_or(first);
            let ext_len = h.payload_offset.map(|o| o as usize);
            let payload_len = h.body_len as usize - ext_len.unwrap_or(0);

            let mut j = Json::new();
            j.raw("t", &first.t_text);
            j.str("t_local", &first.t_local);
            j.num("frame", first.frame);
            j.raw("t_last", &last.t_text);
            j.num("frame_last", last.frame);
            match &ident {
                Ident::Tcp { tcp_stream } => {
                    j.num("tcp_stream", tcp_stream);
                    j.str("conn", &conn_name);
                }
                Ident::Udp { session, peer, path } => {
                    j.str("transport", "udp");
                    j.num("udp_session", session);
                    j.str("conn", &conn_name);
                    j.str("peer", peer);
                    match path {
                        Some(p) => j.str("path", p),
                        None => j.raw("path", "null"),
                    }
                    j.num("packet_id", first.seq);
                    j.num("packet_id_last", last.seq);
                }
            }
            j.str("dir", DIR_NAMES[dir]);
            j.num("off", pos);
            j.num("msg_id", h.msg_id);
            match MSG_NAMES.iter().find(|(id, _)| *id == h.msg_id) {
                Some((_, name)) => j.str("msg_name", name),
                None => j.raw("msg_name", "null"),
            }
            j.num("msg_num", h.msg_num);
            j.num("channel_id", h.channel_id);
            j.num("stream_type", h.stream_type);
            j.num("req_id", req_id(h));
            j.num("response_code", h.response_code);
            j.str("class", &format!("{:#06x}", h.class));
            j.num("body_len", h.body_len);
            match ext_len {
                Some(n) => j.num("ext_len", n),
                None => j.raw("ext_len", "null"),
            }
            j.num("payload_len", payload_len);
            if let Some(seq) = h.replay_seq() {
                j.num("seq", seq);
            }
            if resync {
                j.raw("resync", "true");
                resync = false;
            }
            st.messages += 1;

            let truncated = if lost_at.is_some() {
                Some("packets missing from the capture")
            } else if end > stream.data.len() {
                Some("capture ends")
            } else if stream.overlaps_hole(pos, end) {
                Some("bytes missing from the capture")
            } else {
                None
            };
            if let Some(why) = truncated {
                st.truncated += 1;
                j.str("truncated", why);
                j.str("payload", "missing");
                records.push(Record { t: first.t, frame: first.frame, order: (order_base, dir, pos), json: j.finish() });
                pos = lost_at.unwrap_or(end);
                resync = true;
                continue;
            }

            if h.msg_id == MSG_ID_LOGIN && dir == DEV_TO_APP && h.response_code >> 8 != 0xdd && login_reply_code.is_none() {
                login_reply_code = Some(h.response_code);
            }

            // Which encryption to decode this message with, if any. Login
            // messages are BCEncrypt (or plain) whatever was negotiated;
            // `read_bc` picks the right one from their header.
            let enc = if h.msg_id == MSG_ID_LOGIN {
                Some(match &keying {
                    Keying::Known(EncryptionProtocol::Unencrypted) => EncryptionProtocol::Unencrypted,
                    _ => EncryptionProtocol::BcEncrypt,
                })
            } else {
                match &keying {
                    Keying::Known(enc) => Some(enc.clone()),
                    Keying::NoKey | Keying::NoNonce => None,
                }
            };
            let Some(enc) = enc else {
                st.encrypted += 1;
                j.str("payload", if payload_len == 0 { "none" } else { "encrypted" });
                records.push(Record { t: first.t, frame: first.frame, order: (order_base, dir, pos), json: j.finish() });
                pos = end;
                continue;
            };

            let bin_mode = bin_modes.entry((h.msg_id, req_id(h))).or_default();
            match read_bc(&stream.data[pos..end], &enc, bin_mode) {
                Ok(Some((bc, used))) => {
                    if used != framed.total {
                        st.framing_mismatch += 1;
                    }
                    let in_bin_mode = bin_mode.contains(&h.msg_num);
                    describe_body(&bc, h.msg_id, in_bin_mode, &mut j, st, |xml| {
                        if h.msg_id == MSG_ID_VERSION && dir == DEV_TO_APP {
                            if let Some(fw) = element_text(xml, "firmwareVersion") {
                                if !firmware.iter().any(|f| f == fw) {
                                    firmware.push(fw.to_string());
                                }
                            }
                        }
                    });
                }
                Ok(None) | Err(_) => {
                    // Malformed body (e.g. payload_offset beyond the body):
                    // keep the header, move on to the next message.
                    j.str("payload", "undecodable");
                    st.framing_mismatch += 1;
                }
            }
            records.push(Record { t: first.t, frame: first.frame, order: (order_base, dir, pos), json: j.finish() });
            pos = end;
        }
    }

    // Summary for this connection, on stderr.
    for line in &header {
        eprintln!("{line}");
    }
    let keying_text = match (&keying, &login) {
        (Keying::NoNonce, _) if is_udp => {
            "no nonce in the capture (no legacy login reply, no D2C_CFM nc for this session): header only".to_string()
        }
        (Keying::NoNonce, _) => "no login in the capture: header only".to_string(),
        (Keying::NoKey, _) => "AES, no password: header only after login".to_string(),
        (Keying::Known(EncryptionProtocol::Aes { .. }), Some(l)) => format!("AES (device chose 0xdd{:02x}), key from the password and this connection's nonce", l.enc_byte),
        (Keying::Known(EncryptionProtocol::Aes { .. }), None) => {
            "AES (assumed as Reoling does: no legacy login exchange), key from the password and the nc of this session's D2C_CFM".to_string()
        }
        (Keying::Known(EncryptionProtocol::BcEncrypt), _) => "BCEncrypt (no password needed)".to_string(),
        (Keying::Known(_), _) => "unencrypted".to_string(),
    };
    eprintln!("  encryption: {keying_text}");
    if let (Some(s), Some((n, _)), Some(captured)) = (chosen, &nonce, &login_user_hash) {
        if login_hash(&format!("{}{n}", s.password)) == *captured {
            eprintln!("  password matches the hash in the app's LoginUser request");
        } else {
            eprintln!("  password does NOT match the hash in the app's LoginUser request: AES payloads of this connection will not decode");
        }
    }
    if let Some(s) = chosen {
        if chosen_from_others {
            match primary {
                Some(p) => eprintln!("  password from {} (the one for this device, {}, does not match)", s.label, p.label),
                None => eprintln!("  password from {} (no password is configured for this device)", s.label),
            }
        } else if passwords.count() > 1 {
            eprintln!("  password from {}", s.label);
        }
    }
    if let Some(code) = login_reply_code {
        eprintln!("  login reply response_code {code}");
    }
    if !firmware.is_empty() {
        eprintln!("  device firmwareVersion (msg 80 reply): {}", firmware.join(", "));
    }
    for dir in [APP_TO_DEV, DEV_TO_APP] {
        let s = &stats[dir];
        let r = &streams[dir];
        let mut line = format!(
            "  {}: {} bytes, {} messages: {} xml ({} not well-formed), {} binary, {} header-only",
            DIR_NAMES[dir], r.data.len(), s.messages, s.xml, s.xml_bad, s.binary, s.encrypted
        );
        if !s.not_xml.is_empty() {
            let ids: Vec<String> = s.not_xml.iter().map(|(id, n)| format!("{id}x{n}")).collect();
            let _ = write!(line, ", NOT XML after decryption: {}", ids.join(" "));
        }
        match &r.udp {
            None => {
                let _ = write!(line, "; {} capture gaps, {} retransmitted bytes dropped", r.holes.len(), r.retransmitted_bytes);
            }
            Some(u) if u.unique == 0 => line.push_str("; no Data packets"),
            Some(u) => {
                let _ = write!(
                    line,
                    "; {} Data packets, ids {}..{}: {} missing ({} breaks), {} duplicates ({} bytes) dropped",
                    u.packets, u.first_id, u.last_id, u.missing_ids, r.breaks.len(), u.duplicates, r.retransmitted_bytes
                );
                if u.conflicting > 0 {
                    let _ = write!(line, " ({} with other bytes than the first copy)", u.conflicting);
                }
                if u.out_of_order > 0 {
                    let _ = write!(line, ", {} arrived out of order", u.out_of_order);
                }
            }
        }
        if s.resyncs > 0 {
            let _ = write!(line, ", {} resyncs ({} bytes skipped)", s.resyncs, s.skipped_bytes);
        }
        if s.truncated > 0 {
            let _ = write!(line, ", {} truncated", s.truncated);
        }
        if s.framing_mismatch > 0 {
            let _ = write!(line, ", {} framing mismatches", s.framing_mismatch);
        }
        if s.unknown_class > 0 {
            let _ = write!(line, ", {} unknown header classes", s.unknown_class);
        }
        eprintln!("{line}");
    }
    records
}

/// Adds the decoded extension and payload of `bc` to `j`. `on_xml` sees
/// each XML payload before it is redacted.
fn describe_body(
    bc: &Bc,
    msg_id: u32,
    in_bin_mode: bool,
    j: &mut Json,
    st: &mut DirStats,
    mut on_xml: impl FnMut(&str),
) {
    let BcBody::Modern(ModernMsg { extension_xml, payload }) = &bc.body else {
        // The legacy login upgrade: header only, no body at all.
        j.str("payload", "none");
        return;
    };
    let mut binary_ext = false;
    if let Some(ext) = extension_xml {
        if let Some(text) = as_xml_text(ext) {
            let parsed = Extension::from_bytes(ext).ok();
            binary_ext = parsed.as_ref().and_then(|e| e.binary_data) == Some(1);
            j.str("ext", &redact(text));
            if let Some(channel) = parsed.and_then(|e| e.channel_id) {
                j.num("ext_channel_id", channel);
            }
        }
    }
    let Some(payload) = payload else {
        j.str("payload", "none");
        return;
    };
    // Video/audio/image data never gets dumped, only its length.
    if binary_ext || in_bin_mode {
        st.binary += 1;
        j.str("payload", "binary");
        return;
    }
    match as_xml_text(payload) {
        Some(text) => {
            st.xml += 1;
            on_xml(text);
            let (ok, tags) = xml_summary(text);
            if !ok {
                st.xml_bad += 1;
            }
            j.str("payload", "xml");
            j.raw("xml_ok", if ok { "true" } else { "false" });
            let tags: Vec<String> = tags.iter().map(|t| format!("\"{}\"", json_escape(t))).collect();
            j.raw("tags", &format!("[{}]", tags.join(",")));
            j.str("xml", &redact(text));
        }
        None => {
            *st.not_xml.entry(msg_id).or_default() += 1;
            j.str("payload", "not-xml");
        }
    }
}

// ---------------------------------------------------------------------------
// UDP (BCUDP)
// ---------------------------------------------------------------------------

/// A UDP endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Ep {
    ip: String,
    port: u16,
}

impl Ep {
    fn name(&self) -> String {
        endpoint(&self.ip, self.port)
    }
}

/// One UDP datagram as `tshark` lists it.
struct Datagram {
    frame: u64,
    t: f64,
    t_text: String,
    t_local: String,
    src: Ep,
    dst: Ep,
    payload: Vec<u8>,
}

/// `tshark` display-filter bytes for a BCUDP magic (little-endian on the
/// wire, as Reoling writes it).
fn magic_filter(magic: u32) -> String {
    let bytes: Vec<String> = magic.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
    format!("udp.payload[0:4] == {}", bytes.join(":"))
}

fn read_datagrams(pcap: &Path, udp_only: bool) -> Result<Vec<Datagram>, String> {
    let filter = format!(
        "udp && !icmp && !icmpv6 && ({} || {} || {})",
        magic_filter(MAGIC_HEADER_UDP_DATA),
        magic_filter(MAGIC_HEADER_UDP_ACK),
        magic_filter(MAGIC_HEADER_UDP_NEGO)
    );
    let fields = [
        "frame.number",
        "frame.time_epoch",
        "frame.time",
        "ip.src",
        "ipv6.src",
        "udp.srcport",
        "ip.dst",
        "ipv6.dst",
        "udp.dstport",
        "udp.payload",
    ];
    let mut datagrams = Vec::new();
    let run = run_tshark(pcap, &[], &filter, &fields, |line| match parse_datagram(line) {
        Some(datagram) => {
            datagrams.push(datagram);
            true
        }
        None => false,
    })?;
    if let Some(failure) = run.failure {
        if datagrams.is_empty() && udp_only {
            return Err(format!("tshark failed ({failure})"));
        }
        eprintln!("warning: tshark (UDP pass) exited with {failure}");
    }
    if run.bad_lines > 0 {
        eprintln!("warning: skipped {} tshark UDP lines that did not parse", run.bad_lines);
    }
    Ok(datagrams)
}

fn parse_datagram(line: &str) -> Option<Datagram> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() < 10 {
        return None;
    }
    let pick = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { a.to_string() };
    let payload_hex: String = f[9].chars().filter(|c| *c != ':').collect();
    Some(Datagram {
        frame: f[0].parse().ok()?,
        t: f[1].parse().ok()?,
        t_text: f[1].to_string(),
        t_local: f[2].to_string(),
        src: Ep { ip: pick(f[3], f[4]), port: f[5].parse().ok()? },
        dst: Ep { ip: pick(f[6], f[7]), port: f[8].parse().ok()? },
        payload: hex_decode(&payload_hex)?,
    })
}

/// A P2P discovery/handshake packet, decrypted.
struct P2pMsg {
    tid: u32,
    crc_ok: bool,
    /// The element under `<P2P>` (e.g. `D2C_CFM`); empty if none.
    name: String,
    /// Reoling's own parse, when it models this message.
    typed: Option<UdpXml>,
    /// The decrypted XML, if it is text.
    text: Option<String>,
}

/// The ids a P2P message carries (whichever it has).
#[derive(Default)]
struct P2pIds {
    sid: Option<u32>,
    cid: Option<i32>,
    did: Option<i32>,
    conn: Option<String>,
    rsp: Option<i32>,
    nc: Option<String>,
    /// Device addresses the register server announced (`dev`, `dmap`).
    dev_addrs: Vec<IpPort>,
}

impl P2pMsg {
    /// From Reoling's own parse when it has one, else from the message's
    /// own elements (for messages Reoling doesn't model, or whose shape
    /// differs from its model).
    fn ids(&self) -> P2pIds {
        let mut ids = P2pIds::default();
        let Some(typed) = &self.typed else {
            let leaves = self.text.as_deref().map(p2p_leaves).unwrap_or_default();
            let get = |path: &str| leaves.iter().find(|(p, _)| p == path).map(|(_, v)| v.as_str());
            ids.sid = get("sid").and_then(|v| v.parse().ok());
            ids.cid = get("cid").and_then(|v| v.parse().ok());
            ids.did = get("did").and_then(|v| v.parse().ok());
            ids.conn = get("conn").map(str::to_string);
            ids.rsp = get("rsp").and_then(|v| v.parse().ok());
            ids.nc = get("nc").map(str::to_string);
            for addr in ["dev", "dmap"] {
                if let (Some(ip), Some(port)) = (get(&format!("{addr}/ip")), get(&format!("{addr}/port"))) {
                    if let Ok(port) = port.parse() {
                        ids.dev_addrs.push(IpPort { ip: ip.to_string(), port });
                    }
                }
            }
            return ids;
        };
        match typed {
            UdpXml::C2mQ(_) | UdpXml::M2cQr(_) => {}
            UdpXml::C2rC(x) => ids.cid = Some(x.cid),
            UdpXml::R2cCr(x) => {
                ids.sid = x.sid;
                ids.rsp = Some(x.rsp);
                ids.dev_addrs = x.dev.iter().chain(x.dmap.iter()).cloned().collect();
            }
            UdpXml::R2cT(x) => {
                ids.sid = Some(x.sid);
                ids.cid = Some(x.cid);
                ids.dev_addrs = x.dev.iter().chain(x.dmap.iter()).cloned().collect();
            }
            UdpXml::C2dC(x) => ids.cid = Some(x.cid),
            UdpXml::D2cCr(x) => {
                ids.rsp = Some(x.rsp);
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
            }
            UdpXml::C2dT(x) => {
                ids.sid = Some(x.sid);
                ids.conn = Some(x.conn.clone());
                ids.cid = Some(x.cid);
            }
            UdpXml::D2cCfm(x) => {
                ids.sid = Some(x.sid);
                ids.conn = Some(x.conn.clone());
                ids.rsp = Some(x.rsp);
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
                ids.nc = x.nc.clone();
            }
            UdpXml::C2rCfm(x) => {
                ids.sid = Some(x.sid);
                ids.conn = Some(x.conn.clone());
                ids.rsp = Some(x.rsp);
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
            }
            UdpXml::R2cDisc(x) => ids.sid = Some(x.sid),
            UdpXml::C2dDisc(x) => {
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
            }
            UdpXml::D2cDisc(x) => {
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
            }
            UdpXml::C2dHb(x) => {
                ids.cid = Some(x.cid);
                ids.did = Some(x.did);
            }
        }
        ids
    }

    /// Reoling's name for the message (its `UdpXml` variant).
    fn reoling_type(&self) -> Option<String> {
        let debug = format!("{:?}", self.typed.as_ref()?);
        Some(debug.split(['(', ' ', '{']).next().unwrap_or("").to_string())
    }
}

enum UdpKind {
    Data { conn_id: i32, packet_id: u32, reserved: u32, payload: Vec<u8> },
    Ack { conn_id: i32, group_id: u32, packet_id: u32, rate: u32, reserved: u32, bitmap: Vec<u8> },
    P2p(P2pMsg),
    Bad(String),
}

struct UdpPacket {
    dg: Datagram,
    kind: UdpKind,
}

/// Parses one BCUDP datagram with Reoling's own code: `read_bcudp` for Data
/// and Ack; for P2P packets its XML cipher, CRC and `UdpXml` directly, since
/// `read_bcudp` refuses the messages Reoling doesn't model (the header
/// layout is `write_bcudp`'s: magic, length, 1, tid, crc, then the XML).
fn classify(payload: &[u8]) -> UdpKind {
    let word = |at: usize| payload.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let Some(magic) = word(0) else { return UdpKind::Bad("shorter than a magic".to_string()) };
    if magic == MAGIC_HEADER_UDP_NEGO {
        let (Some(len), Some(tid), Some(crc)) = (word(4), word(12), word(16)) else {
            return UdpKind::Bad("truncated P2P header".to_string());
        };
        let Some(encrypted) = (len as usize).checked_add(20).and_then(|end| payload.get(20..end)) else {
            return UdpKind::Bad("truncated P2P payload".to_string());
        };
        let plain = xml_crypto::decrypt(tid, encrypted);
        let text = as_xml_text(&plain).map(str::to_string);
        let name = text.as_deref().and_then(p2p_name).unwrap_or_default();
        return UdpKind::P2p(P2pMsg {
            tid,
            crc_ok: calc_crc(encrypted) == crc,
            name,
            typed: UdpXml::from_bytes(&plain).ok(),
            text,
        });
    }
    // `read_bcudp` skips the third header word; keep it, in case it ever
    // isn't 0.
    let reserved = word(8).unwrap_or(0);
    match read_bcudp(payload) {
        Ok(Some((BcUdp::Data(d), _))) => {
            UdpKind::Data { conn_id: d.connection_id, packet_id: d.packet_id, reserved, payload: d.payload }
        }
        Ok(Some((BcUdp::Ack(a), _))) => UdpKind::Ack {
            conn_id: a.connection_id,
            group_id: a.group_id,
            packet_id: a.packet_id,
            rate: a.received_bytes_per_sec,
            reserved,
            bitmap: a.payload,
        },
        Ok(Some((BcUdp::Discovery(_), _))) => UdpKind::Bad("unexpected discovery".to_string()),
        Ok(None) => UdpKind::Bad("truncated".to_string()),
        Err(e) => UdpKind::Bad(e.to_string()),
    }
}

/// The name of the element under `<P2P>` (or the root's, for another root).
fn p2p_name(text: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut depth = 0usize;
    let mut root: Option<String> = None;
    loop {
        let (name, opens) = match reader.read_event() {
            Ok(Event::Start(e)) => (String::from_utf8_lossy(e.name().as_ref()).into_owned(), true),
            Ok(Event::Empty(e)) => (String::from_utf8_lossy(e.name().as_ref()).into_owned(), false),
            Ok(Event::End(_)) => {
                if depth <= 1 {
                    return root;
                }
                depth -= 1;
                continue;
            }
            Ok(Event::Eof) | Err(_) => return root,
            Ok(_) => continue,
        };
        match depth {
            0 if name == "P2P" => {}
            0 => return Some(name),
            1 => return Some(name),
            _ => {}
        }
        if depth == 0 {
            root = Some(name);
        }
        if opens {
            depth += 1;
        }
    }
}

/// The text of every element inside the message element of a P2P XML, by
/// its path below it (`sid`, `dev/ip`, ...), in document order.
fn p2p_leaves(text: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack: Vec<String> = Vec::new();
    let mut leaves = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => stack.push(String::from_utf8_lossy(e.name().as_ref()).into_owned()),
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(e)) if stack.len() > 2 => {
                let Ok(value) = e.unescape() else { continue };
                let value = value.trim();
                if !value.is_empty() {
                    leaves.push((stack[2..].join("/"), value.to_string()));
                }
            }
            Ok(Event::Eof) | Err(_) => return leaves,
            Ok(_) => {}
        }
    }
}

/// Whether the sender of a P2P message named `name` is the app (`C2x`),
/// or the receiver is (`D2C`, `R2C`, `M2C`).
fn p2p_sender_is_app(name: &str) -> Option<bool> {
    if name.starts_with("C2") {
        Some(true)
    } else if name.starts_with("D2C") || name.starts_with("R2C") || name.starts_with("M2C") {
        Some(false)
    } else {
        None
    }
}

/// One Ack packet of a session.
struct AckInfo {
    frame: u64,
    t: f64,
    t_text: String,
    conn_id: i32,
    group_id: u32,
    packet_id: u32,
    rate: u32,
    reserved: u32,
    bitmap: Vec<u8>,
}

/// The Data/Ack traffic between one app endpoint and one peer endpoint with
/// one pair of connection ids.
struct UdpSession {
    id: usize,
    app: Ep,
    peer: Ep,
    /// The `connection_id` of the packets each way: app->dev packets carry
    /// the device's id (`did`), dev->app ones the app's (`cid`).
    conn_ids: [Option<i32>; 2],
    sides_from: &'static str,
    /// Data packets each way, in capture order (`seq` = packet id).
    data: [Vec<Segment>; 2],
    /// Acks sent each way, in capture order.
    acks: [Vec<AckInfo>; 2],
    reserved_nonzero: usize,
    nonce: Option<String>,
    nonce_from: Option<String>,
    nonce_note: Option<&'static str>,
    path: Option<String>,
    device_ips: Vec<IpAddr>,
    t_first: f64,
    t_first_text: String,
    t_first_local: String,
    t_last: f64,
    t_last_local: String,
}

#[derive(Default)]
struct UdpCapture {
    packets: Vec<UdpPacket>,
    /// Packets kept (all, or those `--device` selects).
    keep: Vec<bool>,
    sessions: Vec<UdpSession>,
    /// Per packet: the session of a P2P packet with the same two endpoints.
    p2p_session: Vec<Option<usize>>,
}

/// Which of the two endpoints of Data/Ack traffic is the app, and how that
/// was decided.
fn decide_sides(a: &Ep, b: &Ep, packets: &[UdpPacket], app_hosts: &HashSet<String>) -> (Ep, Ep, &'static str) {
    let between = |p: &UdpPacket, from: &Ep, to: &Ep| p.dg.src == *from && p.dg.dst == *to;
    // The connection ids of the Data/Ack packets each way ([a->b, b->a]).
    // A packet carries its receiver's id: app->dev ones the device's
    // (`did`), dev->app ones the app's (`cid`).
    let mut conn_ids: [HashSet<i32>; 2] = Default::default();
    for p in packets {
        let (UdpKind::Data { conn_id, .. } | UdpKind::Ack { conn_id, .. }) = &p.kind else { continue };
        if between(p, a, b) {
            conn_ids[0].insert(*conn_id);
        } else if between(p, b, a) {
            conn_ids[1].insert(*conn_id);
        }
    }
    // 1. The P2P packets between the two that belong to this traffic: a
    // packet votes only when it carries a `cid` and/or `did` and they are
    // this traffic's ids in the roles its vote gives the two sides. Others
    // on the same endpoints (e.g. a relay passing on handshake packets of
    // other connections) don't count.
    let fits = |ids: &P2pIds, a_is_app: bool| {
        let (to_dev, to_app) = if a_is_app { (&conn_ids[0], &conn_ids[1]) } else { (&conn_ids[1], &conn_ids[0]) };
        (ids.cid.is_some() || ids.did.is_some())
            && ids.cid.map_or(true, |cid| to_app.contains(&cid))
            && ids.did.map_or(true, |did| to_dev.contains(&did))
    };
    let mut votes = [0usize; 2]; // a is the app / b is the app
    for p in packets {
        let UdpKind::P2p(m) = &p.kind else { continue };
        let from_a = between(p, a, b);
        if !from_a && !between(p, b, a) {
            continue;
        }
        let Some(sender_is_app) = p2p_sender_is_app(&m.name) else { continue };
        let a_is_app = sender_is_app == from_a;
        if fits(&m.ids(), a_is_app) {
            votes[usize::from(!a_is_app)] += 1;
        }
    }
    if votes[0] != votes[1] {
        return if votes[0] > votes[1] { (a.clone(), b.clone(), "p2p handshake") } else { (b.clone(), a.clone(), "p2p handshake") };
    }
    // 2. Hosts seen acting as the app elsewhere in the capture.
    match (app_hosts.contains(&a.ip), app_hosts.contains(&b.ip)) {
        (true, false) => return (a.clone(), b.clone(), "host sent C2x messages"),
        (false, true) => return (b.clone(), a.clone(), "host sent C2x messages"),
        _ => {}
    }
    // 3. The side whose stream starts with a login request.
    let starts_with_login = |from: &Ep, to: &Ep| {
        packets
            .iter()
            .filter(|p| between(p, from, to))
            .filter_map(|p| match &p.kind {
                UdpKind::Data { packet_id, payload, .. } => Some((*packet_id, payload)),
                _ => None,
            })
            .min_by_key(|(id, _)| *id)
            .and_then(|(_, payload)| read_header(payload).ok().flatten())
            .is_some_and(|(h, _)| h.msg_id == MSG_ID_LOGIN && (h.class == 0x6514 || h.class == 0x6414))
    };
    match (starts_with_login(a, b), starts_with_login(b, a)) {
        (true, false) => return (a.clone(), b.clone(), "login request"),
        (false, true) => return (b.clone(), a.clone(), "login request"),
        _ => {}
    }
    // 4. A guess: the app sends less.
    let sent = |from: &Ep, to: &Ep| -> usize {
        packets
            .iter()
            .filter(|p| between(p, from, to))
            .map(|p| match &p.kind {
                UdpKind::Data { payload, .. } => payload.len(),
                _ => 0,
            })
            .sum()
    };
    if sent(a, b) <= sent(b, a) {
        (a.clone(), b.clone(), "GUESSED: less data sent")
    } else {
        (b.clone(), a.clone(), "GUESSED: less data sent")
    }
}

fn build_udp(datagrams: Vec<Datagram>, device: Option<&str>) -> UdpCapture {
    let packets: Vec<UdpPacket> = datagrams
        .into_iter()
        .map(|mut dg| {
            let kind = classify(&dg.payload);
            dg.payload = Vec::new();
            UdpPacket { dg, kind }
        })
        .collect();
    if packets.is_empty() {
        return UdpCapture::default();
    }

    // Hosts seen acting as the app.
    let mut app_hosts: HashSet<String> = HashSet::new();
    for p in &packets {
        if let UdpKind::P2p(m) = &p.kind {
            match p2p_sender_is_app(&m.name) {
                Some(true) => app_hosts.insert(p.dg.src.ip.clone()),
                Some(false) => app_hosts.insert(p.dg.dst.ip.clone()),
                None => false,
            };
        }
    }

    // Endpoint pairs with Data/Ack traffic, in order of first appearance.
    let mut pairs: Vec<(Ep, Ep)> = Vec::new();
    let mut pair_seen: HashSet<(Ep, Ep)> = HashSet::new();
    for p in &packets {
        if matches!(p.kind, UdpKind::Data { .. } | UdpKind::Ack { .. }) {
            let key = if p.dg.src <= p.dg.dst { (p.dg.src.clone(), p.dg.dst.clone()) } else { (p.dg.dst.clone(), p.dg.src.clone()) };
            if pair_seen.insert(key.clone()) {
                pairs.push(key);
            }
        }
    }

    // Sessions: per pair, per pair of connection ids.
    struct Draft {
        app: Ep,
        peer: Ep,
        sides_from: &'static str,
        conn_ids: [Option<i32>; 2],
    }
    let mut drafts: Vec<Draft> = Vec::new();
    for (a, b) in &pairs {
        let (app, peer, sides_from) = decide_sides(a, b, &packets, &app_hosts);
        let mut ids: [Vec<i32>; 2] = Default::default();
        for p in &packets {
            let dir = if p.dg.src == app && p.dg.dst == peer {
                APP_TO_DEV
            } else if p.dg.src == peer && p.dg.dst == app {
                DEV_TO_APP
            } else {
                continue;
            };
            let conn_id = match &p.kind {
                UdpKind::Data { conn_id, .. } | UdpKind::Ack { conn_id, .. } => *conn_id,
                _ => continue,
            };
            if !ids[dir].contains(&conn_id) {
                ids[dir].push(conn_id);
            }
        }
        // (cid, did) pairs the P2P packets on the app's endpoint announce.
        let announced: HashSet<(i32, i32)> = packets
            .iter()
            .filter(|p| p.dg.src == app || p.dg.dst == app)
            .filter_map(|p| match &p.kind {
                UdpKind::P2p(m) => {
                    let ids = m.ids();
                    Some((ids.cid?, ids.did?))
                }
                _ => None,
            })
            .collect();
        let mut used_d2a = vec![false; ids[DEV_TO_APP].len()];
        let mut left_a2d = Vec::new();
        for &did in &ids[APP_TO_DEV] {
            let found = ids[DEV_TO_APP]
                .iter()
                .enumerate()
                .position(|(k, &cid)| !used_d2a[k] && announced.contains(&(cid, did)));
            match found {
                Some(k) => {
                    used_d2a[k] = true;
                    drafts.push(Draft { app: app.clone(), peer: peer.clone(), sides_from, conn_ids: [Some(did), Some(ids[DEV_TO_APP][k])] });
                }
                None => left_a2d.push(did),
            }
        }
        let left_d2a: Vec<i32> = ids[DEV_TO_APP].iter().zip(&used_d2a).filter(|(_, used)| !**used).map(|(id, _)| *id).collect();
        if left_a2d.len() == left_d2a.len() {
            for (did, cid) in left_a2d.iter().zip(&left_d2a) {
                drafts.push(Draft { app: app.clone(), peer: peer.clone(), sides_from, conn_ids: [Some(*did), Some(*cid)] });
            }
        } else {
            for did in left_a2d {
                drafts.push(Draft { app: app.clone(), peer: peer.clone(), sides_from, conn_ids: [Some(did), None] });
            }
            for cid in left_d2a {
                drafts.push(Draft { app: app.clone(), peer: peer.clone(), sides_from, conn_ids: [None, Some(cid)] });
            }
        }
    }

    let mut sessions: Vec<UdpSession> = drafts
        .into_iter()
        .map(|d| UdpSession {
            id: 0,
            app: d.app,
            peer: d.peer,
            conn_ids: d.conn_ids,
            sides_from: d.sides_from,
            data: Default::default(),
            acks: Default::default(),
            reserved_nonzero: 0,
            nonce: None,
            nonce_from: None,
            nonce_note: None,
            path: None,
            device_ips: Vec::new(),
            t_first: f64::INFINITY,
            t_first_text: String::new(),
            t_first_local: String::new(),
            t_last: f64::NEG_INFINITY,
            t_last_local: String::new(),
        })
        .collect();
    let mut by_key: HashMap<(Ep, Ep, usize, i32), usize> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        for dir in [APP_TO_DEV, DEV_TO_APP] {
            if let Some(id) = s.conn_ids[dir] {
                by_key.insert((s.app.clone(), s.peer.clone(), dir, id), i);
            }
        }
    }
    let mut packets = packets;
    for p in &mut packets {
        let conn_id = match &p.kind {
            UdpKind::Data { conn_id, .. } | UdpKind::Ack { conn_id, .. } => *conn_id,
            _ => continue,
        };
        let a2d = by_key.get(&(p.dg.src.clone(), p.dg.dst.clone(), APP_TO_DEV, conn_id)).map(|&i| (i, APP_TO_DEV));
        let d2a = by_key.get(&(p.dg.dst.clone(), p.dg.src.clone(), DEV_TO_APP, conn_id)).map(|&i| (i, DEV_TO_APP));
        let Some((i, dir)) = a2d.or(d2a) else { continue };
        let s = &mut sessions[i];
        if p.dg.t < s.t_first {
            s.t_first = p.dg.t;
            s.t_first_text = p.dg.t_text.clone();
            s.t_first_local = p.dg.t_local.clone();
        }
        if p.dg.t >= s.t_last {
            s.t_last = p.dg.t;
            s.t_last_local = p.dg.t_local.clone();
        }
        match &mut p.kind {
            UdpKind::Data { packet_id, reserved, payload, .. } => {
                if *reserved != 0 {
                    s.reserved_nonzero += 1;
                }
                s.data[dir].push(Segment {
                    frame: p.dg.frame,
                    t: p.dg.t,
                    t_text: p.dg.t_text.clone(),
                    t_local: p.dg.t_local.clone(),
                    tcp_stream: 0,
                    src: p.dg.src.ip.clone(),
                    sport: p.dg.src.port,
                    dst: p.dg.dst.ip.clone(),
                    dport: p.dg.dst.port,
                    seq: *packet_id,
                    syn: false,
                    payload: std::mem::take(payload),
                });
            }
            UdpKind::Ack { conn_id, group_id, packet_id, rate, reserved, bitmap } => {
                if *reserved != 0 {
                    s.reserved_nonzero += 1;
                }
                s.acks[dir].push(AckInfo {
                    frame: p.dg.frame,
                    t: p.dg.t,
                    t_text: p.dg.t_text.clone(),
                    conn_id: *conn_id,
                    group_id: *group_id,
                    packet_id: *packet_id,
                    rate: *rate,
                    reserved: *reserved,
                    bitmap: std::mem::take(bitmap),
                });
            }
            _ => {}
        }
    }
    sessions.sort_by(|a, b| a.t_first.total_cmp(&b.t_first));
    for (i, s) in sessions.iter_mut().enumerate() {
        s.id = i;
    }

    // Nonce, path and device addresses from the P2P packets on the app's
    // endpoint.
    for s in &mut sessions {
        let [did, cid] = s.conn_ids;
        let mut best: Option<(u8, usize)> = None;
        for (i, p) in packets.iter().enumerate() {
            if p.dg.dst != s.app {
                continue;
            }
            let UdpKind::P2p(m) = &p.kind else { continue };
            let ids = m.ids();
            for addr in &ids.dev_addrs {
                if let Ok(ip) = addr.ip.parse::<IpAddr>() {
                    if !s.device_ips.contains(&ip) {
                        s.device_ips.push(ip);
                    }
                }
            }
            if m.name != "D2C_CFM" {
                continue;
            }
            let ids_match = (cid.is_some() && ids.cid == cid) || (did.is_some() && ids.did == did);
            let from_peer = p.dg.src == s.peer;
            // A D2C_CFM for other ids belongs to another session, unless it
            // is the only clue there is (then said so).
            let score = match (ids_match, from_peer) {
                (true, true) => 3,
                (true, false) => 2,
                (false, true) => 1,
                (false, false) => continue,
            };
            if best.map_or(true, |(b, _)| score > b) {
                best = Some((score, i));
            }
        }
        if let Some((score, i)) = best {
            let p = &packets[i];
            let UdpKind::P2p(m) = &p.kind else { unreachable!() };
            let ids = m.ids();
            s.path = ids.conn.clone();
            if m.typed.is_none() {
                s.nonce_note = Some("D2C_CFM not parsed by Reoling's UdpXml: ids from its elements");
            }
            if let Some(nc) = ids.nc {
                s.nonce = Some(nc);
                s.nonce_from = Some(format!(
                    "D2C_CFM frame {} from {}{}",
                    p.dg.frame,
                    p.dg.src.name(),
                    if score == 1 { " (its cid/did do NOT match this session's)" } else { "" }
                ));
            }
        }
        if s.path.is_none() {
            // The path the app asked for (C2D_T), for the app's own id.
            s.path = packets.iter().find_map(|p| match &p.kind {
                UdpKind::P2p(m) if p.dg.src == s.app && p.dg.dst == s.peer => {
                    let ids = m.ids();
                    (m.name == "C2D_T" && (cid.is_none() || ids.cid == cid)).then_some(ids.conn).flatten()
                }
                _ => None,
            });
        }
        if let Ok(ip) = s.peer.ip.parse::<IpAddr>() {
            if !s.device_ips.contains(&ip) {
                s.device_ips.insert(0, ip);
            }
        }
    }

    // `--device`: the sessions with it as peer or announced device address,
    // and the P2P packets of their app endpoints or to/from it.
    let device_ip: Option<IpAddr> = device.and_then(|d| d.parse().ok());
    let session_wanted: Vec<bool> = sessions
        .iter()
        .map(|s| match (device, device_ip) {
            (None, _) => true,
            (Some(_), Some(ip)) => s.device_ips.contains(&ip),
            (Some(d), None) => s.peer.ip == d,
        })
        .collect();
    let wanted_apps: HashSet<&Ep> = sessions.iter().zip(&session_wanted).filter(|(_, w)| **w).map(|(s, _)| &s.app).collect();
    let ip_is_device = |ip: &str| match (device, device_ip) {
        (Some(d), Some(dev)) => ip.parse::<IpAddr>().map_or(ip == d, |x| x == dev),
        (Some(d), None) => ip == d,
        (None, _) => false,
    };
    let mut keep: Vec<bool> = packets
        .iter()
        .map(|p| match &p.kind {
            UdpKind::P2p(_) | UdpKind::Bad(_) => {
                device.is_none()
                    || wanted_apps.contains(&p.dg.src)
                    || wanted_apps.contains(&p.dg.dst)
                    || ip_is_device(&p.dg.src.ip)
                    || ip_is_device(&p.dg.dst.ip)
            }
            _ => false,
        })
        .collect();
    // Data/Ack packets are kept with their session.
    for (p, k) in packets.iter().zip(keep.iter_mut()) {
        if matches!(p.kind, UdpKind::Data { .. } | UdpKind::Ack { .. }) {
            *k = sessions.iter().zip(&session_wanted).any(|(s, w)| {
                *w && ((p.dg.src == s.app && p.dg.dst == s.peer) || (p.dg.src == s.peer && p.dg.dst == s.app))
            });
        }
    }
    let mut kept_sessions = Vec::new();
    for (s, w) in sessions.into_iter().zip(session_wanted) {
        if w {
            kept_sessions.push(s);
        }
    }
    for (i, s) in kept_sessions.iter_mut().enumerate() {
        s.id = i;
    }
    let sessions = kept_sessions;

    // P2P packets -> a session on the same app socket: the one with the
    // same peer and ids, else the same ids, else the same peer, else any;
    // among several, the latest started by then (else the first).
    let p2p_session = packets
        .iter()
        .map(|p| {
            let UdpKind::P2p(m) = &p.kind else { return None };
            let ids = m.ids();
            let on_socket: Vec<&UdpSession> = sessions.iter().filter(|s| p.dg.src == s.app || p.dg.dst == s.app).collect();
            let same_peer = |s: &&&UdpSession| p.dg.src == s.peer || p.dg.dst == s.peer;
            let same_ids = |s: &&&UdpSession| {
                (ids.cid.is_some() && ids.cid == s.conn_ids[DEV_TO_APP]) || (ids.did.is_some() && ids.did == s.conn_ids[APP_TO_DEV])
            };
            let latest = |candidates: Vec<&&UdpSession>| {
                candidates.iter().rev().find(|s| s.t_first <= p.dg.t).or(candidates.first()).map(|s| s.id)
            };
            let found = [
                on_socket.iter().filter(|s| same_peer(s) && same_ids(s)).collect::<Vec<_>>(),
                on_socket.iter().filter(same_ids).collect(),
                on_socket.iter().filter(same_peer).collect(),
                on_socket.iter().collect(),
            ]
            .into_iter()
            .find(|c| !c.is_empty())
            .and_then(latest);
            found
        })
        .collect();
    if device.is_none() {
        keep.iter_mut().for_each(|k| *k = true);
    }
    let any_kept = keep.iter().any(|k| *k);
    if !any_kept {
        return UdpCapture::default();
    }
    UdpCapture { packets, keep, sessions, p2p_session }
}

/// The P2P packets on `app`'s endpoint, condensed: `NAME(conn)->peer xN`,
/// in order of first appearance.
fn handshake_summary(udp: &UdpCapture, app: &Ep) -> String {
    let mut items: Vec<(String, String, Vec<String>, usize)> = Vec::new(); // (name+conn, arrow, peers, count)
    for (p, keep) in udp.packets.iter().zip(&udp.keep) {
        let UdpKind::P2p(m) = &p.kind else { continue };
        if !keep {
            continue;
        }
        let (arrow, other) = if p.dg.src == *app {
            ("->", &p.dg.dst)
        } else if p.dg.dst == *app {
            ("<-", &p.dg.src)
        } else {
            continue;
        };
        let mut label = if m.name.is_empty() { "?".to_string() } else { m.name.clone() };
        if let Some(conn) = m.ids().conn {
            let _ = write!(label, "({conn})");
        }
        let other = other.name();
        match items.iter_mut().find(|(l, a, _, _)| *l == label && *a == arrow) {
            Some((_, _, peers, n)) => {
                *n += 1;
                if !peers.contains(&other) {
                    peers.push(other);
                }
            }
            None => items.push((label, arrow.to_string(), vec![other], 1)),
        }
    }
    let parts: Vec<String> = items
        .iter()
        .map(|(label, arrow, peers, n)| {
            let to = if peers.len() == 1 { peers[0].clone() } else { format!("{} endpoints", peers.len()) };
            if *n > 1 {
                format!("{label}{arrow}{to} x{n}")
            } else {
                format!("{label}{arrow}{to}")
            }
        })
        .collect();
    if parts.is_empty() {
        "none in the capture".to_string()
    } else {
        parts.join(", ")
    }
}

fn print_udp_overview(udp: &UdpCapture) {
    let (mut data, mut acks, mut p2p, mut bad_crc, mut bad) = (0, 0, 0, 0, 0);
    let mut names: Vec<(String, usize, bool)> = Vec::new(); // name, count, modelled by Reoling
    let mut bad_why: BTreeMap<String, usize> = BTreeMap::new();
    for (p, keep) in udp.packets.iter().zip(&udp.keep) {
        if !keep {
            continue;
        }
        match &p.kind {
            UdpKind::Data { .. } => data += 1,
            UdpKind::Ack { .. } => acks += 1,
            UdpKind::P2p(m) => {
                p2p += 1;
                if !m.crc_ok {
                    bad_crc += 1;
                }
                let name = if m.name.is_empty() { "?".to_string() } else { m.name.clone() };
                match names.iter_mut().find(|(n, _, _)| *n == name) {
                    Some((_, count, _)) => *count += 1,
                    None => names.push((name, 1, m.typed.is_some())),
                }
            }
            UdpKind::Bad(why) => {
                bad += 1;
                *bad_why.entry(why.clone()).or_default() += 1;
            }
        }
    }
    let unparsed: Vec<&str> = names.iter().filter(|(_, _, typed)| !typed).map(|(n, _, _)| n.as_str()).collect();
    let mut line = format!(
        "BCUDP: {} packets: {data} Data, {acks} Ack, {p2p} P2P ({bad_crc} with a bad CRC",
        data + acks + p2p + bad
    );
    if !unparsed.is_empty() {
        let _ = write!(line, "; not parsed by Reoling's UdpXml (unknown to it, or another shape): {}", unparsed.join(" "));
    }
    let _ = write!(line, "), {bad} unparsable; {} data sessions", udp.sessions.len());
    eprintln!("{line}");
    if !bad_why.is_empty() {
        let why: Vec<String> = bad_why.iter().map(|(w, n)| format!("{w} x{n}")).collect();
        eprintln!("  unparsable: {}", why.join(", "));
    }
    if !names.is_empty() {
        let list: Vec<String> = names.iter().map(|(n, c, _)| format!("{n} x{c}")).collect();
        eprintln!("  p2p messages: {}", list.join(", "));
    }
}

fn decode_udp_session(s: &UdpSession, udp: &UdpCapture, passwords: &Passwords) -> Vec<Record> {
    let streams = [Reassembled::from_packets(&s.data[APP_TO_DEV]), Reassembled::from_packets(&s.data[DEV_TO_APP])];
    let conn_name = format!("{}->{}", s.app.name(), s.peer.name());
    let id_text = |id: Option<i32>| id.map_or("?".to_string(), |v| v.to_string());
    let mut header = vec![
        format!(
            "udp session {} {conn_name} (path {}, cid {}, did {}; app side from: {})  [{} .. {}]",
            s.id,
            s.path.as_deref().unwrap_or("unknown"),
            id_text(s.conn_ids[DEV_TO_APP]),
            id_text(s.conn_ids[APP_TO_DEV]),
            s.sides_from,
            s.t_first_local,
            s.t_last_local
        ),
        format!("  p2p on {}: {}", s.app.name(), handshake_summary(udp, &s.app)),
    ];
    if let Some(from) = &s.nonce_from {
        header.push(format!("  nonce: nc of {from}"));
    }
    if let Some(note) = s.nonce_note {
        header.push(format!("  ({note})"));
    }
    let records = if streams.iter().all(|r| r.data.is_empty()) {
        for line in &header {
            eprintln!("{line}");
        }
        eprintln!("  no BC data (only acks and P2P packets)");
        Vec::new()
    } else {
        decode_session(SessionInput {
            ident: Ident::Udp { session: s.id, peer: s.peer.name(), path: s.path.clone() },
            conn_name,
            header,
            streams,
            handshake_nonce: s.nonce.clone(),
            device_ips: s.device_ips.clone(),
            passwords,
        })
    };
    let mut line = "  acks:".to_string();
    for dir in [APP_TO_DEV, DEV_TO_APP] {
        let acks = &s.acks[dir];
        let _ = write!(line, " {} {}", DIR_NAMES[dir], acks.len());
        if let (Some(min), Some(max)) = (acks.iter().map(|a| a.rate).min(), acks.iter().map(|a| a.rate).max()) {
            let top = acks.iter().map(|a| a.packet_id).filter(|&p| p != u32::MAX).max();
            let _ = write!(line, " (rate field {min}..{max}");
            if let Some(top) = top {
                let _ = write!(line, ", up to packet {top}");
            }
            line.push(')');
        }
        if dir == APP_TO_DEV {
            line.push(',');
        }
    }
    if s.reserved_nonzero > 0 {
        let _ = write!(line, "; header word 8..12 NOT 0 in {} packets", s.reserved_nonzero);
    }
    eprintln!("{line}");
    records
}

struct FlowRecord {
    t: f64,
    frame: u64,
    order: usize,
    json: String,
}

/// The flow file: session records, every Ack with what the capture shows
/// next to it, Data per second, and every P2P packet.
fn flow_records(udp: &UdpCapture) -> Vec<FlowRecord> {
    let mut out: Vec<FlowRecord> = Vec::new();
    for s in &udp.sessions {
        let mut j = Json::new();
        j.str("kind", "session");
        j.raw("t", &s.t_first_text);
        j.str("t_local", &s.t_first_local);
        j.num("udp_session", s.id);
        j.str("app", &s.app.name());
        j.str("peer", &s.peer.name());
        match &s.path {
            Some(p) => j.str("path", p),
            None => j.raw("path", "null"),
        }
        for (key, id) in [("conn_id_app_to_dev", s.conn_ids[APP_TO_DEV]), ("conn_id_dev_to_app", s.conn_ids[DEV_TO_APP])] {
            match id {
                Some(id) => j.num(key, id),
                None => j.raw(key, "null"),
            }
        }
        j.str("sides_from", s.sides_from);
        j.str("t_last_local", &s.t_last_local);
        j.raw("data_packets", &format!("[{},{}]", s.data[APP_TO_DEV].len(), s.data[DEV_TO_APP].len()));
        j.raw("acks", &format!("[{},{}]", s.acks[APP_TO_DEV].len(), s.acks[DEV_TO_APP].len()));
        out.push(FlowRecord { t: s.t_first, frame: 0, order: 0, json: j.finish() });

        for dir in [APP_TO_DEV, DEV_TO_APP] {
            // Acks travelling `dir` acknowledge the Data travelling the
            // other way.
            let data = &s.data[1 - dir];
            let mut bytes_before = Vec::with_capacity(data.len() + 1);
            let mut max_before = Vec::with_capacity(data.len() + 1);
            bytes_before.push(0usize);
            max_before.push(None::<u32>);
            for d in data {
                bytes_before.push(bytes_before.last().unwrap() + d.payload.len());
                max_before.push((*max_before.last().unwrap()).max(Some(d.seq)));
            }
            for a in &s.acks[dir] {
                let upto = data.partition_point(|d| d.frame <= a.frame);
                let from = data[..upto].partition_point(|d| d.t <= a.t - 1.0);
                let mut j = Json::new();
                j.str("kind", "ack");
                j.raw("t", &a.t_text);
                j.num("frame", a.frame);
                j.num("udp_session", s.id);
                j.str("dir", DIR_NAMES[dir]);
                j.num("connection_id", a.conn_id);
                j.num("group_id", a.group_id);
                j.num("packet_id", a.packet_id);
                j.num("rate", a.rate);
                if a.reserved != 0 {
                    j.num("reserved", a.reserved);
                }
                j.num("bitmap_len", a.bitmap.len());
                j.num("bitmap_ones", a.bitmap.iter().filter(|&&b| b != 0).count());
                if !a.bitmap.is_empty() && a.bitmap.len() <= BITMAP_SHOWN_MAX {
                    let bits: String = a.bitmap.iter().map(|&b| if b == 0 { '0' } else { '1' }).collect();
                    j.str("bitmap", &bits);
                }
                match max_before[upto] {
                    Some(max) => {
                        j.num("data_sent_max", max);
                        if a.packet_id != u32::MAX {
                            j.num("unacked", i64::from(max) - i64::from(a.packet_id));
                        } else {
                            j.num("unacked", i64::from(max) + 1);
                        }
                    }
                    None => {
                        j.raw("data_sent_max", "null");
                        j.num("unacked", 0);
                    }
                }
                j.num("rx_bytes_1s", bytes_before[upto] - bytes_before[from]);
                out.push(FlowRecord { t: a.t, frame: a.frame, order: 1, json: j.finish() });
            }
        }

        for dir in [APP_TO_DEV, DEV_TO_APP] {
            #[derive(Default)]
            struct Bin {
                t_text: String,
                packets: usize,
                new: usize,
                dups: usize,
                bytes: usize,
                max_id: Option<u32>,
                acks_back: usize,
                ack_rate_last: Option<u32>,
            }
            let mut bins: BTreeMap<i64, Bin> = BTreeMap::new();
            let sec_of = |t: f64| (t - s.t_first).floor() as i64;
            let mut seen = HashSet::new();
            for d in &s.data[dir] {
                let bin = bins.entry(sec_of(d.t)).or_default();
                if bin.t_text.is_empty() {
                    bin.t_text = d.t_text.clone();
                }
                bin.packets += 1;
                bin.bytes += d.payload.len();
                if seen.insert(d.seq) {
                    bin.new += 1;
                } else {
                    bin.dups += 1;
                }
                bin.max_id = bin.max_id.max(Some(d.seq));
            }
            for a in &s.acks[1 - dir] {
                let bin = bins.entry(sec_of(a.t)).or_default();
                if bin.t_text.is_empty() {
                    bin.t_text = a.t_text.clone();
                }
                bin.acks_back += 1;
                bin.ack_rate_last = Some(a.rate);
            }
            for (sec, bin) in bins {
                let mut j = Json::new();
                j.str("kind", "data_1s");
                j.raw("t", &bin.t_text);
                j.num("udp_session", s.id);
                j.str("dir", DIR_NAMES[dir]);
                j.num("sec", sec);
                j.num("packets", bin.packets);
                j.num("new", bin.new);
                j.num("dups", bin.dups);
                j.num("bytes", bin.bytes);
                match bin.max_id {
                    Some(id) => j.num("max_packet_id", id),
                    None => j.raw("max_packet_id", "null"),
                }
                j.num("acks_back", bin.acks_back);
                match bin.ack_rate_last {
                    Some(r) => j.num("ack_rate_last", r),
                    None => j.raw("ack_rate_last", "null"),
                }
                let t = bin.t_text.parse().unwrap_or(s.t_first);
                out.push(FlowRecord { t, frame: 0, order: 2, json: j.finish() });
            }
        }
    }

    for (i, p) in udp.packets.iter().enumerate() {
        if !udp.keep[i] {
            continue;
        }
        let UdpKind::P2p(m) = &p.kind else { continue };
        let ids = m.ids();
        let mut j = Json::new();
        j.str("kind", "p2p");
        j.raw("t", &p.dg.t_text);
        j.str("t_local", &p.dg.t_local);
        j.num("frame", p.dg.frame);
        j.str("src", &p.dg.src.name());
        j.str("dst", &p.dg.dst.name());
        match udp.p2p_session[i] {
            Some(id) => j.num("udp_session", id),
            None => j.raw("udp_session", "null"),
        }
        j.str("msg", &m.name);
        match m.reoling_type() {
            Some(t) => j.str("reoling_type", &t),
            None => j.raw("reoling_type", "null"),
        }
        j.num("tid", m.tid);
        j.raw("crc_ok", if m.crc_ok { "true" } else { "false" });
        if let Some(v) = ids.sid {
            j.num("sid", v);
        }
        if let Some(v) = ids.cid {
            j.num("cid", v);
        }
        if let Some(v) = ids.did {
            j.num("did", v);
        }
        if let Some(v) = &ids.conn {
            j.str("conn", v);
        }
        if let Some(v) = ids.rsp {
            j.num("rsp", v);
        }
        match &m.text {
            Some(text) => j.str("xml", &redact(text)),
            None => j.raw("xml", "null"),
        }
        out.push(FlowRecord { t: p.dg.t, frame: p.dg.frame, order: 3, json: j.finish() });
    }
    out.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.frame.cmp(&b.frame)).then(a.order.cmp(&b.order)));
    out
}

// ---------------------------------------------------------------------------
// XML and JSON helpers
// ---------------------------------------------------------------------------

/// `bytes` as text, if it is UTF-8 that starts like XML (trailing NULs and
/// whitespace dropped).
fn as_xml_text(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().rposition(|b| !matches!(b, 0 | b' ' | b'\n' | b'\r' | b'\t'))? + 1;
    let text = std::str::from_utf8(&bytes[..end]).ok()?;
    text.trim_start().starts_with('<').then_some(text)
}

/// Whether `text` is well-formed XML, and the names of the elements directly
/// under `<body>` (or the root's name, for a root other than `body`).
fn xml_summary(text: &str) -> (bool, Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut depth = 0usize;
    let mut root: Option<String> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut ok = true;
    loop {
        let (name, opens) = match reader.read_event() {
            Ok(Event::Start(e)) => (String::from_utf8_lossy(e.name().as_ref()).into_owned(), true),
            Ok(Event::Empty(e)) => (String::from_utf8_lossy(e.name().as_ref()).into_owned(), false),
            Ok(Event::End(_)) => {
                if depth == 0 {
                    ok = false;
                    break;
                }
                depth -= 1;
                continue;
            }
            Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(_) => {
                ok = false;
                break;
            }
        };
        if depth == 0 {
            if root.is_some() {
                ok = false; // a second root element
            }
            root = Some(name.clone());
        } else if depth == 1 && root.as_deref() == Some("body") && !tags.contains(&name) {
            tags.push(name);
        }
        if opens {
            depth += 1;
        }
    }
    let ok = ok && depth == 0 && root.is_some();
    if root.as_deref() != Some("body") {
        tags = root.into_iter().collect();
    }
    (ok, tags)
}

/// Element names whose text is replaced by `(redacted)`: passwords and
/// similar secrets, device UIDs (`uid`, and the `fakeUid` an NVR's channel
/// list carries: the NVR's own UID plus a suffix), and (inside `LoginUser`)
/// the login hashes, which with the nonce would allow an offline guess of
/// the password.
fn is_secret(name: &str, in_login: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    ["password", "passwd", "pwd", "token", "secret"].iter().any(|s| lower.contains(s))
        || lower == "uid"
        || lower == "fakeuid"
        || lower == "key"
        || (in_login && lower == "username")
}

/// Elements that name a recording (`Id` in file searches and playback
/// opens, `fileId`): on a Home Hub the value ends with the camera's UID.
/// Matched by exact name, so that e.g. a preset's `<id>` stays as it is.
fn is_recording_id(name: &str) -> bool {
    name == "Id" || name == "fileId"
}

/// A recording id with every dash-separated part that could be a UID (one
/// with a letter in it, or 16 characters or more) replaced; the channel,
/// time and counter parts stay.
fn mask_recording_id(value: &str) -> String {
    value
        .split('-')
        .map(|part| {
            if part.chars().any(|c| c.is_ascii_alphabetic()) || part.len() >= 16 {
                "(redacted)"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// `xml` with the text of every secret leaf element replaced, and the UID
/// parts of recording ids.
fn redact(xml: &str) -> String {
    let in_login = xml.contains("<LoginUser");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let from_tag = &rest[lt..];
        let Some(gt) = from_tag.find('>') else {
            out.push_str(from_tag);
            return out;
        };
        let tag = &from_tag[..=gt];
        out.push_str(tag);
        rest = &from_tag[gt + 1..];
        let inner = &tag[1..tag.len() - 1];
        if inner.starts_with(['/', '?', '!']) || inner.ends_with('/') {
            continue;
        }
        let name = inner.split(char::is_whitespace).next().unwrap_or("");
        let secret = is_secret(name, in_login);
        if !secret && !is_recording_id(name) {
            continue;
        }
        if let Some(end) = rest.find(&format!("</{name}>")) {
            if end > 0 && !rest[..end].contains('<') {
                if secret {
                    out.push_str("(redacted)");
                } else {
                    out.push_str(&mask_recording_id(&rest[..end]));
                }
                rest = &rest[end..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A JSON object built field by field (no JSON library in this crate).
struct Json(String);

impl Json {
    fn new() -> Self {
        Json(String::from("{"))
    }
    fn key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        let _ = write!(self.0, "\"{key}\":");
    }
    fn str(&mut self, key: &str, value: &str) {
        self.key(key);
        let _ = write!(self.0, "\"{}\"", json_escape(value));
    }
    fn num(&mut self, key: &str, value: impl std::fmt::Display) {
        self.key(key);
        let _ = write!(self.0, "{value}");
    }
    /// `value` must already be valid JSON (a number, `true`, `null`, ...).
    fn raw(&mut self, key: &str, value: &str) {
        self.key(key);
        self.0.push_str(value);
    }
    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::protocol::bc::codec::write_bc;
    use reoling::protocol::bcudp::codec::write_bcudp;
    use reoling::protocol::bcudp::model::{UdpAck, UdpData, UdpDiscovery};
    use reoling::protocol::bcudp::xml::{C2dHb, D2cCfm};

    fn seg(frame: u64, t: f64, seq: u32, payload: &[u8]) -> Segment {
        Segment {
            frame,
            t,
            t_text: t.to_string(),
            t_local: String::new(),
            tcp_stream: 0,
            src: "10.0.0.1".into(),
            sport: 50000,
            dst: "10.0.0.2".into(),
            dport: 9000,
            seq,
            syn: false,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn reassembles_out_of_order_and_retransmitted_segments_keeping_the_earliest_time() {
        let direction = Direction {
            syn_seq: Some(999),
            segments: vec![
                seg(1, 1.0, 1000, b"abcd"),
                seg(2, 2.0, 1008, b"ijkl"), // arrives before the segment it follows
                seg(3, 3.0, 1004, b"efgh"),
                seg(4, 4.0, 1000, b"abcdefgh"), // retransmission
            ],
        };
        let r = Reassembled::new(&direction);
        assert_eq!(r.data, b"abcdefghijkl");
        assert!(r.holes.is_empty());
        assert_eq!(r.retransmitted_bytes, 8);
        assert_eq!(r.first_seen(0).unwrap().frame, 1);
        assert_eq!(r.first_seen(5).unwrap().frame, 3, "the original, not the later retransmission");
        assert_eq!(r.first_seen(9).unwrap().frame, 2);
    }

    #[test]
    fn records_bytes_missing_from_the_capture_as_a_hole() {
        let direction = Direction {
            syn_seq: None,
            segments: vec![seg(1, 1.0, u32::MAX - 1, b"ab"), seg(2, 2.0, 4, b"gh")], // wraps around
        };
        let r = Reassembled::new(&direction);
        assert_eq!(r.holes, vec![(2, 6)]);
        assert_eq!(r.hole_at(3), Some(6));
        assert!(r.overlaps_hole(0, 3));
        assert!(!r.overlaps_hole(6, 8));
    }

    #[test]
    fn reassembles_udp_packets_by_id_dropping_duplicates_and_breaking_at_missing_ids() {
        let packets = vec![
            seg(1, 1.0, 0, b"ab"),
            seg(2, 1.1, 2, b"ef"), // before id 1
            seg(3, 1.2, 1, b"cd"),
            seg(4, 1.3, 1, b"cd"), // retransmission
            seg(5, 1.4, 2, b"EF"), // a "retransmission" with other bytes
            seg(6, 1.5, 5, b"kl"), // ids 3 and 4 never captured
        ];
        let r = Reassembled::from_packets(&packets);
        assert_eq!(r.data, b"abcdefkl");
        assert_eq!(r.breaks, vec![6]);
        assert_eq!(r.retransmitted_bytes, 4);
        let u = r.udp.as_ref().unwrap();
        assert_eq!(
            *u,
            UdpStreamStats {
                packets: 6,
                unique: 4,
                duplicates: 2,
                conflicting: 1,
                missing_ids: 2,
                first_id: 0,
                last_id: 5,
                out_of_order: 1
            }
        );
        assert_eq!(r.first_seen(4).unwrap().frame, 2, "the first copy of id 2");
        assert_eq!(r.first_seen(2).unwrap().frame, 3);
        assert!(r.overlaps_hole(5, 7) && !r.overlaps_hole(6, 8) && !r.overlaps_hole(0, 6));
        assert_eq!(r.break_inside(0, 8), Some(6));
        assert!(r.is_break(6) && !r.is_break(0));

        // A capture that starts mid-session breaks at 0.
        let r = Reassembled::from_packets(&packets[2..]);
        assert_eq!(r.breaks[0], 0);
    }

    fn bc_bytes(msg_id: u32, payload: &[u8]) -> Vec<u8> {
        let bc = Bc {
            meta: BcMeta { msg_id, channel_id: 0, stream_type: 0, msg_num: 0, response_code: 200, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(payload.to_vec()) }),
        };
        write_bc(&bc, &EncryptionProtocol::Unencrypted)
    }

    #[test]
    fn a_message_across_missing_packets_is_truncated_and_framing_restarts_at_the_break() {
        // Message A spans packets 0-2, packet 1 is lost; message B follows
        // in packet 3.
        let a = bc_bytes(80, &[b'x'; 60]);
        let b = bc_bytes(81, b"<body/>");
        let (a0, rest) = a.split_at(30);
        let (_a1, a2) = rest.split_at(30);
        let mut p2 = a2.to_vec();
        p2.extend_from_slice(&b[..10]);
        let packets = vec![seg(1, 1.0, 0, a0), seg(2, 1.2, 2, &p2), seg(3, 1.3, 3, &b[10..])];
        let r = Reassembled::from_packets(&packets);
        assert_eq!(r.breaks, vec![30]);
        let mut found = Vec::new();
        walk_messages(&r, |pos, framed, _| {
            found.push((pos, framed.header.msg_id));
            false
        });
        assert_eq!(found, vec![(30 + a2.len(), 81)], "A is lost, B is found by the strict search");

        let passwords = Passwords::default();
        let records = decode_session(SessionInput {
            ident: Ident::Udp { session: 7, peer: "10.0.0.2:9".to_string(), path: Some("local".to_string()) },
            conn_name: "a->b".to_string(),
            header: Vec::new(),
            streams: [Reassembled::from_packets(&[]), Reassembled::from_packets(&packets)],
            handshake_nonce: None,
            device_ips: Vec::new(),
            passwords: &passwords,
        });
        assert_eq!(records.len(), 2);
        assert!(records[0].json.contains("\"truncated\":\"packets missing from the capture\""), "{}", records[0].json);
        assert!(records[0].json.contains("\"transport\":\"udp\",\"udp_session\":7,\"conn\":\"a->b\",\"peer\":\"10.0.0.2:9\",\"path\":\"local\",\"packet_id\":0,\"packet_id_last\":0"), "{}", records[0].json);
        assert!(records[1].json.contains("\"msg_id\":81") && records[1].json.contains("\"resync\":true"), "{}", records[1].json);
        assert!(records[1].json.contains("\"packet_id\":2,\"packet_id_last\":3"), "{}", records[1].json);
    }

    fn datagram(frame: u64, t: f64, src: (&str, u16), dst: (&str, u16), payload: Vec<u8>) -> Datagram {
        Datagram {
            frame,
            t,
            t_text: format!("{t:.6}"),
            t_local: String::new(),
            src: Ep { ip: src.0.to_string(), port: src.1 },
            dst: Ep { ip: dst.0.to_string(), port: dst.1 },
            payload,
        }
    }

    #[test]
    fn classifies_bcudp_packets_with_reolings_codec_including_unmodelled_p2p_messages() {
        let data = write_bcudp(&BcUdp::Data(UdpData { connection_id: 211, packet_id: 5, payload: b"hi".to_vec() }));
        assert!(matches!(classify(&data), UdpKind::Data { conn_id: 211, packet_id: 5, reserved: 0, ref payload } if payload == b"hi"));
        let ack = write_bcudp(&BcUdp::Ack(UdpAck {
            connection_id: 286006,
            group_id: 0,
            packet_id: 9,
            received_bytes_per_sec: 123456,
            payload: vec![0, 1],
        }));
        assert!(matches!(classify(&ack), UdpKind::Ack { conn_id: 286006, packet_id: 9, rate: 123456, ref bitmap, .. } if bitmap == &[0, 1]));
        let hb = write_bcudp(&BcUdp::Discovery(UdpDiscovery { tid: 77, payload: UdpXml::C2dHb(C2dHb { cid: 1, did: 2 }) }));
        let UdpKind::P2p(m) = classify(&hb) else { panic!("not p2p") };
        assert!(m.crc_ok);
        assert_eq!(m.name, "C2D_HB");
        assert_eq!(m.reoling_type().as_deref(), Some("C2dHb"));
        assert_eq!((m.ids().cid, m.ids().did), (Some(1), Some(2)));

        // A message Reoling's UdpXml doesn't model still decodes.
        let xml = b"<P2P><D2C_HB><cid>1</cid><did>2</did></D2C_HB></P2P>";
        let encrypted = xml_crypto::encrypt(5, xml);
        let mut packet = Vec::new();
        for word in [MAGIC_HEADER_UDP_NEGO, encrypted.len() as u32, 1, 5, calc_crc(&encrypted) ^ 1] {
            packet.extend_from_slice(&word.to_le_bytes());
        }
        packet.extend_from_slice(&encrypted);
        let UdpKind::P2p(m) = classify(&packet) else { panic!("not p2p") };
        assert!(!m.crc_ok);
        assert_eq!(m.name, "D2C_HB");
        assert!(m.typed.is_none());
        assert_eq!(m.text.as_deref(), Some(std::str::from_utf8(xml).unwrap()));
        assert_eq!((m.ids().cid, m.ids().did), (Some(1), Some(2)), "ids from its own elements");

        // Nested elements don't shadow the message's own ones.
        let m = P2pMsg {
            tid: 0,
            crc_ok: true,
            name: "C2R_DISC".to_string(),
            typed: None,
            text: Some("<P2P>\n<C2R_DISC>\n<sid>7</sid>\n<time><conn>431</conn></time>\n<conn>local</conn>\n<rsp>-12</rsp>\n<dev><ip>10.0.0.5</ip><port>9</port></dev></C2R_DISC>\n</P2P>".to_string()),
        };
        let ids = m.ids();
        assert_eq!((ids.sid, ids.conn.as_deref(), ids.rsp), (Some(7), Some("local"), Some(-12)));
        assert_eq!(ids.dev_addrs, vec![IpPort { ip: "10.0.0.5".to_string(), port: 9 }]);
    }

    #[test]
    fn builds_a_session_from_the_handshake_with_its_nonce_and_sides() {
        let app = ("192.168.1.2", 10820);
        let dev = ("192.168.1.5", 59879);
        let cfm = UdpXml::D2cCfm(D2cCfm {
            sid: 1,
            conn: "local".to_string(),
            rsp: 0,
            cid: 286006,
            did: 211,
            nc: Some("12345".to_string()),
            pl: String::new(),
            lver: 3,
        });
        let packets = vec![
            datagram(1, 1.0, dev, app, write_bcudp(&BcUdp::Discovery(UdpDiscovery { tid: 3, payload: cfm }))),
            datagram(2, 1.1, app, dev, write_bcudp(&BcUdp::Data(UdpData { connection_id: 211, packet_id: 0, payload: b"x".to_vec() }))),
            datagram(3, 1.2, dev, app, write_bcudp(&BcUdp::Ack(UdpAck {
                connection_id: 286006,
                group_id: 0,
                packet_id: 0,
                received_bytes_per_sec: 0,
                payload: Vec::new(),
            }))),
            datagram(4, 1.3, dev, app, write_bcudp(&BcUdp::Data(UdpData { connection_id: 286006, packet_id: 0, payload: b"yy".to_vec() }))),
            datagram(5, 1.4, app, dev, write_bcudp(&BcUdp::Ack(UdpAck {
                connection_id: 211,
                group_id: 0,
                packet_id: 0,
                received_bytes_per_sec: 2,
                payload: Vec::new(),
            }))),
        ];
        let udp = build_udp(packets, None);
        assert_eq!(udp.sessions.len(), 1);
        let s = &udp.sessions[0];
        assert_eq!(s.app.port, 10820);
        assert_eq!(s.sides_from, "p2p handshake");
        assert_eq!(s.conn_ids, [Some(211), Some(286006)]);
        assert_eq!(s.nonce.as_deref(), Some("12345"));
        assert_eq!(s.path.as_deref(), Some("local"));
        assert_eq!((s.data[0].len(), s.data[1].len(), s.acks[0].len(), s.acks[1].len()), (1, 1, 1, 1));
        assert_eq!(udp.p2p_session[0], Some(0));
        let flow = flow_records(&udp);
        let ack = flow.iter().find(|r| r.json.contains("\"kind\":\"ack\"") && r.json.contains("\"dir\":\"app->dev\"")).unwrap();
        assert!(ack.json.contains("\"rate\":2") && ack.json.contains("\"data_sent_max\":0") && ack.json.contains("\"rx_bytes_1s\":2"), "{}", ack.json);

        // `--device` with another IP keeps nothing.
        let udp = build_udp(vec![datagram(2, 1.1, app, dev, write_bcudp(&BcUdp::Data(UdpData { connection_id: 211, packet_id: 0, payload: b"x".to_vec() })))], Some("10.9.9.9"));
        assert!(udp.sessions.is_empty() && udp.packets.is_empty());
    }

    #[test]
    fn parses_password_files_per_device_and_picks_the_devices_own() {
        let parse = |args: &[&str]| parse_args_from(args.iter().map(|s| s.to_string()));
        let opts = parse(&["cap.pcapng", "--password-file", "172.17.3.6=/x/nvr", "--password-file", "/a=b/c"]).unwrap();
        assert_eq!(opts.default_password_file.as_deref(), Some(Path::new("/a=b/c")), "not an IP before '=': a plain path");
        assert_eq!(opts.device_password_files, vec![("172.17.3.6".parse().unwrap(), PathBuf::from("/x/nvr"))]);
        // A second default is refused instead of silently replacing the first.
        let err = parse(&["cap.pcapng", "--password-file", "/x/default", "--password-file", "172.17.3.6=/x/nvr", "--password-file", "/x/other"])
            .err()
            .expect("a second bare --password-file is an error");
        assert!(err.contains("--password-file /x/other") && err.contains("already"), "{err}");

        let secret = |label: &str, password: &str| Secret { label: label.into(), password: password.into() };
        let passwords = Passwords {
            default: Some(secret("hub", "one")),
            per_device: vec![("172.17.3.6".parse().unwrap(), secret("nvr", "two"))],
        };
        let (primary, others) = passwords.pick(&["172.17.3.6".parse().unwrap()]);
        assert_eq!(primary.unwrap().label, "nvr");
        assert_eq!(others.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), vec!["hub"]);
        let (primary, others) = passwords.pick(&["172.17.3.5".parse().unwrap()]);
        assert_eq!(primary.unwrap().label, "hub");
        // The NVR's password is chosen for the Hub only if it matches.
        let hash = login_hash("two12345");
        let (chosen, from_others) = choose_password(primary, &others, Some("12345"), Some(&hash));
        assert_eq!((chosen.unwrap().label.as_str(), from_others), ("nvr", true));
        let (chosen, from_others) = choose_password(primary, &others, Some("12345"), Some("nothing"));
        assert_eq!((chosen.unwrap().label.as_str(), from_others), ("hub", false));
    }

    #[test]
    fn redacts_secret_leaf_elements_only() {
        let xml = "<body><LoginUser><userName>HASH1</userName><password>HASH2</password></LoginUser>\
                   <Uid><uid>9527X</uid></Uid><name>Camera1</name><token>t</token><password/></body>";
        let out = redact(xml);
        for secret in ["HASH1", "HASH2", "9527X", "<token>t<"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(out.contains("<name>Camera1</name>"));
        assert!(out.contains("<password/>"));
        // userName outside a login is a plain user name and stays.
        assert_eq!(redact("<User><userName>admin</userName></User>"), "<User><userName>admin</userName></User>");
    }

    #[test]
    fn redacts_every_uid_bearing_element() {
        // An NVR channel list: the cameras' UIDs and the NVR's fakeUid.
        let channels = "<ChannelInfoList><ChannelInfo><channelId>0</channelId><uid>TESTCAMUID000001</uid>\
                        <fakeUid>NVRUIDPLACEHOLDR_00</fakeUid></ChannelInfo><ChannelInfo><channelId>1</channelId>\
                        <UID>TESTCAMUID000002</UID></ChannelInfo></ChannelInfoList>";
        let out = redact(channels);
        for secret in ["TESTCAMUID", "NVRUID"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        assert!(out.contains("<channelId>1</channelId>"));
        // A Home Hub file-search page and playback open: the Id ends with
        // the camera UID; the parts before it stay readable.
        let page = "<FileInfo><name>0120261017092107</name><Id>1-0-01260917062107-00000-TESTCAMUID000001</Id>\
                    <fileId>1-0-01260917062107-00000-TESTCAMUID000001</fileId></FileInfo>";
        let out = redact(page);
        assert!(!out.contains("TESTCAMUID"), "{out}");
        assert!(out.contains("<Id>1-0-01260917062107-00000-(redacted)</Id>"), "{out}");
        assert!(out.contains("<fileId>1-0-01260917062107-00000-(redacted)</fileId>"), "{out}");
        assert!(out.contains("<name>0120261017092107</name>"));
        // An all-digit UID-length part goes too; an NVR's Id, without one,
        // stays whole.
        assert_eq!(redact("<Id>1-0-1234567890123456</Id>"), "<Id>1-0-(redacted)</Id>");
        assert_eq!(redact("<Id>1-9-0-01260917041133-00000</Id>"), "<Id>1-9-0-01260917041133-00000</Id>");
        // Other ids are not recording ids.
        assert_eq!(redact("<preset><id>3</id></preset>"), "<preset><id>3</id></preset>");
    }

    #[test]
    fn summarizes_and_checks_xml() {
        let (ok, tags) = xml_summary("<?xml version=\"1.0\"?><body><A x=\"1\"><b/></A><C/><A/></body>");
        assert!(ok);
        assert_eq!(tags, vec!["A", "C"]);
        assert!(!xml_summary("<body><A></body>").0);
        assert!(!xml_summary("<body><A>").0);
        assert_eq!(xml_summary("<Extension version=\"1.1\"/>").1, vec!["Extension"]);
        assert_eq!(p2p_name("<P2P>\n<C2M_Q>\n<uid>x</uid></C2M_Q></P2P>").as_deref(), Some("C2M_Q"));
        assert_eq!(p2p_name("<P2P><R2C_DISC/></P2P>").as_deref(), Some("R2C_DISC"));
        assert_eq!(p2p_name("<Other/>").as_deref(), Some("Other"));
    }

    #[test]
    fn escapes_json_strings() {
        assert_eq!(json_escape("a\"b\\c\n\u{1}"), "a\\\"b\\\\c\\n\\u0001");
    }

    #[test]
    fn frames_like_read_bc_including_the_stray_bytes_after_class_0xffff() {
        let bc = Bc {
            meta: BcMeta { msg_id: 80, channel_id: 6, stream_type: 0, msg_num: 0, response_code: 200, class: 0x0000 },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: Some(b"<body/>".to_vec()) }),
        };
        let wire = write_bc(&bc, &EncryptionProtocol::BcEncrypt);
        let framed = frame_at(&wire, true).unwrap();
        let (_, used) = read_bc(&wire, &EncryptionProtocol::BcEncrypt, &mut HashSet::new()).unwrap().unwrap();
        assert_eq!(framed.total, used);

        let header = BcHeader {
            msg_id: MSG_ID_IMAGE_FILE,
            body_len: 0,
            channel_id: 0,
            stream_type: 0,
            msg_num: 0,
            response_code: 0xffff,
            class: 0xffff,
            payload_offset: None,
        };
        let mut wire = reoling::protocol::bc::header_codec::write_header(&header);
        wire.extend_from_slice(&[0; 4]);
        assert_eq!(frame_at(&wire, true).unwrap().total, 24);
        assert!(frame_at(b"\xf0\xde\xbc\x0a garbage that is no header", true).is_none());
    }

    /// A BC message built from its header fields, the body as given (the
    /// payload-offset word wherever Reoling's header rules put one).
    fn raw_message(msg_id: u32, class: u16, response_code: u16, ext: &[u8], payload: &[u8]) -> Vec<u8> {
        let header = BcHeader {
            msg_id,
            body_len: (ext.len() + payload.len()) as u32,
            channel_id: 0x22,
            stream_type: 0x08,
            msg_num: 0,
            response_code,
            class,
            payload_offset: header_has_payload_offset(msg_id, class, response_code).then_some(ext.len() as u32),
        };
        let mut wire = reoling::protocol::bc::header_codec::write_header(&header);
        wire.extend_from_slice(ext);
        wire.extend_from_slice(payload);
        wire
    }

    #[test]
    fn frames_recorded_media_data_with_reolings_own_header_rules() {
        // A playback whose seek used seq 1792348477: its data carry class
        // 0x6ad5 and code 0x113d, and a 24-byte header.
        const SEQ: u32 = 1_792_348_477;
        let ext = b"<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<binaryData>1</binaryData>\n</Extension>\n";
        let open_ok = raw_message(MSG_ID_REPLAY, 0x0000, 200, ext, &[0x31; 32]);
        let data1 = raw_message(MSG_ID_REPLAY, 0x6ad5, 0x113d, ext, &[0x5a; 700]);
        let data2 = raw_message(MSG_ID_REPLAY, 0x6ad5, 0x113d, ext, &[0xa5; 300]);
        let close_ok = raw_message(MSG_ID_REPLAY_CLOSE, 0x0000, 200, b"", b"");
        assert_eq!(&data1[16..20], &[0x3d, 0x11, 0xd5, 0x6a]);
        assert_eq!(&data1[20..24], &(ext.len() as u32).to_le_bytes(), "the payload-offset word");
        let framed = frame_at(&data1, true).expect("the strict search accepts recorded-media data");
        assert_eq!((framed.total, framed.header.replay_seq()), (data1.len(), Some(SEQ)));
        assert!(known_layout(&framed.header));

        // Live video with the same two words is not recorded media: 20-byte
        // header, an unknown class, refused by the strict search.
        let live = raw_message(MSG_ID_VIDEO, 0x6ad5, 0x113d, b"", &[0; 40]);
        let framed = frame_at(&live, false).unwrap();
        assert_eq!((framed.total, framed.header.payload_offset), (60, None));
        assert!(!known_layout(&framed.header) && frame_at(&live, true).is_none());

        // Back to back, the four frame one after the other.
        let mut stream = Vec::new();
        for m in [&open_ok, &data1, &data2, &close_ok] {
            stream.extend_from_slice(m);
        }
        let packets: Vec<Segment> =
            stream.chunks(400).enumerate().map(|(i, c)| seg(i as u64 + 1, 1.0 + i as f64 / 10.0, i as u32, c)).collect();
        let r = Reassembled::from_packets(&packets);
        let mut found = Vec::new();
        walk_messages(&r, |pos, framed, end| {
            found.push((pos, end, framed.header.msg_id));
            false
        });
        let a = open_ok.len();
        let b = a + data1.len();
        let c = b + data2.len();
        assert_eq!(found, vec![(0, a, 5), (a, b, 5), (b, c, 5), (c, c + close_ok.len(), 7)]);

        // Packets lost inside the first data message: the strict search
        // after the break finds the second one.
        let mut first = open_ok.clone();
        first.extend_from_slice(&data1[..50]);
        let mut third = data2.clone();
        third.extend_from_slice(&close_ok);
        let packets = vec![seg(1, 1.0, 0, &first), seg(2, 1.2, 2, &third)];
        let r = Reassembled::from_packets(&packets);
        let mut found = Vec::new();
        walk_messages(&r, |pos, framed, _| {
            found.push((pos, framed.header.msg_id));
            false
        });
        assert_eq!(found, vec![(0, 5), (first.len(), 5), (first.len() + data2.len(), 7)]);

        let passwords = Passwords::default();
        let records = decode_session(SessionInput {
            ident: Ident::Udp { session: 6, peer: "10.0.0.2:9".to_string(), path: Some("relay".to_string()) },
            conn_name: "a->b".to_string(),
            header: Vec::new(),
            streams: [Reassembled::from_packets(&[]), Reassembled::from_packets(&packets)],
            handshake_nonce: None,
            device_ips: Vec::new(),
            passwords: &passwords,
        });
        assert_eq!(records.len(), 4);
        assert!(!records[0].json.contains("\"seq\""), "{}", records[0].json);
        assert!(records[1].json.contains("\"truncated\":\"packets missing from the capture\""), "{}", records[1].json);
        assert!(
            records[2].json.contains("\"class\":\"0x6ad5\",\"body_len\":") && records[2].json.contains(",\"seq\":1792348477,\"resync\":true,"),
            "{}",
            records[2].json
        );
        assert!(records[3].json.contains("\"msg_id\":7") && !records[3].json.contains("\"seq\""), "{}", records[3].json);
    }

    /// A P2P packet with this XML, the way `classify` reads one.
    fn p2p_packet(tid: u32, xml: &str) -> Vec<u8> {
        let encrypted = xml_crypto::encrypt(tid, xml.as_bytes());
        let mut packet = Vec::new();
        for word in [MAGIC_HEADER_UDP_NEGO, encrypted.len() as u32, 1, tid, calc_crc(&encrypted)] {
            packet.extend_from_slice(&word.to_le_bytes());
        }
        packet.extend_from_slice(&encrypted);
        packet
    }

    #[test]
    fn the_side_vote_counts_only_handshake_packets_with_the_sessions_own_ids() {
        let app = ("192.168.1.2", 10560);
        let relay = ("104.0.0.20", 52864);
        let data = |connection_id: i32, payload: &[u8]| {
            write_bcudp(&BcUdp::Data(UdpData { connection_id, packet_id: 0, payload: payload.to_vec() }))
        };
        let cfm = UdpXml::D2cCfm(D2cCfm {
            sid: 1,
            conn: "relay".to_string(),
            rsp: 0,
            cid: 31001,
            did: 467,
            nc: Some("12345".to_string()),
            pl: String::new(),
            lver: 3,
        });
        let mut packets = vec![
            datagram(1, 1.0, relay, app, write_bcudp(&BcUdp::Discovery(UdpDiscovery { tid: 3, payload: cfm }))),
            datagram(2, 1.1, app, relay, data(467, b"x")),
            datagram(3, 1.2, relay, app, data(31001, b"yy")),
        ];
        // The relay passes `C2D_A` packets of other connections on to the
        // app's port: more of them than the session's own handshake has.
        for k in 0..8u32 {
            let xml = format!("<P2P>\n<C2D_A>\n<cid>{}</cid>\n<did>{}</did>\n</C2D_A>\n</P2P>\n", 580001 + k % 2, 656 - k % 2);
            packets.push(datagram(10 + u64::from(k), 2.0 + f64::from(k), relay, app, p2p_packet(7, &xml)));
        }
        let udp = build_udp(packets, None);
        assert_eq!(udp.sessions.len(), 1);
        let s = &udp.sessions[0];
        assert_eq!((s.app.port, s.peer.port), (10560, 52864), "app and relay not swapped");
        assert_eq!(s.sides_from, "p2p handshake");
        assert_eq!(s.conn_ids, [Some(467), Some(31001)]);
        assert_eq!(s.nonce.as_deref(), Some("12345"));
        assert_eq!(s.path.as_deref(), Some("relay"));
        assert_eq!((s.data[APP_TO_DEV].len(), s.data[DEV_TO_APP].len()), (1, 1));
    }
}
