#!/usr/bin/env bash
# Builds Reoling-<version>-x86_64.AppImage from a release binary, with
# linuxdeploy, its gtk and gstreamer plugins, and appimagetool.
#
#   scripts/build-appimage.sh <release binary> <output directory>
#
# The AppImage bundles GTK 4 with its GLib schemas, GDK pixbuf loaders and
# modules, the GStreamer core with the plugins the app uses (gtk4paintablesink,
# the libav decoders, the va hardware decoders where the build machine has the
# plugin, pulse and ALSA sinks), the desktop entry and the icons. The build
# machine's libraries are what gets bundled, so the release builds run on
# Ubuntu 24.04: the result then runs on any distribution with glibc >= 2.39.
#
# Environment:
#   APPIMAGE_TOOLS      directory holding linuxdeploy-x86_64.AppImage,
#                       appimagetool-x86_64.AppImage, linuxdeploy-plugin-gtk.sh
#                       and linuxdeploy-plugin-gstreamer.sh (see
#                       scripts/fetch-appimage-tools.sh); default: found in PATH
#   GST_GTK4_PLUGIN     the libgstgtk4.so to bundle; default: target/gst-gtk4/
#                       libgstgtk4.so (scripts/build-gst-gtk4-plugin.sh), else
#                       the system's
#   APPIMAGE_EXTRACT_AND_RUN=1  lets the tools run where FUSE is unavailable
#                       (containers); the tools read it themselves
#   DEBUG=1             trace
set -euo pipefail
export LC_ALL=C.UTF-8
[ -n "${DEBUG:-}" ] && set -x

