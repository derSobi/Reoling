use crate::protocol::bc::model::*;
use crate::protocol::bc::xml::*;
use crate::protocol::bcmedia::model::{parse_one, BcMediaMessage};
use crate::protocol::crypto::{aes_key_from_password, EncryptionProtocol};
use crate::media_guard::{MediaGuard, Verdict};
use crate::transport::connection::BcConnection;
use crate::transport::discovery::{connect_by_uid, PeerHandle};
use crate::Error;
use md5::{Digest, Md5};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

pub use crate::protocol::bcmedia::model::{VideoFrame, VideoType};

#[derive(Debug, Clone, Default)]
pub struct DeviceInfoSummary {
    pub resolution_name: Option<String>,
}

/// Something the device told us without being asked, or as the answer to a
/// request whose reader was busy streaming.
#[derive(Debug, Clone)]
pub enum DeviceUpdate {
    Channels(Vec<ChannelInfo>),
    ChannelName { channel_id: u8, name: String },
}

/// What the device says it is, as reported after login.
#[derive(Debug, Clone, Default)]
pub struct DeviceIdentity {
    /// The name the owner gave the device.
    pub name: Option<String>,
    /// The model, e.g. a camera, NVR or Home Hub type string.
    pub model: Option<String>,
}

/// Whether a device of this name/model is an NVR or Home Hub (several
/// cameras behind one device) rather than a single camera.
pub fn looks_multi_channel(name: &str, model: &str) -> bool {
    let text = format!("{name} {model}").to_lowercase();
    // "RLN…" are Reolink's NVR model numbers.
    text.contains("nvr") || text.contains("hub") || model.to_lowercase().starts_with("rln")
}

/// How long to wait for the device to describe itself before carrying on
/// without: the name is a nicety, the video must not wait for it.
const IDENTITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Which of the device's streams to request in `start_video`. `Main` is
/// full resolution/bitrate; `Sub` is the lowest-bitrate one; `Extern` sits
/// between them on many cameras (on some it is another lens). Which of them
/// a channel really has is up to the device — see `ChannelInfo::streams`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamProfile {
    #[default]
    Main,
    Extern,
    Sub,
}

/// One channel as the device describes it (NVR / Home Hub push after login).
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_id: u8,
    pub name: String,
    pub online: bool,
    /// The streams the channel offers, highest quality first.
    pub streams: Vec<StreamProfile>,
}

/// Mirrors `transport::discovery`'s established pattern of bounding every
/// network wait explicitly (see `OVERALL_TIMEOUT`/`DNS_TIMEOUT` there) — a
/// silently-unreachable IP (the most common user mistake: right subnet,
/// wrong last octet) would otherwise hang on the OS's own SYN timeout,
/// commonly over a minute, with the connect dialog frozen on "Connecting...".
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct ReolinkClient {
    connection: Arc<Mutex<BcConnection>>,
    encryption: EncryptionProtocol,
    next_msg_num: u16,
    video_task: Option<tokio::task::JoinHandle<()>>,
    /// The stream `start_video` opened, so `stop_video` can name it.
    playing: Option<PlayingStream>,
    /// Keeps a direct (non-relay) connection alive — see
    /// `BcConnection::spawn_direct_keepalive`. `None` on a relay connection
    /// or one built via `from_connection`. Held so the handle isn't
    /// dropped (which would just detach the task, not stop it — it keeps
    /// running for the process's lifetime regardless); never polled
    /// directly.
    direct_keepalive_task: Option<tokio::task::JoinHandle<()>>,
    /// Channel lists the device pushes (NVR / Home Hub) whenever it likes —
    /// during `identity`, while starting video, or mid-stream.
    channel_updates_tx: tokio::sync::mpsc::UnboundedSender<DeviceUpdate>,
    channel_updates_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>>,
}

#[derive(Debug, Clone)]
struct PlayingStream {
    channel_id: u8,
    handle: u32,
    stream_name: &'static str,
    bc_stream_type: u8,
}

