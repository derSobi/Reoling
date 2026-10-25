# <img src="data/icons/hicolor/512x512/apps/de.dersobi.reoling.png" width="64" valign="middle"> **Reoling**  

An unofficial client for Reolink® devices — cameras, NVRs, and Home Hubs —
that connects over Reolink's proprietary "Baichuan" P2P protocol using only
a device's UID (no manual IP/port configuration, no cloud account required).

**Website: <https://reoling.eu>**

Reoling is not affiliated with, endorsed by, or sponsored by Reolink.
"Reolink" is a trademark of its respective owner.

## Screenshots

<p align="center">
  <img src="docs/screenshots/main-screen.png" alt="Live view of a night camera stream, with the Home Hub Pro and RLN36 NVR devices and the camera controls" width="49%">
  <img src="docs/screenshots/remote.png" alt="The camera remote: pan/tilt, zoom, focus, Calibration, Monitor Point and Preset" width="49%">
</p>
<p align="center">
  <img src="docs/screenshots/live-settings.png" alt="Client Settings, Live View &amp; Playback" width="49%">
  <img src="docs/screenshots/download-settings.png" alt="Client Settings, Download Settings" width="49%">
</p>

More on <https://reoling.eu>.

## Status

Version **1.0.1**, with installable packages since 1.0.0 (see
[Installation](#installation)). Login, live video, PTZ and the camera
controls, the camera and device settings pages, and playback with sound,
event bars and downloads work end to end on real hardware — a Home Hub,
an NVR and their cameras — over UDP/P2P only: Reoling races the device's
local, NAT-mapped and relay addresses at the same time and uses whichever
answers first.

**What the app does today**

- Devices: add by UID or IP; every saved device connects at start and shows
  *Connected* / *Not connected*; one password per device, kept in the desktop
  keyring; the device's own name, channels (with their names) and the streams
  each channel offers (main / extern / sub) are read from the device.
- Live view: click a device or one of its channels; the lightest stream is
  the default; Stop freezes the picture and becomes Play; the last stream
  restarts on the next start; fullscreen with a control bar that slides in;
  a latency setting (Low / Balanced / Smooth) trades reaction time for
  smoothness over a poor connection.
- Sound: the stream's audio with a volume control; Talk (microphone to the
  camera, ADPCM).
- Playback (the tab beside Live View): recorded video per camera of a Home
  Hub or NVR, or of a camera added on its own, picked in the sidebar. A
  month calendar marks the days with recordings; the day timeline shows
  them as bands and seeks with a click or the arrow keys (10 s, 60 s with
  Shift), zooms with the wheel or `+`/`-` (Home shows the whole day again)
  and pans with a drag. Play/pause, speeds from 0.25× to 16×, main or sub
  stream, and playback goes on into the next recording by itself. At 1×
  the recording's sound plays in sync with the picture, at the volume set
  for live view; the other speeds are silent. The camera's detections
  are drawn as event bars over the recordings on the day timeline (the
  tooltip names them), a detection filter narrows them to any motion,
  person, vehicle, animal, package or line crossing / zone, and
  previous/next-event buttons jump between them. It runs over the same
  UDP P2P connection as live video, which pauses while the tab is open
  and comes back as it was. A Download button saves a chosen time range —
  a start, a length from 30 s to 15 min or the whole recording, main or
  sub stream — as an MP4 with the recording's sound into the recordings
  folder (another folder can be picked there and is remembered); a range
  that crosses into the next recording comes out as one file. The file is
  named after the device, camera, start and length, its range is marked
  on the timeline, and progress shows under the controls with Cancel and
  "Open folder"; the download runs on if the page moves to another day or
  device. While a device downloads, its playback and live video wait.
- Camera controls, shown per channel from what the camera reports: siren,
  spotlight, and — in a separate "Camera control" window, independent of the
  main window — pan/tilt (hold a direction to move) and zoom/focus on its
  main page (read back from the camera after every change, since zooming
  makes it autofocus); Monitor Point and Preset Points are their own pages,
  reached from there and left with "back", the same drill-down the official
  app uses. Monitor Point: a saved home position with its own thumbnail
  (click it to refresh — a point saved from Reoling has none until this is
  pressed once), Auto Return on/off and its timeout in 1-second steps,
  "Return to Monitor Point", "Reset Monitor Point" to save the current
  position. Presets: laid out like the official app's Preset Points — a
  thumbnail card per preset (or a plain list, via the toggle), a click
  anywhere on it moves the camera there, Edit (name and picture; click the
  picture to refresh it) and Delete beside it, "refresh all" and "add" on
  top. Every opening of the page re-reads all pictures from the camera,
  one at a time (it breaks on overlapping image requests). Calibration
  disables the panel with a spinner while it
  runs — all confirmed working against real hardware.
- Camera settings (the gear button beside Camera control): the camera's
  product picture, model and firmware, and its video encoding per stream.
  Clear (main) and Fluent (sub) can be changed: resolution, frame rate,
  bitrate and I-frame interval, offered only as the camera itself allows;
  the live picture restarts with the new settings. Balanced (extern) is
  shown as it is. Beside Encoding, read-only tabs show what else the
  camera tells about itself, with the values as the camera reports them:
  Info (model, firmware, UID), Image (brightness, contrast, saturation,
  sharpness, hue, corridor mode, day and night switching, the advanced
  picture settings), Display (date, time and camera name on the picture,
  the watermark), Detection (motion sensitivity and alarm delay, smart
  detection types and object sizes, auto tracking), Light & sound
  (status LED, infrared lights, the spotlight and its schedule, siren
  triggers, sound), Schedules (the weekly recording, email, push, FTP
  and siren schedules, drawn as grids) and Network (address mode, IP,
  mask, gateway, MAC, DNS). Rows a model does not have — binning, auto
  tracking, a status LED, "Pet" instead of "Animal" — are left out, as
  the camera's own ability table says. Reload reads a tab again; the
  tabs are read while the camera's live video runs (press Play first if
  it is stopped) and wait while a download holds the connection. Product
  pictures come from Reolink's own site and are cached; until one loads,
  or for a model without one, a drawing of the device kind (camera, NVR,
  Home Hub) stands in.
