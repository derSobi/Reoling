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
# with glibc >= 2.39 and GLib >= 2.80. Where the host has a GTK 4 and a
# GStreamer core no older than the bundled ones, the AppImage runs on
# those instead and keeps its own for other hosts (see "GTK and GStreamer
# core" below and scripts/appimage-hook.sh, the AppRun hook that chooses).
# The build reads the build machine's package database (dpkg) for that.
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
for tool in linuxdeploy-x86_64.AppImage appimagetool-x86_64.AppImage linuxdeploy-plugin-gtk.sh \
            linuxdeploy-plugin-gstreamer.sh patchelf file readelf dpkg-query; do
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
# FFmpeg needs libva, libva-drm and libva-x11 even to decode in software,
# and a host can have libva without libva-x11 (Ubuntu 26.04 with GTK but no
# VA-API stack), so the build machine's copies are kept aside, each in a
# directory of its own, usr/lib/libva-fallback/<soname>/: zz-reoling.sh
# puts on LD_LIBRARY_PATH the directory of each one the host lacks, or all
# of them for a host without libva itself. Their RUNPATH still covers
# usr/lib, where they were.
va_fallback="$appdir/usr/lib/libva-fallback"
mkdir -p "$va_fallback"
for lib in libva.so.2 libva-drm.so.2 libva-x11.so.2 libva-wayland.so.2; do
    if [ -f "$appdir/usr/lib/$lib" ]; then
        mkdir -p "$va_fallback/$lib"
        mv "$appdir/usr/lib/$lib" "$va_fallback/$lib/"
        patchelf --set-rpath '$ORIGIN:$ORIGIN/../..' "$va_fallback/$lib/$lib"
    fi
done
[ -f "$va_fallback/libva.so.2/libva.so.2" ] || { echo "linuxdeploy bundled no libva.so.2 to keep aside" >&2; exit 1; }
echo "libva fallback: $(cd "$va_fallback" && echo *)"

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