/// What `bc` tells us, if it is a pushed channel list or the answer to a
/// channel-name request.
fn pushed_update(bc: &Bc) -> Option<DeviceUpdate> {
    if bc.meta.msg_id != MSG_ID_CHANNEL_INFO && bc.meta.msg_id != MSG_ID_OSD {
        return None;
    }
    if bc.meta.msg_id == MSG_ID_OSD && std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
        let body = match &bc.body {
            BcBody::Modern(ModernMsg { payload: Some(p), .. }) => String::from_utf8_lossy(p)
                .chars()
                .take(1500)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect::<String>(),
            other => format!("{other:?}"),
        };
        eprintln!(
            "DEBUG osd reply: channel={} code={} body={body}",
            bc.meta.channel_id, bc.meta.response_code
        );
    }
    let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = &bc.body else {
        return None;
    };
    let xml = BcXml::from_bytes(payload).ok()?;
    if let Some(list) = xml.channel_info_list {
        let channels = list.into_channels();
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG channel list pushed: {channels:?}");
        }
        return Some(DeviceUpdate::Channels(channels));
    }
    let name = xml.osd_channel_name?.name?.trim().to_string();
    let channel_id = extension_xml
        .as_deref()
        .and_then(|e| Extension::from_bytes(e).ok())
        .and_then(|e| e.channel_id)
        .unwrap_or(bc.meta.channel_id);
    Some(DeviceUpdate::ChannelName { channel_id, name })
}

/// The camera truncates the hex MD5 digest of `input` to 31 characters
/// (uppercase) before comparing it — a quirk inherited from a fixed-size C
/// buffer in the original firmware. Both username and password must be
/// hashed this way for the modern login step.
fn md5_hex_truncated(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02X}")).collect::<String>();
    hex[..31].to_string()
}

impl ReolinkClient {
    /// Resolves `uid` over Reolink's P2P infrastructure and opens the data
    /// connection. Does not log in yet — call `login` next.
    ///
    /// Races every connect candidate the register server offers (LAN-local
    /// direct, NAT-external "map", and relay) simultaneously and uses
    /// whichever answers first — see `transport::discovery::connect_race`
    /// for the real-capture evidence this mirrors exactly, and
    /// `transport::discovery::connect_by_uid`'s doc comment for how this
    /// replaced an earlier, disproven "skip direct whenever relay is
    /// available" strategy. Still strictly UDP/P2P — no TCP.
    pub async fn connect_by_uid(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle = connect_by_uid(&socket, uid).await?;
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Diagnostic-only, not used by the app: identical to `connect_by_uid`,
    /// but prefers the direct P2P path over relay whenever the register
    /// server gave us any device address at all. See
    /// `transport::discovery::connect_by_uid_prefer_direct`.
    pub async fn connect_by_uid_prefer_direct(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle =
            crate::transport::discovery::connect_by_uid_prefer_direct(&socket, uid).await?;
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Diagnostic-only, never the app's default — see
    /// `project-udp-only-scope`: the official Reolink app never uses TCP
    /// for Baichuan except as an extreme last resort, and this project
    /// only targets UDP/P2P. Resolves `uid` the same way
    /// `connect_by_uid_prefer_direct` does (bounded to a short timeout for
    /// the direct attempt specifically — see
    /// `transport::discovery::connect_by_uid_prefer_direct_bounded`), but
    /// when that resolution reaches the device directly (not through
    /// relay), continues the session over TCP:9000 (`connect_by_ip`)
    /// instead of the UDP/P2P transport, purely to isolate whether a given
    /// symptom is transport-layer or not. `.plans/reolink-protocols.md`'s
    /// claim that Baichuan TCP is the NVR/powered-camera family's
    /// "primary" protocol is wrong (reverse-engineered, unverified,
    /// contradicted by real capture evidence) — don't cite it. Falls back
    /// to the UDP session if the device only resolved via relay, or if the
    /// direct TCP connect itself fails.
    pub async fn connect_by_uid_prefer_tcp(uid: &str) -> crate::Result<Self> {
        let socket = Arc::new(crate::transport::bind_udp_socket_with_large_rcvbuf().await?);
        let peer: PeerHandle =
            crate::transport::discovery::connect_by_uid_prefer_direct_bounded(&socket, uid)
                .await?;
        if peer.is_direct {
            const BASIC_SERVICE_PORT: u16 = 9000;
            if let Ok(client) = Self::connect_by_ip(peer.addr.ip(), BASIC_SERVICE_PORT).await {
                return Ok(client);
            }
        }
        let connection = BcConnection::new(socket, peer);
        let keepalive = connection.spawn_direct_keepalive();
        let mut client = Self::from_connection(connection, EncryptionProtocol::Unencrypted);
        client.direct_keepalive_task = keepalive;
        Ok(client)
    }

    /// Connects directly to a device's Baichuan "Basic Service" TCP port
    /// (typically 9000) by IP — the explicit alternative to
    /// `connect_by_uid`'s P2P path, chosen by the user, never a fallback.
    /// No P2P handshake, no relay, no direct-connect keepalive (`C2D_HB`
    /// has no meaning on a stable TCP connection — see
    /// `BcConnection::spawn_direct_keepalive`).
    pub async fn connect_by_ip(ip: std::net::IpAddr, port: u16) -> crate::Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((ip, port)))
            .await
            .map_err(|_| Error::ProtocolError(format!("connecting to {ip}:{port} timed out")))?
            .map_err(|e| Error::ProtocolError(format!("could not connect to {ip}:{port}: {e}")))?;
        let connection = BcConnection::from_tcp(stream);
        Ok(Self::from_connection(connection, EncryptionProtocol::Unencrypted))
    }

