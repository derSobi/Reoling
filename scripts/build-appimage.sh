#!/usr/bin/env bash
# Builds Reoling-<version>-x86_64.AppImage from a release binary, with
# linuxdeploy, its gtk and gstreamer plugins, and appimagetool.
#
#   scripts/build-appimage.sh <release binary> <output directory>
#
# The AppImage bundles GTK 4 with its GLib schemas, GDK pixbuf loaders and
# modules, the GStreamer core with the plugins the app uses (gtk4paintablesink,
# the libav software decoders, the nvcodec (NVIDIA NVDEC) and va (VA-API)
# hardware decoders where the build machine has the plugins, pulse and ALSA
# sinks), the desktop entry and the icons. The hardware decoder plugins load
# the host's drivers at run time (libcuda/libnvcuvid, the host's libva and
# its VA driver); without one they register nothing and decodebin falls back
# to software. The build machine's libraries are what gets bundled (GLib
# and libva excepted, see below: those are the host's), so the release
# builds run on Ubuntu 24.04: the result then runs on any distribution
# with glibc >= 2.39 and GLib >= 2.80.
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
# faad: AAC fallback; pulse and ALSA sinks for autoaudiosink; the hardware
# decoders: nvcodec (NVIDIA NVDEC, through the driver's libcuda/libnvcuvid)
# and va (VA-API). Both live in -bad on Ubuntu 24.04.
optional="faad pulseaudio alsa va nvcodec"
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

# libva stays the host's: a VA driver only loads into the libva it was built
# for (each release looks for its own __vaDriverInit_<version> entry), so the
# bundled libva would reject the drivers of any newer distribution. The va
# plugin, libgstva and the FFmpeg libraries behind the libav decoders link
# libva by soname and find the host's copy through the normal search path.
# FFmpeg needs libva even to decode in software, so the build machine's copy
# is kept aside in usr/lib/libva-fallback for a host that has none:
# zz-reoling.sh puts that directory on LD_LIBRARY_PATH only then.
va_fallback="$appdir/usr/lib/libva-fallback"
mkdir -p "$va_fallback"
for lib in libva.so.2 libva-drm.so.2 libva-x11.so.2 libva-wayland.so.2; do
    if [ -f "$appdir/usr/lib/$lib" ]; then
        mv "$appdir/usr/lib/$lib" "$va_fallback/"
    fi
done
[ -f "$va_fallback/libva.so.2" ] || { echo "linuxdeploy bundled no libva.so.2 to keep as the fallback" >&2; exit 1; }

# GLib stays the host's too. The gtk plugin bundles libglib, libgobject,
# libgio and libgmodule, but GIO loads its modules — the dconf GSettings
# backend GTK reads the desktop's theme name (and so the accent colour)
# from, gvfs, TLS — from the one directory compiled into it, the build
# machine's /usr/lib/x86_64-linux-gnu/gio/modules, which other
# distributions do not have; and a host module built for a newer GLib need
# not load into the bundled one (the host's gvfs module failed that way).
# With the host's GLib its modules just work. The distributions with
# glibc >= 2.39 (Ubuntu 24.04, Fedora 40, Debian 13 and newer) all have
# GLib >= 2.80, the version the bundled GTK 4.14 and GStreamer 1.24 were
# built with, and GLib keeps its ABI across releases. One thing hung on the
# bundled libgmodule: the gtk plugin writes the GDK pixbuf loaders into
# loaders.cache by file name alone, which dlopen found through that
# library's RUNPATH; zz-reoling.sh puts the loaders directory on the
# library search path instead (GTK 4.14 draws the symbolic icons through
# the SVG loader).
rm -f "$appdir"/usr/lib/libglib-2.0.so* "$appdir"/usr/lib/libgobject-2.0.so* \
      "$appdir"/usr/lib/libgio-2.0.so* "$appdir"/usr/lib/libgmodule-2.0.so* \
      "$appdir"/usr/lib/libgthread-2.0.so*

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
# It also forces GTK_THEME=Adwaita:<variant>, which overrides the theme the
# desktop provides and with it the accent colour the app draws its
# connection dot and timeline in; the desktop's theme applies instead (GTK
# reads its name from GSettings, that is through the host GIO's dconf
# backend, see above), and GTK_THEME stays the user's.
gtk_hook="$appdir/apprun-hooks/linuxdeploy-plugin-gtk.sh"
sed -i -e '/^export GDK_BACKEND=x11/d' \
       -e '/^COLOR_SCHEME=/,/^APPIMAGE_GTK_THEME=/d' \
       -e '/^export GTK_THEME=/d' "$gtk_hook"