- Device settings ("Device settings…" in the card menu of a connected Home
  Hub or NVR): the device's own window, read-only, one tab each. Info
  (model, firmware, UID), Storage (each drive's size and fill level, what
  happens when it is full, recording encryption), Network (address, the
  service ports — HTTP, HTTPS, RTSP, RTMP, ONVIF, UPnP —, DDNS, NTP and,
  on a Hub, the Wi-Fi it offers its cameras), Time (time zone, clock,
  date and time format, daylight saving, NTP), Light & sound on a Hub
  (status LED, notification and alarm volume, alarm pause) or Alerts on
  an NVR (recording, email, push and buzzer per camera), Users (the
  accounts, shared access, login lockout) and Maintenance (auto reboot).
  A tab is read when it is first shown and again on Reload; while a
  download holds the connection it says so and reads once that is over.
- Snapshot (PNG in `~/Pictures/Reoling`) and recording (MKV in
  `~/Videos/Reoling`, where Playback's downloads go too).
- Settings ("Client Settings", laid out after the official app's window):
  System Settings (General, Live View & Playback, Alarm), Download Settings,
  Local Record and About. Working: Run at Startup (an XDG autostart entry),
  Theme, System Status (versions, the hardware decoders and the decoders in
  use), Auto Live View, Stretch Mode, decoding (Auto with the integrated or
  the dedicated GPU first / Hardware — with the decoder to use if there are
  several — / Software), latency, and the folders for screenshots and
  recordings. The official app's rows Reoling cannot act on yet (client
  update, add devices automatically, lockscreen password, date format,
  language, detection overlays, scrollview time, alarm beep, temporary and
  download folders, local record) are shown greyed out. Colours are the
  active theme's.

**Not done yet**

- Changing the settings on the new pages: only Encoding is editable; the
  camera and device pages are read-only.
- Playback of several cameras at once (one camera per page today), and
  thumbnails: the device list and the recordings show names and times,
  no pictures.
- Split View, zoom on cameras behind an NVR's sub-channel numbering.
- Playback: one download at a time per device, and the device's playback
  and live video wait while it runs; no download of several cameras or of
  a whole day at once.
- PTZ calibration has no separate "finished" signal from the camera — the
  app only knows the request was accepted, matching what's documented for
  Baichuan generally, not a limitation specific to this capture.
- Deleting a PTZ preset sends `delPos` (this project's best guess when it
  was first written, matched only against message counts in a capture; now
  confirmed working against a real camera).
- Talk has been built from the official app's captured messages but is the
  least tested part.
- Presets saved by an older Reoling (before 0.1.13) have no picture until
  one is taken in their Edit dialog.

**Version history**

- 1.0.1 — `reoling_1.0.1-1_amd64.deb` and `Reoling-1.0.1-x86_64.AppImage`,
  built as 1.0.0's were.
  - AppImage: it runs on the system's GTK 4 and GStreamer where they are
    at least as new as its own (GTK 4.14, GStreamer 1.24), so a desktop
    theme written for a newer GTK renders in full (on newer Ubuntu
    releases this brings back the Playback calendar's marks for the days
    with recordings), and on its own copies otherwise; a line on stderr
    names the choice, and `REOLING_GTK_STACK=host` or `bundled` forces
    it. On a system with libva but without libva-x11 the software decoders
    and AAC sound work too: the AppImage brings the libva libraries the
    system lacks.
  - Decoding: Auto prefers the integrated GPU's hardware decoder over a
    dedicated card's (1.0.0 took the one GStreamer ranked first, NVDEC
    over VA-API), then software; a second Auto mode, "Auto (dedicated GPU
    first)", prefers the card; an Auto stored by 1.0.0 is the first. System
    Status shows the kind of GPU each hardware decoder runs on, where it
    is known, and the decoders the latest live and recorded streams
    started with.
  - Starting Reoling while it runs brings its open window to the front
    instead of logging in to every device a second time.
  - A device refusing a login for another reason than a wrong password
    no longer makes Reoling forget the saved password, and neither does
    a mistyped password or user name in the login dialog.
  - With Theme set to Auto, the theme is loaded once at start instead of
    three times.