# The SVG pixbuf loader (libpixbufloader-svg.so, or libpixbufloader_svg.so
# on newer distributions) links librsvg, which linuxdeploy bundles in
# usr/lib, but gets the RUNPATH $ORIGIN, its own directory, alone: the
# loader then took the host's librsvg, and on a host without one failed to
# load ("librsvg-2.so.2: cannot open shared object file"), leaving GTK
# without the symbolic icons. Its RUNPATH covers usr/lib as well, and the
# loader must resolve the bundled librsvg through it, ahead of the host's
# (the build machine has one, so the smoke tests would not notice).
svg_loaders=("$appdir"/usr/lib/gdk-pixbuf-2.0/*/loaders/libpixbufloader[-_]svg.so)
[ -f "${svg_loaders[0]}" ] || { echo "linuxdeploy bundled no SVG pixbuf loader" >&2; exit 1; }
[ -f "$appdir/usr/lib/librsvg-2.so.2" ] || { echo "linuxdeploy bundled no librsvg-2.so.2 for the SVG loader" >&2; exit 1; }
for loader in "${svg_loaders[@]}"; do
    patchelf --set-rpath '$ORIGIN:$ORIGIN/../../..' "$loader"
    echo "SVG pixbuf loader: ${loader#"$appdir"/}, RUNPATH $(patchelf --print-rpath "$loader")"
    rsvg=$(env -u LD_LIBRARY_PATH ldd "$loader" | awk '$1 == "librsvg-2.so.2" { print $3 }' || true)
    if [ -z "$rsvg" ] || [ "$(realpath -e "$rsvg" || true)" != "$(realpath -e "$appdir/usr/lib/librsvg-2.so.2")" ]; then
        echo "${loader#"$appdir"/} takes librsvg-2.so.2 from ${rsvg:-nowhere}, not from usr/lib" >&2
        exit 1
    fi
done

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

# --- GTK and GStreamer core: the system's or the bundled ones ------------------
# The AppImage runs on the system's GTK 4 where it is no older than the
# bundled one, and on its own copy otherwise; zz-reoling.sh, the hook below
# (scripts/appimage-hook.sh), chooses at every start. A desktop theme is
# written for the GTK it comes with: Ubuntu 26.04's Yaru uses CSS that the
# bundled GTK 4.14 drops, which loses, among others, the Playback
# calendar's marks for the days with recordings. The system's GTK may link
# the system's GStreamer core and -base libraries and bind every symbol of
# them when it loads (Ubuntu 26.04's does), so the GStreamer core changes
# together with GTK. The bundled copies of both move to usr/lib/gtk-fallback,
# where only the bundled mode looks (its LD_LIBRARY_PATH); in the other mode
# the system's libraries of the same names take their place.
#
# Which libraries move: the bundled ones the app links, each with every
# other bundled library built from the same source package (released
# together: cairo with cairo-gobject, the GStreamer -base libraries among
# themselves), as the build machine's package database tells. The app's own
# links are what makes the move necessary: one left in usr/lib would be
# found there through the app's RUNPATH, ahead of the system's copy the
# system's GTK was built for. Everything else stays in usr/lib for both
# modes: the GStreamer plugins, which run on either core, the -bad libraries
# they link (codecs, codecparsers, cuda, va), which have no stable ABI and
# must be the ones the plugins were built with, FFmpeg, and the generic
# libraries (moved as well, they would land on the library search path of
# the system's GLib in the bundled mode, older than it may need). GTK's own
# dependencies cannot serve as the list: they take in all the generic
# libraries, and the build machine's GTK 4.14 links no GStreamer. The moved
# libraries get the RUNPATH $ORIGIN:$ORIGIN/.., so that in the bundled mode
# they find each other and usr/lib as they did there.
fallback="$appdir/usr/lib/gtk-fallback"
mkdir -p "$fallback"
multiarch="$(uname -m)-linux-gnu"
# The source package of the installed package that has the library $1 in
# this machine's library directory.
source_package() {
    local pkg
    pkg=$(dpkg -S "*/$multiarch/$1" 2>/dev/null | awk -F': ' 'index($2, "/") == 1 { print $1; exit }' || true)
    [ -n "$pkg" ] || return 0
    dpkg-query -W -f='${source:Package}' "$pkg"
}
needed() { readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'; }
sources=()
for so in $(needed "$appdir/usr/bin/reoling"); do
    [ -e "$appdir/usr/lib/$so" ] || continue # the host's: glibc, GLib
    src=$(source_package "$so")
    if [ -z "$src" ]; then
        echo "no installed package has $so, which the app links: cannot tell what moves with it" >&2
        exit 1
    fi
    echo "the app links $so (source package $src)"
    sources+=("$src")
done
case " ${sources[*]} " in
    *" gst-plugins-bad1.0 "*)
        echo "the app links a GStreamer -bad library: the bundled plugins need the bundled -bad libraries," >&2
        echo "which have no stable ABI, so they cannot move to usr/lib/gtk-fallback with the GStreamer core" >&2
        exit 1
        ;;
esac
mapfile -t stack_packages < <(dpkg-query -W -f='${db:Status-Abbrev} ${binary:Package} ${source:Package}\n' \
    | awk -v list=" ${sources[*]} " '$1 == "ii" && index(list, " " $3 " ") { print $2 }')
