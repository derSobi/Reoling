# Building the packages

How to build the .deb and the AppImage yourself, as the release does, and
how the release builds and tests them. The release builds on Ubuntu 24.04
LTS so that both packages run on 24.04 and everything newer; build there
for the same result. You need, besides a clone of the repository:

- the Ubuntu 24.04 packages listed in `scripts/noble-packages.txt`;
- Rust 1.92 or newer for the GTK 4 sink (the app itself needs 1.88, see
  [Building from source](../README.md#building-from-source));
- `cargo-deb` for the .deb, and network access (the sink's crate from
  crates.io, the AppImage tools);
- Docker, only to rehearse the release in a container.

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

3. Build GStreamer's GTK 4 paintable sink, which both packages carry
   (Ubuntu 24.04 has no `gstreamer1.0-gtk4`):

   ```bash
   scripts/build-gst-gtk4-plugin.sh target/gst-gtk4
   ```

4. Build the .deb, `target/debian/reoling_<version>-1_amd64.deb`:

   ```bash
   cargo install cargo-deb --locked --version 3.8.0
   cargo deb
   ```

5. Fetch the AppImage tools (linuxdeploy, its plugins and appimagetool, at
   pinned versions), then build `dist/Reoling-<version>-x86_64.AppImage`:

   ```bash
   scripts/fetch-appimage-tools.sh ~/appimage-tools
   APPIMAGE_TOOLS=~/appimage-tools scripts/build-appimage.sh target/release/reoling dist
   ```

Or, with Docker, rehearse the whole release in an `ubuntu:24.04` container
and run the container tests. The packages land in the directory given
(`~/reoling-validate` here; any directory, created if missing):

```bash
scripts/validate-in-docker.sh --host ~/reoling-validate
```

## What it touches

- `target/`: the build, the sink in `target/gst-gtk4` and the .deb in
  `target/debian`;
- `dist/`: the AppImage;
- the tools directory given in step 5 (`~/appimage-tools` above);
- `~/.cache/reoling-build/gst-gtk4`: the sink's sources and build (set
  `GST_GTK4_WORK` for another place);
- `cargo-deb`, installed by `cargo install`;
- the system packages of step 1 (apt), and the Rust crates in `~/.cargo`;
- with the rehearsal: the Docker images it builds and the output directory.

## How the release builds and tests them

The release workflow, `.github/workflows/release.yml`, runs on Ubuntu
24.04:

1. A pushed tag `v<version>` starts it and gets both files uploaded to its
   release. A manual run only keeps them as a workflow artifact. Either way
   the version must be the one in `Cargo.toml`.
2. It installs the packages in `scripts/noble-packages.txt` and Rust
   stable, builds the app and runs the tests, the video zoom rendering
   test among them, under Xvfb.
3. It builds the sink, the .deb (`cargo-deb` 3.8.0) and the AppImage, as
   in [Build](#build).
4. It starts both packages: `--version`, then 20 s on a virtual display.
   The AppImage goes first, on either GTK (the system's, then its own);
   then the .deb is installed and started the same way.
5. It starts the AppImage again in Ubuntu 26.04 and 24.04 containers
   (`scripts/smoke-appimage-in-docker.sh`).
6. Only then does it keep the files as a workflow artifact and, for a tag,
   upload them to the release.

## Updating

For a newer version, update the clone and run the steps again.
`scripts/fetch-appimage-tools.sh` keeps the tools already present with the
right checksum.

## Removing

Delete `target/`, `dist/`, the tools directory and
`~/.cache/reoling-build`; `cargo uninstall cargo-deb` removes `cargo-deb`.
The packages of step 1 stay; remove them with apt if nothing else needs
them.

## Troubleshooting

- **The sink build fails on an older Rust.** It needs Rust 1.92 or newer.
- **`cargo deb` misses `target/gst-gtk4/libgstgtk4.so`.** Run step 3
  first.
- **`build-appimage.sh` says a tool is not found.** For linuxdeploy,
  appimagetool or their plugins, point `APPIMAGE_TOOLS` at the directory
  from step 5; for `patchelf`, `file`, `readelf` or `dpkg-query`, install
  the packages of step 1.
- **The AppImage tools do not start: no FUSE** (in a container, for
  example). Set `APPIMAGE_EXTRACT_AND_RUN=1`, and the tools run without
  it.
