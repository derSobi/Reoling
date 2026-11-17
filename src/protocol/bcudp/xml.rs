use serde::{Deserialize, Serialize};

/// All UDP discovery XML is wrapped in a `<P2P>` root element with exactly
/// one child describing the message; this enum models the child and the
/// wrapper is handled in `to_bytes`/`from_bytes` below.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename = "P2P")]
pub enum UdpXml {
    #[serde(rename = "C2M_Q")]
    C2mQ(C2mQ),
    #[serde(rename = "M2C_Q_R")]
    M2cQr(M2cQr),
    #[serde(rename = "C2R_C")]
    C2rC(C2rC),
    #[serde(rename = "R2C_C_R")]
    R2cCr(R2cCr),
    #[serde(rename = "R2C_T")]
    R2cT(R2cT),
    #[serde(rename = "C2D_C")]
    C2dC(C2dC),
    #[serde(rename = "D2C_C_R")]
    D2cCr(D2cCr),
    #[serde(rename = "C2D_T")]
    C2dT(C2dT),
    #[serde(rename = "D2C_CFM")]
    D2cCfm(D2cCfm),
    #[serde(rename = "C2R_CFM")]
    C2rCfm(C2rCfm),
    /// Register server's unsolicited notice that a session it was tracking
    /// (identified by `sid`) has been torn down — observed in the real
    /// protocol (2026-09-13) sent to the register address itself, roughly
    /// 13s after a `C2D_C` direct-connect attempt got no reply from the
    /// device, i.e. it signals "give up waiting on this session", not a
    /// relay handoff: it carries no relay/device address of its own.
    #[serde(rename = "R2C_DISC")]
    R2cDisc(R2cDisc),
    /// Client's own disconnect notice, sent to the relay/device data-channel
    /// address — observed in the real protocol (2026-09-13) right after a
    /// login attempt was rejected with 401, tearing the session down before
    /// the client reconnects fresh. `BcConnection::recv_bc` ignores this if
    /// it ever arrives back at us (it's a client->device message).
    #[serde(rename = "C2D_DISC")]
    C2dDisc(C2dDisc),
    /// Client's notice to the register server that it abandons a registered session: the official app sends it
    /// three times, 0.5 s apart, when an attempt goes silent before its login completes (our captures).
    #[serde(rename = "C2R_DISC")]
    C2rDisc(C2rDisc),
    /// Device's disconnect notice, symmetric to `C2dDisc` — observed in the
    /// real protocol (2026-09-13) both at ordinary end-of-session teardown
    /// and (in our own testing) right after we sent an extra legacy login
    /// step the device didn't expect on an already-established relay
    /// session. `BcConnection::recv_bc` turns this into a clear error
    /// instead of the generic parse failure it used to cause.
    #[serde(rename = "D2C_DISC")]
    D2cDisc(D2cDisc),
    /// Client-to-device heartbeat, sent to the device's address on the
    /// direct (non-relay) path. Our own real captures never show it (we
    /// never sent one), so its field shape is unverified against a capture.
    /// Without it, real hardware just keeps retransmitting its
    /// `D2C_C_R` every ~500ms and never processes any BC data we send —
    /// confirmed 2026-09-13 against a real direct connection. Sent
    /// periodically for the life of a direct connection; see
    /// `BcConnection::spawn_direct_keepalive`.
    #[serde(rename = "C2D_HB")]
    C2dHb(C2dHb),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
struct P2PWrapper {
    #[serde(rename = "$value")]
    xml: UdpXml,
}

impl UdpXml {
    pub fn to_bytes(&self) -> Vec<u8> {
        let inner = quick_xml::se::to_string(self).expect("UdpXml always serializes");
        format!("<P2P>{inner}</P2P>").into_bytes()
    }

    pub fn from_bytes(buf: &[u8]) -> crate::protocol::Result<Self> {
        let wrapper: P2PWrapper = quick_xml::de::from_reader(buf)?;
        Ok(wrapper.xml)
    }
}

/// Client-to-middleman UID lookup, sent to `p2p*.reolink.com:9999`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2mQ {
    pub uid: String,
    /// Protocol/client version. Missing from earlier drafts of this struct
    /// (found via real packet capture against the official Windows client,
    /// 2026-09-13): most relays silently ignore a C2M_Q with no `ver`,
    /// which is why only a minority ever replied at all before this fix.
    pub ver: u32,
    #[serde(rename = "p")]
    pub os: String,
}

