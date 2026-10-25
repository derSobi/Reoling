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

/// Why a device refused a login, from the reply's response code. Only
/// `WrongCredentials` (401) says the user name or password is wrong, and only
/// it makes the app forget a saved password. Every other refusal, any code not
/// listed here included, keeps the saved password: a locked, busy or confused
/// device says nothing about the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginRefusal {
    /// 401: wrong user name or password.
    WrongCredentials,
    /// 406: refused without a further reason.
    Refused,
    /// 407: locked after too many failed logins.
    Locked,
    /// 408: not set up yet.
    NotSetUp,
    /// 409: too many users signed in.
    TooManyUsers,
    /// 410: too many different addresses signed in.
    TooManyAddresses,
    /// 416: the device asks for its password to be reset.
    ResetRequired,
    /// 419: busy with another client.
    Busy,
    /// 501: the account is not authorised on the device.
    NotAuthorised,
    /// Any other code.
    Other { code: u16 },
}

impl LoginRefusal {
    pub fn from_code(code: u16) -> Self {
        match code {
            401 => Self::WrongCredentials,
            406 => Self::Refused,
            407 => Self::Locked,
            408 => Self::NotSetUp,
            409 => Self::TooManyUsers,
            410 => Self::TooManyAddresses,
            416 => Self::ResetRequired,
            419 => Self::Busy,
            501 => Self::NotAuthorised,
            code => Self::Other { code },
        }
    }

    /// Whether the saved password is wrong and is to be forgotten.
    pub fn forgets_saved_password(self) -> bool {
        self == Self::WrongCredentials
    }

    /// The refusal as the device list shows it.
    pub fn reason(self) -> String {
        match self {
            Self::WrongCredentials => "Incorrect password".to_string(),
            Self::Refused => "The device refused the login".to_string(),
            Self::Locked => "The device is locked after too many failed logins; try again later".to_string(),
            Self::NotSetUp => {
                "The device has not been set up yet; finish its setup in the Reolink app first".to_string()
            }
            Self::TooManyUsers => "Too many users are signed in to the device".to_string(),
            Self::TooManyAddresses => "Too many different addresses are signed in to the device".to_string(),
            Self::ResetRequired => "The device asks for its password to be reset".to_string(),
            Self::Busy => "The device is busy with another client".to_string(),
            Self::NotAuthorised => "This account is not authorised on the device".to_string(),
            Self::Other { code } => format!("Login refused by the device (code {code})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LoginRefusal;

    #[test]
    fn every_known_code_has_its_refusal() {
        let known = [
            (401, LoginRefusal::WrongCredentials),
            (406, LoginRefusal::Refused),
            (407, LoginRefusal::Locked),
            (408, LoginRefusal::NotSetUp),
            (409, LoginRefusal::TooManyUsers),
            (410, LoginRefusal::TooManyAddresses),
            (416, LoginRefusal::ResetRequired),
            (419, LoginRefusal::Busy),
            (501, LoginRefusal::NotAuthorised),
        ];
        for (code, refusal) in known {
            assert_eq!(LoginRefusal::from_code(code), refusal, "code {code}");
        }
    }

    #[test]
    fn unknown_and_neighbouring_codes_are_other() {
        for code in [0, 1, 200, 300, 400, 402, 403, 404, 405, 411, 412, 415, 417, 418, 420, 500, 502, 0xfffe, 0xffff] {
            assert_eq!(LoginRefusal::from_code(code), LoginRefusal::Other { code }, "code {code}");
        }
    }

    #[test]
    fn only_a_wrong_password_forgets_the_saved_one() {
        for code in 0..=u16::MAX {
            assert_eq!(LoginRefusal::from_code(code).forgets_saved_password(), code == 401, "code {code}");
        }
    }

    #[test]
    fn other_refusals_name_their_code() {
        assert_eq!(LoginRefusal::from_code(400).reason(), "Login refused by the device (code 400)");
        assert_eq!(LoginRefusal::from_code(0xffff).reason(), "Login refused by the device (code 65535)");
        assert_eq!(LoginRefusal::from_code(401).reason(), "Incorrect password");
        assert_eq!(
            LoginRefusal::from_code(407).reason(),
            "The device is locked after too many failed logins; try again later"
        );
    }
}
