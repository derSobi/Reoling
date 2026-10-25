//! Which GPU a hardware video decoder runs on, and whether that GPU is an
//! integrated one (part of the processor, sharing its memory) or a dedicated
//! card: the two "Auto" decoding modes prefer one kind over the other (see
//! `settings::auto_ranks`).
//!
//! A GPU's kind is read from sysfs and the udev database, never from the
//! GPU itself: no device node is opened for it, so a dedicated GPU that
//! sleeps is not woken up just to be classified. What these files cannot
//! tell for sure stays `Unknown`. Which GPU a VA-API decoder runs on is
//! the decoder's to tell, and asking it can load the va plugin (see
//! `decoder_kind`).

use gstreamer::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Integrated,
    Dedicated,
    Unknown,
}

impl GpuKind {
    pub fn label(self) -> &'static str {
        match self {
            GpuKind::Integrated => "integrated GPU",
            GpuKind::Dedicated => "dedicated GPU",
            GpuKind::Unknown => "GPU of unknown kind",
        }
    }
}

const INTEL: u16 = 0x8086;
const NVIDIA: u16 = 0x10de;

/// The udev tag switcheroo-control's rules give a dedicated GPU. Its rules
/// ask the kernel driver where they can (amdgpu, xe, nouveau), so for AMD
/// this tag is the one sign used: neither a bridge in front of the GPU nor
/// the size of its memory sets an APU apart from a card. Its absence proves
/// nothing (the package may be missing, or its check may have failed).
const DISCRETE_TAG: &str = "switcheroo-discrete-gpu";

/// Where sysfs and the udev database are: the system's, or a test's.
struct Roots {
    sys: PathBuf,
    udev: PathBuf,
}

impl Roots {
    fn system() -> Self {
        Roots { sys: PathBuf::from("/sys"), udev: PathBuf::from("/run/udev") }
    }
}

/// What sysfs and udev say about one GPU.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GpuFacts {
    vendor: Option<u16>,
    /// The kernel driver bound to it (`i915`, `xe`, `amdgpu`, `nvidia`, ...).
    driver: Option<String>,
    /// The PCI addresses from the root complex down to the GPU: one for a
    /// device on the root bus, more for one behind bridges; empty when the
    /// GPU is no PCI device.
    pci_path: Vec<String>,
    /// One of its DRM nodes carries `DISCRETE_TAG`.
    discrete_tag: bool,
}

/// The kind of GPU these facts describe, from the most to the least certain
/// sign: switcheroo-control's tag, then where the GPU sits on the PCI bus,
/// which only says something for Intel and NVIDIA. An Intel GPU at 00:02.0
/// on the root bus is the one in the processor, one behind a bridge is a
/// card; an NVIDIA GPU behind a bridge is a card (the chipset GPUs it once
/// made sat on the root bus). Anything else is `Unknown`.
fn classify(facts: &GpuFacts) -> GpuKind {
    if facts.discrete_tag {
        return GpuKind::Dedicated;
    }
    let Some(address) = facts.pci_path.last() else { return GpuKind::Unknown };
    let behind_bridge = facts.pci_path.len() > 1;
    match (facts.vendor, facts.driver.as_deref()) {
        (Some(INTEL), Some("i915" | "xe")) if behind_bridge => GpuKind::Dedicated,
        (Some(INTEL), Some("i915" | "xe")) if address.ends_with(":00:02.0") => GpuKind::Integrated,
        (Some(NVIDIA), Some("nvidia" | "nouveau")) if behind_bridge => GpuKind::Dedicated,
        _ => GpuKind::Unknown,
    }
}

/// `0000:00:02.0`: a PCI address as sysfs names the device's directory.
fn is_pci_address(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 12
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b':',
            10 => *b == b'.',
            _ => b.is_ascii_hexdigit(),
        })
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Whether the DRM node with these device numbers (`226:129`) carries
/// `DISCRETE_TAG` in the udev database.
fn node_tagged(roots: &Roots, dev: &str) -> bool {
    let id = format!("c{dev}");
    if roots.udev.join("tags").join(DISCRETE_TAG).join(&id).exists() {
        return true;
    }
    fs::read_to_string(roots.udev.join("data").join(&id))
        .is_ok_and(|data| data.lines().any(|l| l.strip_prefix("G:") == Some(DISCRETE_TAG)))
}