if grep -q 'GTK_THEME\|GDK_BACKEND' "$gtk_hook"; then
    echo "the gtk hook still sets GTK_THEME or GDK_BACKEND; its layout changed:" >&2
    cat "$gtk_hook" >&2
    exit 1
fi

cat > "$appdir/apprun-hooks/zz-reoling.sh" <<'HOOK'
#! /usr/bin/env bash
# Reoling's own hook, sourced after the linuxdeploy ones.

# A plugin registry of its own: the bundled GStreamer is not the system's,
# and sharing ~/.cache/gstreamer-1.0 with it would make both rescan at
# every start.
export GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/reoling/gstreamer-registry.bin"
mkdir -p "$(dirname "$GST_REGISTRY_1_0")" 2>/dev/null || true

# Fallback symbolic icons for the UI, tried after the desktop's own theme.
export REOLING_ICON_PATH="$APPDIR/usr/share/reoling/icons"

# gdk-pixbuf's loaders.cache (GDK_PIXBUF_MODULE_FILE, from the gtk hook)
# names the bundled loaders by file name alone; the library search path
# is what finds them.
export LD_LIBRARY_PATH="$(dirname "$GDK_PIXBUF_MODULE_FILE")/loaders${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# libva is the host's, which its VA drivers were built for. Only a host
# without any libva gets the copy the build machine had: the FFmpeg
# libraries behind the software decoders need libva to load at all.
ldconfig_bin=$(command -v ldconfig || echo /sbin/ldconfig)
if ! "$ldconfig_bin" -p 2>/dev/null | grep -q 'libva\.so\.2 (libc6,x86-64)'; then
    echo "reoling: this system has no libva; using the AppImage's own copy (no VA-API decoding)" >&2
    export LD_LIBRARY_PATH="$APPDIR/usr/lib/libva-fallback${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
HOOK

