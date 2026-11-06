#! /usr/bin/env bash
# Reoling's AppRun hook for the app-only AppImage (the system AppImage,
# Reoling-<version>-system-x86_64.AppImage). scripts/build-appimage-app-only.sh
# copies it into that AppImage as apprun-hooks/zz-reoling.sh, with the file name
# of the release's bundled (embed) AppImage filled in, and AppRun sources it, the
# only hook there, with `set -e`: nothing here may fail unless it means to.
#
# The app-only AppImage carries the app, GStreamer's GTK 4 paintable sink, the icons
# and the desktop entry; GTK 4, GStreamer, its plugins and the decoders are the
# system's. The app and the sink bind every symbol when they load, so a library the
# system lacks would stop the app before it could say anything. So the dynamic
# loader is asked first what it makes of the app and the sink on this system (`ldd
# -r`, as scripts/appimage-hook.sh does; nothing of the app runs), and then the app
# answers --version. Where that fails, this names what is missing on stderr and,
# where stderr is no terminal (a start from the desktop), in a dialog or a
# notification that needs no GTK, with the command that installs it on this
# distribution, and ends AppRun with status 127, as the dynamic loader would have.
# REOLING_SKIP_SYSTEM_CHECK=1 starts the app all the same. The GStreamer elements
# the app uses (parsers, decoders) are the app's to check once it runs
# (REOLING_PLUGINS=system tells it that they are the system's).

reoling_plugins_dir="$APPDIR/usr/lib/gstreamer-1.0"
reoling_bundled_name=@BUNDLED_APPIMAGE@
reoling_releases=https://github.com/derSobi/Reoling/releases
# What the sink needs (scripts/build-gst-gtk4-plugin.sh builds it with the
# features gtk_v4_14 and dmabuf); the app alone would take GTK 4.6 and
# GStreamer 1.14.
reoling_min_gtk=4.14
reoling_min_gst=1.24

