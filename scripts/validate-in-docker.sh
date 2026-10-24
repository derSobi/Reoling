#!/usr/bin/env bash
# Rehearses the release workflow (.github/workflows/release.yml) in a plain
# Ubuntu 24.04 container before anything is pushed: the same packages
# (scripts/noble-packages.txt), rustup, build, tests, cargo deb, the AppImage
# and the smoke tests. Leaves the packages, the .deb's control file and the
# list of installed packages in the output directory.
#
#   mkdir -p /some/output
#   docker run --rm -v "$PWD":/src:ro -v /some/output:/out ubuntu:24.04 \
#       bash /src/scripts/validate-in-docker.sh
#
# Environment (docker run -e):
#   SKIP_TESTS=1   leave out cargo test
set -euo pipefail
src=${1:-/src}
out=${2:-/out}
export DEBIAN_FRONTEND=noninteractive APPIMAGE_EXTRACT_AND_RUN=1 LC_ALL=C.UTF-8
start=$(date +%s)
step() { echo; echo "=== $* [$(( $(date +%s) - start )) s]"; }

step "Ubuntu $(. /etc/os-release && echo "$VERSION_ID"): installing the packages"
apt-get update -qq
# shellcheck disable=SC2046
apt-get install -y -qq --no-install-recommends $(grep -v '^#' "$src/scripts/noble-packages.txt")

step "Rust"
if ! command -v cargo >/dev/null; then
    curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
    sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable >/dev/null
    rm /tmp/rustup-init.sh
fi
export PATH="$HOME/.cargo/bin:$PATH"
cargo --version
rustc --version

step "Sources (without target/ and .git/)"
rm -rf /tmp/src
mkdir -p /tmp/src
tar -C "$src" --exclude=./target --exclude=./.git -cf - . | tar -C /tmp/src -xf -
cd /tmp/src

step "cargo build --release"
cargo build --release --locked

if [ -z "${SKIP_TESTS:-}" ]; then
    step "cargo test --release"
    xvfb-run -a cargo test --release --locked
fi

step "GTK 4 paintable sink plugin"
scripts/build-gst-gtk4-plugin.sh target/gst-gtk4

step "cargo deb"
cargo install cargo-deb --locked --version 3.8.0 >/dev/null
cargo deb
mkdir -p "$out"
rm -f "$out"/reoling_*.deb
cp target/debian/reoling_*.deb "$out/"

step "AppImage"
scripts/fetch-appimage-tools.sh /tmp/appimage-tools
APPIMAGE_TOOLS=/tmp/appimage-tools scripts/build-appimage.sh target/release/reoling "$out"

step "Smoke test: the .deb"
apt-get install -y -qq "$out"/reoling_*.deb
xvfb-run -a reoling --version
dpkg -L reoling
# The app itself, for 20 s on a virtual display with a session bus: it must
# still be running when the time is up (124 from timeout), and nothing it
# loads may have failed to (a module, a pixbuf loader, a symbol, a decoder).
run_for_20s() {
    local rc=0
    dbus-run-session -- xvfb-run -a timeout 20 "$1" 2> /tmp/smoke-stderr.log || rc=$?
    cat /tmp/smoke-stderr.log
    [ "$rc" -eq 124 ] || { echo "$1 ended early with status $rc" >&2; exit 1; }
    if grep -E 'Unable to load image-loading module|Failed to load module|symbol lookup error|undefined symbol|Missing decoder' /tmp/smoke-stderr.log; then
        echo "$1 printed a loading failure (above)" >&2
        exit 1
    fi
    echo "$1 ran for 20 s"
}
run_for_20s reoling

step "Smoke test: the AppImage"
appimage=$(ls "$out"/Reoling-*-x86_64.AppImage)
chmod +x "$appimage"
xvfb-run -a "$appimage" --version
run_for_20s "$appimage"

step "Record"
dpkg-query -W -f='${Package} ${Version}\n' > "$out/noble-installed-packages.txt"
dpkg-deb -I "$out"/reoling_*.deb > "$out/deb-control.txt"
dpkg-deb -c "$out"/reoling_*.deb > "$out/deb-contents.txt"
{
    echo "built $(date -u +%FT%TZ) on Ubuntu $(. /etc/os-release && echo "$VERSION_ID"), $(rustc --version)"
    echo "glibc $(ldd --version | head -n1 | awk '{print $NF}'), GTK $(pkg-config --modversion gtk4), GStreamer $(pkg-config --modversion gstreamer-1.0)"
    ls -l "$out"/reoling_*.deb "$appimage"
} | tee "$out/summary.txt"
step "Done"