# linuxdeploy's AppRun names the hooks it knows; this one sources every hook
# present (the two plugins' and ours, in name order) and runs the app. APPDIR
# is set first: the AppImage runtime provides it, but an extracted AppRun
# (--appimage-extract) does not, and the gstreamer hook, sourced before the
# gtk one (which sets it), would otherwise point the bundled GStreamer at
# the host's plugins.
cat > "$appdir/AppRun" <<'APPRUN'
#! /usr/bin/env bash
# Reoling's AppRun: environment from apprun-hooks/*.sh, then the app.
set -e
this_dir="$(readlink -f "$(dirname "$0")")"
export APPDIR="${APPDIR:-$this_dir}"
for hook in "$this_dir"/apprun-hooks/*.sh; do
    source "$hook"
done
exec "$this_dir"/usr/bin/reoling "$@"
APPRUN
chmod +x "$appdir/AppRun"
rm -f "$appdir/AppRun.wrapped"

# The icon the file manager shows for the .AppImage file: at start the app
# reads .DirIcon and the 512 px hicolor icon out of the mounted image to
# write its own file's thumbnail-cache entries (src/ui/icon.rs,
# install_appimage_integration), and AppImage tools read .DirIcon (some
# the <Icon>.png next to the desktop entry) as well, so both are real PNG
# files, not the symlinks linuxdeploy leaves (a symlink extracted on its
# own points nowhere). 256 px, as AppImage recommends, or the largest size
# the repository has.
icon_name=$(sed -n 's/^Icon=//p' "$repo/data/de.dersobi.reoling.desktop")
[ -n "$icon_name" ] || { echo "the desktop entry has no Icon= key" >&2; exit 1; }
icon_src=""
for size in 256x256 512x512 128x128 64x64 48x48; do
    if [ -f "$repo/data/icons/hicolor/$size/apps/$icon_name.png" ]; then
        icon_src="$repo/data/icons/hicolor/$size/apps/$icon_name.png"
        break
    fi
done
[ -n "$icon_src" ] || { echo "no $icon_name.png icon found under data/icons" >&2; exit 1; }
rm -f "$appdir/.DirIcon" "$appdir/$icon_name.png"
cp "$icon_src" "$appdir/.DirIcon"
cp "$icon_src" "$appdir/$icon_name.png"
file -b "$appdir/.DirIcon" | grep -q '^PNG image data' || { echo "$icon_src is not a PNG" >&2; exit 1; }
echo "AppImage icon: $(file -b "$appdir/.DirIcon" | cut -d, -f1,2) from $icon_src"

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
    # The hardware decoders register only where the build machine has the
    # driver; the plugins travel either way.
    for hw in "nvh264dec nvcodec NVIDIA" "vah264dec va VA-API"; do
        read -r element plugin label <<< "$hw"
        if check "$element" >/dev/null 2>&1; then
            echo "$label decoders: present on this machine"
        else
            echo "$label decoders: not registered here (they need a $label driver; the $plugin plugin is $( [ -f "$appdir/usr/lib/gstreamer-1.0/libgst$plugin.so" ] && echo bundled || echo 'not bundled' ))"
        fi
    done
    if [ -f "$appdir/usr/lib/gstreamer-1.0/libgstnvcodec.so" ] && ! ls "$appdir"/usr/lib/libgstcuda-1.0.so.* >/dev/null 2>&1; then
        echo "the nvcodec plugin is bundled without libgstcuda-1.0" >&2
        exit 1
    fi
    if ls "$appdir"/usr/lib/libva*.so* >/dev/null 2>&1; then
        echo "libva is bundled beside the other libraries; it must come from the host (the fallback copy lives in usr/lib/libva-fallback)" >&2
        exit 1
    fi
    if ls "$appdir"/usr/lib/libglib-2.0.so* "$appdir"/usr/lib/libgio-2.0.so* "$appdir"/usr/lib/libgobject-2.0.so* >/dev/null 2>&1; then
        echo "GLib is bundled; it must come from the host, whose GIO modules it loads" >&2
        exit 1
    fi
    # The loaders the hook puts on the library search path, next to the cache
    # the gtk hook names.
    pixbuf_cache=$(sed -n 's|^export GDK_PIXBUF_MODULE_FILE="\$APPDIR/*\(.*\)"$|\1|p' "$gtk_hook")
    if [ -z "$pixbuf_cache" ] || [ ! -f "$appdir/$pixbuf_cache" ] || ! ls "$appdir/$(dirname "$pixbuf_cache")"/loaders/*.so >/dev/null 2>&1; then
        echo "the gtk hook's GDK_PIXBUF_MODULE_FILE ($pixbuf_cache) or its loaders directory is missing" >&2
        exit 1
    fi
    echo "pixbuf loaders: $(ls "$appdir/$(dirname "$pixbuf_cache")"/loaders/*.so | wc -l) in $(dirname "$pixbuf_cache")/loaders"
else
    echo "gst-inspect-1.0 not found: the bundled plugins were not test-loaded" >&2
fi

# --- AppImage ------------------------------------------------------------------
appimage="$out/Reoling-$version-x86_64.AppImage"
rm -f "$appimage"
ARCH=x86_64 "$appimagetool" --comp zstd "$appdir" "$appimage"
chmod +x "$appimage"
echo "Wrote $appimage ($(du -h "$appimage" | cut -f1))"