# What the dynamic loader makes of the app and of the sink: the app traced
# alone, then the sink with the app's libraries preloaded ahead of it, by
# file and in the order they loaded, as GStreamer finds them loaded when it
# opens the plugin (see reoling_trace in scripts/appimage-hook.sh). The sink
# is preloaded by file name through its directory on the library search path
# (LD_PRELOAD splits at spaces and colons, which the AppImage's own path may
# contain).
reoling_trace() {
    local reoling_app reoling_first
    reoling_app=$(LD_TRACE_LOADED_OBJECTS=1 LD_BIND_NOW=1 LD_WARN=1 "$APPDIR/usr/bin/reoling" 2>&1 || true)
    reoling_first=$(printf '%s\n' "$reoling_app" \
        | awk 'NF == 4 && $2 == "=>" && substr($3, 1, 1) == "/" && index($3, ":") == 0 { printf "%s ", $3 }')
    printf '%s\n' "$reoling_app"
    LD_TRACE_LOADED_OBJECTS=1 LD_BIND_NOW=1 LD_WARN=1 \
        LD_LIBRARY_PATH="$reoling_plugins_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        LD_PRELOAD="${LD_PRELOAD:+$LD_PRELOAD }${reoling_first}libgstgtk4.so" "$APPDIR/usr/bin/reoling" 2>&1 || true
}
# The file a trace ($1) resolved the soname $2 to; empty when it found none.
reoling_resolved() {
    printf '%s\n' "$1" | awk -v so="$2" '$1 == so && $2 == "=>" && substr($3, 1, 1) == "/" { print $3; exit }'
}
# The sonames a trace ($1) found no file for.
reoling_missing() {
    printf '%s\n' "$1" | awk '$2 == "=>" && $3 == "not" && $4 == "found" { print $1 }' | sort -u
}
# What in a trace ($1) would fail to load: a library or a symbol version
# not found, an undefined symbol, a file that cannot be preloaded.
reoling_problems() {
    printf '%s\n' "$1" | grep -E 'not found|undefined symbol|cannot be preloaded' | sort -u || true
}
# The version of a GTK 4 or GStreamer library from its file name:
# libgtk-4.so.1.2200.4 is GTK 4.22.4, libgstreamer-1.0.so.0.2802.0 is
# GStreamer 1.28.2 (see scripts/appimage-hook.sh). Arguments: the major
# version, the soname, the file. Empty for a name of another form.
reoling_version() {
    local real
    real=$(readlink -f "$3" 2>/dev/null || true)
    real=${real##*/}
    if [[ $real == "$2".* && ${real#"$2".} =~ ^([0-9]+)\.([0-9]+)$ ]]; then
        local age=$((10#${BASH_REMATCH[1]} + 10#${BASH_REMATCH[2]}))
        echo "$1.$((age / 100)).$((age % 100))"
    fi
}
# Whether version $1 is at least version $2, by major and minor version.
reoling_at_least() {
    local a1 a2 b1 b2
    IFS=. read -r a1 a2 _ <<< "$1"
    IFS=. read -r b1 b2 _ <<< "$2"
    [ "$a1" -gt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -ge "$b2" ]; }
}
# The command that installs what this AppImage takes from the system, for
# the distribution /etc/os-release names (its ID, then ID_LIKE), and on a
# second line where that distribution keeps the H.264 and H.265 decoders
# outside its own packages; nothing for a distribution not known here. The
# Debian and Ubuntu line is the .deb's dependencies (Cargo.toml,
# [package.metadata.deb] depends, and what dpkg-shlibdeps adds), the sound
# sink left to the desktop, which has one.
reoling_install_line() {
    local reoling_os
    reoling_os=$(if [ -r /etc/os-release ]; then . /etc/os-release; elif [ -r /usr/lib/os-release ]; then
                     . /usr/lib/os-release; fi 2>/dev/null; echo " ${ID:-} ${ID_LIKE:-} ")
    case $reoling_os in
        *" debian "*|*" ubuntu "*)
            echo "sudo apt install libgtk-4-1 libgstreamer-gl1.0-0 gstreamer1.0-plugins-base" \
                 "gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav"
            ;;
        *" fedora "*)
            echo "sudo dnf install gtk4 gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free" \
                 "gstreamer1-plugin-libav gstreamer1-plugin-openh264 openh264"
            echo "H.265, and H.264 without OpenH264, need RPM Fusion (https://rpmfusion.org/Configuration), then:" \
                 "sudo dnf swap ffmpeg-free ffmpeg --allowerasing"
            ;;
        *" opensuse "*|*" suse "*)
            echo "sudo zypper install libgtk-4-1 libgstgl-1_0-0 gstreamer-plugins-base gstreamer-plugins-good" \
                 "gstreamer-plugins-bad gstreamer-plugins-libav"
            echo "H.264 and H.265 decoders come from Packman: sudo zypper install opi, then: opi codecs"
            ;;
        *" arch "*)
            echo "sudo pacman -S --needed gtk4 gst-plugins-base gst-plugins-good gst-plugins-bad-libs gst-libav" \
                 "gst-plugin-va"
            ;;
    esac
}
# $1 as a GVariant string: backslashes, quotes and line ends escaped.
reoling_gvariant() {
    local s=${1//\\/\\\\}
    s=${s//\'/\\\'}
    printf "'%s'" "${s//$'\n'/\\n}"
}
# Shows the text $2 with the title $1 where stderr goes unseen: started from
# the desktop (stderr is no terminal), on a display. A dialog stays until it
# is read, so the desktop's own dialog program comes first, KDE's on KDE, as
# long as it starts (zenity 4 needs GTK 4, which may be what is missing);
# then a notification through D-Bus with GLib's gdbus (GLib is everywhere
# GTK was), libnotify's notify-send, and X11's xmessage. The text has no
# '&', '<' or '>', which a dialog or a notification would take for markup.
reoling_show() {
    local reoling_dialog reoling_status reoling_err
    if [ -t 2 ] || [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
        return 0
    fi
    case ":${XDG_CURRENT_DESKTOP:-}:" in
        *:KDE:*) set -- "$1" "$2" kdialog zenity ;;
        *) set -- "$1" "$2" zenity kdialog ;;
    esac
    for reoling_dialog in "$3" "$4"; do
        command -v "$reoling_dialog" >/dev/null 2>&1 || continue
        reoling_status=0
        case $reoling_dialog in
            kdialog) reoling_err=$(kdialog --title "$1" --error "$2" 2>&1 >/dev/null) || reoling_status=$? ;;
            zenity) reoling_err=$(zenity --error --title="$1" --text="$2" --width=560 2>&1 >/dev/null) \
                        || reoling_status=$? ;;
        esac
        # The dialog was shown when it ends with 0 (OK) or 1 (closed with the
        # window's button or Esc), but GTK ends zenity with 1 as well when it
        # cannot open the display ("Failed to open display", "cannot open
        # display"). Any other status (126 or 127 from the loader, a crash)
        # showed nothing either: the next way is tried.
        case $reoling_status:$reoling_err in
            1:*"open display"*) ;;
            0:*|1:*) return 0 ;;
        esac
    done
    if command -v gdbus >/dev/null 2>&1 \
        && timeout 5 gdbus call --session --dest org.freedesktop.Notifications \
               --object-path /org/freedesktop/Notifications --method org.freedesktop.Notifications.Notify \
               "'Reoling'" "uint32 0" "'dialog-error'" "$(reoling_gvariant "$1")" "$(reoling_gvariant "$2")" \
               "@as []" "{'urgency': <byte 2>}" "int32 0" >/dev/null 2>&1; then
        return 0
    fi
    if command -v notify-send >/dev/null 2>&1 \
        && timeout 5 notify-send --app-name=Reoling --urgency=critical --icon=dialog-error "$1" "$2" \
               >/dev/null 2>&1; then
        return 0
    fi
    if [ -n "${DISPLAY:-}" ] && command -v xmessage >/dev/null 2>&1; then
        printf '%s\n' "$2" | xmessage -center -title "$1" -buttons OK:0 -default OK -file - >/dev/null 2>&1 || true
    fi
    return 0
}

