//! Passwords live in the desktop keyring (Secret Service / portal), never in
//! the device list on disk. Each call runs on its own short-lived thread with
//! a tokio runtime (the keyring client is async and the GTK main loop is not
//! a tokio runtime); results come back over an `async-channel`.

use std::collections::HashMap;

const APP_ATTR: (&str, &str) = ("application", "de.dersobi.reoling");

fn attributes(key: &str) -> HashMap<&'static str, String> {
    HashMap::from([(APP_ATTR.0, APP_ATTR.1.to_string()), ("device", key.to_string())])
}

fn run<T: Send + 'static>(
    job: impl std::future::Future<Output = T> + Send + 'static,
) -> async_channel::Receiver<T> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to start tokio runtime");
        let _ = tx.send_blocking(runtime.block_on(job));
    });
    rx
}

/// The saved password for the device, or `None` when there is none or the
/// keyring is unavailable.
pub async fn lookup(key: String) -> Option<String> {
    let job = async move {
        let keyring = oo7::Keyring::new().await.ok()?;
        let items = keyring.search_items(&attributes(&key)).await.ok()?;
        let secret = items.first()?.secret().await.ok()?;
        String::from_utf8(secret.to_vec()).ok()
    };
    run(job).recv().await.ok().flatten()
}

/// Saves (replacing any previous) the password. Returns whether it worked.
pub async fn store(key: String, device_name: String, password: String) -> bool {
    let job = async move {
        let Ok(keyring) = oo7::Keyring::new().await else {
            return false;
        };
        keyring
            .create_item(
                &format!("Reoling: {device_name}"),
                &attributes(&key),
                password.as_bytes(),
                true,
            )
            .await
            .is_ok()
    };
    run(job).recv().await.unwrap_or(false)
}

pub async fn forget(key: String) {
    let job = async move {
        if let Ok(keyring) = oo7::Keyring::new().await {
            let _ = keyring.delete(&attributes(&key)).await;
        }
    };
    let _ = run(job).recv().await;
}
