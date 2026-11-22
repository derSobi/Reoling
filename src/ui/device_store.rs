//! The device list, persisted as a small key file in the user's config
//! directory. Passwords are not stored here — see `secrets`.

use crate::ui::bridge::ConnectTarget;
use gtk4::glib::{self, KeyFile, KeyFileFlags};
use std::net::Ipv4Addr;
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
    /// What the device reported about its channels this run (not saved).
    pub channels: Vec<reoling::ChannelInfo>,
    /// What each channel's camera can do, as the device reported it this run.
    pub abilities: std::collections::HashMap<u8, reoling::ChannelAbilities>,
    /// The watched channel's saved presets, as the device last reported them.
    pub presets: Vec<reoling::PtzPreset>,
    /// The device's own model string, as it reported it this run.
    pub model: String,
    /// The encoder values the device allows, from its login reply this run.
    pub encode_profiles: Vec<reoling::encoding::EncodeProfile>,
    /// Where a device added by UID is on its network, as it reported itself, once a connection to that address
    /// reported the device's UID; tried directly at every start on `lan_network`.
    pub lan_ip: Option<Ipv4Addr>,
    /// The network this machine was on when `lan_ip` was saved (`lan_search::current_network`).
    pub lan_network: Option<Ipv4Addr>,
    /// An address the device reported at which something else answered: not tried again.
    pub lan_rejected: Option<Ipv4Addr>,
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
            channels: Vec::new(),
            abilities: Default::default(),
            presets: Vec::new(),
            model: String::new(),
            encode_profiles: Vec::new(),
            lan_ip: None,
            lan_network: None,
            lan_rejected: None,
        }
    }

    /// Takes the address the device reported for itself, with this machine on `network`. Confirmed (the device
    /// is there): saved for the next start. Not confirmed: remembered as rejected, so that it is not tried at
    /// every start. An address saved earlier stays: a failed test connection is no reason to give up the rule
    /// that keeps the LAN login to that address, which only the user lifts (the warning's "Continue").
    pub fn learn_address(&mut self, ip: Ipv4Addr, network: Ipv4Addr, confirmed: bool) {
        if confirmed {
            (self.lan_ip, self.lan_network, self.lan_rejected) = (Some(ip), Some(network), None);
        } else {
            self.lan_rejected = Some(ip);
        }
    }

    /// Whether a connection should learn where the device is: with nothing saved yet, or on the network the
    /// saved address belongs to (`current`). Elsewhere a test connection would go to an address that means
    /// something else there, and its failure would be remembered against the device's real address.
    pub fn learns_address_on(&self, current: Option<Ipv4Addr>) -> bool {
        matches!(self.target, ConnectTarget::Uid(_))
            && current.is_some()
            && (self.lan_ip.is_none() || self.lan_network == current)
    }

    /// Forgets everything learned about where the device is.
    pub fn forget_address(&mut self) {
        (self.lan_ip, self.lan_network, self.lan_rejected) = (None, None, None);
    }

    /// The saved address to try directly, when this machine is on the network it was saved on (`current`).
    pub fn saved_address_for(&self, current: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
        let on_that_network = current.is_some() && current == self.lan_network;
        (matches!(self.target, ConnectTarget::Uid(_)) && on_that_network).then_some(self.lan_ip).flatten()
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("reoling").join("devices.ini")
}

/// The saved devices and the one that was being watched last.
pub fn load() -> (Vec<Device>, Option<String>) {
    let file = KeyFile::new();
    if file.load_from_file(path(), KeyFileFlags::NONE).is_err() {
        return (Vec::new(), None);
    }
    devices_from(&file)
}

/// `load` from a key file that is already read.
fn devices_from(file: &KeyFile) -> (Vec<Device>, Option<String>) {
    let last = file.string("app", "last_device").ok().map(|s| s.to_string());
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
            channels: Vec::new(),
            abilities: Default::default(),
            presets: Vec::new(),
            model: String::new(),
            encode_profiles: Vec::new(),
            lan_ip: get("lan_ip").and_then(|s| s.parse().ok()),
            lan_network: get("lan_network").and_then(|s| s.parse().ok()),
            lan_rejected: get("lan_rejected").and_then(|s| s.parse().ok()),
        });
    }
    (devices, last)
}

pub fn save(devices: &[Device], last_device: Option<&str>) {
    let file = key_file_of(devices, last_device);
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = file.save_to_file(&path) {
        eprintln!("could not save the device list: {e}");
    }
}