The full history, with the date of every version, is in
[CHANGELOG.md](CHANGELOG.md).

## How it works

Reoling implements the Baichuan protocol from scratch, in Rust, based on
observed wire behavior (see [Acknowledgments](#acknowledgments)). It never
requires the official Reolink cloud account or app — you connect directly
to a device by its UID, and Reoling resolves the P2P path itself.

A UDP receive task keeps draining the socket and acknowledging packets
independently of the video pipeline, and every video frame is pushed
straight into GStreamer from the network thread, so a busy GTK main loop
never affects playback. GStreamer's `decodebin` picks the decoder, in the
order the Decoding setting gives it: by default a hardware decoder on the
integrated GPU first, then one on a dedicated GPU, then software.

## Project layout

```text
src/
├── protocol/    Wire protocol only: binary framing, XML, BCEncrypt/AES.
│                No I/O, no async runtime — a pure, testable codec.
├── transport/   P2P/UDP: UID resolution, NAT traversal, relay fallback,
│                and the reliable BCUDP connection.
├── client.rs    Client/session API: login and live video;
│                `client/replay.rs`: recordings, events, playback and
│                downloads; `client/settings.rs`: the settings pages.
├── talk.rs      ADPCM encoding for Talk.
├── ui/          GTK4 + GStreamer desktop app (main window, sidebar,
│                dialogs, settings, playback, downloads, video and audio).
└── main.rs      The `reoling` binary.
examples/        Small diagnostic tools (UID probe, media trace analysis).
data/            Icons and the desktop entry.
docs/            Screenshots used by this README.
scripts/         Packaging: the GTK 4 sink build, the AppImage tools and
                 build, and a rehearsal of the release in a container.
```

Reoling is a Linux-only desktop application.

## Installation

Every release on GitHub — <https://github.com/derSobi/Reoling/releases> —
carries two packages for x86_64 Linux, both built on Ubuntu 24.04 LTS by
the release workflow (`.github/workflows/release.yml`) so that they run on
24.04 and everything newer:

- **`reoling_1.0.1-1_amd64.deb`** for Ubuntu 24.04 LTS or newer and
  Debian 13 or newer (and derivatives with at least those library
  versions): `sudo apt install ./reoling_1.0.1-1_amd64.deb`. apt
  installs the GTK 4 and GStreamer runtime with it — the base, good and
  bad plugin sets (parsers, muxers, the nvcodec and va hardware decoders),
  the libav software decoders and a PulseAudio, PipeWire or ALSA sink —
  and recommends a VA-API driver (`va-driver-all`, or the one for your
  GPU) for hardware decoding and a keyring (GNOME Keyring or KWallet) for
  the device passwords. Ubuntu 24.04 has no `gstreamer1.0-gtk4` package, so
  the .deb carries GStreamer's GTK 4 paintable sink itself, in
  `/usr/lib/reoling/gstreamer-1.0`, a directory `reoling` adds to its
  plugin registry when it starts. Reoling then appears in the
  applications menu; `reoling --version` prints the installed version.
- **`Reoling-1.0.1-x86_64.AppImage`** for any other distribution with
  glibc 2.39 and GLib 2.80 or newer (Ubuntu 24.04, Fedora 40, Debian 13
  and later): `chmod +x Reoling-1.0.1-x86_64.AppImage` and run it.
  The AppImage mounts itself with FUSE 2 (`libfuse2` or `libfuse2t64`);
  without it, `--appimage-extract-and-run` unpacks it to a
  temporary directory for that run, and `--appimage-extract` unpacks it
  for good. Inside are GTK 4, GStreamer with the parsers, muxers, decoders
  (libav in software; the nvcodec and va plugins for hardware) and sound
  sinks the app uses, its GTK 4 sink, librsvg and the icons it draws. It
  uses the system's GTK 4 and GStreamer instead where they are at least as
  new as its own (GTK 4.14, GStreamer 1.24), so it looks like the
  desktop's other applications with the desktop's own theme, and its
  bundled copies otherwise; a line on stderr says which
  (`reoling: GTK and GStreamer core: host` or `bundled`), and
  `REOLING_GTK_STACK=bundled` or `host` forces the choice. From the system
  it always takes glibc, GLib (whose GIO modules read the desktop's
  settings), Mesa (OpenGL/EGL) or the NVIDIA driver's GL, the X11 or
  Wayland client libraries, fontconfig and the fonts, PulseAudio or
  PipeWire (`pipewire-pulse`) or ALSA for sound, `libva` (the FFmpeg
  libraries behind the software decoders need it too, with `libva-drm` and
  `libva-x11`; any of them the system lacks comes from the AppImage), the
  hardware decoding driver — NVIDIA's `libcuda`/`libnvcuvid` for NVDEC, or
  a VA-API driver for the system's `libva` — and a Secret Service keyring
  (GNOME Keyring, KWallet) for the device passwords. It follows the
  desktop's GTK theme (and with it the accent colour); on the bundled GTK,
  a theme written for a newer GTK may print a few "Theme parser error"
  lines and lose some of its looks. It keeps its own GStreamer registry
  under `~/.cache/reoling`. The first
  time it runs it also gives its own file the Reoling icon in GNOME Files,
  which otherwise shows the bare "executable" icon for an AppImage: it
  copies the Reoling icon into your icon theme (under
  `~/.local/share/icons/hicolor`, for the sizes the system theme lacks)
  and sets it as the file's custom icon — the GIO attribute
  `metadata::custom-icon`, which Files shows at every zoom level and gvfs
  keeps per user and per path; by hand that is
  `gio set <file> metadata::custom-icon file://<icon>`. It also writes the
  file's thumbnail-cache entries under `~/.cache/thumbnails` for the file
  managers and dialogs that read the cache but no custom icon. Both are
  checked at every start, so a replaced file (a new download) or a moved
  one gets its icon back the next time it runs. Other file managers
  (Dolphin among them) may still show the generic icon. Where the .deb is
  not installed, it also writes the per-user desktop entry
  `~/.local/share/applications/de.dersobi.reoling.desktop`, which starts
  the AppImage file: Reoling then appears in the applications menu and the
  dock shows its icon. The entry names the copy started last (a source
  build writes the same entry for itself), and the menu hides it once that
  file is gone. A per-user entry hides the .deb's of the same name, so
  once the .deb is installed, the next start of either package (the menu
  still starts the AppImage until then) removes the entry Reoling wrote,
  and the .deb's takes over. If the AppImage file is deleted before that,
  Reoling is missing from the menu until `reoling` is started once from a
  terminal. That is all it touches, and only for the AppImage file it was
  started from: it installs no thumbnailer and no file type association,
  so nothing of Reoling's runs on other files.

Both packages decode video in hardware where they can: NVDEC
(`nvh264dec`/`nvh265dec`) on an NVIDIA GPU, VA-API (`vah264dec`/
`vah265dec`) on an Intel or AMD one. With Decoding set to Auto (integrated
GPU first), the default, Reoling orders the hardware decoders itself:
those on the integrated GPU first, then those on a GPU whose kind it
cannot tell, then those on a dedicated card, all of them ahead of the
libav software decoders, which take over where no driver is present. Auto
(dedicated GPU first) tries the card first. A GPU counts as dedicated when
switcheroo-control tags it so, or when it is an Intel or NVIDIA GPU behind
a PCI bridge; an Intel GPU at 00:02.0 on the root bus is the integrated
one, and any other GPU is of unknown kind. Settings → Live View &
Playback → Decoding switches to Hardware (with a choice of device when
there are several) or Software; System Status lists the hardware decoders
found (with the kind of GPU, where known), the decoding mode in use and
the decoders the latest live and recorded streams started with. Hardware
on a machine without a usable hardware decoder decodes in software
instead of leaving the picture black, and the Decoding row says so.

The workflow installs the .deb and starts both packages (`--version`, then
20 s on a virtual display) on Ubuntu 24.04, the AppImage there on either
GTK, and the AppImage again in Ubuntu 26.04 and 24.04 containers
(`scripts/smoke-appimage-in-docker.sh`) before it uploads them. To build
them yourself — the sink build needs a current stable Rust, and
`scripts/validate-in-docker.sh --host <output directory>` rehearses the
whole release in an `ubuntu:24.04` container and runs the container tests:

```bash
scripts/build-gst-gtk4-plugin.sh target/gst-gtk4   # the GTK 4 sink, where the distribution has none
cargo install cargo-deb && cargo deb                # target/debian/reoling_1.0.1-1_amd64.deb
scripts/fetch-appimage-tools.sh ~/appimage-tools    # linuxdeploy, its plugins, appimagetool (pinned)
APPIMAGE_TOOLS=~/appimage-tools scripts/build-appimage.sh target/release/reoling dist
```

## Building from source

Requires:

- Rust (edition 2021, `rust-version` 1.75+ — see `Cargo.toml`)
- GTK4 (`>= 4.6`, Ubuntu 22.04's baseline) and its development headers
- GStreamer, including `gstreamer-app`, the GTK4 paintable sink
  (`gstreamer1.0-gtk4`, or `scripts/build-gst-gtk4-plugin.sh` where the
  distribution has none) and a decoder plugin (`gstreamer1.0-libav` for
  software decoding; the nvcodec and va plugins from
  `gstreamer1.0-plugins-bad` for NVIDIA and VA-API hardware decoding)

```bash
cargo build --release
cargo run --release
```

Add a device with the **+** in the device list (its UID, or IP and port),
then log in with the device's username and password.

To run the full test suite (protocol codec, transport and session logic —
no real hardware required):

```bash
cargo test
```

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE) for the full text and
third-party component notices (GTK4, GStreamer, and the Rust crates
Reoling depends on).

## Acknowledgments

Reoling's understanding of the Baichuan protocol was built by reading
(never copying code from) other reverse-engineering efforts, most notably
[neolink](https://github.com/QuantumEntangledAndy/neolink) and Reolink's
own [reolink-cli](https://github.com/reolink/reolink-cli) (LAN-only,
closed-source, documentation only). See [LICENSE](LICENSE) for the full
list and their own licenses.
