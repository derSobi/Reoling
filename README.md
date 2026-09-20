# <img src="data/icons/hicolor/512x512/apps/de.dersobi.reoling.png" width="64" valign="middle"> **Reoling**  

An unofficial client for Reolink® devices — cameras, NVRs, and Home Hubs —
that connects over Reolink's proprietary "Baichuan" P2P protocol using only
a device's UID (no manual IP/port configuration, no cloud account required).

Reoling is not affiliated with, endorsed by, or sponsored by Reolink.
"Reolink" is a trademark of its respective owner.

## Status

Early development (0.1.0). The Baichuan protocol (binary framing, XML
payloads, BCEncrypt/AES encryption, P2P discovery and NAT traversal),
login and live video work end-to-end against real hardware, rendered in
the desktop app itself (H.264 and H.265, auto-detected). Connections go
over UDP/P2P: Reoling races the device's local, NAT-mapped and relay
addresses at the same time and uses whichever answers first, exactly as
the official client does.

There is no packaged release yet — see [Installation](#installation).

## How it works

Reoling implements the Baichuan protocol from scratch, in Rust, based on
observed wire behavior (see [Acknowledgments](#acknowledgments)). It never
requires the official Reolink cloud account or app — you connect directly
to a device by its UID, and Reoling resolves the P2P path itself.

A UDP receive task keeps draining the socket and acknowledging packets
independently of the video pipeline, and every video frame is pushed
straight into GStreamer from the network thread, so a busy GTK main loop
never affects playback. GStreamer's `decodebin` picks the best available
decoder (hardware or software).

## Project layout

```text
src/
├── protocol/    Wire protocol only: binary framing, XML, BCEncrypt/AES.
│                No I/O, no async runtime — a pure, testable codec.
├── transport/   P2P/UDP: UID resolution, NAT traversal, relay fallback,
│                and the reliable BCUDP connection.
├── client.rs    Client/session API: login and live video.
├── ui/          GTK4 + GStreamer desktop app (connect dialog, video view).
└── main.rs      The `reoling` binary.
examples/        Small diagnostic tools (UID probe, media trace analysis).
data/            Icons.
```

Reoling is a Linux-only desktop application.

## Installation

Not yet packaged. Planned distribution once the app reaches a usable
state:

- **Ubuntu / Debian**: a `reoling_<version>_amd64.deb` package.
- **Linux, distro-independent**: a `Reoling.AppImage` build.

Until then, build from source (see below).

## Building from source

Requires:

- Rust (edition 2021, `rust-version` 1.75+ — see `Cargo.toml`)
- GTK4 (`>= 4.6`, Ubuntu 22.04's baseline) and its development headers
- GStreamer, including `gstreamer-app`, the GTK4 paintable sink
  (`gstreamer1.0-gtk4`) and a decoder plugin (`gstreamer-libav` for
  software decoding; VA-API/NVDEC plugins for hardware decoding)

```bash
cargo build --release
cargo run --release
```

Connect with the device's UID, a username and a password, and pick the
zero-based channel. `--prefer-tcp` is a diagnostic-only flag that tries a
direct TCP connection when the device is reachable that way; the normal
path is always UDP/P2P.

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