/// What `save` writes.
fn key_file_of(devices: &[Device], last_device: Option<&str>) -> KeyFile {
    let file = KeyFile::new();
    if let Some(last) = last_device {
        file.set_string("app", "last_device", last);
    }
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
        // Only a device added by UID has an address that was learned.
        if matches!(d.target, ConnectTarget::Uid(_)) {
            let learned = [("lan_ip", d.lan_ip), ("lan_network", d.lan_network), ("lan_rejected", d.lan_rejected)];
            for (key, value) in learned {
                if let Some(value) = value {
                    file.set_string(&group, key, &value.to_string());
                }
            }
        }
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid_device() -> Device {
        Device::new(ConnectTarget::Uid("9527000EXAMPLE01".to_string()))
    }

    fn reloaded(devices: &[Device]) -> Vec<Device> {
        let text = key_file_of(devices, None).to_data();
        let file = KeyFile::new();
        file.load_from_data(&text, KeyFileFlags::NONE).unwrap();
        devices_from(&file).0
    }

    #[test]
    fn a_learned_address_is_saved_with_a_uid_device_only() {
        let address = |text: &str| -> Ipv4Addr { text.parse().unwrap() };
        let (ip, network, rejected) = (address("192.0.2.5"), address("192.0.2.255"), address("192.0.2.9"));
        let mut by_uid = uid_device();
        (by_uid.lan_ip, by_uid.lan_network, by_uid.lan_rejected) = (Some(ip), Some(network), Some(rejected));
        let mut by_address = Device::new(ConnectTarget::Ip { addr: "192.0.2.7".parse().unwrap(), port: 9000 });
        (by_address.lan_ip, by_address.lan_network) = (Some(ip), Some(network));
        let back = reloaded(&[by_uid, by_address, uid_device()]);
        let learned = (back[0].lan_ip, back[0].lan_network, back[0].lan_rejected);
        assert_eq!(learned, (Some(ip), Some(network), Some(rejected)));
        assert_eq!((back[1].lan_ip, back[1].lan_network), (None, None), "a device added by address");
        assert_eq!((back[2].lan_ip, back[2].lan_network, back[2].lan_rejected), (None, None, None), "nothing learned");
    }

    #[test]
    fn a_file_without_the_keys_or_with_bad_values_loads_as_nothing_learned() {
        let file = KeyFile::new();
        let text = "[device 0000000000000001]\nname=A\nuid=9527000EXAMPLE01\nlan_ip=not an address\nlan_network=\n\
                    [device 0000000000000002]\nname=B\nuid=9527000EXAMPLE02\n";
        file.load_from_data(text, KeyFileFlags::NONE).unwrap();
        let (devices, _) = devices_from(&file);
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|d| d.lan_ip.is_none() && d.lan_network.is_none() && d.lan_rejected.is_none()));
    }

    #[test]
    fn a_reported_address_is_saved_when_confirmed_and_remembered_as_rejected_when_not() {
        let (old, new, home): (Ipv4Addr, Ipv4Addr, Ipv4Addr) =
            ("192.0.2.5".parse().unwrap(), "192.0.2.6".parse().unwrap(), "192.0.2.255".parse().unwrap());
        let mut device = uid_device();
        device.learn_address(old, home, true);
        assert_eq!((device.lan_ip, device.lan_network, device.lan_rejected), (Some(old), Some(home), None));
        // The device now says it is elsewhere, and the test connection there fails: the saved address stays, and
        // the new one is not tried again.
        device.learn_address(new, home, false);
        assert_eq!((device.lan_ip, device.lan_network, device.lan_rejected), (Some(old), Some(home), Some(new)));
        let elsewhere: Ipv4Addr = "198.51.100.255".parse().unwrap();
        assert!(device.learns_address_on(Some(home)));
        assert!(!device.learns_address_on(Some(elsewhere)), "on another network than the saved one");
        assert!(!device.learns_address_on(None), "no network to tie an address to");
        assert!(uid_device().learns_address_on(Some(elsewhere)), "nothing saved yet");
        device.learn_address(new, home, true);
        assert_eq!((device.lan_ip, device.lan_network, device.lan_rejected), (Some(new), Some(home), None));
        device.forget_address();
        assert_eq!((device.lan_ip, device.lan_network, device.lan_rejected), (None, None, None));
    }

    #[test]
    fn a_saved_address_is_used_only_on_the_network_it_was_saved_on() {
        let address = |text: &str| -> Ipv4Addr { text.parse().unwrap() };
        let (ip, home, elsewhere) = (address("192.0.2.5"), address("192.0.2.255"), address("198.51.100.255"));
        let mut device = uid_device();
        assert_eq!(device.saved_address_for(Some(home)), None, "nothing saved");
        (device.lan_ip, device.lan_network) = (Some(ip), Some(home));
        assert_eq!(device.saved_address_for(Some(home)), Some(ip));
        assert_eq!(device.saved_address_for(Some(elsewhere)), None, "another network");
        assert_eq!(device.saved_address_for(None), None, "no network to go by");
        device.lan_network = None;
        assert_eq!(device.saved_address_for(None), None, "saved without a network");
        device.target = ConnectTarget::Ip { addr: "192.0.2.7".parse().unwrap(), port: 9000 };
        device.lan_network = Some(home);
        assert_eq!(device.saved_address_for(Some(home)), None, "a device added by address");
    }
}