/// The facts about the GPU whose device directory (under `/sys/devices`)
/// is `device`.
fn facts_of(roots: &Roots, device: &Path) -> GpuFacts {
    let devices = fs::canonicalize(roots.sys.join("devices")).unwrap_or_else(|_| roots.sys.join("devices"));
    let mut components = device
        .strip_prefix(&devices)
        .map(|rest| rest.iter().filter_map(|c| c.to_str()).map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter();
    // `pci0000:00/0000:00:06.0/0000:02:00.0`: a host bridge, then the chain.
    let pci_path = match components.next() {
        Some(host) if host.starts_with("pci") => components.take_while(|c| is_pci_address(c)).collect(),
        _ => Vec::new(),
    };
    let vendor = read_trimmed(&device.join("vendor"))
        .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let driver = fs::read_link(device.join("driver"))
        .ok()
        .and_then(|link| link.file_name().and_then(|n| n.to_str()).map(str::to_string));
    let discrete_tag = fs::read_dir(device.join("drm")).is_ok_and(|nodes| {
        nodes
            .filter_map(Result::ok)
            .filter_map(|node| read_trimmed(&node.path().join("dev")))
            .any(|dev| node_tagged(roots, &dev))
    });
    GpuFacts { vendor, driver, pci_path, discrete_tag }
}

/// The kind of the GPU behind a DRM node (`/dev/dri/renderD129`).
fn node_kind(roots: &Roots, node_path: &str) -> GpuKind {
    let Some(node) = Path::new(node_path).file_name().and_then(|n| n.to_str()) else { return GpuKind::Unknown };
    if !(node.starts_with("renderD") || node.starts_with("card")) {
        return GpuKind::Unknown;
    }
    match fs::canonicalize(roots.sys.join("class/drm").join(node).join("device")) {
        Ok(device) => classify(&facts_of(roots, &device)),
        Err(_) => GpuKind::Unknown,
    }
}

/// The kind of the NVIDIA GPUs: NVDEC names no device a decoder runs on
/// (only a CUDA device number, in an order of CUDA's own), so this is a
/// kind only when every NVIDIA GPU on the machine is of it.
fn nvidia_kind(roots: &Roots) -> GpuKind {
    let Ok(entries) = fs::read_dir(roots.sys.join("bus/pci/devices")) else { return GpuKind::Unknown };
    let kinds: Vec<GpuKind> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::canonicalize(entry.path()).ok())
        .filter(|device| read_trimmed(&device.join("class")).is_some_and(|c| c.starts_with("0x03")))
        .map(|device| facts_of(roots, &device))
        .filter(|facts| facts.vendor == Some(NVIDIA))
        .map(|facts| classify(&facts))
        .collect();
    match kinds.split_first() {
        Some((first, rest)) if rest.iter().all(|k| k == first) => *first,
        _ => GpuKind::Unknown,
    }
}

/// The device a decoder factory decodes on, as far as it tells.
enum DecoderDevice {
    /// A DRM node (`/dev/dri/renderD129`).
    Node(String),
    /// One of the NVIDIA GPUs.
    Nvidia,
    Unknown,
}

/// VA-API decoders name their DRM node in `device-path`, read from an
/// element that is created for it and never started; NVDEC decoders run on
/// an NVIDIA GPU. Every other decoder's device is not known here. Creating
/// the element opens no device once the va plugin is loaded, but loading it
/// does: the plugin opens every DRM render node and initialises the VA
/// driver of each. Without `load`, a VA-API decoder whose plugin is not
/// loaded yet stays `Unknown`.
fn decoder_device(factory: &gstreamer::ElementFactory, load: bool) -> DecoderDevice {
    match factory.plugin_name().as_deref() {
        Some("va") if load || factory.plugin().is_some_and(|plugin| plugin.is_loaded()) => factory
            .create()
            .build()
            .ok()
            .filter(|element| element.find_property("device-path").is_some())
            .and_then(|element| element.property_value("device-path").get::<Option<String>>().ok().flatten())
            .map_or(DecoderDevice::Unknown, DecoderDevice::Node),
        Some("nvcodec") => DecoderDevice::Nvidia,
        _ => DecoderDevice::Unknown,
    }
}

