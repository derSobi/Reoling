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
    /// Device's disconnect notice, symmetric to `C2dDisc` — observed in the
    /// real protocol (2026-09-13) both at ordinary end-of-session teardown
    /// and (in our own testing) right after we sent an extra legacy login
    /// step the device didn't expect on an already-established relay
    /// session. `BcConnection::recv_bc` turns this into a clear error
    /// instead of the generic parse failure it used to cause.
    #[serde(rename = "D2C_DISC")]
    D2cDisc(D2cDisc),
    /// Client-to-device heartbeat, sent to the device's address on the
    /// direct (non-relay) path — read (not copied) from `neolink`'s
    /// `bcudp/xml.rs`, since our own real captures never show it (we never
    /// sent one). Without it, real hardware just keeps retransmitting its
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

/// Client-to-device direct connect attempt (NAT traversal).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct C2dC {
    pub uid: String,
    pub cli: ClientList,
    pub cid: i32,
    pub mtu: u32,
    pub debug: bool,
    #[serde(rename = "p")]
    pub os: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct ClientList {
    pub port: u32,
}

/// Device's reply confirming the direct connection and its own connection id.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct D2cCr {
    pub timer: Timer,
    pub rsp: i32,
    pub cid: i32,
    pub did: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Timer {
    pub def: u32,
    pub hb: u32,
    pub hbt: u32,
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
    /// **Correction, 2026-09-16**: an earlier version of this comment,
    /// sourced from `bairelay`'s `login_sigv3.rs` (read-only reference),
    /// claimed `nc` was absent for `conn: "local"`/`"map"` (relay-only,
    /// cloud-account-gated sigV3). A real capture of the official Windows
    /// client connecting directly over VPN contradicts that: its
    /// `conn: "local"` `D2C_CFM` carried `nc` (e.g. `1031078972`) exactly
    /// like the relay path does. Treat `nc` as present whenever the
    /// server sends it, regardless of `conn`, and use it the same way on
    /// every path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nc: Option<String>,
    /// Carries additional data for the modern login mechanisms (sigV3 /
    /// ECDHE) per `bairelay`'s `login.rs` (read-only reference) — not yet
    /// consumed by anything here, just parsed and kept available.
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
        // Regression test for the missing `pl`/`lver` fields (found via the
        // 2026-09-16 Wireshark audit of the official Windows client): every
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

    #[test]
    fn c2d_c_round_trips() {
        let xml = UdpXml::C2dC(C2dC {
            uid: "9527000EXAMPLE01".to_string(),
            cli: ClientList { port: 40000 },
            cid: 1,
            mtu: 1350,
            debug: false,
            os: "LINUX".to_string(),
        });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }

    #[test]
    fn c2d_hb_round_trips() {
        // Field shape read (not copied) from neolink's bcudp/xml.rs — no
        // real capture of this message exists yet, since we never sent one
        // before discovering it was missing (see `UdpXml::C2dHb`).
        let xml = UdpXml::C2dHb(C2dHb { cid: 1, did: 2 });
        let bytes = xml.to_bytes();
        assert_eq!(UdpXml::from_bytes(&bytes).unwrap(), xml);
    }
}