    /// Test/advanced entry point: wraps an already-established
    /// `BcConnection` (used directly in tests to skip P2P discovery, which
    /// is covered separately in `transport::discovery`'s own tests).
    pub fn from_connection(connection: BcConnection, encryption: EncryptionProtocol) -> Self {
        let (channel_updates_tx, channel_updates_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            connection: Arc::new(Mutex::new(connection)),
            encryption,
            next_msg_num: 1,
            video_task: None,
            playing: None,
            direct_keepalive_task: None,
            channel_updates_tx,
            channel_updates_rx: Some(channel_updates_rx),
        }
    }

    /// The channel lists this device pushes, as they arrive. Can be taken
    /// once.
    pub fn take_channel_updates(
        &mut self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<DeviceUpdate>> {
        self.channel_updates_rx.take()
    }

    /// Whether the P2P handshake delivered a relay login nonce (see
    /// `PeerHandle::nonce`) — exposed for diagnostics only, to tell apart
    /// "no nonce was available, `login` used the legacy fallback" from
    /// "a nonce was available and something else went wrong".
    pub async fn has_relay_nonce(&self) -> bool {
        self.connection.lock().await.peer_nonce().is_some()
    }

    /// Diagnostic-only: which address this connection is actually talking
    /// to — lets a caller tell a direct connection apart from a relay one.
    pub async fn peer_addr(&self) -> std::net::SocketAddr {
        self.connection.lock().await.peer_addr()
    }

    fn next_msg_num(&mut self) -> u16 {
        let n = self.next_msg_num;
        self.next_msg_num = self.next_msg_num.wrapping_add(1);
        n
    }

    /// Login is a host-level operation, not a per-channel one — confirmed
    /// 2026-09-13 against a real TCP:9000 capture of the official Windows
    /// client logging into a Home Hub Pro NVR
    /// (`.plans/docu/Wireshark/Reolink-ALL-Login_only_with_Home_Hub_Pro.pcapng`):
    /// both the legacy `LoginUpgrade` and the modern `LoginUser` request
    /// carry `channel_id: 0` in their `BcMeta`, even though the session
    /// goes on to stream a non-zero camera channel afterward. This
    /// contradicts an earlier reading of neolink's `BcCameraOpt::channel_id`
    /// doc comment ("Channel the camera is on 0 unless using a NVR"), which
    /// led this code to thread the target camera's channel into login's
    /// `BcMeta` too (see bug #13/#14 in `.plans/reolink-linux-project.md`) — real
    /// capture evidence for the exact device class this project targets
    /// overrides that inference. `channel_id` is passed only to
    /// `start_video`/`stop_video`, which is where the real client's own
    /// per-channel routing (`Extension`/`channelId` XML, wire `channel+1`)
    /// actually happens.
    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
    ) -> crate::Result<DeviceInfoSummary> {
        let channel_id = 0;
        let msg_num = self.next_msg_num();

        let relay_nonce = self.connection.lock().await.peer_nonce().map(str::to_string);
        let nonce = if let Some(nonce) = relay_nonce {
            // Confirmed 2026-09-13 against a real capture: over the relay
            // path, the real client never performs the legacy
            // login/nonce-reply BC exchange below at all — the nonce
            // already arrived in the relay handshake's D2C_CFM (see
            // `PeerHandle::nonce`). Sending the legacy step anyway is what
            // was causing the device to tear the session down with
            // D2C_DISC right after connecting.
            nonce
        } else {
            let legacy_login = Bc {
                meta: BcMeta {
                    msg_id: MSG_ID_LOGIN,
                    channel_id,
                    stream_type: 0,
                    msg_num,
                    // 0xdc02 was our own invention (bug #12): we assumed
                    // it meant "control-channel-only AES", to keep the
                    // video feed unencrypted for our GStreamer pipeline
                    // (which expects raw H.264). That premise was wrong on
                    // two counts, found reading (not copying) both
                    // `neolink` and `bairelay`, 2026-09-13: (1) neither
                    // reference ever sends anything but 0xdc00/0xdc01/
                    // 0xdc12 here — 0xdc02 isn't a real, recognized value,
                    // and a real NVR (Hub Pro) was observed transport-ACKing
                    // this exact legacy login message and then immediately
                    // `D2C_DISC`-ing with no application-level reply at
                    // all, on both the direct and relay paths, across every
                    // other field we varied — consistent with the device
                    // rejecting an unrecognized request outright rather
                    // than degrading it; (2) neolink's own doc comment says
                    // "the reolink camera only encrypt the control
                    // messages[;] the camera feed is always accessible" —
                    // i.e. this byte was never the video-encryption knob we
                    // thought it was, so there was never a reason to ask
                    // for anything less than full AES here. Now requesting
                    // 0xdc12 (Aes), matching both references exactly.
                    response_code: 0xdc12,
                    class: 0x6514,
                },
                body: BcBody::Legacy(LegacyMsg::LoginUpgrade),
            };
            self.connection.lock().await.send_bc(&legacy_login, &self.encryption).await?;
            let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
            let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
                return Err(Error::ProtocolError("expected an Encryption reply".to_string()));
            };
            BcXml::from_bytes(&payload)?
                .encryption
                .ok_or_else(|| Error::ProtocolError("missing nonce in Encryption reply".to_string()))?
                .nonce
        };