/// The kind of GPU a hardware decoder factory decodes on. For a VA-API
/// decoder this loads the va plugin if nothing has yet (see
/// `decoder_device`), as the first stream decoded with VA-API does: only
/// the "Auto" modes ask this, which order the VA-API decoders themselves.
pub fn decoder_kind(factory: &gstreamer::ElementFactory) -> GpuKind {
    kind_of(factory, true)
}

/// `decoder_kind` without loading a plugin: a VA-API decoder's GPU is
/// `Unknown` until a stream or an "Auto" mode has loaded the va plugin.
/// System Status asks this, which "Hardware" and "Software" open too.
pub fn loaded_decoder_kind(factory: &gstreamer::ElementFactory) -> GpuKind {
    kind_of(factory, false)
}

fn kind_of(factory: &gstreamer::ElementFactory, load: bool) -> GpuKind {
    let roots = Roots::system();
    match decoder_device(factory, load) {
        DecoderDevice::Node(path) => node_kind(&roots, &path),
        DecoderDevice::Nvidia => nvidia_kind(&roots),
        DecoderDevice::Unknown => GpuKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A sysfs and udev tree for one test, removed again afterwards.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("reoling-gpu-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("sys/devices")).unwrap();
            fs::create_dir_all(root.join("sys/class/drm")).unwrap();
            fs::create_dir_all(root.join("sys/bus/pci/devices")).unwrap();
            fs::create_dir_all(root.join("udev/data")).unwrap();
            Fixture { root }
        }

        fn roots(&self) -> Roots {
            Roots { sys: self.root.join("sys"), udev: self.root.join("udev") }
        }

        /// A PCI GPU at `path` under `/sys/devices` (`pci0000:00/0000:00:02.0`)
        /// with the render node `renderD<minor>`, as the kernel lays them out.
        fn gpu(&self, path: &str, vendor: &str, driver: &str, minor: u32) {
            let sys = self.root.join("sys");
            let device = sys.join("devices").join(path);
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("vendor"), format!("{vendor}\n")).unwrap();
            fs::write(device.join("class"), "0x030000\n").unwrap();
            let drivers = sys.join("bus/pci/drivers").join(driver);
            fs::create_dir_all(&drivers).unwrap();
            symlink(&drivers, device.join("driver")).unwrap();
            let node = format!("renderD{minor}");
            let node_dir = device.join("drm").join(&node);
            fs::create_dir_all(&node_dir).unwrap();
            fs::write(node_dir.join("dev"), format!("226:{minor}\n")).unwrap();
            symlink("../..", node_dir.join("device")).unwrap();
            symlink(&node_dir, sys.join("class/drm").join(&node)).unwrap();
            let address = Path::new(path).file_name().unwrap();
            symlink(&device, sys.join("bus/pci/devices").join(address)).unwrap();
        }

        /// What switcheroo-control's rules leave in the udev database for a
        /// dedicated GPU's render node.
        fn tag(&self, minor: u32) {
            let tags = self.root.join("udev/tags").join(DISCRETE_TAG);
            fs::create_dir_all(&tags).unwrap();
            fs::write(tags.join(format!("c226:{minor}")), "").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn intel_gpu_on_the_root_bus_is_integrated() {
        let f = Fixture::new("intel-igpu");
        f.gpu("pci0000:00/0000:00:02.0", "0x8086", "i915", 129);
        let facts = facts_of(&f.roots(), &fs::canonicalize(f.root.join("sys/class/drm/renderD129/device")).unwrap());
        assert_eq!(facts.vendor, Some(INTEL));
        assert_eq!(facts.driver.as_deref(), Some("i915"));
        assert_eq!(facts.pci_path, vec!["0000:00:02.0".to_string()]);
        assert!(!facts.discrete_tag);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD129"), GpuKind::Integrated);
    }

    #[test]
    fn intel_card_behind_a_bridge_is_dedicated() {
        let f = Fixture::new("intel-arc");
        f.gpu("pci0000:00/0000:00:01.0/0000:01:00.0/0000:02:01.0/0000:03:00.0", "0x8086", "xe", 130);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD130"), GpuKind::Dedicated);
    }

    #[test]
    fn nvidia_card_behind_a_bridge_is_dedicated() {
        // As on the machine this was written on: Intel at 00:02.0, NVIDIA
        // behind the root port 00:06.0, tagged by switcheroo-control.
        let f = Fixture::new("nvidia");
        f.gpu("pci0000:00/0000:00:02.0", "0x8086", "i915", 129);
        f.gpu("pci0000:00/0000:00:06.0/0000:02:00.0", "0x10de", "nvidia", 128);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD128"), GpuKind::Dedicated);
        assert_eq!(nvidia_kind(&f.roots()), GpuKind::Dedicated);
        f.tag(128);
        assert_eq!(nvidia_kind(&f.roots()), GpuKind::Dedicated);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD129"), GpuKind::Integrated);
    }

    #[test]
    fn amd_is_classified_by_the_tag_alone() {
        // An APU sits behind an internal bridge too, and its "VRAM" is
        // carved out of system memory: neither tells it from a card.
        let f = Fixture::new("amd");
        f.gpu("pci0000:00/0000:00:08.1/0000:c1:00.0", "0x1002", "amdgpu", 128);
        f.gpu("pci0000:00/0000:00:01.1/0000:01:00.0/0000:02:00.0/0000:03:00.0", "0x1002", "amdgpu", 129);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD128"), GpuKind::Unknown);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD129"), GpuKind::Unknown);
        // switcheroo-control asked amdgpu, which knows.
        f.tag(129);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD128"), GpuKind::Unknown);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD129"), GpuKind::Dedicated);
    }

    #[test]
    fn the_tag_is_read_from_the_udev_data_file_as_well() {
        let f = Fixture::new("udev-data");
        f.gpu("pci0000:00/0000:00:03.1/0000:0a:00.0", "0x1002", "amdgpu", 128);
        fs::write(f.root.join("udev/data/c226:128"), "E:ID_PATH=pci-0000:0a:00.0\nG:seat\nG:switcheroo-discrete-gpu\n")
            .unwrap();
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD128"), GpuKind::Dedicated);
    }

    #[test]
    fn missing_or_odd_devices_are_unknown() {
        let f = Fixture::new("missing");
        // No such node, no sysfs at all, not a DRM node.
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD128"), GpuKind::Unknown);
        let none = Roots { sys: f.root.join("nowhere"), udev: f.root.join("nowhere") };
        assert_eq!(node_kind(&none, "/dev/dri/renderD128"), GpuKind::Unknown);
        assert_eq!(nvidia_kind(&none), GpuKind::Unknown);
        assert_eq!(node_kind(&f.roots(), "/dev/null"), GpuKind::Unknown);
        assert_eq!(node_kind(&f.roots(), ""), GpuKind::Unknown);
        // An Intel GPU on the root bus elsewhere than 00:02.0 (a virtual
        // machine's), or one with no driver bound.
        f.gpu("pci0000:00/0000:00:05.0", "0x8086", "i915", 130);
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD130"), GpuKind::Unknown);
        let device = f.root.join("sys/devices/pci0000:00/0000:00:05.0");
        fs::remove_file(device.join("driver")).unwrap();
        fs::write(device.join("vendor"), "garbage").unwrap();
        assert_eq!(node_kind(&f.roots(), "/dev/dri/renderD130"), GpuKind::Unknown);
        // No NVIDIA GPU at all.
        assert_eq!(nvidia_kind(&f.roots()), GpuKind::Unknown);
    }

    #[test]
    fn nvidia_gpus_of_different_kinds_are_unknown() {
        let f = Fixture::new("nvidia-mixed");
        f.gpu("pci0000:00/0000:00:01.0/0000:01:00.0", "0x10de", "nvidia", 128);
        f.gpu("pci0000:00/0000:00:10.0", "0x10de", "nvidia", 129);
        assert_eq!(nvidia_kind(&f.roots()), GpuKind::Unknown);
    }

    #[test]
    fn a_gpu_that_is_no_pci_device_is_unknown() {
        let facts = GpuFacts { vendor: Some(NVIDIA), driver: Some("nvidia".into()), ..Default::default() };
        assert_eq!(classify(&facts), GpuKind::Unknown);
        let tagged = GpuFacts { discrete_tag: true, ..facts };
        assert_eq!(classify(&tagged), GpuKind::Dedicated);
    }

    #[test]
    fn pci_addresses() {
        assert!(is_pci_address("0000:00:02.0"));
        assert!(is_pci_address("0000:c1:00.0"));
        assert!(!is_pci_address("pci0000:00"));
        assert!(!is_pci_address("drm"));
        assert!(!is_pci_address("0000:00:02"));
    }
}
