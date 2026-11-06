//! What this system offers the app, checked once at start: the GTK and
//! GStreamer versions and the GStreamer elements each feature builds its
//! pipelines from. A missing part is reported (a dialog at start, System
//! Status, one line on stderr, `--check-requirements`) with what brings it on
//! this distribution, instead of a panic or a black picture. Everything that
//! decides is a pure function of `Facts`.

use crate::ui::settings::{self, Installed};
use gstreamer::prelude::*;
use gtk4::prelude::*;
use reoling::VideoType;
use std::sync::OnceLock;

/// The GTK and GStreamer our picture element is built for
/// (scripts/build-gst-gtk4-plugin.sh: gtk_v4_14, GStreamer 1.24's
/// libraries). Older ones cannot load it.
const SINK_GTK: (u32, u32) = (4, 14);
const SINK_GSTREAMER: (u32, u32) = (1, 24);
/// The AppImage that brings GStreamer, its plugins and the decoders along.
const EMBED_APPIMAGE: &str = concat!("Reoling-", env!("CARGO_PKG_VERSION"), "-embed-x86_64.AppImage");
const ISSUES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

/// One thing a feature cannot do without.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    /// A GStreamer element, by factory name.
    Element(&'static str),
    /// Any one of these (the AAC decoders).
    AnyOf(&'static [&'static str]),
    /// A decoder decodebin can pick for the codec, hardware or software.
    Decoder(VideoType),
    /// An audio output autoaudiosink can use.
    AudioOutput,
    /// An audio input autoaudiosrc can use.
    AudioInput,
}

/// The AAC decoders the sound's chains take, the first one there.
const AAC_DECODERS: &[&str] = &["avdec_aac", "faad"];

impl Need {
    /// How people read it.
    fn label(self) -> String {
        match self {
            Need::Element(name) => name.to_string(),
            Need::AnyOf(AAC_DECODERS) => "an AAC decoder".to_string(),
            Need::AnyOf(names) => names.join(" or "),
            Need::Decoder(video_type) => format!("an {} decoder", codec_label(video_type)),
            Need::AudioOutput => "an audio output".to_string(),
            Need::AudioInput => "an audio input".to_string(),
        }
    }

    /// Its part of `Report::dismiss_key`.
    fn key(self) -> String {
        match self {
            Need::Element(name) => name.to_string(),
            Need::AnyOf(AAC_DECODERS) => "aac-decoder".to_string(),
            Need::AnyOf(names) => names.join("|"),
            Need::Decoder(VideoType::H264) => "decoder-h264".to_string(),
            Need::Decoder(VideoType::H265) => "decoder-h265".to_string(),
            Need::AudioOutput => "audio-output".to_string(),
            Need::AudioInput => "audio-input".to_string(),
        }
    }
}

fn codec_label(video_type: VideoType) -> &'static str {
    match video_type {
        VideoType::H264 => "H.264",
        VideoType::H265 => "H.265",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feature {
    Picture,
    H264,
    H265,
    Sound,
    Talk,
    Recording,
    Downloads,
}

impl Feature {
    /// Without it no video of a codec (or none at all) can be shown.
    fn required(self) -> bool {
        matches!(self, Feature::Picture | Feature::H264 | Feature::H265)
    }

    /// How people read it, in a sentence.
    fn label(self) -> &'static str {
        match self {
            Feature::Picture => "video",
            Feature::H264 => "H.264 video",
            Feature::H265 => "H.265 video",
            Feature::Sound => "sound",
            Feature::Talk => "talk",
            Feature::Recording => "recording",
            Feature::Downloads => "downloads",
        }
    }

    /// How people read it, at the start of a line.
    fn title(self) -> &'static str {
        match self {
            Feature::Picture => "Video",
            Feature::H264 => "H.264 video",
            Feature::H265 => "H.265 video",
            Feature::Sound => "Sound",
            Feature::Talk => "Talk",
            Feature::Recording => "Recording",
            Feature::Downloads => "Downloads",
        }
    }

    /// Its part of `Report::dismiss_key`.
    fn key(self) -> &'static str {
        match self {
            Feature::Picture => "picture",
            Feature::H264 => "h264",
            Feature::H265 => "h265",
            Feature::Sound => "sound",
            Feature::Talk => "talk",
            Feature::Recording => "recording",
            Feature::Downloads => "downloads",
        }
    }
}

/// What each feature builds its pipelines from, taken from the code: the live
/// and replay pictures (video_view.rs `build_sink`, `build_replay_sink`), the
/// sound of both and the microphone (audio.rs, video_view.rs
/// `build_replay_audio`), a live recording (video_view.rs `Recorder`) and a
/// download (recording_file.rs). The parsers count for their codec, also in
/// a recording or a download. No GStreamer encoder: talk's ADPCM is our own.
const NEEDS: &[(Feature, &[Need])] = &[
    (
        Feature::Picture,
        &[
            Need::Element("appsrc"),
            Need::Element("queue"),
            Need::Element("decodebin"),
            Need::Element("videoconvert"),
            Need::Element("gtk4paintablesink"),
        ],
    ),
    (Feature::H264, &[Need::Element("h264parse"), Need::Decoder(VideoType::H264)]),
    (Feature::H265, &[Need::Element("h265parse"), Need::Decoder(VideoType::H265)]),
    (
        Feature::Sound,
        &[
            Need::Element("aacparse"),
            Need::AnyOf(AAC_DECODERS),
            Need::Element("audioconvert"),
            Need::Element("audioresample"),
            Need::Element("volume"),
            Need::Element("autoaudiosink"),
            Need::AudioOutput,
        ],
    ),
    (
        Feature::Talk,
        &[
            Need::Element("autoaudiosrc"),
            Need::Element("audioconvert"),
            Need::Element("audioresample"),
            Need::Element("appsink"),
            Need::AudioInput,
        ],
    ),
    (Feature::Recording, &[Need::Element("matroskamux"), Need::Element("filesink")]),
    (Feature::Downloads, &[Need::Element("qtmux"), Need::Element("aacparse"), Need::Element("filesink")]),
];

/// Where the GStreamer plugins come from, as the AppRun hook says
/// (`REOLING_PLUGINS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plugins {
    /// The embed AppImage's own: anything missing is a packaging bug.
    Bundled,
    /// The system's, under the app-only AppImage.
    SystemFromAppImage,
    /// The system's, for an installed package or a source build.
    System,
}

fn plugins_of(value: Option<&str>) -> Plugins {
    match value {
        Some("bundled") => Plugins::Bundled,
        Some("system") => Plugins::SystemFromAppImage,
        _ => Plugins::System,
    }
}

/// The distribution families whose package names the hints know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Debian,
    Fedora,
    Suse,
    Arch,
    Other,
}

/// The family of an os-release file: `ID`, then each word of `ID_LIKE`, the
/// first one known.
fn family_of(os_release: &str) -> Family {
    let value = |key: &str| {
        os_release.lines().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            Some(value.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase())
        })
    };
    let (id, like) = (value("ID").unwrap_or_default(), value("ID_LIKE").unwrap_or_default());
    std::iter::once(id.as_str())
        .chain(like.split_whitespace())
        .find_map(|word| match word {
            "debian" | "ubuntu" => Some(Family::Debian),
            "fedora" => Some(Family::Fedora),
            "suse" | "sles" => Some(Family::Suse),
            word if word.starts_with("opensuse") => Some(Family::Suse),
            "arch" => Some(Family::Arch),
            _ => None,
        })
        .unwrap_or(Family::Other)
}

