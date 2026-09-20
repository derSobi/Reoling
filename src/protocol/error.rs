use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not resolve UID {uid} against any P2P relay")]
    UidResolutionFailed { uid: String },

    #[error("P2P negotiation with the device timed out")]
    P2pNegotiationTimeout,

    #[error("login failed, camera returned response code {code}")]
    LoginFailed { code: u16 },

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("connection lost")]
    ConnectionLost,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::DeError),

    #[error("invalid IP address: {0}")]
    AddrParse(#[from] std::net::AddrParseError),
}

pub type Result<T> = std::result::Result<T, Error>;
