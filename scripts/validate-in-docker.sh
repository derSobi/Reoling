#!/usr/bin/env bash
# Rehearses the release workflow (.github/workflows/release.yml) in a plain
# Ubuntu 24.04 container before anything is pushed: the same packages
# (scripts/noble-packages.txt), rustup, build, tests, cargo deb, the AppImage
# and the smoke tests (the AppImage first, then the .deb, as there). Leaves
# the packages, the .deb's control file and the list of installed packages
# in the output directory.
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

# The smoke tests of the release workflow. The app runs for 20 s on a
# virtual display with a session bus and an empty configuration (no devices
# to reach): it must still be running when the time is up (124 from
# timeout), and nothing it loads may have failed to (a module, a pixbuf
# loader, a GStreamer plugin, a library, a symbol, a decoder). Ubuntu's
# xvfb-run sends the program's stderr to its stdout, so the log takes both.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
export XDG_CONFIG_HOME=/tmp/smoke-config XDG_CACHE_HOME=/tmp/smoke-cache XDG_DATA_HOME=/tmp/smoke-data
rm -rf "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
run_for_20s() {
    local rc=0
    dbus-run-session -- xvfb-run -a timeout 20 "$1" > /tmp/smoke.log 2>&1 || rc=$?
    cat /tmp/smoke.log
    [ "$rc" -eq 124 ] || { echo "$1 ended early with status $rc" >&2; exit 1; }
    if grep -E 'Unable to load image-loading module|Failed to load module|Failed to load plugin|cannot open shared object file|symbol lookup error|undefined symbol|Missing decoder' /tmp/smoke.log; then
        echo "$1 printed a loading failure (above)" >&2
        exit 1
    fi
    echo "$1 ran for 20 s"
    # No build installs a thumbnailer.
    [ ! -e "$XDG_DATA_HOME/thumbnailers" ] || { echo "$1 wrote a thumbnailer entry; Reoling installs none" >&2; exit 1; }
}

# The AppImage first, before the .deb is installed, as on a machine that has
# only the AppImage: it copies its icon into the user's icon theme, writes
# its file's thumbnail-cache entries and a per-user desktop entry that starts
# the file (the file's custom icon needs gvfs, which the container lacks).
step "Smoke test: the AppImage"
appimage=$(realpath "$out"/Reoling-*-x86_64.AppImage)
chmod +x "$appimage"
test "$(xvfb-run -a "$appimage" --version)" = "reoling $version"
run_for_20s "$appimage"
for size in 16 22 24 32 48 64 128 256 512; do
    png="$XDG_DATA_HOME/icons/hicolor/${size}x${size}/apps/de.dersobi.reoling.png"
    test -s "$png" || { echo "the AppImage did not copy its $size px icon into the user icon theme ($png)" >&2; exit 1; }
    cmp "$png" "data/icons/hicolor/${size}x${size}/apps/de.dersobi.reoling.png"
done
key=$(printf 'file://%s' "$appimage" | md5sum | cut -c1-32)
for size in large x-large; do
    png="$XDG_CACHE_HOME/thumbnails/$size/$key.png"
    test -s "$png" || { echo "the AppImage wrote no $size thumbnail of itself ($png)" >&2; exit 1; }
    file "$png"
done
entry="$XDG_DATA_HOME/applications/de.dersobi.reoling.desktop"
test -s "$entry" || { echo "the AppImage wrote no desktop entry ($entry)" >&2; exit 1; }
cat "$entry"
for line in "Exec=\"$appimage\"" "TryExec=$appimage" "Icon=de.dersobi.reoling" "X-Reoling-Generated=true"; do
    grep -qFx "$line" "$entry" || { echo "the AppImage's desktop entry lacks $line" >&2; exit 1; }
done
if grep -q '^MimeType=' "$entry"; then
    echo "the AppImage's desktop entry claims MIME types" >&2
    exit 1
fi
desktop-file-validate "$entry"

# Then the .deb, installed; it does none of the above, and its start
# removes the AppImage's per-user desktop entry, which would hide its own.
step "Smoke test: the .deb"
apt-get install -y -qq "$out"/reoling_*.deb
test "$(xvfb-run -a reoling --version)" = "reoling $version"
dpkg -L reoling
run_for_20s reoling
[ ! -e "$entry" ] || { echo "the .deb left the AppImage's per-user desktop entry in place ($entry)" >&2; exit 1; }

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