/// /etc/os-release, else /usr/lib/os-release (the host's, also under an
/// AppImage); empty if neither reads.
fn os_release() -> String {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

/// Every element name in `NEEDS`, sorted, each once.
fn element_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = NEEDS
        .iter()
        .flat_map(|(_, needs)| needs.iter())
        .flat_map(|need| match need {
            Need::Element(name) => std::slice::from_ref(name),
            Need::AnyOf(names) => names,
            _ => &[],
        })
        .copied()
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// What the system offers, as `Facts::gather` reads it (tests make their
/// own).
#[derive(Debug, Clone, Default)]
struct Facts {
    gtk: (u32, u32, u32),
    gstreamer: (u32, u32, u32),
    /// The element names of `NEEDS` the registry has.
    elements: Vec<&'static str>,
    /// Which of `LIBAV` and `OPENH264` the registry has loaded (see
    /// `plugin_loaded`): their packages are named only without them.
    loaded_plugins: Vec<&'static str>,
    h264: Installed,
    h265: Installed,
    audio_output: bool,
    audio_input: bool,
    /// The gtk4paintablesink in use: its plugin's version and file.
    sink: Option<(String, String)>,
    /// The plugin registry GStreamer writes (see `registry_of`).
    registry: Option<String>,
}

/// GStreamer's FFmpeg plugin (gst-libav) and OpenH264's, by the names the
/// registry has them under.
const LIBAV: &str = "libav";
const OPENH264: &str = "openh264";

/// Whether a plugin the registry has under its own name, with these flags, is
/// loaded. A plugin file that fails to load is in the registry under its file
/// name instead (gst-libav's as libgstlibav.so), blacklisted and without
/// elements, so looking `LIBAV` or `OPENH264` up does not find it and it
/// counts as absent; the flag is a guard should a registry ever have one
/// under its own name.
fn plugin_loaded(flags: gstreamer::PluginFlags) -> bool {
    !flags.contains(gstreamer::PluginFlags::BLACKLISTED)
}

/// The plugin registry GStreamer writes, `~` for the home folder `home`: the
/// one the AppRun hooks give it (`GST_REGISTRY_1_0`), else GStreamer's own
/// default in the user's cache folder `cache` (`XDG_CACHE_HOME`, else
/// ~/.cache), a file per architecture.
fn registry_of(variable: Option<String>, cache: &std::path::Path, home: &str) -> String {
    let path = variable
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| format!("{}/registry.*.bin", cache.join("gstreamer-1.0").display()));
    match path.strip_prefix(home).filter(|rest| !home.is_empty() && rest.starts_with('/')) {
        Some(rest) => format!("~{rest}"),
        None => path,
    }
}

impl Facts {
    /// Reads the registry: after `gstreamer::init` and `scan_private_plugins`.
    /// Ranks are the ones GStreamer gave (see `settings::installed_decoders`),
    /// whatever a Decoding mode changed since. Loads no plugin, except
    /// OpenH264's decoder where it is the one software H.264 decoder (see
    /// `settings::installed_decoders`).
    fn gather() -> Self {
        let (major, minor, micro, _) = gstreamer::version();
        let elements =
            element_names().into_iter().filter(|name| gstreamer::ElementFactory::find(name).is_some()).collect();
        let gst_registry = gstreamer::Registry::get();
        let loaded_plugins = [LIBAV, OPENH264]
            .into_iter()
            .filter(|name| gst_registry.find_plugin(name).is_some_and(|plugin| plugin_loaded(plugin.plugin_flags())))
            .collect();
        let sink = gstreamer::ElementFactory::find("gtk4paintablesink").and_then(|factory| factory.plugin());
        let sink = sink.map(|plugin| {
            let file = plugin.filename().map(|file| file.display().to_string()).unwrap_or_default();
            (plugin.version().to_string(), file)
        });
        let registry = registry_of(
            std::env::var("GST_REGISTRY_1_0").ok(),
            &glib::user_cache_dir(),
            &glib::home_dir().display().to_string(),
        );
        Facts {
            gtk: (gtk4::major_version(), gtk4::minor_version(), gtk4::micro_version()),
            gstreamer: (major, minor, micro),
            elements,
            loaded_plugins,
            h264: settings::installed_decoders(VideoType::H264),
            h265: settings::installed_decoders(VideoType::H265),
            audio_output: has_device_element("Sink"),
            audio_input: has_device_element("Source"),
            sink,
            registry: Some(registry),
        }
    }

    fn has(&self, need: Need) -> bool {
        match need {
            Need::Element(name) => self.elements.contains(&name),
            Need::AnyOf(names) => names.iter().any(|name| self.elements.contains(name)),
            Need::Decoder(video_type) => self.installed(video_type).any(),
            Need::AudioOutput => self.audio_output,
            Need::AudioInput => self.audio_input,
        }
    }

    fn installed(&self, video_type: VideoType) -> Installed {
        match video_type {
            VideoType::H264 => self.h264,
            VideoType::H265 => self.h265,
        }
    }

    /// GTK or GStreamer older than our picture element is built for.
    fn too_old(&self) -> bool {
        (self.gtk.0, self.gtk.1) < SINK_GTK || (self.gstreamer.0, self.gstreamer.1) < SINK_GSTREAMER
    }
}

/// Whether an audio element autoaudiosink (`Sink`) / autoaudiosrc (`Source`)
/// would consider exists: its klass names the direction and Audio, and its
/// rank is MARGINAL or more (the auto elements themselves are left out).
fn has_device_element(direction: &str) -> bool {
    let marginal = gstreamer::Rank::MARGINAL;
    gstreamer::Registry::get()
        .features(gstreamer::ElementFactory::static_type())
        .into_iter()
        .filter_map(|feature| feature.downcast::<gstreamer::ElementFactory>().ok())
        .any(|factory| {
            let klass = factory.klass();
            klass.contains(direction)
                && klass.contains("Audio")
                && factory.rank() >= marginal
                && !factory.name().starts_with("auto")
        })
}

/// One feature that misses something.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Gap {
    feature: Feature,
    missing: Vec<Need>,
}

fn gaps_of(facts: &Facts) -> Vec<Gap> {
    NEEDS
        .iter()
        .filter_map(|(feature, needs)| {
            let missing: Vec<Need> = needs.iter().copied().filter(|need| !facts.has(*need)).collect();
            (!missing.is_empty()).then(|| Gap { feature: *feature, missing })
        })
        .collect()
}

/// The GStreamer module an element ships in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Module {
    Core,
    Base,
    Good,
    Bad,
    Libav,
    Gtk4,
    Unknown,
}

fn module_of(need: Need) -> Module {
    match need {
        Need::Element("queue" | "filesink") => Module::Core,
        Need::Element(
            "appsrc" | "appsink" | "decodebin" | "videoconvert" | "audioconvert" | "audioresample" | "volume",
        ) => Module::Base,
        Need::Element("autoaudiosink" | "autoaudiosrc" | "aacparse" | "qtmux" | "matroskamux")
        | Need::AudioOutput
        | Need::AudioInput => Module::Good,
        Need::Element("h264parse" | "h265parse") => Module::Bad,
        Need::AnyOf(_) | Need::Decoder(_) => Module::Libav,
        Need::Element("gtk4paintablesink") => Module::Gtk4,
        Need::Element(_) => Module::Unknown,
    }
}

