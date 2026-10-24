#!/usr/bin/env bash
# Builds GStreamer's GTK 4 paintable sink (gtk4paintablesink, the
# gst-plugin-gtk4 crate from gst-plugins-rs) for distributions whose
# GStreamer packages do not ship it: Ubuntu 24.04 has no gstreamer1.0-gtk4.
# The Debian package and the AppImage carry the resulting libgstgtk4.so.
#
#   scripts/build-gst-gtk4-plugin.sh <output directory>
#
# writes <output directory>/libgstgtk4.so. Needs the GTK 4 and GStreamer
# development packages (libgtk-4-dev, libgstreamer1.0-dev,
# libgstreamer-plugins-base1.0-dev) and a Rust toolchain.
#
# Environment:
#   GST_GTK4_VERSION   crate version (default below; pinned with its SHA-256)
#   GST_GTK4_SHA256    checksum of the .crate file (required for another version)
#   GST_GTK4_FEATURES  cargo features (default: GL on Wayland and X11, DMA-BUF
#                      import, which needs GTK >= 4.14 and GStreamer >= 1.24)
#   GST_GTK4_WORK      where the sources and the build go
#                      (default: ${XDG_CACHE_HOME:-~/.cache}/reoling-build/gst-gtk4)
set -euo pipefail
export LC_ALL=C.UTF-8

if [ $# -ne 1 ]; then
    echo "usage: $0 <output directory>" >&2
    exit 2
fi
out=$1

default_version=0.15.2
default_sha256=9d22a27972c25e706b428e78356458446314c70eb31aef07d4a32b843a6dea94
version=${GST_GTK4_VERSION:-$default_version}
if [ "$version" = "$default_version" ]; then
    sha256=${GST_GTK4_SHA256:-$default_sha256}
elif [ -n "${GST_GTK4_SHA256:-}" ]; then
    sha256=$GST_GTK4_SHA256
else
    echo "GST_GTK4_VERSION=$version is not the pinned version: set GST_GTK4_SHA256 for it" >&2
    exit 2
fi
features=${GST_GTK4_FEATURES:-wayland,x11glx,x11egl,dmabuf,gtk_v4_14}
work=${GST_GTK4_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/reoling-build/gst-gtk4}

crate="gst-plugin-gtk4-$version"
mkdir -p "$work" "$out"
archive="$work/$crate.crate"
if ! [ -f "$archive" ] || ! echo "$sha256  $archive" | sha256sum --check --status; then
    echo "Downloading $crate from crates.io"
    curl -fsSL --retry 3 -o "$archive.part" "https://static.crates.io/crates/gst-plugin-gtk4/$crate.crate"
    if ! echo "$sha256  $archive.part" | sha256sum --check --status; then
        echo "$crate.crate: checksum mismatch, expected $sha256, got $(sha256sum "$archive.part" | cut -d' ' -f1)" >&2
        rm -f "$archive.part"
        exit 1
    fi
    mv "$archive.part" "$archive"
fi

rm -rf "$work/$crate"
tar -xzf "$archive" -C "$work"

echo "Building $crate with features: $features"
cargo build --release --manifest-path "$work/$crate/Cargo.toml" --target-dir "$work/target" --features "$features"

so="$work/target/release/libgstgtk4.so"
cp "$so" "$out/libgstgtk4.so"
echo "Wrote $out/libgstgtk4.so ($(du -h "$out/libgstgtk4.so" | cut -f1))"

# The plugin must register on this machine's GStreamer: load it alone, into
# a throw-away registry, and ask for the element.
if command -v gst-inspect-1.0 >/dev/null; then
    registry=$(mktemp)
    trap 'rm -f "$registry"' EXIT
    details=$(GST_PLUGIN_SYSTEM_PATH_1_0="$(cd "$out" && pwd)" GST_PLUGIN_PATH_1_0="" GST_REGISTRY_1_0="$registry" \
        gst-inspect-1.0 gtk4paintablesink 2>&1 || true)
    if grep -q "Long-name" <<< "$details"; then
        echo "gtk4paintablesink registers"
    else
        echo "gtk4paintablesink does not register from $out/libgstgtk4.so:" >&2
        echo "$details" >&2
        exit 1
    fi
else
    echo "gst-inspect-1.0 not found: the plugin was not test-loaded" >&2
fi