/// Middleman's reply: the register and relay server addresses for this UID.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct M2cQr {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reg: Option<IpPort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<IpPort>,
    /// A number the official app echoes in its `C2R_DISC` (our captures). Kept as text: refusing answers carry
    /// an empty or non-numeric one, which must not fail the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct IpPort {
    pub ip: String,
    pub port: u16,
}

/// Client-to-register: "register my address for this UID and give me the
/// device's address if you have it".
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2rC {
    pub uid: String,
    pub cli: IpPort,
    pub relay: IpPort,
    pub cid: i32,
    pub debug: bool,
    pub family: u8,
    #[serde(rename = "p")]
    pub os: String,
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    pub revision: Option<i32>,
}

/// Register's reply: session id plus, if known, the device's own address.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct R2cCr {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev: Option<IpPort>,
    /// The device's address as seen from outside its NAT — a third
    /// connect candidate alongside `dev` (LAN-local) and `relay`. Missing
    /// from this struct entirely until 2026-09-16, when a real capture of
    /// the official Windows client's connect sequence showed every
    /// `R2C_C_R` carrying this field and the client racing `C2D_T` against
    /// all three candidates (`conn`: "local"/"map"/"relay") concurrently,
    /// not falling back to relay only when `dev`/direct failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmap: Option<IpPort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<IpPort>,
    pub nat: String,
    pub sid: Option<u32>,
    pub rsp: i32,
}

/// Register's reply on the direct (non-relay) path: no `rsp` field — its
/// mere arrival means success, with the device's address if the register
/// server already has one on file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct R2cT {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dmap: Option<IpPort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev: Option<IpPort>,
    pub cid: i32,
    pub sid: u32,
}

/// Client's LAN discovery of a UID: broadcast to port 2018 alongside the
/// server lookup (our captures), in the captured element order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2dC {
    pub uid: String,
    pub cli: ClientList,
    pub cid: i32,
    pub mtu: u32,
    /// `0` on the wire: a `bool` would be written as "false".
    pub debug: u32,
    #[serde(rename = "p")]
    pub os: String,
    #[serde(default)]
    pub pl: String,
    #[serde(default)]
    pub lver: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct ClientList {
    pub port: u32,
}

/// A device's answer to `C2D_C`: its connection id for a session-less
/// direct session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct D2cCr {
    pub rsp: i32,
    pub cid: i32,
    pub did: i32,
}

/// Client-to-device (sent to the relay address) asking to use the relay as
/// the data path when direct connect wasn't possible.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2dT {
    pub sid: u32,
    pub conn: String,
    pub cid: i32,
    pub mtu: u32,
    /// Present (empty) in every real captured `C2D_T`, alongside `lver` —
    /// confirmed against the official Windows client, 2026-09-16. Unknown
    /// purpose; the real client always sends it as an empty element, so we
    /// mirror that rather than guessing at real content.
    #[serde(default)]
    pub pl: String,
    /// Modern-login-protocol version negotiated here; the real client
    /// always sends `3` — confirmed against the official Windows client,
    /// 2026-09-16. `#[serde(default)]` keeps this backward-compatible with
    /// any older captured/test XML that predates this field.
    #[serde(default)]
    pub lver: u32,
}