/// What brings a missing need on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Remedy {
    /// Packages from the distribution's repositories.
    Packages(&'static [&'static str]),
    /// One line of text beyond installing packages: a repository to add
    /// first, what a package replaces.
    Steps(&'static str),
    /// A decoder whose library was just swapped may not show until the
    /// plugin registry is written again (see `Advice::lines`).
    ClearRegistry,
    /// GTK or GStreamer older than our picture element needs.
    TooOld,
    /// The embed AppImage should bring it: a packaging bug to report.
    Report,
    /// No package known for this family: the GStreamer module that has it.
    Module(&'static str),
}

/// Fedora's software H.264 decoder: OpenH264, the plugin and the library.
const FEDORA_OPENH264: &str = "openh264 comes from Fedora's fedora-cisco-openh264 repository (enabled by default) and \
     replaces noopenh264, a stand-in that decodes nothing.";
/// Fedora's own FFmpeg (ffmpeg-free) decodes no H.265.
const FEDORA_RPM_FUSION: &str = "H.265 needs the full FFmpeg from RPM Fusion: enable it \
     (https://rpmfusion.org/Configuration), then: sudo dnf swap ffmpeg-free ffmpeg --allowerasing";
/// openSUSE's own FFmpeg decodes neither H.264 nor H.265.
const PACKMAN: &str = "H.264 and H.265 decoders come from Packman: sudo zypper install opi && opi codecs";
/// gstreamer1.0-gtk4 is in Debian 13 and Ubuntu 26.04 on, not in Ubuntu
/// 24.04 or Debian 12 (the archives' package lists): an apt line naming it
/// would install nothing there, so it has lines of its own.
const DEBIAN_SINK: &str =
    "Debian 13, Ubuntu 26.04 and later have gtk4paintablesink: sudo apt install gstreamer1.0-gtk4";
const DEBIAN_SINK_OLDER: &str = concat!(
    "Older releases have none: the Reoling .deb (Ubuntu 24.04 and later) brings its own, and so does Reoling-",
    env!("CARGO_PKG_VERSION"),
    "-embed-x86_64.AppImage."
);

/// The packages that bring `module` on `family` (`None`: none known).
fn package_of(module: Module, family: Family) -> Option<&'static [&'static str]> {
    Some(match (family, module) {
        (Family::Debian, Module::Core) => &["libgstreamer1.0-0"],
        (Family::Debian, Module::Base) => &["gstreamer1.0-plugins-base"],
        (Family::Debian, Module::Good) => &["gstreamer1.0-plugins-good"],
        (Family::Debian, Module::Bad) => &["gstreamer1.0-plugins-bad"],
        (Family::Debian, Module::Libav) => &["gstreamer1.0-libav"],
        (Family::Fedora, Module::Core) => &["gstreamer1"],
        (Family::Fedora, Module::Base) => &["gstreamer1-plugins-base"],
        (Family::Fedora, Module::Good) => &["gstreamer1-plugins-good"],
        (Family::Fedora, Module::Bad) => &["gstreamer1-plugins-bad-free"],
        (Family::Fedora, Module::Libav) => &["gstreamer1-plugin-libav"],
        (Family::Fedora, Module::Gtk4) => &["gstreamer1-plugin-gtk4"],
        (Family::Suse, Module::Core) => &["gstreamer"],
        (Family::Suse, Module::Base) => &["gstreamer-plugins-base"],
        (Family::Suse, Module::Good) => &["gstreamer-plugins-good"],
        (Family::Suse, Module::Bad) => &["gstreamer-plugins-bad"],
        (Family::Suse, Module::Libav) => &["gstreamer-plugins-libav"],
        (Family::Arch, Module::Core) => &["gstreamer"],
        (Family::Arch, Module::Base) => &["gst-plugins-base-libs"],
        (Family::Arch, Module::Good) => &["gst-plugins-good"],
        (Family::Arch, Module::Bad) => &["gst-plugins-bad-libs"],
        (Family::Arch, Module::Libav) => &["gst-libav"],
        (Family::Arch, Module::Gtk4) => &["gst-plugin-gtk4"],
        _ => return None,
    })
}

/// The GStreamer module, for a family whose package names are not known.
fn module_label(module: Module) -> &'static str {
    match module {
        Module::Core => "GStreamer itself",
        Module::Base => "GStreamer's base plugins",
        Module::Good => "GStreamer's good plugins",
        Module::Bad => "GStreamer's bad plugins",
        Module::Libav => "GStreamer's FFmpeg plugin (gst-libav)",
        Module::Gtk4 => "GStreamer's GTK 4 plugin (gtk4paintablesink, from gst-plugins-rs)",
        Module::Unknown => "the GStreamer plugin that has it",
    }
}

/// What brings `need` on this system. The decoders: a package with a
/// software decoder, never a hardware one (that needs a driver as well; System
/// Status lists the hardware decoders there are).
///
/// A decoder package (gst-libav's, OpenH264's) whose plugin is loaded already
/// is not named, as installing it changes nothing: gst-libav registers
/// decoders only for the codecs the system's FFmpeg has (Fedora's ffmpeg-free
/// decodes no H.265), and Fedora's OpenH264 plugin is loaded on the noopenh264
/// stand-in too, which only the library replaces. A missing element's own
/// plugin is missing or failed to load, so its package is always named.
fn remedies(need: Need, facts: &Facts, family: Family, plugins: Plugins) -> Vec<Remedy> {
    if plugins == Plugins::Bundled {
        return vec![Remedy::Report];
    }
    let module = module_of(need);
    if module == Module::Gtk4 && plugins == Plugins::SystemFromAppImage {
        // The AppImage brings its own; the system's GTK or GStreamer may be
        // too old to load it.
        return vec![if facts.too_old() { Remedy::TooOld } else { Remedy::Report }];
    }
    let loaded = |plugin: &str| facts.loaded_plugins.contains(&plugin);
    // The package that brings `plugin`, unless it is loaded.
    let package = |plugin: &str, packages: &'static [&'static str]| {
        (!loaded(plugin)).then_some(Remedy::Packages(packages))
    };
    match (family, need) {
        (Family::Fedora, Need::Decoder(VideoType::H264)) => package(OPENH264, &["gstreamer1-plugin-openh264"])
            .into_iter()
            .chain([Remedy::Packages(&["openh264"]), Remedy::Steps(FEDORA_OPENH264)])
            .collect(),
        (Family::Fedora, Need::Decoder(VideoType::H265)) => package(LIBAV, &["gstreamer1-plugin-libav"])
            .into_iter()
            .chain([Remedy::Steps(FEDORA_RPM_FUSION), Remedy::ClearRegistry])
            .collect(),
        (Family::Suse, Need::Decoder(_)) => vec![Remedy::Steps(PACKMAN)],
        (Family::Debian, Need::Element("gtk4paintablesink")) => {
            vec![Remedy::Steps(DEBIAN_SINK), Remedy::Steps(DEBIAN_SINK_OLDER)]
        }
        (Family::Suse, Need::Element("gtk4paintablesink")) => vec![Remedy::Module(module_label(module))],
        // gst-libav is loaded but has no such decoder: no package brings it
        // here, and a registry written before the system's FFmpeg changed
        // can hide one.
        _ if module == Module::Libav && loaded(LIBAV) => vec![Remedy::ClearRegistry],
        _ => match package_of(module, family) {
            Some(packages) => vec![Remedy::Packages(packages)],
            None => vec![Remedy::Module(module_label(module))],
        },
    }
}

/// What brings a set of missing needs, put together for one message.
#[derive(Debug, Default, PartialEq, Eq)]
struct Advice {
    /// The one install command, its packages deduplicated in order.
    command: Option<String>,
    modules: Vec<&'static str>,
    steps: Vec<&'static str>,
    clear_registry: bool,
    too_old: bool,
    report: bool,
}

impl Advice {
    /// Anything for the user to install or set up.
    fn installs(&self) -> bool {
        self.command.is_some() || !self.modules.is_empty() || !self.steps.is_empty()
    }
}

/// The install command for `packages` on `family`.
fn install_command(family: Family, packages: &[&str]) -> Option<String> {
    let command = match family {
        Family::Debian => "sudo apt install",
        Family::Fedora => "sudo dnf install",
        Family::Suse => "sudo zypper install",
        Family::Arch => "sudo pacman -S --needed",
        Family::Other => return None,
    };
    (!packages.is_empty()).then(|| format!("{command} {}", packages.join(" ")))
}

/// The report of this system: its facts and what they lack.
pub struct Report {
    facts: Facts,
    gaps: Vec<Gap>,
    family: Family,
    plugins: Plugins,
}

static REPORT: OnceLock<Report> = OnceLock::new();

/// This system's report, gathered on the first call (after
/// `gstreamer::init` and `scan_private_plugins`).
pub fn check() -> &'static Report {
    REPORT.get_or_init(|| {
        let plugins = plugins_of(std::env::var("REOLING_PLUGINS").ok().as_deref());
        Report::new(Facts::gather(), family_of(&os_release()), plugins)
    })
}

