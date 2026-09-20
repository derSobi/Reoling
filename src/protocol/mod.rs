//! Baichuan wire protocol for Reolink cameras/NVRs: binary framing, XML
//! payloads, and encryption (BCEncrypt/AES), built from scratch against the
//! observed wire format. No transport or session logic lives here — see
//! [`crate::transport`] for the P2P/UDP transport and [`crate::client`] for
//! the client/session API built on top of it.

pub mod bc;
pub mod bcudp;
pub mod bcmedia;
pub mod crypto;
mod error;

pub use error::{Error, Result};