reoling_out=$(reoling_trace)
reoling_gtk_file=$(reoling_resolved "$reoling_out" libgtk-4.so.1)
reoling_gst_file=$(reoling_resolved "$reoling_out" libgstreamer-1.0.so.0)
reoling_gtk=$(reoling_version 4 libgtk-4.so.1 "$reoling_gtk_file")
reoling_gst=$(reoling_version 1 libgstreamer-1.0.so.0 "$reoling_gst_file")

# What is missing, by what provides it (each named once), with the sonames
# for the record; what is there but too old; and anything else that stops
# the app or the sink from loading.
reoling_lacks=""
reoling_sonames=""
for reoling_f in $(reoling_missing "$reoling_out"); do
    case $reoling_f in
        libgtk-4.so.*|libgraphene-1.0.so.*|libpango*|libcairo*|libgdk_pixbuf*|libwayland-client.so.*)
            reoling_label="GTK 4" ;;
        libgst*) reoling_label=GStreamer ;;
        libglib-2.0.so.*|libgobject-2.0.so.*|libgio-2.0.so.*|libgmodule-2.0.so.*) reoling_label=GLib ;;
        *) reoling_label=$reoling_f ;;
    esac
    case ", $reoling_lacks, " in
        *", $reoling_label, "*) ;;
        *) reoling_lacks="$reoling_lacks${reoling_lacks:+, }$reoling_label" ;;
    esac
    reoling_sonames="$reoling_sonames${reoling_sonames:+ }$reoling_f"
done
reoling_old=""
if [ -n "$reoling_gtk" ] && ! reoling_at_least "$reoling_gtk" "$reoling_min_gtk"; then
    reoling_old="GTK $reoling_gtk (Reoling needs $reoling_min_gtk)"
fi
if [ -n "$reoling_gst" ] && ! reoling_at_least "$reoling_gst" "$reoling_min_gst"; then
    reoling_old="$reoling_old${reoling_old:+, }GStreamer $reoling_gst (Reoling needs $reoling_min_gst)"
fi
if printf '%s\n' "$reoling_out" | grep -q "GLIBC_[0-9.]*' not found"; then
    reoling_old="$reoling_old${reoling_old:+, }the C library, glibc (Reoling needs 2.35)"
fi
reoling_other=""
if [ -z "$reoling_lacks$reoling_old" ]; then
    # A problem in a library of the system's own, which the app and the sink
    # do not reach, is noted and does not stop the start.
    reoling_other=$(reoling_problems "$reoling_out" | grep -F -e /usr/bin/reoling -e /libgstgtk4.so | head -n1 || true)
    reoling_f=$(reoling_problems "$reoling_out" | grep -v -F -e /usr/bin/reoling -e /libgstgtk4.so | head -n1 || true)
    if [ -n "$reoling_f" ]; then
        echo "reoling: note: the dynamic loader reports, in a library of the system's: $(printf '%s' "$reoling_f" \
            | tr -s '\t ' ' ' | sed 's/^ //')" >&2
    fi
    if [ -n "$reoling_other" ]; then
        reoling_other="The dynamic loader reports: $(printf '%s' "$reoling_other" | tr -s '\t ' ' ' | sed 's/^ //')"
    else
        reoling_v=$("$APPDIR/usr/bin/reoling" --version 2>&1 || true)
        if ! printf '%s\n' "$reoling_v" | grep -q '^reoling [0-9]'; then
            reoling_other="The app does not start: $(printf '%s\n' "$reoling_v" | head -n1)"
        fi
    fi
fi