if [ $# -ne 2 ]; then
    echo "usage: $0 <release binary> <output directory>" >&2
    exit 2
fi
binary=$(realpath "$1")
out=$2
repo=$(cd "$(dirname "$0")/.." && pwd)

[ -x "$binary" ] || { echo "$binary is not an executable" >&2; exit 1; }
version=$("$binary" --version | awk '$1 == "reoling" { print $2 }')
[ -n "$version" ] || { echo "$binary --version did not answer 'reoling <version>'" >&2; exit 1; }

# The tools, pinned by scripts/fetch-appimage-tools.sh.
if [ -n "${APPIMAGE_TOOLS:-}" ]; then
    export PATH="$(realpath "$APPIMAGE_TOOLS"):$PATH"
fi
for tool in linuxdeploy-x86_64.AppImage appimagetool-x86_64.AppImage linuxdeploy-plugin-gtk.sh linuxdeploy-plugin-gstreamer.sh patchelf file; do
    command -v "$tool" >/dev/null || { echo "$tool not found (set APPIMAGE_TOOLS or install it)" >&2; exit 1; }
done
linuxdeploy=$(command -v linuxdeploy-x86_64.AppImage)
appimagetool=$(command -v appimagetool-x86_64.AppImage)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir="$work/AppDir"
mkdir -p "$out"
out=$(realpath "$out")

# --- GStreamer plugins -----------------------------------------------------
# linuxdeploy-plugin-gstreamer copies every plugin it finds in
# GSTREAMER_PLUGINS_DIR, so the ones the app needs are staged on their own.
plugins_dir=$(pkg-config --variable=pluginsdir gstreamer-1.0 2>/dev/null || true)
[ -d "$plugins_dir" ] || plugins_dir=/usr/lib/$(uname -m)-linux-gnu/gstreamer-1.0
[ -d "$plugins_dir" ] || { echo "no GStreamer plugin directory found" >&2; exit 1; }

stage="$work/gst-plugins"
mkdir -p "$stage"
# appsrc, decodebin, queue/identity/filesink/fakesink, typefind, the
# converters and volume (core, -base); aacparse, qtmux, matroskamux,
# autoaudiosink (-good); h264parse/h265parse (-bad); avdec_* (libav).
required="coreelements app playback typefindfunctions audioconvert audioresample volume videoconvertscale audioparsers isomp4 matroska autodetect videoparsersbad libav"
# faad: AAC fallback; pulse and ALSA sinks for autoaudiosink; va: VA-API decoders.
optional="faad pulseaudio alsa va"
for name in $required; do
    [ -f "$plugins_dir/libgst$name.so" ] || { echo "GStreamer plugin libgst$name.so is missing from $plugins_dir" >&2; exit 1; }
    cp "$plugins_dir/libgst$name.so" "$stage/"
done
for name in $optional; do
    if [ -f "$plugins_dir/libgst$name.so" ]; then
        cp "$plugins_dir/libgst$name.so" "$stage/"
    else
        echo "note: optional GStreamer plugin libgst$name.so is not installed, not bundled"
    fi
done
gtk4_plugin=${GST_GTK4_PLUGIN:-}
if [ -z "$gtk4_plugin" ]; then
    if [ -f "$repo/target/gst-gtk4/libgstgtk4.so" ]; then
        gtk4_plugin="$repo/target/gst-gtk4/libgstgtk4.so"
    elif [ -f "$plugins_dir/libgstgtk4.so" ]; then
        gtk4_plugin="$plugins_dir/libgstgtk4.so"
    else
        echo "no gtk4paintablesink plugin: run scripts/build-gst-gtk4-plugin.sh target/gst-gtk4 first" >&2
        exit 1
    fi
fi
echo "GTK 4 paintable sink: $gtk4_plugin"
cp "$gtk4_plugin" "$stage/libgstgtk4.so"

# --- AppDir ----------------------------------------------------------------
icons=()
for png in "$repo"/data/icons/hicolor/*/apps/de.dersobi.reoling.png; do
    icons+=(--icon-file "$png")
done
export LINUXDEPLOY="$linuxdeploy" DEPLOY_GTK_VERSION=4 GSTREAMER_PLUGINS_DIR="$stage"
"$linuxdeploy" --appdir "$appdir" \
    --executable "$binary" \
    --desktop-file "$repo/data/de.dersobi.reoling.desktop" \
    "${icons[@]}" \
    --plugin gtk --plugin gstreamer

# The gtk plugin bundles the GObject introspection typelibs; nothing here
# uses them.
rm -rf "$appdir/usr/lib/girepository-1.0"

# The symbolic icons the UI asks for, as a fallback for a desktop whose
# icon theme lacks some of them: GTK tries the theme first and these last
# (the app adds REOLING_ICON_PATH to its icon search path).
fallback_icons="$appdir/usr/share/reoling/icons"
mkdir -p "$fallback_icons"
missing=()
for name in $(grep -rhoE '"[a-z0-9-]+-symbolic"' "$repo/src/ui" | tr -d '"' | sort -u); do
    svg=$(find /usr/share/icons/Adwaita -name "$name.svg" -print -quit 2>/dev/null || true)
    if [ -n "$svg" ]; then
        cp "$svg" "$fallback_icons/"
    else
        missing+=("$name")
    fi
done
[ ${#missing[@]} -eq 0 ] || echo "note: not in this machine's Adwaita icon theme, not bundled: ${missing[*]}"

# The gtk plugin's hook forces GDK_BACKEND=x11 (a GTK 3 workaround); GTK 4
# runs natively on Wayland, so the choice is left to GTK and to the user.
sed -i '/^export GDK_BACKEND=x11/d' "$appdir/apprun-hooks/linuxdeploy-plugin-gtk.sh"

cat > "$appdir/apprun-hooks/zz-reoling.sh" <<'HOOK'
#! /usr/bin/env bash
# Reoling's own hook, sourced after the linuxdeploy ones.

# A plugin registry of its own: the bundled GStreamer is not the system's,
# and sharing ~/.cache/gstreamer-1.0 with it would make both rescan at
# every start.
export GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/reoling/gstreamer-registry.bin"
mkdir -p "$(dirname "$GST_REGISTRY_1_0")" 2>/dev/null || true

# The bundled libva looks for the host's VA-API drivers; name the usual
# places, since its built-in path is the build machine's.
if [ -z "${LIBVA_DRIVERS_PATH:-}" ]; then
    export LIBVA_DRIVERS_PATH="/usr/lib/x86_64-linux-gnu/dri:/usr/lib64/dri:/usr/lib/dri"
fi

# Fallback symbolic icons for the UI, tried after the desktop's own theme.
export REOLING_ICON_PATH="$APPDIR/usr/share/reoling/icons"
HOOK

# linuxdeploy's AppRun names the hooks it knows; this one sources every hook
# present (the two plugins' and ours, in name order) and runs the app.
cat > "$appdir/AppRun" <<'APPRUN'
#! /usr/bin/env bash
# Reoling's AppRun: environment from apprun-hooks/*.sh, then the app.
set -e
this_dir="$(readlink -f "$(dirname "$0")")"
for hook in "$this_dir"/apprun-hooks/*.sh; do
    source "$hook"
done
exec "$this_dir"/usr/bin/reoling "$@"
APPRUN
chmod +x "$appdir/AppRun"
rm -f "$appdir/AppRun.wrapped"

# --- Check the bundle before packing it ---------------------------------------
# Every element the app creates must register from the bundled plugins
# alone (a throw-away registry, the AppDir's libraries, its own scanner).
if command -v gst-inspect-1.0 >/dev/null; then
    scanner=$(find "$appdir/usr/lib" -type f -name gst-plugin-scanner | head -n1)
    check() {
        env -i PATH="$PATH" HOME="$work" \
            LD_LIBRARY_PATH="$appdir/usr/lib" \
            GST_PLUGIN_SYSTEM_PATH_1_0="$appdir/usr/lib/gstreamer-1.0" GST_PLUGIN_PATH_1_0="" \
            GST_PLUGIN_SCANNER_1_0="$scanner" GST_REGISTRY_1_0="$work/registry.bin" \
            gst-inspect-1.0 "$@"
    }
    failed=()
    for element in appsrc decodebin queue identity filesink fakesink videoconvert audioconvert audioresample volume \
                   h264parse h265parse aacparse qtmux matroskamux autoaudiosink autoaudiosrc \
                   avdec_h264 avdec_h265 avdec_aac gtk4paintablesink; do
        check "$element" >/dev/null 2>&1 || failed+=("$element")
    done
    if [ ${#failed[@]} -ne 0 ]; then
        echo "elements missing from the bundle: ${failed[*]}" >&2
        check gtk4paintablesink || true
        exit 1
    fi
    echo "bundled plugins: $(check 2>/dev/null | tail -n1)"
    if check vah264dec >/dev/null 2>&1; then
        echo "va decoders: present on this machine"
    else
        echo "va decoders: not registered here (they need a VA-API device; the va plugin is $( [ -f "$appdir/usr/lib/gstreamer-1.0/libgstva.so" ] && echo bundled || echo 'not bundled' ))"
    fi
else
    echo "gst-inspect-1.0 not found: the bundled plugins were not test-loaded" >&2
fi

# --- AppImage ------------------------------------------------------------------
appimage="$out/Reoling-$version-x86_64.AppImage"
rm -f "$appimage"
ARCH=x86_64 "$appimagetool" --comp zstd "$appdir" "$appimage"
chmod +x "$appimage"
echo "Wrote $appimage ($(du -h "$appimage" | cut -f1))"
