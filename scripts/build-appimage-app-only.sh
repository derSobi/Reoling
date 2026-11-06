#!/usr/bin/env bash
# Builds the system AppImage, Reoling-<version>-system-x86_64.AppImage (the
# name is scripts/appimage-common.sh's), from a release binary: the app,
# GStreamer's GTK 4 paintable sink (scripts/build-gst-gtk4-plugin.sh), the
# desktop entry and the icons, about 6 MB. GTK 4, the GStreamer core, its
# plugins and the decoders are the system's: GTK 4.14 and GStreamer 1.24 or
# newer, as the sink needs, with the base, good, bad and libav plugins. The
# AppImage's hook (scripts/appimage-app-only-hook.sh) asks the dynamic loader
# at every start whether the system has the libraries and, where it lacks
# one, names what is missing and the command that installs it instead of
# starting the app. The embed AppImage, which brings all of that along, is
# scripts/build-appimage.sh's.
#
#   scripts/build-appimage-app-only.sh <release binary> <output directory>
#
# No linuxdeploy: nothing of the build machine is copied. The AppDir has the
# embed AppImage's layout for what both carry (usr/bin/reoling, the sink in
# usr/lib/gstreamer-1.0, the desktop entry in usr/share/applications and a
# link to it at the root, the hicolor icons, .DirIcon and <Icon>.png, the
# fallback symbolic icons) and the same AppRun, so the app gives the file its
# icon and its menu entry as it does for the embed AppImage (src/ui/icon.rs).
#
# Environment:
#   APPIMAGE_TOOLS      directory holding appimagetool-x86_64.AppImage and
#                       runtime-x86_64 (see scripts/fetch-appimage-tools.sh);
#                       default: appimagetool found in PATH, with the runtime
#                       it downloads
#   GST_GTK4_PLUGIN     the libgstgtk4.so to carry, with its LICENSE-MPL-2.0
#                       and SOURCE beside it; default: target/gst-gtk4/
#                       libgstgtk4.so (scripts/build-gst-gtk4-plugin.sh)
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
. "$repo/scripts/appimage-common.sh"

[ -x "$binary" ] || { echo "$binary is not an executable" >&2; exit 1; }
version=$("$binary" --version | awk '$1 == "reoling" { print $2 }')
[ -n "$version" ] || { echo "$binary --version did not answer 'reoling <version>'" >&2; exit 1; }

# The tools: appimagetool pinned by scripts/fetch-appimage-tools.sh; patchelf
# and pkg-config for the checks below.
if [ -n "${APPIMAGE_TOOLS:-}" ]; then
    PATH="$(realpath "$APPIMAGE_TOOLS"):$PATH"
    export PATH
fi
for tool in appimagetool-x86_64.AppImage file strip patchelf pkg-config; do
    command -v "$tool" >/dev/null || { echo "$tool not found (set APPIMAGE_TOOLS or install it)" >&2; exit 1; }
done
appimagetool=$(command -v appimagetool-x86_64.AppImage)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir="$work/AppDir"
mkdir -p "$out"
out=$(realpath "$out")
bundled_name=$(appimage_file_name bundled "$version")

# --- The app and the sink ------------------------------------------------------
# The app, stripped as linuxdeploy strips the embed AppImage's (12.6 MB for
# 1.0.4 instead of 15.5), and without the RUNPATH linuxdeploy gives that one:
# nothing in this AppImage is a library for it.
install -D -m 755 "$binary" "$appdir/usr/bin/reoling"
strip "$appdir/usr/bin/reoling"
# GStreamer's GTK 4 sink, ours, never the build machine's, which comes with
# neither its licence nor the note of where its source is, both of which go
# beside it. It sits where the embed AppImage has its plugins; the hook puts
# that directory first on GST_PLUGIN_PATH_1_0, ahead of a distribution's own
# sink.
sink=${GST_GTK4_PLUGIN:-$repo/target/gst-gtk4/libgstgtk4.so}
sink_dir=$(dirname "$sink")
for f in "$sink" "$sink_dir/LICENSE-MPL-2.0" "$sink_dir/SOURCE"; do
    [ -f "$f" ] || { echo "no $f: run scripts/build-gst-gtk4-plugin.sh target/gst-gtk4 first" >&2; exit 1; }
done
echo "GTK 4 paintable sink: $sink"
install -D -m 644 "$sink" "$appdir/usr/lib/gstreamer-1.0/libgstgtk4.so"
install -D -m 644 "$sink_dir/LICENSE-MPL-2.0" "$appdir/usr/share/doc/gst-plugin-gtk4/LICENSE-MPL-2.0"
install -D -m 644 "$sink_dir/SOURCE" "$appdir/usr/share/doc/gst-plugin-gtk4/SOURCE"

