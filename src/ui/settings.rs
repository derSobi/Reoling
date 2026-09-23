//! Application settings: colour theme and video decoding. Stored in a small
//! key file next to the device list and applied at start and on change.

use glib::translate::{FromGlib, IntoGlib};
use gstreamer::prelude::*;
use gtk4::glib::{self, KeyFile, KeyFileFlags};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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
    /// GStreamer's own choice (what `decodebin` ranks highest).
    #[default]
    Auto,
    Hardware,
    Software,
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
            decoding: match get("decoding").as_deref() {
                Some("hardware") => Decoding::Hardware,
                Some("software") => Decoding::Software,
                _ => Decoding::Auto,
            },
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
        file.set_string(
            "settings",
            "decoding",
            match self.decoding {
                Decoding::Auto => "auto",
                Decoding::Hardware => "hardware",
                Decoding::Software => "software",
            },
        );
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
    if !name.is_empty() {
        settings.set_gtk_theme_name(Some(&name));
    }
    settings.set_gtk_application_prefer_dark_theme(dark);
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
    let mut out: Vec<HardwareDecoder> = Vec::new();
    for (factory, hardware) in video_decoders() {
        // A rank of zero means decodebin never picks it (and could not plug
        // the converters its output needs): offering it gives a black picture.
        if !hardware || original_rank(&factory) <= 0 {
            continue;
        }
        let (key, label) = hardware_key(&factory);
        if !out.iter().any(|d| d.key == key) {
            out.push(HardwareDecoder { key, label });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
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

    let chosen = settings
        .hardware_decoder
        .as_ref()
        .filter(|key| hardware_decoders().iter().any(|d| &d.key == *key));
    for (factory, hardware) in &decoders {
        restore(factory);
        if settings.decoding != Decoding::Hardware {
            continue;
        }
        // Hardware only: software decoders are never picked; with a chosen
        // device, the other devices are not either.
        let excluded = if *hardware {
            chosen.is_some_and(|key| hardware_key(factory).0 != *key)
        } else {
            true
        };
        if excluded {
            never(factory);
        }
    }
    FORCE_SOFTWARE.store(settings.decoding == Decoding::Software, Ordering::Relaxed);
}