if [ -n "$reoling_lacks$reoling_old$reoling_other" ]; then
    # The same text on stderr, a line at a time behind "reoling: ", and in
    # the dialog or the notification.
    reoling_text="This Reoling AppImage takes GTK 4 ($reoling_min_gtk or newer) and GStreamer"
    reoling_text="$reoling_text ($reoling_min_gst or newer) with its plugins and decoders from the system."
    if [ -n "$reoling_lacks" ]; then
        reoling_install=$(reoling_install_line)
        reoling_text="$reoling_text"$'\n'"This system lacks $reoling_lacks."
        if [ -n "$reoling_install" ]; then
            reoling_text="$reoling_text Install them with:"$'\n'"$reoling_install"
        else
            reoling_text="$reoling_text Install them with the system's package manager."
        fi
    fi
    if [ -n "$reoling_old" ]; then
        reoling_text="$reoling_text"$'\n'"Too old on this system: $reoling_old."
    fi
    if [ -n "$reoling_other" ]; then
        reoling_text="$reoling_text"$'\n'"$reoling_other"
    fi
    reoling_text="$reoling_text"$'\n'"Or use $reoling_bundled_name, which brings its own GTK 4 and GStreamer:"
    reoling_text="$reoling_text $reoling_releases"
    echo "reoling: cannot start${reoling_sonames:+ (missing: $reoling_sonames)}" >&2
    printf '%s\n' "$reoling_text" | sed 's/^/reoling: /' >&2
    if [ "${REOLING_SKIP_SYSTEM_CHECK:-}" = 1 ]; then
        echo "reoling: REOLING_SKIP_SYSTEM_CHECK=1: starting all the same" >&2
    else
        reoling_show "Reoling cannot start" "${reoling_text//$'\n'/$'\n\n'}"
        exit 127
    fi
fi

if [ -n "${REOLING_GTK_STACK:-}" ]; then
    echo "reoling: REOLING_GTK_STACK=$REOLING_GTK_STACK is ignored: this AppImage brings no GTK or GStreamer" \
         "of its own" >&2
fi
echo "reoling: GTK and GStreamer core: host (the system's GTK ${reoling_gtk:-4 of unknown version}," \
     "GStreamer ${reoling_gst:-1 of unknown version}; app-only AppImage, the system's plugins)" >&2
# For the app's requirements check: the GStreamer plugins are the system's,
# so a missing one is named with this distribution's package and the embed
# AppImage as the other way.
export REOLING_PLUGINS=system

# GStreamer's GTK 4 sink from the AppImage, ahead of a distribution's own
# (gstreamer1.0-gtk4 and the like, of another version): GStreamer takes a
# plugin file name once, from the first directory that has it, and
# GST_PLUGIN_PATH comes before the system's directories. GST_PLUGIN_PATH_1_0
# hides GST_PLUGIN_PATH, so the user's value of either goes behind ours.
# GST_PLUGIN_SYSTEM_PATH_1_0 stays as it is: unset, the system's plugins and
# ~/.local/share/gstreamer-1.0/plugins.
reoling_f=${GST_PLUGIN_PATH_1_0-${GST_PLUGIN_PATH-}}
export GST_PLUGIN_PATH_1_0="$reoling_plugins_dir${reoling_f:+:$reoling_f}"
# A plugin registry of its own: the sink's directory is a new mount point at
# every start, which would have the user's shared registry
# (~/.cache/gstreamer-1.0) rewritten at every start of Reoling and of every
# other GStreamer program after it. Not the bundled AppImage's either, whose
# registries hold its own plugins.
GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/reoling/gstreamer-registry-system-${reoling_gst:-unknown}.bin"
export GST_REGISTRY_1_0
mkdir -p "$(dirname "$GST_REGISTRY_1_0")" 2>/dev/null || true

# The image's data directory first, as the bundled AppImage's gtk hook puts
# it: GTK finds the app's icon there, for the window and the dock, before
# the app has copied it into the user's icon theme (src/ui/icon.rs). Unset,
# XDG_DATA_DIRS stands for /usr/local/share:/usr/share.
export XDG_DATA_DIRS="$APPDIR/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# Fallback symbolic icons for the UI, tried after the desktop's own theme.
export REOLING_ICON_PATH="$APPDIR/usr/share/reoling/icons"

unset reoling_plugins_dir reoling_bundled_name reoling_releases reoling_min_gtk reoling_min_gst reoling_out \
      reoling_gtk_file reoling_gst_file reoling_gtk reoling_gst reoling_lacks reoling_sonames reoling_f reoling_label \
      reoling_old reoling_other reoling_v reoling_text reoling_install
unset -f reoling_trace reoling_resolved reoling_missing reoling_problems reoling_version reoling_at_least \
         reoling_install_line reoling_gvariant reoling_show