impl Report {
    fn new(facts: Facts, family: Family, plugins: Plugins) -> Self {
        Report { gaps: gaps_of(&facts), facts, family, plugins }
    }

    /// `--check-requirements`' exit status: 0 with nothing missing, 1 when no
    /// video of a codec (or none at all) can be shown, 2 when only another
    /// feature misses something.
    pub fn exit_code(&self) -> i32 {
        if self.gaps.is_empty() {
            0
        } else if self.gaps.iter().any(|gap| gap.feature.required()) {
            1
        } else {
            2
        }
    }

    /// What "Don't show this again" remembers: every missing need with its
    /// feature, sorted, so that a new gap shows the dialog again. `None` with
    /// nothing missing.
    pub fn dismiss_key(&self) -> Option<String> {
        let mut keys: Vec<String> = self
            .gaps
            .iter()
            .flat_map(|gap| gap.missing.iter().map(move |need| format!("{}:{}", gap.feature.key(), need.key())))
            .collect();
        keys.sort();
        keys.dedup();
        (!keys.is_empty()).then(|| keys.join(","))
    }

    fn missing_needs(&self) -> Vec<Need> {
        let mut needs: Vec<Need> = Vec::new();
        for need in self.gaps.iter().flat_map(|gap| gap.missing.iter()) {
            if !needs.contains(need) {
                needs.push(*need);
            }
        }
        needs
    }

    /// What brings `needs` here.
    fn advice(&self, needs: &[Need]) -> Advice {
        let mut advice = Advice::default();
        let mut packages: Vec<&'static str> = Vec::new();
        for remedy in needs.iter().flat_map(|need| remedies(*need, &self.facts, self.family, self.plugins)) {
            match remedy {
                Remedy::Packages(names) => {
                    for name in names {
                        if !packages.contains(name) {
                            packages.push(name);
                        }
                    }
                }
                Remedy::Steps(text) if !advice.steps.contains(&text) => advice.steps.push(text),
                Remedy::Module(label) if !advice.modules.contains(&label) => advice.modules.push(label),
                Remedy::Steps(_) | Remedy::Module(_) => {}
                Remedy::ClearRegistry => advice.clear_registry = true,
                Remedy::TooOld => advice.too_old = true,
                Remedy::Report => advice.report = true,
            }
        }
        advice.command = install_command(self.family, &packages);
        advice
    }

    /// The advice as lines; `install` comes before the command ("Install
    /// them with:\n"), and `it` names what was missing ("these parts", "one").
    fn advice_lines(&self, advice: &Advice, install: &str, it: &str) -> Vec<String> {
        let mut lines = Vec::new();
        if advice.too_old {
            let ((gtk_major, gtk_minor, gtk_micro), (gst_major, gst_minor, gst_micro)) =
                (self.facts.gtk, self.facts.gstreamer);
            lines.push(format!(
                "This system's GTK {gtk_major}.{gtk_minor}.{gtk_micro} and GStreamer \
                 {gst_major}.{gst_minor}.{gst_micro} are older than the picture needs (GTK {}.{}, GStreamer {}.{}): \
                 use {EMBED_APPIMAGE}, which brings its own.",
                SINK_GTK.0, SINK_GTK.1, SINK_GSTREAMER.0, SINK_GSTREAMER.1
            ));
        }
        if let Some(command) = &advice.command {
            lines.push(format!("{install}{command}"));
        }
        if !advice.modules.is_empty() {
            lines.push(format!("Install with the system's package manager: {}.", advice.modules.join(", ")));
        }
        lines.extend(advice.steps.iter().map(|step| step.to_string()));
        if advice.installs() {
            lines.push("Then start Reoling again.".to_string());
        }
        if advice.clear_registry {
            let registry = self.facts.registry.as_deref().unwrap_or("~/.cache/gstreamer-1.0/registry.*.bin");
            lines.push(format!("If the decoder still does not show up, remove {registry} and start Reoling again."));
        }
        if advice.report {
            lines.push(match self.plugins {
                // Under the app-only AppImage only our own display element is.
                Plugins::SystemFromAppImage => {
                    format!("This AppImage should bring its own gtk4paintablesink: please report this at {ISSUES}")
                }
                Plugins::Bundled | Plugins::System => {
                    format!("This AppImage should bring {it} along: please report this at {ISSUES}")
                }
            });
        }
        if self.plugins == Plugins::SystemFromAppImage && (advice.installs() || advice.clear_registry) {
            lines.push(format!("Or use {EMBED_APPIMAGE}, which brings {it} along."));
        }
        lines
    }

    /// The report for people (the dialog at start, `--check-requirements`):
    /// per gap "- <feature>: <needs>", then what brings them. Empty with
    /// nothing missing.
    pub fn text(&self) -> String {
        if self.gaps.is_empty() {
            return String::new();
        }
        let mut lines = vec!["Some parts Reoling needs are not on this system:".to_string()];
        lines.extend(self.gaps.iter().map(|gap| format!("- {}: {}", gap.feature.title(), labels(&gap.missing))));
        let advice = self.advice(&self.missing_needs());
        lines.extend(self.advice_lines(&advice, "Install them with:\n", "these parts"));
        lines.join("\n")
    }

