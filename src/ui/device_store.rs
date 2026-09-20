//! The device list, persisted as a small key file in the user's config
//! directory. Passwords are not stored here — see `secrets`.

use crate::ui::bridge::ConnectTarget;
use gtk4::glib::{self, KeyFile, KeyFileFlags};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Device {
    /// Stable identity, also the keyring lookup key.
    pub key: String,
    pub name: String,
    pub target: ConnectTarget,
    pub username: String,
    pub channel: u8,
    /// An NVR or Home Hub (several cameras behind one device), as far as the
    /// device has told us. Unknown until the first successful login.
    pub multi_channel: bool,
}

impl Device {
    /// The name is a placeholder (the address) until the device reports its
    /// own after the first login.
    pub fn new(target: ConnectTarget) -> Self {
        let name = match &target {
            ConnectTarget::Uid(uid) => uid.clone(),
            ConnectTarget::Ip { addr, .. } => addr.to_string(),
        };
        Self {
            key: format!("{:016x}", rand::random::<u64>()),
            name,
            target,
            username: "admin".to_string(),
            channel: 0,
            multi_channel: false,
        }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("reoling").join("devices.ini")
}

pub fn load() -> Vec<Device> {
    let file = KeyFile::new();
    if file.load_from_file(path(), KeyFileFlags::NONE).is_err() {
        return Vec::new();
    }
    let mut devices = Vec::new();
    for group in file.groups().iter().map(|g| g.to_string()) {
        let get = |k: &str| file.string(&group, k).ok().map(|s| s.to_string());
        let Some(key) = group.strip_prefix("device ").map(str::to_string) else {
            continue;
        };
        let target = if let Some(uid) = get("uid") {
            ConnectTarget::Uid(uid)
        } else if let (Some(addr), Some(port)) = (
            get("ip").and_then(|s| s.parse().ok()),
            get("port").and_then(|s| s.parse().ok()),
        ) {
            ConnectTarget::Ip { addr, port }
        } else {
            continue;
        };
        devices.push(Device {
            key,
            name: get("name").unwrap_or_default(),
            target,
            username: get("username").unwrap_or_else(|| "admin".to_string()),
            channel: get("channel").and_then(|s| s.parse().ok()).unwrap_or(0),
            multi_channel: get("multi_channel").as_deref() == Some("true"),
        });
    }
    devices
}

pub fn save(devices: &[Device]) {
    let file = KeyFile::new();
    for d in devices {
        let group = format!("device {}", d.key);
        file.set_string(&group, "name", &d.name);
        match &d.target {
            ConnectTarget::Uid(uid) => file.set_string(&group, "uid", uid),
            ConnectTarget::Ip { addr, port } => {
                file.set_string(&group, "ip", &addr.to_string());
                file.set_string(&group, "port", &port.to_string());
            }
        }
        file.set_string(&group, "username", &d.username);
        file.set_string(&group, "channel", &d.channel.to_string());
        file.set_string(&group, "multi_channel", &d.multi_channel.to_string());
    }
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = file.save_to_file(&path) {
        eprintln!("could not save the device list: {e}");
    }
}
