//! Application settings: colour theme and video decoding. Stored in a small
//! key file next to the device list and applied at start and on change.

use crate::ui::gpu::{self, GpuKind};
use glib::translate::{FromGlib, IntoGlib};
use gstreamer::prelude::*;
use gtk4::glib::{self, KeyFile, KeyFileFlags};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Whatever the system is set to.
    #[default]
    Auto,
    Light,
    Dark,
}

/// How long the picture is held back to ride out network jitter: shorter
/// reacts faster (camera movement), longer smooths over a bad connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Latency {
    /// 300 ms
    Low,
    /// 1 s
    #[default]
    Balanced,
    /// 3 s
    Smooth,
}

impl Latency {
    pub fn millis(self) -> u64 {
        match self {
            Latency::Low => 300,
            Latency::Balanced => 1000,
            Latency::Smooth => 3000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decoding {
    /// A hardware decoder, the integrated GPU's ahead of a dedicated one's
    /// (see `auto_ranks`); software when none fits.
    #[default]
    AutoIntegrated,
    /// The same with the dedicated GPU's first.
    AutoDedicated,
    Hardware,
    Software,
}

impl Decoding {
    /// The mode stored as `value` in the settings file. "auto" is what
    /// Reoling 1.0.0 wrote for its one "Auto" mode; it stands for the
    /// default. Anything unknown is the default too.
    fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("auto_dedicated") => Decoding::AutoDedicated,
            Some("hardware") => Decoding::Hardware,
            Some("software") => Decoding::Software,
            _ => Decoding::AutoIntegrated,
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Decoding::AutoIntegrated => "auto",
            Decoding::AutoDedicated => "auto_dedicated",
            Decoding::Hardware => "hardware",
            Decoding::Software => "software",
        }
    }

    /// The kind of GPU an "Auto" mode tries first.
    fn preferred_gpu(self) -> Option<GpuKind> {
        match self {
            Decoding::AutoIntegrated => Some(GpuKind::Integrated),
            Decoding::AutoDedicated => Some(GpuKind::Dedicated),
            Decoding::Hardware | Decoding::Software => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub theme: Theme,
    pub decoding: Decoding,
    pub latency: Latency,
    /// With `Decoding::Hardware`: the decoder group to use (see
    /// `hardware_decoders`); `None` leaves the pick among them to GStreamer.
    pub hardware_decoder: Option<String>,
    /// 0.0..=1.0
    pub volume: f64,
    /// Fill the whole view with the picture instead of keeping its aspect.
    pub stretch: bool,
    /// Start showing the last watched device as soon as it connects.
    pub auto_live_view: bool,
    /// Where snapshots / recordings go; `None` is `~/Pictures/Reoling` /
    /// `~/Videos/Reoling`.
    pub screenshot_dir: Option<String>,
    pub recording_dir: Option<String>,
}

fn path() -> PathBuf {
    glib::user_config_dir().join("reoling").join("settings.ini")
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            decoding: Decoding::default(),
            latency: Latency::default(),
            hardware_decoder: None,
            volume: 0.5,
            stretch: false,
            auto_live_view: true,
            screenshot_dir: None,
            recording_dir: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let file = KeyFile::new();
        if file.load_from_file(path(), KeyFileFlags::NONE).is_err() {
            return Self::default();
        }
        let get = |k: &str| file.string("settings", k).ok().map(|s| s.to_string());
        Self {
            theme: match get("theme").as_deref() {
                Some("light") => Theme::Light,
                Some("dark") => Theme::Dark,
                _ => Theme::Auto,
            },
            decoding: Decoding::from_setting(get("decoding").as_deref()),
            latency: match get("latency").as_deref() {
                Some("low") => Latency::Low,
                Some("smooth") => Latency::Smooth,
                _ => Latency::Balanced,
            },
            hardware_decoder: get("hardware_decoder").filter(|s| !s.is_empty()),
            stretch: get("stretch").as_deref() == Some("true"),
            auto_live_view: get("auto_live_view").as_deref() != Some("false"),
            screenshot_dir: get("screenshot_dir").filter(|s| !s.is_empty()),
            recording_dir: get("recording_dir").filter(|s| !s.is_empty()),
            volume: get("volume").and_then(|v| v.parse().ok()).map_or(0.5, |v: f64| v.clamp(0.0, 1.0)),
        }
    }

    pub fn save(&self) {
        let file = KeyFile::new();
        file.set_string(
            "settings",
            "theme",
            match self.theme {
                Theme::Auto => "auto",
                Theme::Light => "light",
                Theme::Dark => "dark",
            },
        );
        file.set_string("settings", "decoding", self.decoding.setting());
        file.set_string(
            "settings",
            "latency",
            match self.latency {
                Latency::Low => "low",
                Latency::Balanced => "balanced",
                Latency::Smooth => "smooth",
            },
        );
        file.set_string("settings", "volume", &self.volume.to_string());
        file.set_string("settings", "stretch", &self.stretch.to_string());
        file.set_string("settings", "auto_live_view", &self.auto_live_view.to_string());
        file.set_string("settings", "screenshot_dir", self.screenshot_dir.as_deref().unwrap_or(""));
        file.set_string("settings", "recording_dir", self.recording_dir.as_deref().unwrap_or(""));
        file.set_string("settings", "hardware_decoder", self.hardware_decoder.as_deref().unwrap_or(""));
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = file.save_to_file(&path) {
            eprintln!("could not save the settings: {e}");
        }
    }

    /// Applies both settings. Decoding takes effect for streams started
    /// afterwards.
    pub fn apply(&self) {
        apply_theme(self.theme);
        apply_decoding(self);
        LATENCY_MS.store(self.latency.millis(), Ordering::Relaxed);
    }
}

/// Where snapshots (Pictures) or recordings and downloads (Videos) go: the
/// folder `chosen` in the settings, else `~/Pictures/Reoling` /
/// `~/Videos/Reoling`, created on demand.
pub fn media_dir(chosen: Option<&str>, kind: glib::UserDirectory) -> Option<PathBuf> {
    let dir = match chosen {
        Some(dir) => PathBuf::from(dir),
        None => glib::user_special_dir(kind)?.join("Reoling"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

// --- theme -----------------------------------------------------------------

/// What the system had set when we started, to go back to on "Auto".
struct SystemTheme {
    name: Option<String>,
    prefers_dark: bool,
}

static SYSTEM_THEME: OnceLock<SystemTheme> = OnceLock::new();

/// Whether a GTK 4 theme of this name is installed.
fn theme_installed(name: &str) -> bool {
    let mut dirs = vec![
        glib::home_dir().join(".themes"),
        glib::user_data_dir().join("themes"),
    ];
    dirs.extend(glib::system_data_dirs().into_iter().map(|d| d.join("themes")));
    dirs.iter().any(|d| d.join(name).join("gtk-4.0").is_dir())
}

/// Light and dark are usually separate themes of one family ("Yaru" and
/// "Yaru-dark"), and a "-dark" theme ignores the dark-preference flag, so
/// the variant is picked by name where one exists.
fn apply_theme(theme: Theme) {
    let Some(settings) = gtk4::Settings::default() else { return };
    let system = SYSTEM_THEME.get_or_init(|| SystemTheme {
        name: settings.gtk_theme_name().map(|n| n.to_string()),
        prefers_dark: settings.is_gtk_application_prefer_dark_theme(),
    });
    let system_name = system.name.clone().unwrap_or_default();
    let base = system_name.trim_end_matches("-dark").to_string();
    let (name, dark) = match theme {
        Theme::Auto => (system_name, system.prefers_dark),
        Theme::Light => (base, false),
        Theme::Dark => {
            let variant = format!("{base}-dark");
            (if theme_installed(&variant) { variant } else { system_name }, true)
        }
    };
    let current = settings.gtk_theme_name();
    let (set_name, set_dark) =
        theme_changes(theme, current.as_deref(), settings.is_gtk_application_prefer_dark_theme(), &name, dark);
    if let Some(name) = set_name {
        settings.set_gtk_theme_name(Some(name));
    }
    if let Some(dark) = set_dark {
        settings.set_gtk_application_prefer_dark_theme(dark);
    }
}

/// Which of the two settings have to be set for `theme`, the theme `name`
/// (left alone when empty) with the dark preference `dark`, while GTK has
/// `current_name` and `current_dark`. Setting either one reloads the whole
/// theme, even to the value it already has (three theme loads at every
/// start, two of them for nothing), so an unchanged one is not set, with
/// one exception: a name the app sets is its own, and GTK no longer takes
/// the desktop's from then on. "Light" and "Dark" set it even unchanged, so
/// that switching the desktop's style does not override them; "Auto" leaves
/// it to follow the desktop.
fn theme_changes<'a>(
    theme: Theme,
    current_name: Option<&str>,
    current_dark: bool,
    name: &'a str,
    dark: bool,
) -> (Option<&'a str>, Option<bool>) {
    let set_name = (!name.is_empty() && (theme != Theme::Auto || current_name != Some(name))).then_some(name);
    let set_dark = (current_dark != dark).then_some(dark);
    (set_name, set_dark)
}

// --- decoding --------------------------------------------------------------

static FORCE_SOFTWARE: AtomicBool = AtomicBool::new(false);
static LATENCY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);

/// How long new pipelines hold the picture (and sound) back.
pub fn latency() -> std::time::Duration {
    std::time::Duration::from_millis(LATENCY_MS.load(Ordering::Relaxed))
}

/// Whether new pipelines must use software decoders only.
pub fn force_software() -> bool {
    FORCE_SOFTWARE.load(Ordering::Relaxed)
}

/// One hardware decoder (a GPU or a decoding engine), possibly offering both
/// H.264 and H.265.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareDecoder {
    pub key: String,
    pub label: String,
}

/// The video decoder factories GStreamer knows, with whether each is
/// hardware, for H.264/H.265 (the codecs the devices send).
fn video_decoders() -> Vec<(gstreamer::ElementFactory, bool)> {
    let registry = gstreamer::Registry::get();
    registry
        .features(gstreamer::ElementFactory::static_type())
        .into_iter()
        .filter_map(|f| f.downcast::<gstreamer::ElementFactory>().ok())
        .filter(|f| {
            let klass = f.klass();
            klass.contains("Decoder") && klass.contains("Video")
        })
        .filter(|f| {
            f.static_pad_templates().iter().any(|t| {
                t.direction() == gstreamer::PadDirection::Sink
                    && (t.caps().to_string().contains("video/x-h264")
                        || t.caps().to_string().contains("video/x-h265"))
            })
        })
        .map(|f| {
            let hardware = f.klass().contains("Hardware");
            (f, hardware)
        })
        .collect()
}

/// A hardware decoder factory's device: its plugin plus, for VA-API, the GPU
/// named in its long name.
fn hardware_key(factory: &gstreamer::ElementFactory) -> (String, String) {
    let long = factory.metadata("long-name").unwrap_or_default();
    let device = long.split_once(" in ").map(|(_, d)| d.to_string()).unwrap_or_default();
    let plugin = factory.plugin_name().map(|p| p.to_string()).unwrap_or_default();
    let label = long
        .replace(" H.264", "")
        .replace(" H.265", "")
        .replace(" Decoder", "");
    (format!("{plugin}|{device}"), label)
}

const AUTOSTART_FILE: &str = "de.dersobi.reoling.desktop";

fn autostart_path() -> PathBuf {
    glib::user_config_dir().join("autostart").join(AUTOSTART_FILE)
}

/// Whether Reoling starts with the desktop session.
pub fn autostart_enabled() -> bool {
    autostart_path().is_file()
}

/// Adds or removes the XDG autostart entry.
pub fn set_autostart(on: bool) {
    let path = autostart_path();
    if !on {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "reoling".to_string());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Reoling\nComment=Client for Reolink cameras\nExec={exe}\nIcon=de.dersobi.reoling\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, entry) {
        eprintln!("could not enable autostart: {e}");
    }
}

/// The hardware decoders on this machine, one entry per device.
pub fn hardware_decoders() -> Vec<HardwareDecoder> {
    hardware_devices().into_iter().map(|(decoder, _)| decoder).collect()
}

/// The hardware decoders, each with the kind of GPU it decodes on as far as
/// that is known without loading a plugin (see `gpu::loaded_decoder_kind`),
/// for System Status.
pub fn hardware_decoder_kinds() -> Vec<(HardwareDecoder, GpuKind)> {
    hardware_devices().into_iter().map(|(decoder, factory)| (decoder, gpu::loaded_decoder_kind(&factory))).collect()
}

/// `hardware_decoders`, each with the first of its factories.
fn hardware_devices() -> Vec<(HardwareDecoder, gstreamer::ElementFactory)> {
    let mut out: Vec<(HardwareDecoder, gstreamer::ElementFactory)> = Vec::new();
    for (factory, hardware) in video_decoders() {
        // A rank of zero means decodebin never picks it (and could not plug
        // the converters its output needs): offering it gives a black picture.
        if !hardware || original_rank(&factory) <= 0 {
            continue;
        }
        let (key, label) = hardware_key(&factory);
        if !out.iter().any(|(d, _)| d.key == key) {
            out.push((HardwareDecoder { key, label }, factory));
        }
    }
    out.sort_by(|a, b| a.0.label.cmp(&b.0.label));
    out
}

/// The ranks decoders had before we touched any, to restore on "Auto".
static ORIGINAL_RANKS: OnceLock<Vec<(String, i32)>> = OnceLock::new();

/// A decoder's rank as GStreamer had it, whatever we changed since.
fn original_rank(factory: &gstreamer::ElementFactory) -> i32 {
    ORIGINAL_RANKS
        .get()
        .and_then(|all| all.iter().find(|(n, _)| *n == factory.name().as_str()))
        .map_or_else(|| factory.rank().into_glib(), |(_, rank)| *rank)
}

/// The mode that is applied for `wanted`: "Hardware" needs a hardware
/// decoder to pick, and without one it would take every decoder out of
/// decodebin's reach (a black picture), so it then behaves as "Auto".
fn effective_decoding(wanted: Decoding, hardware_present: bool) -> Decoding {
    match wanted {
        Decoding::Hardware if !hardware_present => Decoding::default(),
        other => other,
    }
}

/// Whether a decoder is taken out of decodebin's reach (rank NONE) in
/// `mode`: in "Hardware", every software decoder, and with a chosen
/// device (`chosen`, see `hardware_key`) the other devices too. "Auto"
/// and "Software" take none out ("Auto" orders them, see `auto_ranks`;
/// "Software" asks decodebin itself for software decoders).
fn excluded(mode: Decoding, hardware: bool, key: &str, chosen: Option<&str>) -> bool {
    mode == Decoding::Hardware && (!hardware || chosen.is_some_and(|c| c != key))
}

/// One video decoder as `auto_ranks` sees it.
#[derive(Debug, Clone)]
struct Ranked {
    name: String,
    /// The rank GStreamer gave it.
    rank: i32,
    hardware: bool,
    /// For a hardware decoder, the kind of GPU it decodes on.
    kind: GpuKind,
}

/// The ranks the "Auto" modes give decoders, as (name, rank) for each one
/// that changes. Every hardware decoder GStreamer ranks primary or higher
/// (one it means decodebin to pick on its own) moves above every software
/// decoder: first those on the `prefer` kind of GPU, then those on a GPU of
/// unknown kind (which may well be of that kind), then those on the other
/// kind, in GStreamer's own order within each. Each gets a rank of its own,
/// so decodebin's tie-break by name never picks between two GPUs (it took
/// NVDEC over VA-API, "nv" before "va", both at primary + 1). Software
/// decoders and the hardware ones GStreamer ranks lower (its fallbacks,
/// such as Quick Sync) keep their ranks: without a hardware decoder that
/// fits, decodebin goes on to software as before.
fn auto_ranks(decoders: &[Ranked], prefer: GpuKind) -> Vec<(String, i32)> {
    let primary = gstreamer::Rank::PRIMARY.into_glib();
    let top_software = decoders.iter().filter(|d| !d.hardware).map(|d| d.rank).max().unwrap_or(primary).max(primary);
    let tier = |kind: GpuKind| match kind {
        k if k == prefer => 0,
        GpuKind::Unknown => 1,
        _ => 2,
    };
    let mut moved: Vec<&Ranked> = decoders.iter().filter(|d| d.hardware && d.rank >= primary).collect();
    moved.sort_by(|a, b| tier(a.kind).cmp(&tier(b.kind)).then(b.rank.cmp(&a.rank)).then(a.name.cmp(&b.name)));
    let count = moved.len() as i32;
    moved.iter().enumerate().map(|(i, d)| (d.name.clone(), top_software + count - i as i32)).collect()
}

/// Whether the "Hardware" setting is running in software for want of a
/// hardware decoder (reported once on stderr and in System Status).
static HARDWARE_FALLBACK: AtomicBool = AtomicBool::new(false);

/// `true` while "Hardware" is set but no hardware decoder exists, so
/// decoding runs in software.
pub fn hardware_fallback() -> bool {
    HARDWARE_FALLBACK.load(Ordering::Relaxed)
}

fn apply_decoding(settings: &Settings) {
    let decoders = video_decoders();
    let originals = ORIGINAL_RANKS.get_or_init(|| {
        decoders
            .iter()
            .map(|(f, _)| (f.name().to_string(), f.rank().into_glib()))
            .collect()
    });
    let restore = |f: &gstreamer::ElementFactory| {
        if let Some((_, rank)) = originals.iter().find(|(n, _)| *n == f.name().as_str()) {
            f.set_rank(unsafe { gstreamer::Rank::from_glib(*rank) });
        }
    };
    let never = |f: &gstreamer::ElementFactory| f.set_rank(gstreamer::Rank::NONE);

    let available = hardware_decoders();
    let mode = effective_decoding(settings.decoding, !available.is_empty());
    let fallback = mode != settings.decoding;
    if fallback && !HARDWARE_FALLBACK.swap(true, Ordering::Relaxed) {
        eprintln!("no hardware decoder found: \"Hardware\" decoding runs in software");
    }
    if !fallback {
        HARDWARE_FALLBACK.store(false, Ordering::Relaxed);
    }
    // A chosen device that is not there any more does not narrow the pick.
    let chosen = settings.hardware_decoder.as_deref().filter(|key| available.iter().any(|d| d.key == *key));
    for (factory, hardware) in &decoders {
        restore(factory);
        if excluded(mode, *hardware, &hardware_key(factory).0, chosen) {
            never(factory);
        }
    }
    if let Some(prefer) = mode.preferred_gpu() {
        let primary = gstreamer::Rank::PRIMARY.into_glib();
        let ranked: Vec<Ranked> = decoders
            .iter()
            .map(|(factory, hardware)| {
                let rank = original_rank(factory);
                // Only the decoders `auto_ranks` moves need their GPU's kind
                // (asking a VA-API one loads the va plugin: `gpu::decoder_kind`).
                let kind = if *hardware && rank >= primary { gpu::decoder_kind(factory) } else { GpuKind::Unknown };
                Ranked { name: factory.name().to_string(), rank, hardware: *hardware, kind }
            })
            .collect();
        let ranks = auto_ranks(&ranked, prefer);
        for (name, rank) in &ranks {
            if let Some((factory, _)) = decoders.iter().find(|(f, _)| f.name() == name.as_str()) {
                factory.set_rank(gstreamer::Rank::from(*rank));
            }
        }
        if std::env::var("REOLING_DEBUG_DECODER").is_ok() {
            let order: Vec<String> = ranks
                .iter()
                .map(|(name, rank)| {
                    let kind = ranked.iter().find(|d| d.name == *name).map_or(GpuKind::Unknown, |d| d.kind);
                    format!("{name} {rank} ({})", kind.label())
                })
                .collect();
            eprintln!("DECODING ORDER {:?}: {}, then software", mode, order.join(", "));
        }
    }
    FORCE_SOFTWARE.store(mode == Decoding::Software, Ordering::Relaxed);
}

/// The video decoders the latest live and recorded streams started with
/// (factory names), for System Status.
static LIVE_DECODER: Mutex<Option<String>> = Mutex::new(None);
static REPLAY_DECODER: Mutex<Option<String>> = Mutex::new(None);

/// Notes the video decoder a live (`replay` false) or recorded stream
/// started with.
pub fn note_decoder(factory: &gstreamer::ElementFactory, replay: bool) {
    let slot = if replay { &REPLAY_DECODER } else { &LIVE_DECODER };
    if let Ok(mut name) = slot.lock() {
        *name = Some(factory.name().to_string());
    }
}

/// The decoder the latest live (`replay` false) or recorded stream started
/// with, described for people: the device and the kind of its GPU for a
/// hardware decoder, else that it decodes in software.
pub fn decoder_in_use(replay: bool) -> Option<String> {
    let slot = if replay { &REPLAY_DECODER } else { &LIVE_DECODER };
    let name = slot.lock().ok()?.clone()?;
    let Some(factory) = gstreamer::ElementFactory::find(&name) else { return Some(name) };
    if !factory.klass().contains("Hardware") {
        return Some(format!("{name}, software"));
    }
    let label = hardware_key(&factory).1;
    Some(match gpu::loaded_decoder_kind(&factory) {
        GpuKind::Unknown => format!("{label} ({name})"),
        kind => format!("{label} ({name}, {})", kind.label()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: [Decoding; 2] = [Decoding::AutoIntegrated, Decoding::AutoDedicated];

    #[test]
    fn auto_and_software_are_applied_as_set() {
        for present in [true, false] {
            for auto in AUTO {
                assert_eq!(effective_decoding(auto, present), auto);
            }
            assert_eq!(effective_decoding(Decoding::Software, present), Decoding::Software);
        }
    }

    #[test]
    fn hardware_needs_a_hardware_decoder() {
        assert_eq!(effective_decoding(Decoding::Hardware, true), Decoding::Hardware);
        // Without one, "Hardware" must not leave decodebin empty-handed.
        assert_eq!(effective_decoding(Decoding::Hardware, false), Decoding::AutoIntegrated);
    }

    #[test]
    fn auto_takes_no_decoder_out() {
        // `auto_ranks` orders them instead.
        for auto in AUTO {
            assert!(!excluded(auto, true, "nvcodec|", None));
            assert!(!excluded(auto, false, "libav|", None));
            assert!(!excluded(auto, true, "va|GPU A", Some("va|GPU B")));
        }
    }

    #[test]
    fn software_leaves_the_ranks_to_force_sw_decoders() {
        assert!(!excluded(Decoding::Software, true, "nvcodec|", None));
        assert!(!excluded(Decoding::Software, false, "libav|", None));
    }

    #[test]
    fn hardware_drops_software_and_other_devices() {
        assert!(excluded(Decoding::Hardware, false, "libav|", None));
        assert!(!excluded(Decoding::Hardware, true, "nvcodec|", None));
        assert!(!excluded(Decoding::Hardware, true, "nvcodec|", Some("nvcodec|")));
        assert!(excluded(Decoding::Hardware, true, "va|GPU A", Some("nvcodec|")));
        assert!(!excluded(Decoding::Hardware, true, "va|GPU A", Some("va|GPU A")));
    }

    #[test]
    fn a_device_chosen_by_1_0_0_still_narrows_hardware() {
        // The key 1.0.0 stored for the Intel GPU's VA-API decoders.
        let intel = "va|Intel(R) Gen Graphics";
        assert!(!excluded(Decoding::Hardware, true, intel, Some(intel)));
        assert!(excluded(Decoding::Hardware, true, "nvcodec|", Some(intel)));
        assert!(excluded(Decoding::Hardware, false, "libav|", Some(intel)));
    }

    #[test]
    fn decoding_settings_from_1_0_0_and_back() {
        // "auto" was 1.0.0's one Auto mode: it now prefers the integrated
        // GPU, and is still written so, for 1.0.0 to read it as Auto.
        assert_eq!(Decoding::from_setting(Some("auto")), Decoding::AutoIntegrated);
        assert_eq!(Decoding::from_setting(Some("hardware")), Decoding::Hardware);
        assert_eq!(Decoding::from_setting(Some("software")), Decoding::Software);
        assert_eq!(Decoding::from_setting(Some("auto_dedicated")), Decoding::AutoDedicated);
        for unknown in [None, Some(""), Some("fastest"), Some("AUTO")] {
            assert_eq!(Decoding::from_setting(unknown), Decoding::default());
        }
        assert_eq!(Decoding::default(), Decoding::AutoIntegrated);
        for mode in [Decoding::AutoIntegrated, Decoding::AutoDedicated, Decoding::Hardware, Decoding::Software] {
            assert_eq!(Decoding::from_setting(Some(mode.setting())), mode);
        }
        assert_eq!(Decoding::AutoIntegrated.setting(), "auto");
    }

    /// The H.264 and H.265 decoders of a machine like the one this was
    /// written on: an Intel GPU (VA-API, Quick Sync) and an NVIDIA card.
    fn this_machine(intel: GpuKind, nvidia: GpuKind) -> Vec<Ranked> {
        let d = |name: &str, rank: i32, hardware: bool, kind: GpuKind| Ranked {
            name: name.to_string(),
            rank,
            hardware,
            kind: if hardware { kind } else { GpuKind::Unknown },
        };
        vec![
            d("avdec_h264", 256, false, GpuKind::Unknown),
            d("avdec_h265", 256, false, GpuKind::Unknown),
            d("openh264dec", 64, false, GpuKind::Unknown),
            d("libde265dec", 128, false, GpuKind::Unknown),
            d("nvh264dec", 257, true, nvidia),
            d("nvh265dec", 257, true, nvidia),
            d("vah264dec", 257, true, intel),
            d("vah265dec", 257, true, intel),
            d("qsvh264dec", 64, true, intel),
            d("qsvh265dec", 64, true, intel),
            d("vulkanh264device1dec", 0, true, nvidia),
        ]
    }

    /// The names in the order decodebin tries them (rank, then name), with
    /// `ranks` applied.
    fn decodebin_order(decoders: &[Ranked], ranks: &[(String, i32)], codec: &str) -> Vec<String> {
        let mut all: Vec<(String, i32)> = decoders
            .iter()
            .filter(|d| d.name.contains(codec))
            .map(|d| (d.name.clone(), ranks.iter().find(|(n, _)| *n == d.name).map_or(d.rank, |(_, r)| *r)))
            .filter(|(_, rank)| *rank > 0)
            .collect();
        all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        all.into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn auto_integrated_tries_the_intel_gpu_then_the_card_then_software() {
        let decoders = this_machine(GpuKind::Integrated, GpuKind::Dedicated);
        let ranks = auto_ranks(&decoders, GpuKind::Integrated);
        assert_eq!(
            decodebin_order(&decoders, &ranks, "264"),
            ["vah264dec", "nvh264dec", "avdec_h264", "openh264dec", "qsvh264dec"]
        );
        assert_eq!(
            decodebin_order(&decoders, &ranks, "265"),
            ["vah265dec", "nvh265dec", "avdec_h265", "libde265dec", "qsvh265dec"]
        );
    }

    #[test]
    fn auto_dedicated_tries_the_card_then_the_intel_gpu_then_software() {
        let decoders = this_machine(GpuKind::Integrated, GpuKind::Dedicated);
        let ranks = auto_ranks(&decoders, GpuKind::Dedicated);
        assert_eq!(
            decodebin_order(&decoders, &ranks, "264"),
            ["nvh264dec", "vah264dec", "avdec_h264", "openh264dec", "qsvh264dec"]
        );
        assert_eq!(
            decodebin_order(&decoders, &ranks, "265"),
            ["nvh265dec", "vah265dec", "avdec_h265", "libde265dec", "qsvh265dec"]
        );
    }

    #[test]
    fn auto_ranks_are_distinct_above_software_and_leave_the_rest_alone() {
        for prefer in [GpuKind::Integrated, GpuKind::Dedicated] {
            let decoders = this_machine(GpuKind::Integrated, GpuKind::Dedicated);
            let ranks = auto_ranks(&decoders, prefer);
            let moved: Vec<&str> = ranks.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(moved.len(), 4);
            for name in ["vah264dec", "vah265dec", "nvh264dec", "nvh265dec"] {
                assert!(moved.contains(&name), "{name} not moved");
            }
            let mut values: Vec<i32> = ranks.iter().map(|(_, r)| *r).collect();
            values.sort();
            values.dedup();
            assert_eq!(values.len(), ranks.len(), "two decoders share a rank");
            let top_software = decoders.iter().filter(|d| !d.hardware).map(|d| d.rank).max().unwrap();
            assert!(values.iter().all(|r| *r > top_software));
        }
    }

    #[test]
    fn a_gpu_of_unknown_kind_comes_after_the_preferred_kind_before_the_other() {
        // An AMD APU (unknown without switcheroo-control) next to an NVIDIA
        // card: it may well be the integrated GPU asked for.
        let decoders = this_machine(GpuKind::Unknown, GpuKind::Dedicated);
        let ranks = auto_ranks(&decoders, GpuKind::Integrated);
        assert_eq!(decodebin_order(&decoders, &ranks, "264")[..2], ["vah264dec", "nvh264dec"]);
        let ranks = auto_ranks(&decoders, GpuKind::Dedicated);
        assert_eq!(decodebin_order(&decoders, &ranks, "264")[..2], ["nvh264dec", "vah264dec"]);
        // Both unknown: GStreamer's order, still with ranks of their own.
        let decoders = this_machine(GpuKind::Unknown, GpuKind::Unknown);
        let ranks = auto_ranks(&decoders, GpuKind::Integrated);
        assert_eq!(decodebin_order(&decoders, &ranks, "264")[..3], ["nvh264dec", "vah264dec", "avdec_h264"]);
        assert_ne!(ranks[0].1, ranks[1].1);
    }

    #[test]
    fn without_hardware_auto_changes_nothing() {
        let decoders: Vec<Ranked> = this_machine(GpuKind::Unknown, GpuKind::Unknown)
            .into_iter()
            .filter(|d| !d.hardware || d.rank < 256)
            .collect();
        for prefer in [GpuKind::Integrated, GpuKind::Dedicated] {
            assert!(auto_ranks(&decoders, prefer).is_empty());
            assert!(auto_ranks(&[], prefer).is_empty());
        }
    }

    #[test]
    fn auto_sets_no_theme_setting_gtk_already_has() {
        // "Auto" at start asks for what GTK already has: nothing reloads, and
        // the theme goes on following the desktop.
        let auto = Theme::Auto;
        assert_eq!(theme_changes(auto, Some("Yaru-dark"), true, "Yaru-dark", true), (None, None));
        assert_eq!(theme_changes(auto, Some("Adwaita"), false, "Adwaita", false), (None, None));
        // No theme name known: only the dark preference can be set.
        assert_eq!(theme_changes(auto, None, false, "", false), (None, None));
        assert_eq!(theme_changes(auto, None, false, "", true), (None, Some(true)));
        assert_eq!(theme_changes(Theme::Dark, None, false, "", true), (None, Some(true)));
    }

    #[test]
    fn light_and_dark_keep_their_theme_name_even_unchanged() {
        // Set even when GTK has it, so a desktop switching its style later
        // does not override the choice; the dark preference is not.
        assert_eq!(theme_changes(Theme::Dark, Some("Yaru-dark"), true, "Yaru-dark", true), (Some("Yaru-dark"), None));
        assert_eq!(theme_changes(Theme::Light, Some("Yaru"), false, "Yaru", false), (Some("Yaru"), None));
    }

    #[test]
    fn switching_the_theme_sets_what_changes() {
        // Yaru-dark (dark) -> Light: the light variant, no dark preference.
        assert_eq!(theme_changes(Theme::Light, Some("Yaru-dark"), true, "Yaru", false), (Some("Yaru"), Some(false)));
        // Light -> Dark.
        assert_eq!(theme_changes(Theme::Dark, Some("Yaru"), false, "Yaru-dark", true), (Some("Yaru-dark"), Some(true)));
        // Dark -> Auto on a dark desktop: nothing left to change.
        assert_eq!(theme_changes(Theme::Auto, Some("Yaru-dark"), true, "Yaru-dark", true), (None, None));
        // Light -> Auto on a dark desktop.
        assert_eq!(theme_changes(Theme::Auto, Some("Yaru"), false, "Yaru-dark", true), (Some("Yaru-dark"), Some(true)));
        // Dark -> Light where the theme has no "-dark" variant: the same
        // name, the preference flips.
        assert_eq!(
            theme_changes(Theme::Light, Some("Adwaita"), true, "Adwaita", false),
            (Some("Adwaita"), Some(false))
        );
    }
}