        self.encryption = EncryptionProtocol::Aes {
            key: aes_key_from_password(password, &nonce),
        };

        let modern_login = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        login_user: Some(LoginUser {
                            version: XML_VERSION.to_string(),
                            user_name: md5_hex_truncated(&format!("{username}{nonce}")),
                            password: md5_hex_truncated(&format!("{password}{nonce}")),
                            user_ver: 1,
                        }),
                        login_net: Some(LoginNet::default()),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        self.connection.lock().await.send_bc(&modern_login, &self.encryption).await?;
        let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
        if reply.meta.response_code != 200 {
            return Err(Error::LoginFailed { code: reply.meta.response_code });
        }
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
            return Err(Error::ProtocolError("expected a DeviceInfo reply".to_string()));
        };
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        Ok(DeviceInfoSummary {
            resolution_name: device_info.resolution.and_then(|r| r.name),
        })
    }

    async fn send_empty_request(&mut self, msg_id: u32) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Asks the logged-in device for its name and model. Best effort: any
    /// failure or silence yields an empty identity rather than an error.
    pub async fn identity(&mut self) -> DeviceIdentity {
        // What the official app sends first after login (seen in a capture
        // against a Home Hub, and confirmed by a probe: the Hub pushes its
        // channel list, with the channels' names, only after these two).
        // Answers are picked up as they come.
        let _ = self.send_empty_request(MSG_ID_STREAM_INFO).await;
        let _ = self.send_empty_request(MSG_ID_SUBSCRIBE).await;
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VERSION,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        let mut info: Option<VersionInfo> = None;
        let exchange = async {
            if self.connection.lock().await.send_bc(&request, &self.encryption).await.is_err() {
                return;
            }
            for _ in 0..16 {
                let Ok(reply) = self.connection.lock().await.recv_bc(&self.encryption).await
                else {
                    return;
                };
                if let Some(update) = pushed_update(&reply) {
                    let _ = self.channel_updates_tx.send(update);
                    continue;
                }
                if reply.meta.msg_id != MSG_ID_VERSION {
                    continue;
                }
                if let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body {
                    info = BcXml::from_bytes(&payload).ok().and_then(|x| x.version_info);
                }
                return;
            }
        };
        let _ = tokio::time::timeout(IDENTITY_TIMEOUT, exchange).await;
        let (name, model) = info.map(|i| (i.name, i.model)).unwrap_or_default();
        if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
            eprintln!("DEBUG identity reply: name={name:?} model={model:?}");
        }
        DeviceIdentity { name, model }
    }

    /// Diagnostic-only (`REOLING_PROBE_PUSH`): sends the empty requests the
    /// official app sends right after login (192 and 146, seen in a capture
    /// against a Home Hub, which then pushed its channel list) and prints
    /// every reply, decrypted, for a few seconds.
    pub async fn probe_pushes(&mut self) {
        for msg_id in [192u32, 146] {
            let msg_num = self.next_msg_num();
            let request = Bc {
                meta: BcMeta {
                    msg_id,
                    channel_id: 0,
                    stream_type: 0,
                    msg_num,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
            };
            let sent = self.connection.lock().await.send_bc(&request, &self.encryption).await;
            eprintln!("PROBE sent {msg_id}: {sent:?}");
        }
        let listen = async {
            loop {
                let Ok(bc) = self.connection.lock().await.recv_bc(&self.encryption).await else {
                    return;
                };
                let text = match &bc.body {
                    BcBody::Modern(ModernMsg { payload: Some(p), .. }) => {
                        let t = String::from_utf8_lossy(p);
                        t.chars().take(6000).map(|c| if c == '\n' { ' ' } else { c }).collect::<String>()
                    }
                    other => format!("{other:?}").chars().take(200).collect(),
                };
                eprintln!(
                    "PROBE msg_id={} code={} num={} body={}",
                    bc.meta.msg_id, bc.meta.response_code, bc.meta.msg_num, text
                );
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), listen).await;
    }

    /// Diagnostic-only, not used by `login`: sends only the modern
    /// `LoginUser` with an empty nonce, skipping the legacy step
    /// unconditionally (regardless of `peer_nonce`) and matching the real
    /// captured client's exact message shape (`class=0x0000`, `msg_num=0`)
    /// byte-for-byte. Exists to test, against real hardware, whether a
    /// device that disconnects during `login`'s legacy step will accept a
    /// login with no nonce at all — isolating "device dislikes our login
    /// message" from "device dislikes our session" before investigating
    /// further. Delete once that question is answered.
    pub async fn login_probe_empty_nonce(
        &mut self,
        username: &str,
        password: &str,
    ) -> crate::Result<DeviceInfoSummary> {
        let nonce = String::new();
        self.encryption = EncryptionProtocol::Aes {
            key: aes_key_from_password(password, &nonce),
        };
        let modern_login = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: 0,
                response_code: 0,
                class: 0x0000,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        login_user: Some(LoginUser {
                            version: XML_VERSION.to_string(),
                            user_name: md5_hex_truncated(&format!("{username}{nonce}")),
                            password: md5_hex_truncated(&format!("{password}{nonce}")),
                            user_ver: 1,
                        }),
                        login_net: Some(LoginNet::default()),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        self.connection.lock().await.send_bc(&modern_login, &self.encryption).await?;
        let reply = self.connection.lock().await.recv_bc(&self.encryption).await?;
        if reply.meta.response_code != 200 {
            return Err(Error::LoginFailed { code: reply.meta.response_code });
        }
        let BcBody::Modern(ModernMsg { payload: Some(payload), .. }) = reply.body else {
            return Err(Error::ProtocolError("expected a DeviceInfo reply".to_string()));
        };
        let device_info = BcXml::from_bytes(&payload)?.device_info.unwrap_or_default();
        Ok(DeviceInfoSummary {
            resolution_name: device_info.resolution.and_then(|r| r.name),
        })
    }

    pub async fn start_video(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        self.start_video_with_trace(channel_id, quality, None).await
    }

    /// Diagnostic variant: records every decrypted video `Bc` message
    /// (with its arrival timestamp) before `BcMedia` parsing, via
    /// `media_trace::MediaTrace`. The normal application (and `start_video`
    /// above) passes `None` and pays no extra cost.
    pub async fn start_video_with_trace(
        &mut self,
        channel_id: u8,
        quality: StreamProfile,
        trace: Option<Arc<crate::media_trace::MediaTrace>>,
    ) -> crate::Result<ReceiverStream<crate::Result<VideoFrame>>> {
        let msg_num = self.next_msg_num();
        // handle/stream_type per quality. `Main`'s values (handle 0,
        // BcMeta.stream_type 0, XML streamType "mainStream") come straight
        // from our own real capture and are not in question.
        //
        // `Sub`'s values (handle 256, BcMeta.stream_type 1, XML streamType
        // "subStream") are NOT independently confirmed against our own
        // Wireshark capture — that capture only covers Main-stream Preview
        // usage. They're inferred from cross-checking reference
        // implementations (neolink, bairelay) per the 2026-09-16 audit
        // (section 21). Treat as unverified until confirmed against real
        // hardware requesting a Sub stream.
        let (handle, stream_type_str, bc_meta_stream_type) = match quality {
            StreamProfile::Main => (0, "mainStream", 0),
            StreamProfile::Sub => (256, "subStream", 1),
            StreamProfile::Extern => (1024, "externStream", 0),
        };
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id,
                stream_type: bc_meta_stream_type,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        preview: Some(Preview {
                            version: XML_VERSION.to_string(),
                            channel_id,
                            handle,
                            stream_type: Some(stream_type_str.to_string()),
                        }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        // DEBUG-ONLY, temporary (`REOLING_DEBUG_NEGOTIATION`): our own
        // plaintext Preview request and the camera's reply, to compare
        // against the official client's for a negotiated-parameter mismatch
        // that could explain a lower camera bitrate. No credentials here.
        let debug_negotiation = std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok();
        if debug_negotiation {
            if let BcBody::Modern(ModernMsg { payload: Some(p), .. }) = &request.body {
                eprintln!(
                    "NEGOTIATION preview request: channel_id={} stream_type={} class={:#x} xml={}",
                    request.meta.channel_id,
                    request.meta.stream_type,
                    request.meta.class,
                    String::from_utf8_lossy(p)
                );
            }
        }
        self.playing = Some(PlayingStream {
            channel_id,
            handle,
            stream_name: stream_type_str,
            bc_stream_type: bc_meta_stream_type,
        });
        self.connection.lock().await.send_bc(&request, &self.encryption).await?;
        // Pushes (channel lists, alarm events, ...) can come between our
        // request and the answer; they are not the answer.
        let ack = loop {
            let bc = self.connection.lock().await.recv_bc(&self.encryption).await?;
            if let Some(update) = pushed_update(&bc) {
                let _ = self.channel_updates_tx.send(update);
            } else if bc.meta.msg_id == MSG_ID_VIDEO && bc.meta.msg_num == msg_num {
                // Same message number as our request: an earlier stream's
                // leftover frames carry theirs.
                break bc;
            }
        };
        if debug_negotiation {
            eprintln!(
                "NEGOTIATION preview reply: response_code={} class={:#x} body={:?}",
                ack.meta.response_code, ack.meta.class, ack.body
            );
        }
        if ack.meta.response_code != 200 {
            return Err(Error::ProtocolError(format!(
                "camera rejected the video stream request with code {}",
                ack.meta.response_code
            )));
        }

        let (tx, rx) = channel(32);
        let connection = Arc::clone(&self.connection);
        let encryption = self.encryption.clone();
        let channel_updates = self.channel_updates_tx.clone();
        self.video_task = Some(tokio::spawn(async move {
            let mut buffer: Vec<u8> = Vec::new();
            let mut guard = MediaGuard::default();
            loop {
                let bc = {
                    let mut conn = connection.lock().await;
                    match conn.recv_bc(&encryption).await {
                        Ok(bc) => bc,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                };
                if let Some(update) = pushed_update(&bc) {
                    let _ = channel_updates.send(update);
                    continue;
                }
                if bc.meta.msg_id != MSG_ID_VIDEO {
                    continue;
                }
                if let Some(trace) = &trace {
                    trace.record(&bc);
                }
                let BcBody::Modern(ModernMsg { extension_xml, payload: Some(payload) }) = bc.body
                else {
                    continue;
                };
                let extension =
                    extension_xml.as_deref().and_then(|xml| Extension::from_bytes(xml).ok());
                match guard.judge(extension.as_ref(), &payload) {
                    Verdict::Accept => {}
                    Verdict::Skip => continue,
                    Verdict::Damaged => {
                        // The partial unit being assembled is damaged too.
                        buffer.clear();
                        continue;
                    }
                }
                buffer.extend_from_slice(&payload);
                loop {
                    match parse_one(&buffer) {
                        Ok(Some((msg, used))) => {
                            buffer.drain(..used);
                            if let BcMediaMessage::Info { width, height } = &msg {
                                if std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok() {
                                    eprintln!("NEGOTIATION stream info unit: {width}x{height}");
                                }
                            }
                            if let BcMediaMessage::Video(frame) = msg {
                                if tx.send(Ok(frame)).await.is_err() {
                                    return; // receiver dropped
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
            }
        }));
        Ok(ReceiverStream::new(rx))
    }

    /// Stops the stream `start_video` opened (or, if none is known, whatever
    /// `channel_id` is sending), naming it the way the official app does.
    pub async fn stop_video(&mut self, channel_id: u8) -> crate::Result<()> {
        if let Some(task) = self.video_task.take() {
            task.abort();
        }
        let playing = self.playing.take();
        let msg_num = self.next_msg_num();
        let payload = playing.as_ref().map(|p| {
            BcXml {
                preview: Some(Preview {
                    version: XML_VERSION.to_string(),
                    channel_id: p.channel_id,
                    handle: p.handle,
                    stream_type: Some(p.stream_name.to_string()),
                }),
                ..Default::default()
            }
            .to_bytes()
        });
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO_STOP,
                channel_id: playing.as_ref().map_or(channel_id, |p| p.channel_id),
                stream_type: playing.as_ref().map_or(0, |p| p.bc_stream_type),
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Asks for the names of these channels. The answers come back through
    /// `take_channel_updates` as `DeviceUpdate::ChannelName`.
    pub async fn request_channel_names(&mut self, channel_ids: &[u8]) {
        for &channel_id in channel_ids {
            let msg_num = self.next_msg_num();
            let extension = Extension {
                version: XML_VERSION.to_string(),
                channel_id: Some(channel_id),
                ..Default::default()
            };
            let request = Bc {
                meta: BcMeta {
                    msg_id: MSG_ID_OSD,
                    channel_id,
                    stream_type: 0,
                    msg_num,
                    response_code: 0,
                    class: 0x6414,
                },
                body: BcBody::Modern(ModernMsg {
                    extension_xml: Some(extension.to_bytes()),
                    payload: None,
                }),
            };
            let _ = self.connection.lock().await.send_bc(&request, &self.encryption).await;
        }
    }

    /// Reads whatever the device sends while no video is running (pushed
    /// channel lists, alarm events), forwarding channel lists to
    /// `take_channel_updates`. Returns only when the connection fails.
    /// Cancel-safe: partial messages are kept by the connection.
    pub async fn wait_for_pushes(&mut self) -> crate::Error {
        loop {
            let received = self.connection.lock().await.recv_bc(&self.encryption).await;
            match received {
                Ok(bc) => {
                    if let Some(update) = pushed_update(&bc) {
                        let _ = self.channel_updates_tx.send(update);
                    }
                }
                Err(Error::ReplyTimeout) => {}
                Err(e) => return e,
            }
        }
    }

    pub async fn logout(&mut self) -> crate::Result<()> {
        let msg_num = self.next_msg_num();
        let request = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGOUT,
                channel_id: 0,
                stream_type: 0,
                msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        self.connection.lock().await.send_bc(&request, &self.encryption).await
    }

    /// Tells the device this session is ending, so it stops streaming
    /// immediately instead of continuing until its own idle timeout — see
    /// `BcConnection::disconnect`'s doc comment. Confirmed real
    /// 2026-09-16: closing the official Windows app makes the camera stop
    /// sending right away; closing this app previously left it sending
    /// until its own TTL expired, because nothing was ever called here.
    /// Callers should invoke this before dropping the client (or the
    /// window that owns it) rather than just letting it fall out of
    /// scope.
    ///
    /// Order matters here: `video_task` (still running, looping on
    /// `connection.lock()` for every frame) is stopped *first* so it
    /// can't keep re-acquiring the lock ahead of this call; the P2P-level
    /// `C2D_DISC` — the one that actually matters for this symptom — goes
    /// out next; `logout`'s BC-level message is best-effort and last,
    /// since it can fail (e.g. the connection is already half-gone)
    /// without that being a reason to skip or delay the disconnect that
    /// does matter.
    pub async fn disconnect(&mut self) {
        if let Some(task) = self.video_task.take() {
            task.abort();
        }
        self.connection.lock().await.disconnect().await;
        let _ = self.logout().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;
    use tokio_stream::StreamExt;

    /// Drives a `BcConnection` as if it were the camera: legacy login,
    /// modern login, then a Preview ack followed by one video Iframe.
    async fn run_fake_camera(mut conn: BcConnection) {
        // 1. Legacy login upgrade -> reply with nonce, unencrypted for simplicity.
        let legacy = conn.recv_bc(&EncryptionProtocol::Unencrypted).await.unwrap();
        assert_eq!(legacy.meta.msg_id, MSG_ID_LOGIN);
        let nonce_reply = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: legacy.meta.msg_num,
                response_code: 0,
                class: 0x6614,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        encryption: Some(Encryption {
                            version: XML_VERSION.to_string(),
                            type_: "md5".to_string(),
                            nonce: "TESTNONCE".to_string(),
                        }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        conn.send_bc(&nonce_reply, &EncryptionProtocol::Unencrypted).await.unwrap();

        // From here on the real camera (and our client) switches to AES,
        // keyed from the password and the nonce just exchanged.
        let enc = EncryptionProtocol::Aes {
            key: aes_key_from_password("swordfish", "TESTNONCE"),
        };

        // 2. Modern login -> reply with DeviceInfo, response_code 200.
        let modern = conn.recv_bc(&enc).await.unwrap();
        assert_eq!(modern.meta.msg_id, MSG_ID_LOGIN);
        let device_info_reply = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_LOGIN,
                channel_id: 0,
                stream_type: 0,
                msg_num: modern.meta.msg_num,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: None,
                payload: Some(
                    BcXml {
                        device_info: Some(DeviceInfo { version: None, resolution: None }),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
            }),
        };
        conn.send_bc(&device_info_reply, &enc).await.unwrap();

        // 3. Preview request -> ack (response_code 200, empty body).
        let preview = conn.recv_bc(&enc).await.unwrap();
        assert_eq!(preview.meta.msg_id, MSG_ID_VIDEO);
        let ack = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 200,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg { extension_xml: None, payload: None }),
        };
        conn.send_bc(&ack, &enc).await.unwrap();

        // 4. One video data message: Extension says binary, payload is a raw
        // bcmedia Iframe unit.
        let mut media_bytes = Vec::new();
        media_bytes.extend_from_slice(&0x63643030u32.to_le_bytes()); // Iframe magic
        media_bytes.extend_from_slice(b"H264");
        let frame_data = vec![0, 0, 0, 1, 0x67];
        media_bytes.extend_from_slice(&(frame_data.len() as u32).to_le_bytes());
        media_bytes.extend_from_slice(&0u32.to_le_bytes());
        media_bytes.extend_from_slice(&999u32.to_le_bytes());
        media_bytes.extend_from_slice(&0u32.to_le_bytes());
        media_bytes.extend_from_slice(&frame_data);
        let pad = (8 - frame_data.len() % 8) % 8;
        media_bytes.extend(std::iter::repeat(0u8).take(pad));

        let video_data = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(
                    Extension {
                        version: XML_VERSION.to_string(),
                        binary_data: Some(1),
                        channel_id: Some(0),
                        encrypt_len: Some(media_bytes.len() as u32),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
                payload: Some(media_bytes),
            }),
        };
        conn.send_bc(&video_data, &enc).await.unwrap();

        // 5. A second Iframe on the same connection, to prove start_video
        // keeps yielding frames without a second start_video call.
        let mut media_bytes_2 = Vec::new();
        media_bytes_2.extend_from_slice(&0x63643030u32.to_le_bytes()); // Iframe magic
        media_bytes_2.extend_from_slice(b"H264");
        let frame_data_2 = vec![9, 9, 9];
        media_bytes_2.extend_from_slice(&(frame_data_2.len() as u32).to_le_bytes());
        media_bytes_2.extend_from_slice(&0u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&1000u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&0u32.to_le_bytes());
        media_bytes_2.extend_from_slice(&frame_data_2);
        let pad_2 = (8 - frame_data_2.len() % 8) % 8;
        media_bytes_2.extend(std::iter::repeat(0u8).take(pad_2));
        let video_data_2 = Bc {
            meta: BcMeta {
                msg_id: MSG_ID_VIDEO,
                channel_id: 0,
                stream_type: 0,
                msg_num: preview.meta.msg_num,
                response_code: 0,
                class: 0x6414,
            },
            body: BcBody::Modern(ModernMsg {
                extension_xml: Some(
                    Extension {
                        version: XML_VERSION.to_string(),
                        binary_data: Some(1),
                        channel_id: Some(0),
                        encrypt_len: Some(media_bytes_2.len() as u32),
                        ..Default::default()
                    }
                    .to_bytes(),
                ),
                payload: Some(media_bytes_2),
            }),
        };
        conn.send_bc(&video_data_2, &enc).await.unwrap();
    }

    #[tokio::test]
    async fn login_and_start_video_against_a_fake_camera() {
        let client_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let camera_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let client_addr = client_socket.local_addr().unwrap();
        let camera_addr = camera_socket.local_addr().unwrap();

        let camera_conn = BcConnection::new(
            camera_socket,
            PeerHandle { addr: client_addr, local_connection_id: 2, remote_connection_id: 1, nonce: None, is_direct: false },
        );
        let fake_camera = tokio::spawn(run_fake_camera(camera_conn));

        let client_conn = BcConnection::new(
            client_socket,
            PeerHandle { addr: camera_addr, local_connection_id: 1, remote_connection_id: 2, nonce: None, is_direct: false },
        );
        let mut client = ReolinkClient::from_connection(client_conn, EncryptionProtocol::Unencrypted);

        let _device_info = client.login("admin", "swordfish").await.unwrap();

        let mut frames = client.start_video(0, StreamProfile::Main).await.unwrap();
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.data, vec![0, 0, 0, 1, 0x67]);
        assert_eq!(frame.microseconds, 999);

        // The background reader task keeps yielding frames without a
        // second start_video call — this is Task 18's whole point.
        let second = frames.next().await.unwrap().unwrap();
        assert_eq!(second.data, vec![9, 9, 9]);
        assert_eq!(second.microseconds, 1000);

        fake_camera.await.unwrap();
    }

    #[tokio::test]
    async fn login_and_start_video_against_a_fake_tcp_camera() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let fake_camera = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_fake_camera(BcConnection::from_tcp(stream)).await;
        });

        let mut client = ReolinkClient::connect_by_ip(addr.ip(), addr.port()).await.unwrap();

        let _device_info = client.login("admin", "swordfish").await.unwrap();

        let mut frames = client.start_video(0, StreamProfile::Main).await.unwrap();
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.data, vec![0, 0, 0, 1, 0x67]);
        assert_eq!(frame.microseconds, 999);

        fake_camera.await.unwrap();
    }
}