[ ${#sources[@]} -gt 0 ] || { echo "the app links none of the bundled libraries" >&2; exit 1; }
[ ${#stack_packages[@]} -gt 0 ] || { echo "no installed package comes from ${sources[*]}" >&2; exit 1; }
declare -A moved_bases=()
while read -r path; do
    name=${path##*/}
    if [ -e "$appdir/usr/lib/$name" ]; then
        moved_bases[${name%%.so*}]=1
    fi
done < <(dpkg -L "${stack_packages[@]}" | grep -E "/$multiarch/lib[^/]*\.so\.[0-9][^/]*$" | sort -u)
[ ${#moved_bases[@]} -gt 0 ] || { echo "none of the bundled libraries comes from ${sources[*]}" >&2; exit 1; }
for base in "${!moved_bases[@]}"; do
    for f in "$appdir/usr/lib/$base".so*; do
        mv "$f" "$fallback/"
    done
done
for f in "$fallback"/*; do
    if [ -f "$f" ] && [ ! -L "$f" ]; then
        patchelf --set-rpath '$ORIGIN:$ORIGIN/..' "$f"
    fi
done
for so in libgtk-4.so.1 libgstreamer-1.0.so.0; do
    [ -f "$fallback/$so" ] || { echo "$so did not move to usr/lib/gtk-fallback" >&2; exit 1; }
done
for so in $(needed "$appdir/usr/bin/reoling"); do
    if [ -e "$appdir/usr/lib/$so" ]; then
        echo "the app links $so, which stayed in usr/lib: the app would take it from there with the system's GTK" >&2
        exit 1
    fi
done
mapfile -t moved < <(cd "$fallback" && ls)
echo "GTK fallback (usr/lib/gtk-fallback, from ${sources[*]}): ${moved[*]}"

# The bundled GSettings schemas are the bundled GTK's: in the GLib schema
# directory under usr/share, which the gtk hook puts first on XDG_DATA_DIRS,
# they would shadow the system's GTK's own (older keys in the same schema
# names), so they live under usr/share/reoling, named by the gtk hook's
# GSETTINGS_SCHEMA_DIR, which the system's GTK goes without. Reoling itself
# has no schema.
schemas="$appdir/usr/share/glib-2.0/schemas"
[ -f "$schemas/gschemas.compiled" ] || { echo "the gtk plugin bundled no compiled GSettings schemas" >&2; exit 1; }
mkdir -p "$appdir/usr/share/reoling/gtk-fallback"
mv "$schemas" "$appdir/usr/share/reoling/gtk-fallback/schemas"
rmdir "$appdir/usr/share/glib-2.0" 2>/dev/null || true
schema_line='export GSETTINGS_SCHEMA_DIR="$APPDIR/usr/share/reoling/gtk-fallback/schemas"'
sed -i "s|^export GSETTINGS_SCHEMA_DIR=.*|$schema_line|" "$gtk_hook"
if ! grep -qxF "$schema_line" "$gtk_hook"; then
    echo "the gtk hook sets no GSETTINGS_SCHEMA_DIR to point at the moved schemas; its layout changed:" >&2
    cat "$gtk_hook" >&2
    exit 1
fi

# Reoling's own hook, sourced after the linuxdeploy ones, with the versions
# of the bundled GTK and GStreamer (the build machine's, which linuxdeploy
# copied) it compares the system's with.
bundled_gtk=$(pkg-config --modversion gtk4)
bundled_gst=$(pkg-config --modversion gstreamer-1.0)
for v in "$bundled_gtk" "$bundled_gst"; do
    [[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "pkg-config gave GTK or GStreamer the version '$v'" >&2; exit 1; }
done
sed -e "s/@BUNDLED_GTK@/$bundled_gtk/" -e "s/@BUNDLED_GSTREAMER@/$bundled_gst/" "$repo/scripts/appimage-hook.sh" \
    > "$appdir/apprun-hooks/zz-reoling.sh"
if grep -q '@BUNDLED_' "$appdir/apprun-hooks/zz-reoling.sh"; then
    echo "scripts/appimage-hook.sh has a placeholder left unfilled" >&2
    exit 1
fi
echo "bundled GTK $bundled_gtk, GStreamer $bundled_gst"

# linuxdeploy's AppRun names the hooks it knows; this one sources every hook
# present (the two plugins' and ours, in name order) and runs the app. APPDIR
# is set first, to the directory AppRun is in: the AppImage runtime sets it
# to just that, but an extracted AppRun (--appimage-extract) gets no APPDIR,
# or one inherited from another AppImage (a runtime's APPDIR and APPIMAGE
# reach every process its app starts), and the gstreamer hook, sourced
# before the gtk one, would otherwise point the bundled GStreamer at the
# host's plugins, or at the other image's. An APPIMAGE that did not come
# with this directory names another AppImage's file and is dropped: the app
# gives the file named by APPIMAGE its icon.
cat > "$appdir/AppRun" <<'APPRUN'
#! /usr/bin/env bash
# Reoling's AppRun: environment from apprun-hooks/*.sh, then the app.
set -e
this_dir="$(readlink -f "$(dirname "$0")")"
if [ -z "${APPDIR:-}" ] || [ "$(readlink -f "$APPDIR" || true)" != "$this_dir" ]; then
    unset APPIMAGE
fi
export APPDIR="$this_dir"
for hook in "$this_dir"/apprun-hooks/*.sh; do
    source "$hook"
done
exec "$this_dir"/usr/bin/reoling "$@"
APPRUN
chmod +x "$appdir/AppRun"
rm -f "$appdir/AppRun.wrapped"

# The icon the file manager shows for the .AppImage file: at start the app
# copies the hicolor icons out of the mounted image into the user's icon
# theme (the file's custom icon, which GNOME Files shows, points at one of
# them) and reads .DirIcon and the 512 px hicolor icon to write its own
# file's thumbnail-cache entries (src/ui/icon.rs,
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
# Where no package installed Reoling's desktop entry, the app writes a
# per-user one for the AppImage file from the bundled one, which it reads
# from usr/share/applications (the copy at the root is a link to it).
[ -f "$appdir/usr/share/applications/de.dersobi.reoling.desktop" ] \
    || { echo "linuxdeploy put no desktop entry in usr/share/applications" >&2; exit 1; }

# --- Check the bundle before packing it ---------------------------------------
# A command run in the environment AppRun gives the app on this machine:
# the hooks sourced as AppRun sources them (with `set -e`), an empty
# environment otherwise, REOLING_GTK_STACK=$1 unless $1 is empty.
in_app_env() {
    local stack=$1
    shift
    env -i PATH="$PATH" HOME="$work/home" XDG_CACHE_HOME="$work/cache" ${stack:+REOLING_GTK_STACK=$stack} \
        bash -c 'set -e; export APPDIR="$1"; shift; for hook in "$APPDIR"/apprun-hooks/*.sh; do source "$hook"; done
                 exec "$@"' _ "$appdir" "$@"
}
# The dynamic loader's view of the app, as in zz-reoling.sh's probe: the
# app alone, and when $1 is "plugins" then every bundled plugin too, with
# the libraries the app loaded preloaded ahead of them by file, in their
# order (as the app has them loaded when GStreamer loads a plugin).
cat > "$work/trace.sh" <<'TRACE'
trace() {
    LD_TRACE_LOADED_OBJECTS=1 LD_BIND_NOW=1 LD_WARN=1 \
        LD_LIBRARY_PATH="$APPDIR/usr/lib/gstreamer-1.0${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$APPDIR/usr/bin/reoling"
}
app=$(trace || true)
printf '%s\n' "$app"
if [ "${1:-}" = plugins ]; then
    preload=$(printf '%s\n' "$app" \
        | awk 'NF == 4 && $2 == "=>" && substr($3, 1, 1) == "/" && index($3, ":") == 0 { printf "%s ", $3 }')
    for f in "$APPDIR"/usr/lib/gstreamer-1.0/*.so; do preload="$preload ${f##*/}"; done
    LD_PRELOAD="$preload" trace
fi
TRACE

# The GTK choice, on this machine, whose GTK and GStreamer are the very
# ones bundled: the hook must take them as the system's ("host", with the
# versions it reads from the system's file names equal to the bundled
# ones), and then nothing of usr/lib may load with the app and every moved
# library must come from the system, the plugins' included; with
# REOLING_GTK_STACK=bundled every moved library must come from
# usr/lib/gtk-fallback. In both, the loader must find every library and
# bind every symbol of the app and the plugins, as the hook's probe does on
# the user's machine.
for stack in host bundled; do
    force=""
    [ "$stack" = host ] || force=$stack
    in_app_env "$force" bash "$work/trace.sh" plugins > "$work/trace-$stack.txt" 2> "$work/hook-$stack.txt" || true
    in_app_env "$force" bash "$work/trace.sh" > "$work/trace-$stack-app.txt" 2>> "$work/hook-$stack.txt" || true
    mode_lines=$(grep '^reoling: GTK and GStreamer core: ' "$work/hook-$stack.txt" | sort -u || true)
    case $mode_lines in
        *$'\n'*|"") echo "the hook named no single GTK choice ($stack):" >&2; cat "$work/hook-$stack.txt" >&2; exit 1 ;;
    esac
    echo "$stack: $mode_lines"
    expected="reoling: GTK and GStreamer core: $stack ("
    [ "$stack" = bundled ] || expected="$expected""the system's GTK $bundled_gtk, GStreamer $bundled_gst)"
    if [ "${mode_lines#"$expected"}" = "$mode_lines" ]; then
        echo "on this machine the hook should have said '$expected...'" >&2
        exit 1
    fi
    if grep -E 'not found|undefined symbol|cannot be preloaded' "$work/trace-$stack.txt" "$work/hook-$stack.txt"; then
        echo "the app or a bundled plugin does not load with the $stack GTK stack (above)" >&2
        exit 1
    fi
    while read -r so arrow path _; do
        [ "$arrow" = "=>" ] && [ -e "$fallback/$so" ] || continue
        if [ "$stack" = host ]; then
            case $path in
                "$appdir"/*) echo "with the system's stack, $so came from $path" >&2; exit 1 ;;
            esac
        else
            case $path in
                "$fallback"/*) ;;
                *) echo "with the bundled stack, $so came from $path" >&2; exit 1 ;;
            esac
        fi
    done < "$work/trace-$stack.txt"
    if [ "$stack" = host ] && grep -F "$appdir/" "$work/trace-$stack-app.txt"; then
        echo "with the system's stack, the app itself loaded the libraries above from the AppImage" >&2
        exit 1
    fi
    for so in libgtk-4.so.1 libgstreamer-1.0.so.0; do
        if ! grep -q "^[[:space:]]*$so => " "$work/trace-$stack-app.txt"; then
            echo "$stack: the app loads no $so" >&2
            exit 1
        fi
    done
done

# Every element the app creates must register from the bundled plugins
# alone (a throw-away registry, the bundled scanner), on either stack.
if command -v gst-inspect-1.0 >/dev/null; then
    check() { in_app_env "$check_stack" gst-inspect-1.0 "$@"; }
    for check_stack in host bundled; do
        failed=()
        for element in appsrc decodebin queue identity filesink fakesink videoconvert audioconvert audioresample \
                       volume h264parse h265parse aacparse qtmux matroskamux autoaudiosink autoaudiosrc \
                       avdec_h264 avdec_h265 avdec_aac gtk4paintablesink; do
            check "$element" >/dev/null 2>&1 || failed+=("$element")
        done
        if [ ${#failed[@]} -ne 0 ]; then
            echo "elements missing from the bundle with the $check_stack GTK stack: ${failed[*]}" >&2
            check gtk4paintablesink || true
            exit 1
        fi
        echo "bundled plugins, $check_stack GTK stack: $(check 2>/dev/null | tail -n1)"
    done
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
    # Whether any of the patterns matches a file.
    any_file() { local p; for p in "$@"; do compgen -G "$p" >/dev/null && return 0; done; return 1; }
    if any_file "$appdir/usr/lib/libva*.so*" "$fallback/libva*.so*"; then
        echo "libva is bundled beside the other libraries; it must come from the host (the fallback copy lives in usr/lib/libva-fallback)" >&2
        exit 1
    fi
    if any_file "$appdir"/usr/lib/{,gtk-fallback/}{libglib-2.0,libgio-2.0,libgobject-2.0}.so*; then
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
