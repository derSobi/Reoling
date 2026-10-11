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

    #[error("timed out waiting for a reply")]
    ReplyTimeout,

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

/// Why a device refused a login, for the refusals that are NOT a wrong
/// password — the saved password is fine and must be kept. `None` for 401
/// (wrong user name or password) and for any code not listed here, which
/// keep being treated as a password problem.
pub fn login_refusal_reason(code: u16) -> Option<&'static str> {
    Some(match code {
        406 => "The device refused the login",
        407 => "The device is locked after too many failed logins; try again later",
        408 => "The device has not been set up yet; finish its setup in the Reolink app first",
        409 => "Too many users are signed in to the device",
        410 => "Too many different addresses are signed in to the device",
        416 => "The device asks for its password to be reset",
        419 => "The device is busy with another client",
        501 => "This account is not authorised on the device",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::login_refusal_reason;

    #[test]
    fn only_non_password_refusals_have_a_reason() {
        assert_eq!(login_refusal_reason(401), None);
        assert_eq!(login_refusal_reason(400), None);
        assert!(login_refusal_reason(407).is_some());
        assert!(login_refusal_reason(409).is_some());
    }
}
