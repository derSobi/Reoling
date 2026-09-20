//! Manual diagnostic tool, not part of the app: tries to resolve a UID
//! against the real Reolink P2P infrastructure and reports what happened.
//! Usage: cargo run --example probe_uid -- <UID>

use std::sync::Arc;
use tokio::net::UdpSocket;

#[tokio::main]
async fn main() {
    let uid = std::env::args().nth(1).expect("usage: probe_uid <UID>");
    println!("Resolving UID {uid} against the real Reolink P2P relays...");

    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await.expect("bind failed"));
    println!("Local socket bound at {:?}", socket.local_addr());

    let start = std::time::Instant::now();
    match reoling::transport::discovery::connect_by_uid(&socket, &uid).await {
        Ok(peer) => {
            println!(
                "SUCCESS in {:?}: peer at {:?}, local_connection_id={}, remote_connection_id={}",
                start.elapsed(),
                peer.addr,
                peer.local_connection_id,
                peer.remote_connection_id
            );
        }
        Err(e) => {
            println!("FAILED after {:?}: {e}", start.elapsed());
        }
    }
}