/// Confirmation from the relay that it will forward traffic for this pair.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct D2cCfm {
    pub sid: u32,
    pub conn: String,
    pub rsp: i32,
    pub cid: i32,
    pub did: i32,
    /// The login nonce, delivered here rather than through any BC-level
    /// message — confirmed 2026-09-13 against a real capture: over the
    /// relay path, the real client never performs the legacy
    /// login/nonce-reply BC exchange at all. It goes straight to the
    /// modern `LoginUser` request, hashing username/password against this
    /// `nc` value exactly as if it were the nonce from that legacy step
    /// (`MD5(username + nc)`, truncated the same way).
    ///
    /// **Correction, 2026-09-16**: an earlier version of this comment
    /// claimed `nc` was absent for `conn: "local"`/`"map"`. A real
    /// capture of the official Windows
    /// client connecting directly over VPN contradicts that: its
    /// `conn: "local"` `D2C_CFM` carried `nc` (e.g. `1031078972`) exactly
    /// like the relay path does. Treat `nc` as present whenever the
    /// server sends it, regardless of `conn`, and use it the same way on
    /// every path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nc: Option<String>,
    /// Its meaning is unverified — not yet consumed by anything here,
    /// just parsed and kept available.
    /// `#[serde(default)]` keeps parsing backward-compatible with older
    /// captured/test XML that predates this field.
    #[serde(default)]
    pub pl: String,
    /// Modern-login-protocol version, mirroring `C2dT::lver` — confirmed
    /// `3` in real captures. Not yet consumed by anything here.
    /// `#[serde(default)]` keeps parsing backward-compatible with older
    /// captured/test XML that predates this field.
    #[serde(default)]
    pub lver: u32,
}

/// Client's final ack of the relay path back to the register server.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2rCfm {
    pub sid: u32,
    pub conn: String,
    pub rsp: i32,
    pub cid: i32,
    pub did: i32,
}

/// See [`UdpXml::R2cDisc`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct R2cDisc {
    pub sid: u32,
}

/// See [`UdpXml::C2dDisc`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2dDisc {
    pub cid: i32,
    pub did: i32,
}

/// See [`UdpXml::C2rDisc`]. The fields in the order of our captures; `rsp` 0 and `conn` the path when a
/// `D2C_CFM` confirmed the session, else -1 and "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2rDisc {
    pub uid: String,
    pub sid: u32,
    pub time: DiscTime,
    pub conn: String,
    pub rsp: i32,
    pub cid: i32,
    pub send: DiscCounters,
    pub recv: DiscCounters,
    /// The accepted lookup answer's `debug`.
    pub debug: u32,
}

/// Milliseconds in a `C2R_DISC`: from the attempt's start to the accepted lookup answer and to the first
/// `D2C_CFM` (0 without one), and from that `D2C_CFM` to the abandon.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct DiscTime {
    pub query: i64,
    pub setup: i64,
    pub conn: i64,
}

/// One direction's counters in a `C2R_DISC`, in the captured order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct DiscCounters {
    pub spd: i64,
    pub cnt: i64,
    pub cntr: i64,
    pub size: i64,
    pub sizer: i64,
    pub lc: i64,
    pub rc: i64,
    pub sizew: i64,
    pub itvl: i64,
}

/// See [`UdpXml::D2cDisc`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct D2cDisc {
    pub cid: i32,
    pub did: i32,
}

