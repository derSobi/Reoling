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

Version **1.0.3**, with installable packages since 1.0.0 (see
[Installation](#installation)). Login, live video, PTZ, the camera and
device settings pages, and playback with sound, events and downloads
work end to end on a real Home Hub, NVR and their cameras, over UDP/P2P
only (local, NAT-mapped or relay address, whichever answers first).

**What the app does today**

- Devices: add by UID or IP; each connects at start, with its own name,
  channels and streams; one password per device, in the desktop keyring.
- Live view of a device or channel, the lightest stream first, Stop and
  Play, a latency setting; fullscreen controls hide after 2.5 s unused.
- Digital zoom in live view and playback: the wheel zooms at the pointer
  up to 16×, a left drag pans, a middle click shows the whole picture.
- Sound with a volume control, and Talk from the computer's microphone
  to the camera.
- Playback per camera: a month calendar, a day timeline to seek and zoom,
  speeds from 0.25× to 16×, main or sub stream, sound at 1×.
- Events: the camera's detections as bars on the timeline, a filter by
  type, and buttons to jump to the previous or next event.
- Downloads of a time range, up to 15 min or the whole recording, as an
  MP4 with sound; the device's live video and playback wait meanwhile.
- Camera controls, as each camera reports them: siren, spotlight, and a
  Camera control window for pan/tilt, zoom/focus and calibration.
- Monitor Point (a home position with Auto Return) and Preset Points,
  with pictures; go to, add, edit and delete the presets.
- Camera settings: main and sub stream Encoding (resolution, frame rate,
  bitrate, I-frame interval) can be changed; the other tabs are shown.
- Save and Revert on a Home Hub's cameras for a few Display, Image and
  Light & sound rows (E1 Outdoor PoE) and noise reduction (RLC-1224A).
- Device settings of a Home Hub or NVR, read-only: info, storage,
  network, time, sound or alerts, users and maintenance.
- Snapshot (PNG in `~/Pictures/Reoling`) and recording (MKV in
  `~/Videos/Reoling`, where downloads go too).
- Client Settings: run at startup, the theme (Auto, Light or Dark), Auto
  Live View, Stretch Mode, System Status, snapshot and recording folders.
- Decoding: Auto tries the integrated GPU first (or the dedicated one),
  then software; Hardware or Software can be set instead.

Each of these in full: [docs/FEATURES.md](docs/FEATURES.md).

**Not done yet**

- Changing settings beyond Encoding and the Home Hub camera rows above.
- Split View; the camera's own zoom behind an NVR's sub-channel numbering.
- Several cameras on one Playback page; thumbnails in the lists.
- One download at a time per device; no multi-camera or whole-day batch.
- Presets saved before 0.1.13 have no picture until one is taken in Edit.
- Talk is the least tested part; PTZ calibration has no "finished" signal.

**Version history**

- 1.0.3 — the .deb and the AppImage, built as 1.0.2's were. Fullscreen
  controls lie under the picture, as wide as it, so on an ultrawide
  monitor they no longer sit beside it. On X11 the windows' `WM_CLASS`
  is `de.dersobi.reoling`, as the desktop entries say, so a dock can
  show Reoling's icon. A connection that stays up for days skips the
  request numbers the device sends its own messages under. The .deb
  also installs docs/FEATURES.md, and both packages carry the GTK 4 sink
  stripped; Cargo.toml names Rust 1.88 as the minimum.

The full history, with the date of every version, is in
[CHANGELOG.md](CHANGELOG.md).

## Tested devices

Reoling is built and tested on a Home Hub Pro and two RLN36 NVRs and the
cameras on them. All three are added by UID and reached over UDP/P2P: the
Hub on its local network and through the relay, the two NVRs through the
relay only.

| Device | What | Model | Hardware | Firmware |
|---|---|---|---|---|
| Home Hub Pro | the Hub itself | Reolink Home Hub Pro | BASE_WENNT3NA5 | v3.3.0.605 |
| Home Hub Pro | 2 cameras | E1 Outdoor PoE | IPC_560SD88MP | v3.1.0.5223 |
| Home Hub Pro | 1 camera | RLC-1224A | IPC_NT18NA612MP | v3.2.0.6011 |
| RLN36 NVR (1) | the NVR itself | RLN36 | N5MB01 | v3.6.5.597 |
| RLN36 NVR (1) | 9 cameras | RLC-1224A | IPC_523D8128M12MP | v3.1.0.2174 |
| RLN36 NVR (1) | 3 cameras | RLC-820A | IPC_56064M8MP | v3.1.0.2368 |
| RLN36 NVR (1) | 1 camera | E1 Outdoor PoE | IPC_560SD88MP | v3.1.0.2448 |
| RLN36 NVR (1) | 1 camera | P334 | IPC_NT2NA48MP | v3.1.0.4666 |
| RLN36 NVR (2) | the NVR itself | RLN36 | N5MB01 | v3.5.0.329 |
| RLN36 NVR (2) | 1 camera | Reolink Duo 2 PoE | IPC_529B17B8MP | v3.0.0.1889 |
| RLN36 NVR (2) | 1 camera | RLC-1224A | IPC_523D8128M12MP | v3.1.0.2174 |
| RLN36 NVR (2) | 5 cameras | RLC-820A | IPC_56064M8MP | v3.1.0.1782 |
| RLN36 NVR (2) | 1 camera | P330 | IPC_NT2NA48MP | v3.1.0.4054 |

Live view is tested on all three of the Hub's cameras, on the first NVR's
RLC-1224A, on the second NVR's Reolink Duo 2 PoE and P330 (main and sub
stream, and the P330's extern stream) and on the main and sub stream of
one of its RLC-820A, with a switch from the main stream back to the sub
stream. Playback, downloads and the settings pages are tested on cameras
of the Hub and the first NVR: the Hub's E1 Outdoor PoE and RLC-1224A, the
first NVR's RLC-1224A and P334, and the encoding settings also on the
first NVR's E1 Outdoor PoE. On the second NVR, the camera settings window
was opened on all eight cameras without changing anything (the Info page
on all of them, the Display page on the P330); Playback does not list its
recordings yet, although the NVR answers the searches. PTZ is tested on
the Hub's E1 Outdoor PoE cameras. The first NVR's RLC-820A cameras have
no individual tests recorded. None of these cameras runs on a battery, so
we have not tested battery or 4G cameras ourselves.

**Reported by users**

- Works: RLC-823S1, on Reoling 1.0.1
  ([issue #3](https://github.com/derSobi/Reoling/issues/3)).
- Not working yet: Reolink Go (1st generation, firmware v1.0.213.00)
  connects and accepts the login, but refuses the live view request for
  the sub stream with code 400
  ([issue #3](https://github.com/derSobi/Reoling/issues/3)).

Other Reolink models may work too, but are untested. If you try one, please
open an [issue](https://github.com/derSobi/Reoling/issues) with the model,
its firmware and what works or fails (no UID or password).

## How it works

Reoling implements the Baichuan protocol from scratch, in Rust, based on
observed wire behavior (see [Protocol](#protocol)). It needs no Reolink
cloud account and no other software — you connect directly to a device by
its UID, and Reoling resolves the P2P path itself.

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
│                downloads; `client/settings.rs`: the settings pages
│                and their writes.
├── talk.rs      ADPCM encoding for Talk.
├── ui/          GTK4 + GStreamer desktop app (main window, sidebar,
│                dialogs, settings, playback, downloads, video and audio).
└── main.rs      The `reoling` binary.
examples/        Small diagnostic tools (UID probe, media trace analysis).
data/            Icons and the desktop entry.
docs/            FEATURES.md (every feature), the installation, hardware
                 decoding and package-building guides, and the README's
                 screenshots.
scripts/         Packaging: the GTK 4 sink build, the AppImage tools and
                 build, and a rehearsal of the release in a container.
```

Reoling is a Linux-only desktop application.

## Installation

Download a package from the
[latest release](https://github.com/derSobi/Reoling/releases/latest). Both
are for x86_64 Linux, built on Ubuntu 24.04 LTS so that they run on 24.04
and everything newer.

- **.deb**, for Ubuntu 24.04 LTS or newer, Debian 13 or newer and their
  derivatives. apt installs it with everything it needs:
  `sudo apt install ./reoling_*_amd64.deb` (with several in the folder,
  type the full name). Guide: [docs/INSTALL-DEB.md](docs/INSTALL-DEB.md).
- **AppImage**, for any other distribution with glibc 2.39 and GLib 2.80
  or newer. Make it executable, then start it by its full name
  (`<version>` as in the file's name): `chmod +x Reoling-*-x86_64.AppImage`,
  then `./Reoling-<version>-x86_64.AppImage`.
  Guide: [docs/INSTALL-APPIMAGE.md](docs/INSTALL-APPIMAGE.md).

Both decode video in hardware where a driver allows it, and in software
otherwise: [docs/HARDWARE-DECODING.md](docs/HARDWARE-DECODING.md). To build
the packages yourself, and for how the release tests them:
[docs/BUILDING-PACKAGES.md](docs/BUILDING-PACKAGES.md).

## Building from source

Requires:

- Rust 1.88+ (edition 2021; required by the locked dependencies)
- GTK4 (`>= 4.6`, Ubuntu 22.04's baseline) with its Wayland backend, and
  its development headers
- `libwayland-client` 1.20 or newer and its development headers
  (`libwayland-dev`, which `libgtk-4-dev` pulls in)
- GStreamer, including `gstreamer-app`, the GTK4 paintable sink
  (`gstreamer1.0-gtk4`, or `scripts/build-gst-gtk4-plugin.sh`, which needs
  Rust 1.92 or newer, where the distribution has none) and a decoder
  plugin (`gstreamer1.0-libav` for software decoding; the nvcodec and va
  plugins from `gstreamer1.0-plugins-bad` for NVIDIA and VA-API hardware
  decoding)

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

The video zoom rendering test needs a display and runs separately. The
release workflow and Docker validation run it under Xvfb:

```bash
cargo test --bin reoling ui::zoom_picture::tests::widget_renders_and_resets -- --ignored --exact
```

To build the .deb and the AppImage, see
[docs/BUILDING-PACKAGES.md](docs/BUILDING-PACKAGES.md).

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE) for the full text and
third-party component notices (GTK4, GStreamer, and the Rust crates
Reoling depends on).

## Protocol

Reoling's Baichuan implementation is checked against packet captures of the
project's own devices and against probes run on them. Third-party licenses
are in [LICENSE](LICENSE).