    /// `--check-requirements`: the versions, where the plugins come from, the
    /// display element, the decoders, then "Missing: nothing", or "Missing:"
    /// with `dismiss_key` and `text`.
    pub fn check_text(&self) -> String {
        let (gtk, gst) = (self.facts.gtk, self.facts.gstreamer);
        let decoders = |installed: Installed| match (installed.software, installed.hardware) {
            (true, true) => "software and hardware",
            (true, false) => "software",
            (false, true) => "hardware",
            (false, false) => "none",
        };
        let mut lines = vec![
            format!("Reoling {}", env!("CARGO_PKG_VERSION")),
            format!("GTK {}.{}.{}", gtk.0, gtk.1, gtk.2),
            format!("GStreamer {}.{}.{}", gst.0, gst.1, gst.2),
            self.plugins_line(),
            self.display_line(),
            format!("H.264 decoders: {}", decoders(self.facts.h264)),
            format!("H.265 decoders: {}", decoders(self.facts.h265)),
        ];
        match self.dismiss_key() {
            None => lines.push("Missing: nothing".to_string()),
            Some(key) => {
                lines.push(format!("Missing: {key}"));
                lines.push(self.text());
            }
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// Where the GStreamer plugins come from (see `Plugins`).
    fn plugins_line(&self) -> String {
        let source = match self.plugins {
            Plugins::Bundled => "bundled with the AppImage",
            Plugins::SystemFromAppImage => "the system's, under the AppImage",
            Plugins::System => "the system's",
        };
        format!("Plugins: {source}")
    }

    fn display_line(&self) -> String {
        match &self.facts.sink {
            Some((version, file)) => format!("Display: gtk4paintablesink {version} ({file})"),
            None => "Display: gtk4paintablesink missing".to_string(),
        }
    }

    /// One line on stderr at start, `None` with nothing missing; "; " ends
    /// each part, so that no command runs into the text after it.
    pub fn summary_line(&self) -> Option<String> {
        if self.gaps.is_empty() {
            return None;
        }
        let missing: Vec<String> = self
            .gaps
            .iter()
            .flat_map(|gap| gap.missing.iter().map(move |need| format!("{} ({})", need.label(), gap.feature.label())))
            .collect();
        let advice = self.advice(&self.missing_needs());
        let mut line = format!("reoling: missing on this system: {}", missing.join(", "));
        let said: Vec<String> = self
            .advice_lines(&advice, "install: ", "these parts")
            .into_iter()
            .filter(|line| line != "Then start Reoling again.")
            .collect();
        if !said.is_empty() {
            line.push_str("; ");
            line.push_str(&said.join("; "));
        }
        Some(line)
    }

    /// System Status: where the plugins come from and the display element in
    /// use, as `check_text` says them, then "Missing: nothing" or one line per
    /// gap and what brings them. The decoders are System Status' own lines.
    pub fn status_lines(&self) -> Vec<String> {
        let mut lines = vec![self.plugins_line(), self.display_line()];
        if self.gaps.is_empty() {
            lines.push("Missing: nothing".to_string());
            return lines;
        }
        lines.extend(
            self.gaps.iter().map(|gap| format!("Missing for {}: {}", gap.feature.label(), labels(&gap.missing))),
        );
        let advice = self.advice(&self.missing_needs());
        lines.extend(self.advice_lines(&advice, "Install them with: ", "these parts"));
        lines
    }

    /// The message over a picture whose pipeline could not be built because
    /// `element` is missing, with what brings it, a line each (a command
    /// ends its line).
    pub fn missing_element_text(&self, element: &str) -> String {
        let mut text = format!("Cannot show this stream: {element} is missing.");
        let need = NEEDS
            .iter()
            .flat_map(|(_, needs)| needs.iter())
            .copied()
            .find(|need| matches!(need, Need::Element(name) if *name == element));
        if let Some(need) = need {
            let advice = self.advice(&[need]);
            for line in self.advice_lines(&advice, "Install it with: ", "it") {
                text.push('\n');
                text.push_str(&line);
            }
        }
        text
    }

    /// The message over a picture no decoder took (see
    /// `video_view::watch_for_no_decoder`). With decoders for the codec
    /// installed (the Decoding setting kept them out, or they failed to
    /// start) it names no setting, as the mode may already be Auto; without,
    /// it says what brings one, a line each, as `missing_element_text`.
    pub fn no_decoder_text(&self, video_type: VideoType) -> String {
        let codec = codec_label(video_type);
        let installed = self.facts.installed(video_type);
        let need = Need::Decoder(video_type);
        if installed.any() {
            let mut text = format!(
                "None of the {codec} decoders on this system could be used for this stream. System Status lists them; \
                 another Decoding setting may help."
            );
            if !installed.software && self.plugins != Plugins::Bundled {
                let advice = self.advice(&[need]);
                if advice.installs() {
                    text.push_str(" A software decoder always works:");
                    for line in self.advice_lines(&advice, "", "one") {
                        text.push('\n');
                        text.push_str(&line);
                    }
                }
            }
            return text;
        }
        let mut text = format!("No {codec} decoder on this system.");
        let advice = self.advice(&[need]);
        for line in self.advice_lines(&advice, "Install one with: ", "one") {
            text.push('\n');
            text.push_str(&line);
        }
        text
    }
}

/// The needs as people read them, in one line.
fn labels(needs: &[Need]) -> String {
    needs.iter().map(|need| need.label()).collect::<Vec<_>>().join(", ")
}

/// The dialog at start: the report, "Don't show this again", OK.
/// `on_dismiss` runs when OK is pressed with the box ticked.
pub fn show_dialog(parent: &gtk4::Window, report: &Report, on_dismiss: impl Fn() + 'static) {
    let (window, content) = crate::ui::dialogs::dialog(Some(parent), "Missing parts", 460);
    let text = gtk4::Label::new(Some(&report.text()));
    text.set_selectable(true);
    // As in System Status: no focus, so no select-all when it opens.
    text.set_focusable(false);
    text.set_wrap(true);
    text.set_xalign(0.0);
    text.set_halign(gtk4::Align::Start);
    content.append(&text);
    let again = gtk4::CheckButton::with_label("Don't show this again");
    content.append(&again);
    let ok = gtk4::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    ok.set_halign(gtk4::Align::End);
    content.append(&ok);
    let w = window.clone();
    ok.connect_clicked(move |_| {
        if again.is_active() {
            on_dismiss();
        }
        w.close();
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Debian 13's live image: everything there, the distribution's sink.
    fn complete() -> Facts {
        Facts {
            gtk: (4, 18, 6),
            gstreamer: (1, 26, 2),
            elements: element_names(),
            loaded_plugins: vec![LIBAV, OPENH264],
            h264: Installed { software: true, hardware: true },
            h265: Installed { software: true, hardware: true },
            audio_output: true,
            audio_input: true,
            sink: Some(("0.13.5".to_string(), "/usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstgtk4.so".to_string())),
            registry: None,
        }
    }

    /// `facts` without these elements and plugins.
    fn without(mut facts: Facts, names: &[&str]) -> Facts {
        facts.elements.retain(|name| !names.contains(name));
        facts.loaded_plugins.retain(|name| !names.contains(name));
        facts
    }

    const NOTHING: Installed = Installed { software: false, hardware: false };
    const SOFTWARE: Installed = Installed { software: true, hardware: false };
    const HARDWARE: Installed = Installed { software: false, hardware: true };

    /// Ubuntu 24.04's desktop: no -bad, no -libav (no parsers, no software
    /// decoders, no avdec_aac, neither plugin; no hardware decoder without
    /// -bad either).
    fn ubuntu_24_04() -> Facts {
        let names = ["h264parse", "h265parse", "avdec_aac", "faad", "gtk4paintablesink", LIBAV, OPENH264];
        let mut facts = without(complete(), &names);
        facts.h264 = NOTHING;
        facts.h265 = NOTHING;
        facts.gtk = (4, 14, 2);
        facts.gstreamer = (1, 24, 2);
        facts.sink = None;
        facts
    }

    fn gaps(report: &Report) -> Vec<(Feature, Vec<Need>)> {
        report.gaps.iter().map(|gap| (gap.feature, gap.missing.clone())).collect()
    }

    #[test]
    fn os_release_families() {
        for (text, family) in [
            ("NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\nID_LIKE=debian\n", Family::Debian),
            ("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n", Family::Debian),
            ("ID=pop\nID_LIKE=\"ubuntu debian\"\n", Family::Debian),
            ("ID=elementary\nID_LIKE=ubuntu\n", Family::Debian),
            ("ID=debian\nVERSION_ID=\"13\"\n", Family::Debian),
            ("ID=fedora\nVERSION_ID=43\n", Family::Fedora),
            ("ID=nobara\nID_LIKE=\"rhel centos fedora\"\n", Family::Fedora),
            ("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n", Family::Suse),
            ("ID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\n", Family::Suse),
            ("ID=arch\n", Family::Arch),
            ("ID=manjaro\nID_LIKE=arch\n", Family::Arch),
            ("ID=endeavouros\nID_LIKE=arch\n", Family::Arch),
            ("ID=nixos\n", Family::Other),
            ("", Family::Other),
            ("ID='debian'\n", Family::Debian),
            // VERSION_ID is no ID, nor is a key that only starts with it.
            ("VERSION_ID=fedora\nID_X=arch\n", Family::Other),
        ] {
            assert_eq!(family_of(text), family, "{text:?}");
        }
    }

    #[test]
    fn plugins_from_the_apprun_hook() {
        assert_eq!(plugins_of(Some("bundled")), Plugins::Bundled);
        assert_eq!(plugins_of(Some("system")), Plugins::SystemFromAppImage);
        assert_eq!(plugins_of(None), Plugins::System);
        assert_eq!(plugins_of(Some("")), Plugins::System);
        assert_eq!(plugins_of(Some("Bundled")), Plugins::System);
    }

    #[test]
    fn the_registry_to_remove_is_the_one_gstreamer_writes() {
        let (cache, home) = (std::path::Path::new("/home/user/.cache"), "/home/user");
        // The AppRun hooks' own.
        let hook = Some("/home/user/.cache/reoling/gstreamer-registry-system-1.28.2.bin".to_string());
        assert_eq!(registry_of(hook, cache, home), "~/.cache/reoling/gstreamer-registry-system-1.28.2.bin");
        // GStreamer's default, in the cache folder XDG_CACHE_HOME names.
        assert_eq!(registry_of(None, cache, home), "~/.cache/gstreamer-1.0/registry.*.bin");
        assert_eq!(registry_of(Some(String::new()), cache, home), "~/.cache/gstreamer-1.0/registry.*.bin");
        let xdg = std::path::Path::new("/home/user/.local/cache");
        assert_eq!(registry_of(None, xdg, home), "~/.local/cache/gstreamer-1.0/registry.*.bin");
        let elsewhere = std::path::Path::new("/var/cache/user");
        assert_eq!(registry_of(None, elsewhere, home), "/var/cache/user/gstreamer-1.0/registry.*.bin");
        // Only the home folder itself becomes `~`.
        let other = std::path::Path::new("/home/user2/c");
        assert_eq!(registry_of(None, other, home), "/home/user2/c/gstreamer-1.0/registry.*.bin");
        assert_eq!(registry_of(None, cache, ""), "/home/user/.cache/gstreamer-1.0/registry.*.bin");
    }

    #[test]
    fn a_complete_system_reports_nothing() {
        for (plugins, source) in [
            (Plugins::Bundled, "Plugins: bundled with the AppImage"),
            (Plugins::SystemFromAppImage, "Plugins: the system's, under the AppImage"),
            (Plugins::System, "Plugins: the system's"),
        ] {
            let report = Report::new(complete(), Family::Debian, plugins);
            assert!(report.gaps.is_empty());
            assert_eq!(report.exit_code(), 0);
            assert_eq!(report.text(), "");
            assert_eq!(report.summary_line(), None);
            assert_eq!(report.dismiss_key(), None);
            // System Status: the Plugins and Display lines `check_text` has,
            // then "Missing"; no decoder lines (System Status has its own).
            let display = "Display: gtk4paintablesink 0.13.5 (/usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstgtk4.so)";
            assert_eq!(report.status_lines(), [source, display, "Missing: nothing"]);
            let check = report.check_text();
            assert!(check.ends_with("\nMissing: nothing\n"), "{check}");
            assert!(check.contains(&format!("\n{source}\n{display}\nH.264 decoders: ")), "{check}");
            assert!(check.contains("\nH.265 decoders: software and hardware\n"), "{check}");
        }
        // With gaps, the same two lines first.
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::SystemFromAppImage);
        let lines = report.status_lines();
        assert_eq!(lines[..2], ["Plugins: the system's, under the AppImage", "Display: gtk4paintablesink missing"]);
        assert_eq!(lines[2], "Missing for video: gtk4paintablesink");
    }

    #[test]
    fn ubuntu_24_04_needs_bad_and_libav() {
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::System);
        // The .deb brings its own sink; here only the codecs and the sound miss.
        let mut facts = ubuntu_24_04();
        facts.elements.push("gtk4paintablesink");
        let report_deb = Report::new(facts, Family::Debian, Plugins::System);
        assert_eq!(
            gaps(&report_deb),
            [
                (Feature::H264, vec![Need::Element("h264parse"), Need::Decoder(VideoType::H264)]),
                (Feature::H265, vec![Need::Element("h265parse"), Need::Decoder(VideoType::H265)]),
                (Feature::Sound, vec![Need::AnyOf(AAC_DECODERS)]),
            ]
        );
        assert_eq!(report_deb.exit_code(), 1);
        let text = report_deb.text();
        assert!(text.contains("\nsudo apt install gstreamer1.0-plugins-bad gstreamer1.0-libav\n"), "{text}");
        assert!(text.ends_with("Then start Reoling again."), "{text}");
        assert!(!text.contains("AppImage"), "{text}");
        assert_eq!(
            report_deb.dismiss_key().as_deref(),
            Some("h264:decoder-h264,h264:h264parse,h265:decoder-h265,h265:h265parse,sound:aac-decoder")
        );
        assert_eq!(
            report_deb.summary_line().as_deref(),
            Some(
                "reoling: missing on this system: h264parse (H.264 video), an H.264 decoder (H.264 video), h265parse \
                 (H.265 video), an H.265 decoder (H.265 video), an AAC decoder (sound); install: sudo apt install \
                 gstreamer1.0-plugins-bad gstreamer1.0-libav"
            )
        );
        // A source build there lacks the distribution's sink too, which 24.04
        // has no package for: the apt line still installs, the sink has lines
        // of its own.
        let text = report.text();
        assert!(text.contains("\nsudo apt install gstreamer1.0-plugins-bad gstreamer1.0-libav\n"), "{text}");
        assert_eq!(text.matches("gstreamer1.0-gtk4").count(), 1, "{text}");
        let sink = format!(
            "\nDebian 13, Ubuntu 26.04 and later have gtk4paintablesink: sudo apt install gstreamer1.0-gtk4\nOlder \
             releases have none: the Reoling .deb (Ubuntu 24.04 and later) brings its own, and so does \
             {EMBED_APPIMAGE}.\nThen start Reoling again."
        );
        assert!(text.ends_with(&sink), "{text}");
        // On stderr, "; " after each command.
        let summary = report.summary_line().expect("missing parts");
        assert!(
            summary.ends_with(&format!(
                "; install: sudo apt install gstreamer1.0-plugins-bad gstreamer1.0-libav; Debian 13, Ubuntu 26.04 and \
                 later have gtk4paintablesink: sudo apt install gstreamer1.0-gtk4; Older releases have none: the \
                 Reoling .deb (Ubuntu 24.04 and later) brings its own, and so does {EMBED_APPIMAGE}."
            )),
            "{summary}"
        );
        assert!(!summary.contains('\n'), "{summary}");
        // The app-only AppImage brings the sink (with GTK 4.14 and GStreamer
        // 1.24 it could load it: a packaging bug), and points at the embed one.
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::SystemFromAppImage);
        let text = report.text();
        assert!(text.contains("\nsudo apt install gstreamer1.0-plugins-bad gstreamer1.0-libav\n"), "{text}");
        assert!(text.contains(&format!("{ISSUES}\n")), "{text}");
        assert!(text.ends_with(&format!("Or use {EMBED_APPIMAGE}, which brings these parts along.")), "{text}");
        assert_eq!(EMBED_APPIMAGE, format!("Reoling-{}-embed-x86_64.AppImage", env!("CARGO_PKG_VERSION")));
    }

    /// What brings H.265 on Fedora once the FFmpeg plugin is there: the swap
    /// to RPM Fusion's FFmpeg, then the registry line.
    const FEDORA_H265: &str = "H.265 needs the full FFmpeg from RPM Fusion: enable it \
                               (https://rpmfusion.org/Configuration), then: sudo dnf swap ffmpeg-free ffmpeg \
                               --allowerasing\nThen start Reoling again.\nIf the decoder still does not show up, \
                               remove ~/.cache/gstreamer-1.0/registry.*.bin and start Reoling again.";

    #[test]
    fn fedora_without_rpm_fusion_misses_only_h265() {
        // Fedora 43/44 Workstation: OpenH264 for H.264, the FFmpeg plugin on
        // ffmpeg-free (its AAC, no H.265), a VA-API GPU without H.265, the
        // distribution's sink. The plugin is loaded: dnf would answer
        // "already installed", the swap is what brings H.265.
        let mut facts = complete();
        facts.h264 = Installed { software: true, hardware: true };
        facts.h265 = NOTHING;
        let report = Report::new(facts, Family::Fedora, Plugins::System);
        assert_eq!(gaps(&report), [(Feature::H265, vec![Need::Decoder(VideoType::H265)])]);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(
            report.text(),
            format!("Some parts Reoling needs are not on this system:\n- H.265 video: an H.265 decoder\n{FEDORA_H265}")
        );
        assert_eq!(report.no_decoder_text(VideoType::H265), format!("No H.265 decoder on this system.\n{FEDORA_H265}"));
        let summary = report.summary_line().expect("missing parts");
        assert!(!summary.contains("dnf install"), "{summary}");
        assert!(summary.contains("; H.265 needs the full FFmpeg from RPM Fusion: enable it"), "{summary}");
        // Without the plugin (not installed, or failed to load: see
        // `plugin_loaded`), and so without its AAC: named, before the swap.
        let mut facts = without(complete(), &[LIBAV, "avdec_aac", "faad"]);
        facts.h265 = NOTHING;
        let report = Report::new(facts.clone(), Family::Fedora, Plugins::System);
        assert_eq!(
            report.text(),
            format!(
                "Some parts Reoling needs are not on this system:\n- H.265 video: an H.265 decoder\n- Sound: an AAC \
                 decoder\nInstall them with:\nsudo dnf install gstreamer1-plugin-libav\n{FEDORA_H265}"
            )
        );
        let install = "No H.265 decoder on this system.\nInstall one with: sudo dnf install gstreamer1-plugin-libav";
        assert_eq!(report.no_decoder_text(VideoType::H265), format!("{install}\n{FEDORA_H265}"));
        // Under the app-only AppImage: its own registry.
        facts.registry = Some("~/.cache/reoling/gstreamer-registry-system-1.28.2.bin".to_string());
        let report = Report::new(facts, Family::Fedora, Plugins::SystemFromAppImage);
        let text = report.no_decoder_text(VideoType::H265);
        assert!(text.starts_with(&format!("{install}\n")), "{text}");
        assert!(text.contains("remove ~/.cache/reoling/gstreamer-registry-system-1.28.2.bin and start"), "{text}");
        assert!(text.ends_with(&format!("Or use {EMBED_APPIMAGE}, which brings one along.")), "{text}");
    }

    #[test]
    fn fedora_h264_names_the_codec_library() {
        // A system without either plugin, no VA-API: both packages.
        let mut facts = without(complete(), &[OPENH264, LIBAV]);
        facts.h264 = NOTHING;
        facts.h265 = NOTHING;
        let report = Report::new(facts, Family::Fedora, Plugins::System);
        let text = report.text();
        assert!(
            text.contains("\nsudo dnf install gstreamer1-plugin-openh264 openh264 gstreamer1-plugin-libav\n"),
            "{text}"
        );
        assert!(text.contains("fedora-cisco-openh264 repository (enabled by default) and replaces noopenh264"));
        let text = report.no_decoder_text(VideoType::H264);
        assert!(
            text.starts_with(
                "No H.264 decoder on this system.\nInstall one with: sudo dnf install gstreamer1-plugin-openh264 \
                 openh264\nopenh264 comes from"
            ),
            "{text}"
        );
        // The stock live image: both plugins, openh264dec on the noopenh264
        // stand-in, which does not count (see `settings::installed_decoders`):
        // the library is named, no plugin package.
        let mut facts = complete();
        facts.h264 = NOTHING;
        facts.h265 = NOTHING;
        let report = Report::new(facts, Family::Fedora, Plugins::System);
        let openh264 = "openh264 comes from Fedora's fedora-cisco-openh264 repository (enabled by default) and \
                        replaces noopenh264, a stand-in that decodes nothing.";
        assert_eq!(
            report.text(),
            format!(
                "Some parts Reoling needs are not on this system:\n- H.264 video: an H.264 decoder\n- H.265 video: an \
                 H.265 decoder\nInstall them with:\nsudo dnf install openh264\n{openh264}\n{FEDORA_H265}"
            )
        );
        let install = "No H.264 decoder on this system.\nInstall one with: sudo dnf install openh264";
        let text = report.no_decoder_text(VideoType::H264);
        assert_eq!(text, format!("{install}\n{openh264}\nThen start Reoling again."));
        // Only a hardware decoder, which failed: the software one is named.
        let mut facts = without(complete(), &[OPENH264]);
        facts.h264 = HARDWARE;
        let report = Report::new(facts, Family::Fedora, Plugins::System);
        let text = report.no_decoder_text(VideoType::H264);
        let software = "A software decoder always works:\nsudo dnf install gstreamer1-plugin-openh264 openh264\n";
        assert!(text.contains(software), "{text}");
    }

    #[test]
    fn opensuse_points_at_packman() {
        // The FFmpeg plugin on openSUSE's own FFmpeg: its AAC, no H.264 or
        // H.265.
        let mut facts = complete();
        facts.h264 = NOTHING;
        facts.h265 = NOTHING;
        let report = Report::new(facts.clone(), Family::Suse, Plugins::System);
        let gaps = "Some parts Reoling needs are not on this system:\n- H.264 video: an H.264 decoder\n- H.265 video: \
                    an H.265 decoder";
        let packman = "H.264 and H.265 decoders come from Packman: sudo zypper install opi && opi codecs";
        assert_eq!(report.text(), format!("{gaps}\n{packman}\nThen start Reoling again."));
        // Without the plugin, and so without an AAC decoder: it is named.
        let report = Report::new(without(facts.clone(), &[LIBAV, "avdec_aac", "faad"]), Family::Suse, Plugins::System);
        assert_eq!(
            report.text(),
            format!(
                "{gaps}\n- Sound: an AAC decoder\nInstall them with:\nsudo zypper install gstreamer-plugins-libav\n\
                 {packman}\nThen start Reoling again."
            )
        );
        // With the plugin but no AAC decoder in it: no package.
        let report = Report::new(without(facts, &["avdec_aac", "faad"]), Family::Suse, Plugins::System);
        assert_eq!(
            report.text(),
            format!(
                "{gaps}\n- Sound: an AAC decoder\n{packman}\nThen start Reoling again.\nIf the decoder still does not \
                 show up, remove ~/.cache/gstreamer-1.0/registry.*.bin and start Reoling again."
            )
        );
    }

    #[test]
    fn a_package_whose_plugin_is_loaded_is_not_named() {
        // The guard in `plugin_loaded`: a plugin flagged blacklisted is not
        // loaded. (One that failed to load is not found by its name at all.)
        assert!(plugin_loaded(gstreamer::PluginFlags::empty()));
        assert!(plugin_loaded(gstreamer::PluginFlags::CACHED));
        assert!(!plugin_loaded(gstreamer::PluginFlags::BLACKLISTED));
        assert!(!plugin_loaded(gstreamer::PluginFlags::CACHED | gstreamer::PluginFlags::BLACKLISTED));
        // The FFmpeg plugin loaded, its H.265 decoder not there: only the
        // registry line, on every family whose hint is a package (openSUSE's
        // is Packman, above; Fedora's the swap).
        let registry = "If the decoder still does not show up, remove ~/.cache/gstreamer-1.0/registry.*.bin and start \
                        Reoling again.";
        let mut facts = complete();
        facts.h265 = NOTHING;
        for (family, install) in [
            (Family::Debian, "Install one with: sudo apt install gstreamer1.0-libav\nThen start Reoling again."),
            (Family::Arch, "Install one with: sudo pacman -S --needed gst-libav\nThen start Reoling again."),
            (
                Family::Other,
                "Install with the system's package manager: GStreamer's FFmpeg plugin (gst-libav).\nThen start \
                 Reoling again.",
            ),
        ] {
            let report = Report::new(facts.clone(), family, Plugins::System);
            let text = report.no_decoder_text(VideoType::H265);
            assert_eq!(text, format!("No H.265 decoder on this system.\n{registry}"), "{family:?}");
            assert_eq!(
                report.text(),
                format!("Some parts Reoling needs are not on this system:\n- H.265 video: an H.265 decoder\n{registry}")
            );
            // Under the app-only AppImage, the embed one has its own FFmpeg.
            let report = Report::new(facts.clone(), family, Plugins::SystemFromAppImage);
            let embed = format!("{registry}\nOr use {EMBED_APPIMAGE}, which brings one along.");
            assert!(report.no_decoder_text(VideoType::H265).ends_with(&embed), "{family:?}");
            // Without it (not installed, or failed to load): its package.
            let report = Report::new(without(facts.clone(), &[LIBAV]), family, Plugins::System);
            let text = report.no_decoder_text(VideoType::H265);
            assert_eq!(text, format!("No H.265 decoder on this system.\n{install}"), "{family:?}");
        }
        // A missing element's package is named, whatever is loaded.
        let report = Report::new(without(complete(), &["h265parse"]), Family::Debian, Plugins::System);
        assert!(report.text().ends_with("\nsudo apt install gstreamer1.0-plugins-bad\nThen start Reoling again."));
    }

    #[test]
    fn arch_needs_gst_libav() {
        let mut facts = without(complete(), &["avdec_aac", "faad", LIBAV]);
        facts.h264 = NOTHING;
        facts.h265 = HARDWARE;
        let report = Report::new(facts, Family::Arch, Plugins::System);
        assert_eq!(
            gaps(&report),
            [
                (Feature::H264, vec![Need::Decoder(VideoType::H264)]),
                (Feature::Sound, vec![Need::AnyOf(AAC_DECODERS)]),
            ]
        );
        assert!(report.text().contains("\nsudo pacman -S --needed gst-libav\n"));
    }

    #[test]
    fn old_gtk_explains_the_missing_picture_element() {
        // Ubuntu 22.04 under the app-only AppImage: the sink cannot load.
        let mut facts = ubuntu_24_04();
        facts.gtk = (4, 6, 9);
        facts.gstreamer = (1, 20, 3);
        let report = Report::new(facts.clone(), Family::Debian, Plugins::SystemFromAppImage);
        let text = report.missing_element_text("gtk4paintablesink");
        assert_eq!(
            text,
            format!(
                "Cannot show this stream: gtk4paintablesink is missing.\nThis system's GTK 4.6.9 and GStreamer \
                 1.20.3 are older than the picture needs (GTK 4.14, GStreamer 1.24): use {EMBED_APPIMAGE}, which \
                 brings its own."
            )
        );
        assert!(report.text().contains("GTK 4.6.9 and GStreamer 1.20.3 are older"));
        // New enough but missing: the distribution's package for an installed
        // build (on Debian and Ubuntu, where there is one), a report for the
        // AppImage.
        facts.gtk = (4, 14, 2);
        facts.gstreamer = (1, 24, 2);
        let report = Report::new(facts.clone(), Family::Debian, Plugins::System);
        assert_eq!(
            report.missing_element_text("gtk4paintablesink"),
            format!(
                "Cannot show this stream: gtk4paintablesink is missing.\nDebian 13, Ubuntu 26.04 and later have \
                 gtk4paintablesink: sudo apt install gstreamer1.0-gtk4\nOlder releases have none: the Reoling .deb \
                 (Ubuntu 24.04 and later) brings its own, and so does {EMBED_APPIMAGE}.\nThen start Reoling again."
            )
        );
        let report = Report::new(facts.clone(), Family::Fedora, Plugins::System);
        assert!(report.missing_element_text("gtk4paintablesink").ends_with(
            "is missing.\nInstall it with: sudo dnf install gstreamer1-plugin-gtk4\nThen start Reoling again."
        ));
        let report = Report::new(facts, Family::Debian, Plugins::SystemFromAppImage);
        let text = report.missing_element_text("gtk4paintablesink");
        let report_it = format!("This AppImage should bring its own gtk4paintablesink: please report this at {ISSUES}");
        assert!(text.ends_with(&report_it), "{text}");
    }

    #[test]
    fn a_bundled_appimage_lacking_something_is_reported() {
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::Bundled);
        let text = report.text();
        assert!(!text.contains("sudo"), "{text}");
        assert!(!text.contains("Then start Reoling again."), "{text}");
        let report_it = format!("This AppImage should bring these parts along: please report this at {ISSUES}");
        assert!(text.ends_with(&report_it), "{text}");
        let report_one = format!("This AppImage should bring one along: please report this at {ISSUES}");
        assert_eq!(report.no_decoder_text(VideoType::H265), format!("No H.265 decoder on this system.\n{report_one}"));
    }