/// See [`UdpXml::C2dHb`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2dHb {
    pub cid: i32,
    pub did: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_captured_r2c_disc() {
        // Captured from the real register server, 2026-09-13: sent
        // unprompted ~13s after a C2D_C direct-connect attempt went
        // unanswered by the device.
        let xml = b"<P2P><R2C_DISC><sid>309336108</sid></R2C_DISC></P2P>";
        assert_eq!(
            UdpXml::from_bytes(xml).unwrap(),
            UdpXml::R2cDisc(R2cDisc { sid: 309336108 })
        );
    }

    #[test]
    fn c2m_q_round_trips() {
        let xml = UdpXml::C2mQ(C2mQ {
            uid: "9527000EXAMPLE01".to_string(),
            ver: 3,
            os: "WIN".to_string(),
        });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }

    #[test]
    fn parses_a_real_captured_c2m_q_from_the_official_windows_client() {
        // Ground truth from a Wireshark capture of the real Windows app
        // (2026-09-13), decrypted with our own xml_crypto — this is what
        // caught the missing `ver` field: earlier drafts of C2mQ omitted it
        // and most relays silently ignored the request as a result.
        let sample = b"<P2P>\n<C2M_Q>\n<uid>9527000EXAMPLE01</uid>\n<ver>3</ver>\n<p>WIN</p>\n</C2M_Q>\n</P2P>";
        let parsed = UdpXml::from_bytes(sample).unwrap();
        assert_eq!(
            parsed,
            UdpXml::C2mQ(C2mQ {
                uid: "9527000EXAMPLE01".to_string(),
                ver: 3,
                os: "WIN".to_string(),
            })
        );
    }

    #[test]
    fn m2c_q_r_round_trips_with_optional_fields() {
        let xml = UdpXml::M2cQr(M2cQr {
            reg: Some(IpPort { ip: "1.2.3.4".to_string(), port: 9999 }),
            relay: Some(IpPort { ip: "1.2.3.5".to_string(), port: 9998 }),
            debug: Some("251658240".to_string()),
        });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }

    #[test]
    fn parses_a_real_captured_m2c_q_r_with_extra_unknown_fields() {
        // Ground truth from the same Wireshark capture: a *successful*
        // M2C_Q_R with real reg/relay data, plus several fields (log, t,
        // timer, retry, mtu, debug, ac, rsp) our M2cQr doesn't model at all.
        // This is the proof that omitting them is fine — serde ignores
        // unrecognized elements by default — so the real bug was never here,
        // it was the missing `ver` in our outbound C2M_Q (see the test
        // above).
        let sample = br#"<P2P><M2C_Q_R><reg><ip>172.239.26.97</ip><port>58200</port></reg><relay><ip>172.239.26.97</ip><port>58100</port></relay><log><ip>172.239.26.97</ip><port>57850</port></log><t><ip>172.239.26.97</ip><port>9996</port></t><timer/><retry/><mtu>1350</mtu><debug>251658240</debug><ac>-1700607721</ac><rsp>0</rsp></M2C_Q_R></P2P>"#;
        let parsed = UdpXml::from_bytes(sample).unwrap();
        assert_eq!(
            parsed,
            UdpXml::M2cQr(M2cQr {
                reg: Some(IpPort { ip: "172.239.26.97".to_string(), port: 58200 }),
                relay: Some(IpPort { ip: "172.239.26.97".to_string(), port: 58100 }),
                debug: Some("251658240".to_string()),
            })
        );
    }

    #[test]
    fn parses_a_real_captured_d2c_cfm_with_the_login_nonce() {
        // Captured from the real relay, 2026-09-13: this `nc` value is the
        // login nonce — never delivered any other way over the relay path
        // (see `D2cCfm::nc`). Independently confirmed by recomputing the
        // real captured `LoginUser` request's username hash from this exact
        // value: `MD5("admin" + "54482718")`, truncated to 31 uppercase hex
        // chars, equals the captured `314C4F5683C50B5583D3256DD1EE320`.
        let xml = br#"<P2P><D2C_CFM><sid>309490496</sid><conn>relay</conn><rsp>0</rsp><cid>101000</cid><did>471</did><pl>V=1;C=2,N=6,P1=59,P2=,P3=,P4=,P5=,P6=;</pl><nc>54482718</nc><lver>3</lver></D2C_CFM></P2P>"#;
        let parsed = UdpXml::from_bytes(xml).unwrap();
        assert_eq!(
            parsed,
            UdpXml::D2cCfm(D2cCfm {
                sid: 309490496,
                conn: "relay".to_string(),
                rsp: 0,
                cid: 101000,
                did: 471,
                nc: Some("54482718".to_string()),
                pl: "V=1;C=2,N=6,P1=59,P2=,P3=,P4=,P5=,P6=;".to_string(),
                lver: 3,
            })
        );
    }

    #[test]
    fn parses_a_real_captured_d2c_cfm_with_conn_local_and_a_nonce() {
        // Captured from a real official-Windows-client connect over VPN,
        // 2026-09-16: `nc` is present here even though `conn` is "local",
        // contradicting the earlier (now-corrected) doc comment on
        // `D2cCfm::nc` that claimed the nonce was relay-only.
        let xml = br#"<P2P><D2C_CFM><sid>322047625</sid><conn>local</conn><rsp>0</rsp><cid>814000</cid><did>113</did><pl>V=1;C=2,N=6,P1=59,P2=,P3=,P4=,P5=,P6=;</pl><nc>1031078972</nc><lver>3</lver></D2C_CFM></P2P>"#;
        let parsed = UdpXml::from_bytes(xml).unwrap();
        assert_eq!(
            parsed,
            UdpXml::D2cCfm(D2cCfm {
                sid: 322047625,
                conn: "local".to_string(),
                rsp: 0,
                cid: 814000,
                did: 113,
                nc: Some("1031078972".to_string()),
                pl: "V=1;C=2,N=6,P1=59,P2=,P3=,P4=,P5=,P6=;".to_string(),
                lver: 3,
            })
        );
    }

    #[test]
    fn parses_a_real_captured_r2c_c_r_with_dmap() {
        // Captured from the real register server, 2026-09-16 (both a VPN
        // and a non-VPN connect attempt): every R2C_C_R carries `dmap`
        // (the device's externally NAT-mapped address) alongside `dev`
        // (LAN-local) and `relay` — a third connect candidate our
        // R2cCr struct didn't model at all until this fix.
        let xml = br#"<P2P><R2C_C_R><dmap><ip>203.0.113.10</ip><port>58894</port></dmap><dev><ip>192.168.1.50</ip><port>58894</port></dev><relay><ip>172.239.26.97</ip><port>54230</port></relay><relayt><ip>172.239.26.97</ip><port>9995</port></relayt><nat>NULL</nat><sid>322047625</sid><rsp>0</rsp><ac>322047625</ac></R2C_C_R></P2P>"#;
        let parsed = UdpXml::from_bytes(xml).unwrap();
        assert_eq!(
            parsed,
            UdpXml::R2cCr(R2cCr {
                dev: Some(IpPort { ip: "192.168.1.50".to_string(), port: 58894 }),
                dmap: Some(IpPort { ip: "203.0.113.10".to_string(), port: 58894 }),
                relay: Some(IpPort { ip: "172.239.26.97".to_string(), port: 54230 }),
                nat: "NULL".to_string(),
                sid: Some(322047625),
                rsp: 0,
            })
        );
    }

    #[test]
    fn c2d_t_round_trips_with_pl_and_lver() {
        // Regression test for the missing `pl`/`lver` fields (found in the
        // 2026-09-16 Wireshark capture of the official Windows client): every
        // real captured C2D_T carries both, alongside the fields we already
        // modeled.
        let xml = UdpXml::C2dT(C2dT {
            sid: 555,
            conn: "relay".to_string(),
            cid: 1,
            mtu: 1350,
            pl: String::new(),
            lver: 3,
        });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }

    #[test]
    fn c2d_t_parses_without_pl_or_lver_for_backward_compatibility() {
        // Older/test XML that predates the pl/lver fields must still parse
        // (defaulting pl to "" and lver to 0) rather than fail outright.
        let xml = b"<P2P><C2D_T><sid>555</sid><conn>relay</conn><cid>1</cid><mtu>1350</mtu></C2D_T></P2P>";
        assert_eq!(
            UdpXml::from_bytes(xml).unwrap(),
            UdpXml::C2dT(C2dT {
                sid: 555,
                conn: "relay".to_string(),
                cid: 1,
                mtu: 1350,
                pl: String::new(),
                lver: 0,
            })
        );
    }

    #[test]
    fn parses_a_real_captured_c2d_disc() {
        // Captured from the real official client, 2026-09-13: sent right
        // after a login attempt was rejected with 401, tearing the failed
        // session down before reconnecting fresh.
        let xml = b"<P2P><C2D_DISC><cid>101000</cid><did>471</did></C2D_DISC></P2P>";
        assert_eq!(
            UdpXml::from_bytes(xml).unwrap(),
            UdpXml::C2dDisc(C2dDisc { cid: 101000, did: 471 })
        );
    }

    #[test]
    fn parses_a_real_captured_d2c_disc() {
        // Captured from a real device, 2026-09-13, at ordinary end-of-session
        // teardown (symmetric to the C2D_DISC the client itself sends).
        let xml = b"<P2P><D2C_DISC><cid>101001</cid><did>472</did></D2C_DISC></P2P>";
        assert_eq!(
            UdpXml::from_bytes(xml).unwrap(),
            UdpXml::D2cDisc(D2cDisc { cid: 101001, did: 472 })
        );
    }

    fn lan_discovery() -> UdpXml {
        UdpXml::C2dC(C2dC {
            uid: "9527000EXAMPLE01".to_string(),
            cli: ClientList { port: 40000 },
            cid: 1,
            mtu: 1350,
            debug: 0,
            os: "WIN".to_string(),
            pl: String::new(),
            lver: 3,
        })
    }

    #[test]
    fn c2d_c_round_trips() {
        let xml = lan_discovery();
        assert_eq!(UdpXml::from_bytes(&xml.to_bytes()).unwrap(), xml);
    }

    /// The codec writes the empty `pl` as `<pl/>`, as it does in every `C2D_T`.
    #[test]
    fn c2d_c_serializes_in_the_captured_order() {
        let body = "<uid>9527000EXAMPLE01</uid><cli><port>40000</port></cli><cid>1</cid><mtu>1350</mtu>\
                    <debug>0</debug><p>WIN</p><pl/><lver>3</lver>";
        assert_eq!(String::from_utf8(lan_discovery().to_bytes()).unwrap(), format!("<P2P><C2D_C>{body}</C2D_C></P2P>"));
    }

    #[test]
    fn d2c_c_r_parses_with_and_without_timer() {
        let expected = UdpXml::D2cCr(D2cCr { rsp: 0, cid: 1, did: 7 });
        let with_timer = b"<P2P><D2C_C_R><timer><def>3000</def><hb>1000</hb><hbt>10000</hbt></timer>\
                           <rsp>0</rsp><cid>1</cid><did>7</did></D2C_C_R></P2P>";
        assert_eq!(UdpXml::from_bytes(with_timer).unwrap(), expected);
        let without = b"<P2P><D2C_C_R><rsp>0</rsp><cid>1</cid><did>7</did></D2C_C_R></P2P>";
        assert_eq!(UdpXml::from_bytes(without).unwrap(), expected);
    }

    /// The tree and element order of the `C2R_DISC` the official app sends when it abandons an attempt that got no
    /// `D2C_CFM`, in our captures (its numbers here; identities replaced).
    #[test]
    fn c2r_disc_serializes_in_the_captured_order() {
        let counters = |rc| DiscCounters { lc: -1, rc, ..Default::default() };
        let xml = UdpXml::C2rDisc(C2rDisc {
            uid: "9527000EXAMPLE01".to_string(),
            sid: 555,
            time: DiscTime { query: 172, setup: 0, conn: 11996 },
            conn: "unknown".to_string(),
            rsp: -1,
            cid: 7,
            send: DiscCounters { lc: 0, ..counters(-1) },
            recv: counters(-1),
            debug: 251658240,
        });
        let counters = |lc, rc| {
            format!("<spd>0</spd><cnt>0</cnt><cntr>0</cntr><size>0</size><sizer>0</sizer><lc>{lc}</lc><rc>{rc}</rc>\
                     <sizew>0</sizew><itvl>0</itvl>")
        };
        let body = format!(
            "<uid>9527000EXAMPLE01</uid><sid>555</sid><time><query>172</query><setup>0</setup><conn>11996</conn></time>\
             <conn>unknown</conn><rsp>-1</rsp><cid>7</cid><send>{}</send><recv>{}</recv><debug>251658240</debug>",
            counters(0, -1),
            counters(-1, -1)
        );
        assert_eq!(String::from_utf8(xml.to_bytes()).unwrap(), format!("<P2P><C2R_DISC>{body}</C2R_DISC></P2P>"));
        assert_eq!(UdpXml::from_bytes(&xml.to_bytes()).unwrap(), xml);
    }

    /// A lookup answer's `debug` is kept when it is a number; 8 of the refusing answers in our captures carry an
    /// empty or non-numeric one, which must not fail the answer.
    #[test]
    fn m2c_q_r_keeps_any_debug_text() {
        let answer = |debug: &str| {
            let xml = format!("<P2P><M2C_Q_R>{debug}<rsp>-3</rsp></M2C_Q_R></P2P>");
            match UdpXml::from_bytes(xml.as_bytes()).unwrap() {
                UdpXml::M2cQr(reply) => reply.debug,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(answer("<debug>251658240</debug>").as_deref(), Some("251658240"));
        assert!(answer("<debug/>").is_none_or(|text| text.is_empty()));
        assert_eq!(answer("<debug>x1</debug>").as_deref(), Some("x1"));
        assert_eq!(answer(""), None);
    }

    #[test]
    fn c2d_hb_round_trips() {
        // Field shape unverified: no real capture of this message exists
        // yet, since we never sent one before discovering it was missing
        // (see `UdpXml::C2dHb`).
        let xml = UdpXml::C2dHb(C2dHb { cid: 1, did: 2 });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }
}
