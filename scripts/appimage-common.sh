#!/usr/bin/env bash
# What the two AppImages of a release share, sourced by
# scripts/build-appimage.sh (the embed AppImage: GTK 4, GStreamer, its
# plugins and the decoders inside), scripts/build-appimage-app-only.sh (the
# system AppImage: the app, its GTK 4 sink, the icons and the desktop entry,
# everything else from the system), scripts/validate-in-docker.sh,
# scripts/smoke-app-only-in-docker.sh and the release workflow: the two file
# names, AppRun, the icons, and the call that packs an AppDir. Both AppImages
# carry the same AppRun, icons and desktop entry at the same places, from
# which the app gives the AppImage file its icon and its menu entry
# (src/ui/icon.rs). Defines functions only.

# The release file name of the AppImage of the variant $1 and the version
# $2: "bundled", the one with GTK 4 and GStreamer inside, is tagged "embed",
# "app-only", the one that takes them from the system, "system". The tag
# sits between the version and the architecture, so that both names end in
# -x86_64.AppImage; the version itself never carries it.
appimage_file_name() {
    case $1 in
        bundled) echo "Reoling-$2-embed-x86_64.AppImage" ;;
        app-only) echo "Reoling-$2-system-x86_64.AppImage" ;;
        *) echo "appimage_file_name: no variant '$1'" >&2; return 2 ;;
    esac
}

# "<variant> <version>" for a release file name ($1, a path or a name); fails
# for another name. The version must be digits and dots, so that no pattern
# takes "1.0.5-embed" for a version.
appimage_variant_of() {
    local name=${1##*/} variant pattern prefix suffix version
    for variant in bundled app-only; do
        pattern=$(appimage_file_name "$variant" '*')
        prefix=${pattern%%\**} suffix=${pattern#*\*}
        [[ $name == "$prefix"*"$suffix" ]] || continue
        version=${name#"$prefix"}
        version=${version%"$suffix"}
        if [[ $version =~ ^[0-9]+(\.[0-9]+)*$ ]]; then
            echo "$variant $version"
            return 0
        fi
    done
    return 1
}

# AppRun, in the AppDir $1. linuxdeploy's AppRun names the hooks it knows;
# this one sources every hook present (in the embed AppImage the two
# linuxdeploy plugins' and ours, in name order; in the system AppImage ours
# alone) and runs the app. APPDIR is set first, to the directory AppRun is
# in: the AppImage runtime sets it to just that, but an extracted AppRun
# (--appimage-extract) gets no APPDIR, or one inherited from another
# AppImage (a runtime's APPDIR and APPIMAGE reach every process its app
# starts), and the gstreamer hook, sourced before the gtk one, would
# otherwise point the bundled GStreamer at the host's plugins, or at the
# other image's. An APPIMAGE that did not come with this directory names
# another AppImage's file and is dropped: the app gives the file named by
# APPIMAGE its icon.
appimage_write_apprun() {
    local appdir=$1
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
}

# The icon the file manager shows for the .AppImage file, from the
# repository $1 into the AppDir $2: at start the app copies the hicolor
# icons out of the mounted image into the user's icon theme (the file's
# custom icon, which GNOME Files shows, points at one of them) and reads
# .DirIcon and the 512 px hicolor icon to write its own file's
# thumbnail-cache entries (src/ui/icon.rs, install_appimage_integration),
# and AppImage tools read .DirIcon (some the <Icon>.png next to the desktop
# entry) as well, so both are real PNG files, not the symlinks linuxdeploy
# leaves (a symlink extracted on its own points nowhere). 256 px, as
# AppImage recommends, or the largest size the repository has.
appimage_root_icon() {
    local repo=$1 appdir=$2 icon_name icon_src size
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
}

# The symbolic icons the UI of the repository $1 asks for, into the AppDir
# $2, as a fallback for a desktop whose icon theme lacks some of them: GTK
# tries the theme first and these last (the app adds REOLING_ICON_PATH, which
# both hooks set, to its icon search path). They come from this machine's
# Adwaita icon theme.
appimage_fallback_icons() {
    local repo=$1 appdir=$2 fallback_icons name svg missing=()
    fallback_icons="$appdir/usr/share/reoling/icons"
    mkdir -p "$fallback_icons"
    for name in $(grep -rhoE '"[a-z0-9-]+-symbolic"' "$repo/src/ui" | tr -d '"' | sort -u); do
        svg=$(find /usr/share/icons/Adwaita -name "$name.svg" -print -quit 2>/dev/null || true)
        if [ -n "$svg" ]; then
            cp "$svg" "$fallback_icons/"
        else
            missing+=("$name")
        fi
    done
    [ ${#missing[@]} -eq 0 ] || echo "note: not in this machine's Adwaita icon theme, not bundled: ${missing[*]}"
}

# Packs the AppDir $2 into the AppImage $3 with appimagetool $1, with the
# runtime APPIMAGE_TOOLS holds when it has one (scripts/fetch-appimage-tools.sh
# fetches it, so that both AppImages of a build start with the same one),
# else the one appimagetool downloads itself.
appimage_pack() {
    local runtime=()
    if [ -n "${APPIMAGE_TOOLS:-}" ] && [ -f "$APPIMAGE_TOOLS/runtime-x86_64" ]; then
        runtime=(--runtime-file "$(realpath "$APPIMAGE_TOOLS/runtime-x86_64")")
    fi
    rm -f "$3"
    ARCH=x86_64 "$1" --comp zstd "${runtime[@]}" "$2" "$3"
    chmod +x "$3"
    echo "Wrote $3 ($(du -h "$3" | cut -f1))"
}