    #[test]
    fn a_hardware_decoder_alone_is_enough() {
        let mut facts = complete();
        facts.h265 = HARDWARE;
        facts.h264 = SOFTWARE;
        let report = Report::new(facts, Family::Fedora, Plugins::System);
        assert!(report.gaps.is_empty());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn optional_gaps_exit_2_and_keep_their_key() {
        let mut facts = complete();
        facts.audio_output = false;
        let report = Report::new(facts.clone(), Family::Debian, Plugins::System);
        assert_eq!(gaps(&report), [(Feature::Sound, vec![Need::AudioOutput])]);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.dismiss_key().as_deref(), Some("sound:audio-output"));
        // The key does not depend on the order the facts came in.
        let mut reordered = facts.clone();
        reordered.elements.reverse();
        assert_eq!(Report::new(reordered, Family::Debian, Plugins::System).dismiss_key(), report.dismiss_key());
        // Another missing set, another key.
        facts.audio_input = false;
        let other = Report::new(facts, Family::Debian, Plugins::System);
        assert_eq!(other.dismiss_key().as_deref(), Some("sound:audio-output,talk:audio-input"));
        assert_eq!(other.exit_code(), 2);
        // A missing picture decides over the rest.
        let facts = without(complete(), &["qtmux", "videoconvert"]);
        let report = Report::new(facts, Family::Debian, Plugins::System);
        assert_eq!(report.exit_code(), 1);
        assert!(report.check_text().contains("\nMissing: downloads:qtmux,picture:videoconvert\n"));
    }