# --- Desktop entry, icons, AppRun and the hook ---------------------------------
# Where linuxdeploy puts them in the embed AppImage, and the same files: the
# release workflow compares them.
install -D -m 644 "$repo/data/de.dersobi.reoling.desktop" "$appdir/usr/share/applications/de.dersobi.reoling.desktop"
ln -s usr/share/applications/de.dersobi.reoling.desktop "$appdir/de.dersobi.reoling.desktop"
for png in "$repo"/data/icons/hicolor/*/apps/de.dersobi.reoling.png; do
    install -D -m 644 "$png" "$appdir/usr/share/icons/hicolor/${png#"$repo"/data/icons/hicolor/}"
done
appimage_fallback_icons "$repo" "$appdir"
appimage_root_icon "$repo" "$appdir"
appimage_write_apprun "$appdir"
# The hook, the only one, with the embed AppImage's file name for its advice.
mkdir -p "$appdir/apprun-hooks"
sed "s/@BUNDLED_APPIMAGE@/$bundled_name/" "$repo/scripts/appimage-app-only-hook.sh" \
    > "$appdir/apprun-hooks/zz-reoling.sh"
if grep -q '@BUNDLED_' "$appdir/apprun-hooks/zz-reoling.sh"; then
    echo "scripts/appimage-app-only-hook.sh has a placeholder left unfilled" >&2
    exit 1
fi

# --- Check the AppDir before packing it ------------------------------------------
# What it holds: the app and the sink are its only ELF files and the sink its
# only library, so that nothing of a GTK stack in usr/lib can meet the
# system's (the sink's RUNPATH, if it has one, reaches usr/lib), and the icons
# and the desktop entry the app reads are where it reads them.
elves=$(find "$appdir" -type f -print0 | while IFS= read -r -d '' f; do
            case $(file -b "$f") in ELF*) echo "${f#"$appdir"/}" ;; esac
        done | sort)
if [ "$elves" != $'usr/bin/reoling\nusr/lib/gstreamer-1.0/libgstgtk4.so' ]; then
    echo "the AppDir's ELF files should be the app and the sink alone:" >&2
    echo "$elves" >&2
    exit 1
fi
libs=$(cd "$appdir" && find . -name '*.so*' ! -path ./usr/lib/gstreamer-1.0/libgstgtk4.so)
[ -z "$libs" ] || { echo "the AppDir holds libraries besides the sink: $libs" >&2; exit 1; }
for size in 16 22 24 32 48 64 128 256 512; do
    png="$appdir/usr/share/icons/hicolor/${size}x${size}/apps/de.dersobi.reoling.png"
    [ -f "$png" ] || { echo "no ${size}x$size icon in the AppDir" >&2; exit 1; }
done
if [ ! -L "$appdir/de.dersobi.reoling.desktop" ] || [ ! -f "$appdir/de.dersobi.reoling.desktop" ]; then
    echo "the desktop entry at the AppDir's root is no link to usr/share/applications" >&2
    exit 1
fi

# A command run in the environment AppRun gives the app on this machine: the
# hook sourced as AppRun sources it (with `set -e`), an empty environment
# otherwise. Nothing here runs the app but the hook's own --version.
in_app_env() {
    env -i PATH="$PATH" HOME="$work/home" XDG_CACHE_HOME="$work/cache" \
        bash -c 'set -e; export APPDIR="$1"; shift; for hook in "$APPDIR"/apprun-hooks/*.sh; do source "$hook"; done
                 exec "$@"' _ "$appdir" "$@"
}
# The build machine has GTK 4, GStreamer and every plugin the app uses: the
# hook must take them, say so in exactly one line, with the versions it reads
# from the libraries' file names equal to pkg-config's, and give the app the
# environment the app and its requirements check expect.
gtk=$(pkg-config --modversion gtk4)
gst=$(pkg-config --modversion gstreamer-1.0)
if ! in_app_env env > "$work/env.txt" 2> "$work/hook.txt"; then
    echo "the hook does not start the app on this machine:" >&2
    cat "$work/hook.txt" >&2
    exit 1
fi
said=$(grep '^reoling: ' "$work/hook.txt" || true)
expected="reoling: GTK and GStreamer core: host (the system's GTK $gtk, GStreamer $gst; app-only AppImage,"
expected+=" the system's plugins)"
if [ "$said" != "$expected" ]; then
    echo "on this machine the hook should have said exactly '$expected':" >&2
    cat "$work/hook.txt" >&2
    exit 1
fi
echo "$said"
for line in "REOLING_PLUGINS=system" "GST_PLUGIN_PATH_1_0=$appdir/usr/lib/gstreamer-1.0" \
            "GST_REGISTRY_1_0=$work/cache/reoling/gstreamer-registry-system-$gst.bin" \
            "XDG_DATA_DIRS=$appdir/usr/share:/usr/local/share:/usr/share" \
            "REOLING_ICON_PATH=$appdir/usr/share/reoling/icons"; do
    grep -qxF "$line" "$work/env.txt" || { echo "the hook's environment lacks $line" >&2; exit 1; }
done
if grep -E '^(GST_PLUGIN_SYSTEM_PATH(_1_0)?|reoling_[a-z_]*)=' "$work/env.txt"; then
    echo "the hook left the above in the app's environment" >&2
    exit 1
fi
# Every element the app creates registers from this machine's plugins in the
# hook's environment, and the sink is the AppImage's.
if command -v gst-inspect-1.0 >/dev/null; then
    failed=()
    for element in appsrc decodebin queue identity filesink fakesink videoconvert audioconvert audioresample \
                   volume h264parse h265parse aacparse qtmux matroskamux autoaudiosink autoaudiosrc \
                   avdec_h264 avdec_h265 avdec_aac gtk4paintablesink; do
        in_app_env gst-inspect-1.0 "$element" >/dev/null 2>&1 || failed+=("$element")
    done
    if [ ${#failed[@]} -ne 0 ]; then
        echo "elements missing on this machine in the hook's environment: ${failed[*]}" >&2
        exit 1
    fi
    file=$(in_app_env gst-inspect-1.0 gtk4paintablesink 2>/dev/null | awk '$1 == "Filename" { print $2 }')
    if [ "$file" != "$appdir/usr/lib/gstreamer-1.0/libgstgtk4.so" ]; then
        echo "gtk4paintablesink comes from ${file:-nowhere}, not from the AppImage" >&2
        exit 1
    fi
    echo "gtk4paintablesink: the AppImage's, $(in_app_env gst-inspect-1.0 gtk4paintablesink 2>/dev/null \
        | awk '$1 == "Version" { print $2 }')"
else
    echo "gst-inspect-1.0 not found: the elements were not test-loaded" >&2
fi

# The refusal, on copies of the AppDir, through AppRun: a library the system
# lacks, and a GTK older than the hook asks for. Each must end AppRun with
# status 127 before the app (asked for --version, should it start) runs.
# A library the system lacks: the hook names it, the install command for
# this distribution and the embed AppImage.
refusal() {
    local rc=0
    env -i PATH="$PATH" HOME="$work/home" XDG_CACHE_HOME="$work/cache" ${skip:+REOLING_SKIP_SYSTEM_CHECK=$skip} \
        bash "$1/AppRun" --version > "$work/refusal.txt" 2>&1 || rc=$?
    echo "$rc"
}
cp -a "$appdir" "$work/AppDir-missing"
patchelf --add-needed libreoling-selftest-missing.so.0 "$work/AppDir-missing/usr/bin/reoling"
skip=""
rc=$(refusal "$work/AppDir-missing")
os=$(. /etc/os-release 2>/dev/null; echo " ${ID:-} ${ID_LIKE:-} ")
problems=()
[ "$rc" = 127 ] || problems+=("status $rc, not 127")
grep -qxF 'reoling: cannot start (missing: libreoling-selftest-missing.so.0)' "$work/refusal.txt" \
    || problems+=("no 'cannot start (missing: ...)' line naming the library")
case $os in
    *" debian "*|*" ubuntu "*)
        grep -q '^reoling: sudo apt install libgtk-4-1 ' "$work/refusal.txt" || problems+=("no apt install line") ;;
esac
grep -qF "Or use $bundled_name" "$work/refusal.txt" || problems+=("no 'Or use $bundled_name'")
if [ ${#problems[@]} -ne 0 ]; then
    echo "a missing library should have stopped AppRun with the hook's advice: ${problems[*]}" >&2
    cat "$work/refusal.txt" >&2
    exit 1
fi
echo "refusal, a library missing: status 127, $(grep -c '^reoling: ' "$work/refusal.txt") lines of advice"
# A GTK too old: no install command (the distribution's own GTK is what is too
# old), the embed AppImage; and with REOLING_SKIP_SYSTEM_CHECK=1 the start all
# the same.
cp -a "$appdir" "$work/AppDir-old"
sed -i 's/^reoling_min_gtk=.*/reoling_min_gtk=99.0/' "$work/AppDir-old/apprun-hooks/zz-reoling.sh"
rc=$(refusal "$work/AppDir-old")
if [ "$rc" != 127 ] || grep -q '^reoling: sudo ' "$work/refusal.txt" \
    || ! grep -q '^reoling: Too old on this system: GTK [0-9.]* (Reoling needs 99.0)' "$work/refusal.txt" \
    || ! grep -qF "Or use $bundled_name" "$work/refusal.txt"; then
    echo "a GTK too old should have stopped AppRun with status 127, 'Too old on this system' and no install line:" >&2
    echo "status $rc" >&2
    cat "$work/refusal.txt" >&2
    exit 1
fi
echo "refusal, GTK too old: status 127, no install line"
skip=1
rc=$(refusal "$work/AppDir-old")
if [ "$rc" != 0 ] || ! grep -qxF "reoling $version" "$work/refusal.txt" \
    || ! grep -qF 'REOLING_SKIP_SYSTEM_CHECK=1: starting all the same' "$work/refusal.txt"; then
    echo "with REOLING_SKIP_SYSTEM_CHECK=1 AppRun should have started the app all the same (status $rc):" >&2
    cat "$work/refusal.txt" >&2
    exit 1
fi
echo "REOLING_SKIP_SYSTEM_CHECK=1: the app starts all the same"
rm -rf "$work/AppDir-missing" "$work/AppDir-old"

# --- AppImage ------------------------------------------------------------------
appimage_pack "$appimagetool" "$appdir" "$out/$(appimage_file_name app-only "$version")"
