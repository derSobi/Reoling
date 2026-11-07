# Building the packages

How to build the .deb and the two AppImages yourself, as the release does, and how the release builds and tests them. The release builds on Ubuntu 24.04 LTS so that the packages run on 24.04 and everything newer; build there for the same result. You need, besides a clone of the repository:

- the Ubuntu 24.04 packages listed in `scripts/noble-packages.txt`;
- Rust 1.92 or newer for the GTK 4 sink (the app itself needs 1.88, see [Building from source](../README.md#building-from-source));
- `cargo-deb` for the .deb, and network access (the sink's crate from crates.io, the AppImage tools and runtime);
- Docker, only to rehearse the release in a container and for the container smoke tests.

## Build

Run these from the top of the repository.

1. Install the build packages, the same set the release installs:

   ```bash
   sudo apt-get update
   sudo apt-get install --no-install-recommends $(grep -v '^#' scripts/noble-packages.txt)
   ```

2. Build the app:

   ```bash
   cargo build --release --locked
   ```

3. Build GStreamer's GTK 4 paintable sink, which all three packages carry and use ahead of one the distribution may have (Ubuntu 24.04 has no `gstreamer1.0-gtk4`; Debian 13 and Ubuntu 26.04 have another version). Beside `libgstgtk4.so` it writes the sink's licence, `LICENSE-MPL-2.0`, and `SOURCE`, which says where its source is; the system AppImage carries both:

   ```bash
   scripts/build-gst-gtk4-plugin.sh target/gst-gtk4
   ```

4. Build the .deb, `target/debian/reoling_<version>-1_amd64.deb`:

   ```bash
   cargo install cargo-deb --locked --version 3.8.0
   cargo deb
   ```

5. Fetch the AppImage tools (linuxdeploy, its plugins and appimagetool, at pinned versions) and the AppImage runtime, then build both AppImages:

   ```bash
   scripts/fetch-appimage-tools.sh ~/appimage-tools
   export APPIMAGE_TOOLS=~/appimage-tools
   scripts/build-appimage.sh target/release/reoling dist
   scripts/build-appimage-app-only.sh target/release/reoling dist
   ```

   - `scripts/build-appimage.sh` builds the embed AppImage, `dist/Reoling-<version>-embed-x86_64.AppImage`, with linuxdeploy: GTK 4, GStreamer, its plugins and the decoders inside, about 100 MB.
   - `scripts/build-appimage-app-only.sh` builds the system AppImage, `dist/Reoling-<version>-system-x86_64.AppImage`, without linuxdeploy: the app, the sink, the icons and the desktop entry, about 6 MB.
   - `scripts/appimage-common.sh`, which both source, holds what they share: the two file names, AppRun, the icons, and the appimagetool call. Both AppImages carry the same AppRun, icons and desktop entry at the same places.
   - The runtime, `runtime-x86_64`, is type2-runtime's `continuous` build, which appimagetool would otherwise download by itself. It has no fixed version to pin, so `scripts/fetch-appimage-tools.sh` fetches it at every call and prints its SHA-256; both builds start their AppImage with that one file.

   Before it packs, `scripts/build-appimage-app-only.sh` checks the system AppImage on this machine, which needs GTK 4.14 and GStreamer 1.24 or newer with every plugin the app uses (the packages of step 1 have them):
   - the app and the sink are its only programs and libraries, and the icons and the desktop entry are where the app reads them;
   - its hook (`scripts/appimage-app-only-hook.sh`) takes this machine's GTK and GStreamer, says so in exactly one line with the versions `pkg-config` gives, and sets the environment the app expects;
   - every GStreamer element the app uses registers, and `gtk4paintablesink` is the AppImage's own;
   - on copies: a library the system lacks, and a GTK too old, each stop the start with status 127 before the app runs, and `REOLING_SKIP_SYSTEM_CHECK=1` starts it all the same.

Or, with Docker, rehearse the whole release in an `ubuntu:24.04` container and run the container smoke tests. The packages, and their `SHA256SUMS`, land in the directory given (`~/reoling-validate` here; any directory, created if missing):

```bash
scripts/validate-in-docker.sh --host ~/reoling-validate
```

It runs the system AppImage's Ubuntu 24.04 scenarios (with `ALL_DISTROS=1` set, every distribution's) and both of the .deb's. For AppImages built elsewhere, name them after the directory: it then runs only their smoke tests.

```bash
scripts/validate-in-docker.sh --host ~/reoling-validate dist/Reoling-<version>-*-x86_64.AppImage
```

## The smoke tests

Each package is started for 20 s on a virtual display with a session bus and an empty configuration, so no device is contacted: it must still be running when the time is up, and nothing it loads may fail to load. A critical warning from GTK, GLib or GStreamer (`Gdk-CRITICAL **: ...`) fails the run as well.

Each package's requirements report is checked as well, by `scripts/assert-requirements.sh <program> <status> <gaps> <sink> [<text>...] [--absent <text>...]`. It runs `<program> --check-requirements` with no display and requires the exit status, exactly the missing features given (`none`, or a list of `picture`, `h264`, `h265`, `sound`, `talk`, `recording`, `downloads`), the `gtk4paintablesink` file and version in the `Display:` line, and each text given (an install command, the embed AppImage's name); a text after `--absent` (the package of a plugin the system has loaded) must not be there.

The embed AppImage, in containers (`scripts/smoke-appimage-in-docker.sh <AppImage> [<scenario>...]`):

| Scenario | System | What it must do |
|---|---|---|
| `ubuntu-26.04-yaru-dark` | Ubuntu 26.04, Yaru-dark | take the system's GTK, with no theme parser error |
| `ubuntu-26.04-no-libva-x11` | Ubuntu 26.04 without `libva-x11` | bring its own; the software decoders register |
| `ubuntu-24.04-no-gtk` | Ubuntu 24.04 without GTK 4 | take its own GTK and GStreamer |

In each, the report must give status 0, nothing missing, the AppImage's own sink and `Plugins: bundled with the AppImage`; the app's line for a missing part, `reoling: missing on this system:`, fails the test.

The system AppImage, in containers (`scripts/smoke-app-only-in-docker.sh <AppImage> [<scenario>...]`, with `DEB=<the .deb>` for `ubuntu-24.04-deb-deps`):

| Scenario | System | Report |
|---|---|---|
| `ubuntu-24.04-bare`, `debian-13`, `ubuntu-26.04`, `archlinux` | as it comes, then with the command it printed | 0 |
| `fedora-44` | as it comes, then with the command it printed | 1: H.265 |
| `opensuse-tumbleweed` | as it comes, then with the command it printed | 1: H.265 |
| `ubuntu-24.04-desktop` | what an Ubuntu 24.04 desktop installs | 1: H.264, H.265, sound |
| `ubuntu-24.04-deb-deps` | the .deb's dependencies | 0 |
| `fedora-44-noopenh264` | a Fedora desktop, OpenH264 on the noopenh264 stand-in | 1: H.264, H.265 |
| `ubuntu-24.04-notify` | started from a desktop with no dialog program | refused, one notification |
| `ubuntu-24.04-zenity-fails` | the same, with a zenity that does not start | refused, one notification |
| `ubuntu-24.04-zenity-closed` | the same, with a dialog the user closes | refused, no notification |
| `ubuntu-24.04-zenity-no-display` | the same, with a zenity that cannot open the display | refused, one notification |

- A scenario "as it comes" starts from the distribution's bare image: the AppImage must refuse with status 127 and print exactly that distribution's install command and the embed AppImage's name. The test then runs that command (without recommended or weak dependencies), adds, on Debian and Ubuntu, the desktop's sound plugin, and the distribution's own GTK 4 sink where it has one, and starts the AppImage again.
- Where it starts, it must name the system's GTK in exactly one line, map the system's `libgtk-4`, and write a plugin registry of its own whose one `gtk4paintablesink` is the AppImage's; the report must give the status and missing features of the table, with the advice texts for that distribution, and print the app's line at start exactly when something is missing.
- The report must not name the package of a plugin that is loaded there: on Fedora `gstreamer1-plugin-libav` (with the noopenh264 stand-in also `gstreamer1-plugin-openh264`), on openSUSE `gstreamer-plugins-libav`.
- The release runs the Ubuntu 24.04 scenarios in its job **.deb and AppImages**, and each other distribution in a job of its own.

The .deb, in containers of the distributions that have a GTK 4 sink of their own (`scripts/smoke-deb-in-docker.sh <.deb> [<scenario>...]`):

| Scenario | System | Report |
|---|---|---|
| `ubuntu-26.04-gtk4` | Ubuntu 26.04 with `gstreamer1.0-gtk4` | 0 |
| `debian-13-gtk4` | Debian 13 with `gstreamer1.0-gtk4` | 0 |

- The image has the distribution's sink and the .deb's dependencies, and apt installs the .deb in it without a network. `reoling --version` must give the .deb's version; the 20 s run must show no loading failure, critical warning or missing part, and write no thumbnailer and no per-user desktop entry.
- The report must give status 0, nothing missing and the .deb's own sink, `/usr/lib/reoling/gstreamer-1.0/libgstgtk4.so`, in the version `scripts/build-gst-gtk4-plugin.sh` builds, not the distribution's.
- The plugin registry must be Reoling's own, `~/.cache/reoling/gstreamer-registry-deb-<GStreamer version>.bin`, and the one other GStreamer programs share (`~/.cache/gstreamer-1.0`) must not be written. With `GST_REGISTRY_FORK=no`, where GStreamer loads the plugins in the app's own process while it scans them, the report must be the same, with no registry and with the one that start wrote.
- The release runs each in a job of its own; its job **.deb and AppImages** runs the .deb on Ubuntu 24.04, which has no such sink.

## What it touches

- `target/`: the build, the sink in `target/gst-gtk4` (with `LICENSE-MPL-2.0` and `SOURCE`) and the .deb in `target/debian`;
- `dist/`: the two AppImages;
- the tools directory given in step 5 (`~/appimage-tools` above), with the runtime, `runtime-x86_64`;
- `~/.cache/reoling-build/gst-gtk4`: the sink's sources and build (set `GST_GTK4_WORK` for another place);
- `cargo-deb`, installed by `cargo install`;
- the system packages of step 1 (apt), and the Rust crates in `~/.cargo`;
- with the rehearsal and the container smoke tests: the Docker images they build (`reoling-smoke:*`, `reoling-smoke-app-only:*` and `reoling-smoke-deb:*`, set `SMOKE_IMAGE_PREFIX` for other names) and the output directory.

## How the release builds and tests them

The release workflow, `.github/workflows/release.yml`, runs on Ubuntu 24.04, in four jobs:

1. A pushed tag `v<version>` starts it and gets the three packages uploaded to its release. A manual run only keeps them, with their `SHA256SUMS`, as a workflow artifact. Either way the version must be the one in `Cargo.toml`.
2. The job **.deb and AppImages**:
   - installs the packages in `scripts/noble-packages.txt` and Rust stable, builds the app and runs the tests, the video zoom rendering test among them, under Xvfb;
   - builds the sink, the .deb (`cargo-deb` 3.8.0) and both AppImages, as in [Build](#build), and checks that both AppImages carry the same AppRun, icons and desktop entry;
   - starts the packages: `--version`, then 20 s on a virtual display, where no loading failure and no missing part may show. The embed AppImage goes first, on either GTK (the system's, then its own), then the system AppImage, whose own plugin registry must name its own sink and the video elements; then the .deb is installed and started the same way. Each package's requirements report must give status 0, nothing missing and its own sink;
   - runs the container smoke tests of the embed AppImage and the Ubuntu 24.04 scenarios of the system AppImage (see [The smoke tests](#the-smoke-tests));
   - writes `SHA256SUMS` and keeps the three packages and it as the workflow artifact.
3. The jobs **App-only AppImage**, one per distribution, run the system AppImage of that artifact on Debian 13, Ubuntu 26.04, Fedora 44 (also with the noopenh264 stand-in), openSUSE Tumbleweed and Arch Linux.
4. The jobs **.deb on ubuntu-26.04-gtk4** and **.deb on debian-13-gtk4** install the .deb of that artifact beside the distribution's `gstreamer1.0-gtk4`, whose sink it must not use (see [The smoke tests](#the-smoke-tests)).
5. Only when all of them have passed, and only for a tag, the job **Upload to the release** uploads the .deb and the two AppImages to it. `SHA256SUMS` stays in the artifact, for the release notes.

## Updating

For a newer version, update the clone and run the steps again. `scripts/fetch-appimage-tools.sh` keeps the tools already present with the right checksum, and fetches the runtime again.

## Removing

Delete `target/`, `dist/`, the tools directory and `~/.cache/reoling-build`; `cargo uninstall cargo-deb` removes `cargo-deb`. The packages of step 1 stay; remove them with apt if nothing else needs them. Remove the Docker images with `docker image rm`.

## Troubleshooting

- **The sink build fails on an older Rust.** It needs Rust 1.92 or newer.
- **`cargo deb` misses `target/gst-gtk4/libgstgtk4.so`.** Run step 3 first.
- **`build-appimage-app-only.sh` says `no .../target/gst-gtk4/LICENSE-MPL-2.0: run scripts/build-gst-gtk4-plugin.sh target/gst-gtk4 first`** (or `SOURCE`). The sink was built before the sink build wrote them: run step 3 again.
- **`build-appimage.sh` or `build-appimage-app-only.sh` says a tool is not found.** For linuxdeploy, appimagetool or their plugins, point `APPIMAGE_TOOLS` at the directory from step 5; for `patchelf`, `file`, `readelf`, `pkg-config` or `dpkg-query`, install the packages of step 1.
- **`fetch-appimage-tools.sh` fails on `runtime-x86_64`.** The runtime comes from GitHub (`https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64`) at every call; check the network and run it again.
- **`build-appimage-app-only.sh` stops at a check: an element missing, or a different GTK or GStreamer version.** The machine lacks a package of step 1, or `pkg-config` describes another GTK or GStreamer than the one the libraries are: build on Ubuntu 24.04 with those packages.
- **The AppImage tools do not start: no FUSE** (in a container, for example). Set `APPIMAGE_EXTRACT_AND_RUN=1`, and the tools run without it.