    #[test]
    fn per_view_texts_name_what_to_install() {
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::System);
        assert_eq!(
            report.missing_element_text("h265parse"),
            "Cannot show this stream: h265parse is missing.\nInstall it with: sudo apt install \
             gstreamer1.0-plugins-bad\nThen start Reoling again."
        );
        assert_eq!(report.missing_element_text("nosuchelement"), "Cannot show this stream: nosuchelement is missing.");
        // Decoders there, none took the stream: no Auto advice (the mode may
        // be Auto already), System Status and the setting.
        let report = Report::new(complete(), Family::Debian, Plugins::System);
        let text = report.no_decoder_text(VideoType::H265);
        assert_eq!(
            text,
            "None of the H.265 decoders on this system could be used for this stream. System Status lists them; \
             another Decoding setting may help."
        );
        assert!(!text.contains("Auto"));
        // None there: the package.
        let report = Report::new(ubuntu_24_04(), Family::Debian, Plugins::System);
        assert_eq!(
            report.no_decoder_text(VideoType::H265),
            "No H.265 decoder on this system.\nInstall one with: sudo apt install gstreamer1.0-libav\nThen start \
             Reoling again."
        );
        // Another distribution: the GStreamer module.
        let report = Report::new(ubuntu_24_04(), Family::Other, Plugins::System);
        assert!(report.no_decoder_text(VideoType::H264).contains("GStreamer's FFmpeg plugin (gst-libav)"));
    }

    #[test]
    fn no_text_looks_like_a_loading_failure() {
        // The packaging smoke tests fail a run whose output has any of these
        // (scripts/smoke-appimage-in-docker.sh, .github/workflows/release.yml).
        let failures = [
            "Unable to load image-loading module",
            "Failed to load module",
            "Failed to load plugin",
            "cannot open shared object file",
            "symbol lookup error",
            "undefined symbol",
            "Missing decoder",
        ];
        let mut facts = without(ubuntu_24_04(), &["queue", "qtmux", "appsink", "autoaudiosrc"]);
        facts.audio_output = false;
        facts.audio_input = false;
        for family in [Family::Debian, Family::Fedora, Family::Suse, Family::Arch, Family::Other] {
            for plugins in [Plugins::Bundled, Plugins::SystemFromAppImage, Plugins::System] {
                let report = Report::new(facts.clone(), family, plugins);
                let mut texts = vec![
                    report.text(),
                    report.check_text(),
                    report.summary_line().unwrap_or_default(),
                    report.status_lines().join("\n"),
                    report.no_decoder_text(VideoType::H264),
                    report.no_decoder_text(VideoType::H265),
                ];
                texts.extend(["h264parse", "gtk4paintablesink", "appsrc"].map(|e| report.missing_element_text(e)));
                for text in texts {
                    for failure in failures {
                        assert!(!text.contains(failure), "{family:?} {plugins:?}: {text}");
                    }
                }
            }
        }
    }
}
