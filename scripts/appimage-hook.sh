#! /usr/bin/env bash
# Reoling's own AppRun hook. scripts/build-appimage.sh copies it into the
# AppImage as apprun-hooks/zz-reoling.sh, with the versions of the bundled
# GTK and GStreamer filled in, and AppRun sources it after the linuxdeploy
# hooks (which point GTK and GStreamer at the bundled modules, loaders,
# schemas and plugins). AppRun runs with `set -e`: nothing here may fail.
#
# GTK and the GStreamer core come from the system where it has them in
# versions no older than the bundled ones, and from the AppImage otherwise.
# A desktop theme is written for the GTK it ships with (Ubuntu 26.04's Yaru
# uses CSS that GTK 4.14 does not parse), so the system's GTK draws the app
# the way the desktop's own applications look. The system's GTK may link
# the system's GStreamer core and -base libraries (Ubuntu 26.04's does, and
# binds every symbol at load time), so those two always come from the same
# place: the AppImage keeps its copies of both, and of the libraries built
# alongside them, in usr/lib/gtk-fallback, where only the bundled mode
# looks (LD_LIBRARY_PATH). Everything else stays in usr/lib for both: the
# GStreamer plugins, which run on either core, the -bad libraries they were
# built with (codecs, codecparsers, cuda, va: no stable ABI), FFmpeg, and
# the generic libraries the bundled objects find through their RUNPATH.
#
# The choice is made by asking the dynamic loader rather than by file
# names: it resolves the app and every bundled plugin against the system
# the way the app will run (`ldd -r`, without running anything) and must
# find every library and bind every symbol; then the app answers
# --version on the system's libraries. REOLING_GTK_STACK=host or bundled
# forces the choice (the probe still runs, for libva and the versions).
# One line on stderr names it.

reoling_lib="$APPDIR/usr/lib"
reoling_bundled_gtk=@BUNDLED_GTK@
reoling_bundled_gst=@BUNDLED_GSTREAMER@

# The bundled plugins, by file name: the probe preloads them through their
# directory on the library search path (LD_PRELOAD splits at spaces and
# colons, which the AppImage's own path may contain).
reoling_plugins=""
for reoling_f in "$reoling_lib"/gstreamer-1.0/*.so; do
    if [ -f "$reoling_f" ]; then
        reoling_plugins="$reoling_plugins${reoling_plugins:+ }${reoling_f##*/}"
    fi
done

# What the dynamic loader makes of the app and every bundled plugin, with
# the library directories $1 (may be empty) searched first: one
# "<soname> => <file>" line per library, "=> not found" for one it cannot
# find, "undefined symbol" for a symbol nothing provides (every symbol is
# bound up front), as `ldd -r` prints it. Nothing of the app runs. The app
# loads its own libraries when it starts and the plugins only later, which
# then take what of theirs is loaded already; so the app is traced alone
# first, and then the plugins with the app's libraries preloaded ahead of
# them, by file and in the order they loaded (a file whose path has a space
# or a colon, which LD_PRELOAD cannot name, loads in the plugins' turn).
# The output is that of both traces, the app's first: a library preloaded
# by file shows in the second as "<file> (<address>)", without its soname.
reoling_trace() {
    local reoling_path reoling_app reoling_first
    reoling_path="$reoling_lib/gstreamer-1.0${1:+:$1}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    reoling_app=$(LD_TRACE_LOADED_OBJECTS=1 LD_BIND_NOW=1 LD_WARN=1 LD_LIBRARY_PATH="$reoling_path" \
        "$APPDIR/usr/bin/reoling" 2>&1 || true)
    reoling_first=$(printf '%s\n' "$reoling_app" \
        | awk 'NF == 4 && $2 == "=>" && substr($3, 1, 1) == "/" && index($3, ":") == 0 { printf "%s ", $3 }')
    printf '%s\n' "$reoling_app"
    LD_TRACE_LOADED_OBJECTS=1 LD_BIND_NOW=1 LD_WARN=1 LD_LIBRARY_PATH="$reoling_path" \
        LD_PRELOAD="${LD_PRELOAD:+$LD_PRELOAD }$reoling_first$reoling_plugins" "$APPDIR/usr/bin/reoling" 2>&1 || true
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
# not found, an undefined symbol, a plugin that cannot be preloaded.
reoling_problems() {
    printf '%s\n' "$1" | grep -E 'not found|undefined symbol|cannot be preloaded' | sort -u || true
}
# The version of a GTK 4 or GStreamer library from its file name, which
# both projects' builds derive from it: libgtk-4.so.1.2200.4 is GTK 4.22.4
# (binary age 2204, interface age 4), libgstreamer-1.0.so.0.2802.0 is
# GStreamer 1.28.2. Arguments: the major version, the soname, the file.
# Empty for a name of another form.
reoling_version() {
    local real
    real=$(readlink -f "$3" 2>/dev/null || true)
    real=${real##*/}
    if [[ $real == "$2".* && ${real#"$2".} =~ ^([0-9]+)\.([0-9]+)$ ]]; then
        local age=$((10#${BASH_REMATCH[1]} + 10#${BASH_REMATCH[2]}))
        echo "$1.$((age / 100)).$((age % 100))"
    fi
}
# Whether version $1 is at least version $2, by major and minor version
# (new API comes only with a new minor version).
reoling_at_least() {
    local a1 a2 b1 b2
    IFS=. read -r a1 a2 _ <<< "$1"
    IFS=. read -r b1 b2 _ <<< "$2"
    [ "$a1" -gt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -ge "$b2" ]; }
}

reoling_host=$(reoling_trace "")

# libva is the system's, which its VA drivers were built for (a driver
# loads only into the libva release it was made for). The FFmpeg libraries
# behind the software decoders link libva, libva-drm and libva-x11 and do
# not load without them, so each one the system lacks comes from
# usr/lib/libva-fallback/<soname>/, a directory per library; a system
# without libva itself gets the whole bundled set, and no VA-API decoding.
reoling_va_missing=$(reoling_missing "$reoling_host" | grep '^libva' || true)
if [ -n "$reoling_va_missing" ]; then
    reoling_va_dirs=""
    reoling_va_names=""
    for reoling_f in "$reoling_lib"/libva-fallback/*/; do
        reoling_f=${reoling_f%/}
        if printf '%s\n' "$reoling_va_missing" | grep -qxF -e libva.so.2 -e "${reoling_f##*/}"; then
            reoling_va_dirs="$reoling_va_dirs${reoling_va_dirs:+:}$reoling_f"
            reoling_va_names="$reoling_va_names${reoling_va_names:+, }${reoling_f##*/}"
        fi
    done
    if [ -n "$reoling_va_dirs" ]; then
        if printf '%s\n' "$reoling_va_missing" | grep -qxF libva.so.2; then
            echo "reoling: this system has no libva; using the AppImage's own copy (no VA-API decoding)" >&2
        else
            echo "reoling: this system lacks $reoling_va_names; using the AppImage's copy" >&2
        fi
        export LD_LIBRARY_PATH="$reoling_va_dirs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        reoling_host=$(reoling_trace "")
    fi
fi

reoling_gtk_file=$(reoling_resolved "$reoling_host" libgtk-4.so.1)
reoling_gst_file=$(reoling_resolved "$reoling_host" libgstreamer-1.0.so.0)
reoling_gtk=$(reoling_version 4 libgtk-4.so.1 "$reoling_gtk_file")
reoling_gst=$(reoling_version 1 libgstreamer-1.0.so.0 "$reoling_gst_file")
reoling_mode=""
reoling_why=""
case ${REOLING_GTK_STACK:-} in
    host|bundled)
        reoling_mode=$REOLING_GTK_STACK
        reoling_why="REOLING_GTK_STACK=$REOLING_GTK_STACK"
        ;;
    "") ;;
    *) reoling_why="REOLING_GTK_STACK=$REOLING_GTK_STACK is neither host nor bundled, ignored" ;;
esac
if [ -z "$reoling_mode" ]; then
    reoling_note=$reoling_why
    reoling_why=""
    if [ -z "$reoling_gtk_file" ]; then
        reoling_why="the system has no GTK 4"
    elif [ -z "$reoling_gtk" ]; then
        reoling_why="no version in the name of the system's $reoling_gtk_file"
    elif ! reoling_at_least "$reoling_gtk" "$reoling_bundled_gtk"; then
        reoling_why="the system's GTK $reoling_gtk is older"
    elif [ -z "$reoling_gst_file" ]; then
        reoling_why="the system has no GStreamer 1"
    elif [ -z "$reoling_gst" ]; then
        reoling_why="no version in the name of the system's $reoling_gst_file"
    elif ! reoling_at_least "$reoling_gst" "$reoling_bundled_gst"; then
        reoling_why="the system's GStreamer $reoling_gst is older"
    else
        # A failure the bundled libraries share (a sound library the
        # system lacks, say) does not count against the system's.
        reoling_problems_host=$(reoling_problems "$reoling_host")
        if [ -n "$reoling_problems_host" ]; then
            reoling_problems_bundled=$(reoling_problems "$(reoling_trace "$reoling_lib/gtk-fallback")")
            reoling_new=$(printf '%s\n' "$reoling_problems_host" \
                | grep -vxF -f <(printf '%s\n' "$reoling_problems_bundled") | head -n1 || true)
            if [ -n "$reoling_new" ]; then
                reoling_new=$(printf '%s' "$reoling_new" | tr -s '\t ' ' ' | sed 's/^ //')
                reoling_why="with the system's libraries: $reoling_new"
            fi
        fi
        if [ -z "$reoling_why" ]; then
            case $("$APPDIR/usr/bin/reoling" --version 2>/dev/null || true) in
                "reoling "*) reoling_mode=host ;;
                *) reoling_why="the app does not start on the system's libraries" ;;
            esac
        fi
    fi
    if [ -n "$reoling_note" ]; then
        reoling_why="$reoling_note${reoling_why:+; $reoling_why}"
    fi
fi
[ -n "$reoling_mode" ] || reoling_mode=bundled

if [ "$reoling_mode" = host ]; then
    # The system's GTK finds its own modules, pixbuf loaders, print
    # backends and GSettings schemas; the bundled ones are for the bundled
    # GTK (the AppImage brings no schema of its own).
    unset GTK_DATA_PREFIX GTK_EXE_PREFIX GTK_PATH GDK_PIXBUF_MODULE_FILE GSETTINGS_SCHEMA_DIR GI_TYPELIB_PATH
    reoling_what="the system's GTK ${reoling_gtk:-4 of unknown version}, GStreamer ${reoling_gst:-1 of unknown version}"
else
    # gdk-pixbuf's loaders.cache (GDK_PIXBUF_MODULE_FILE, from the gtk
    # hook) names the bundled loaders by file name alone; the library
    # search path is what finds them.
    if [ -n "${GDK_PIXBUF_MODULE_FILE:-}" ]; then
        reoling_f=$(dirname "$GDK_PIXBUF_MODULE_FILE")
        export LD_LIBRARY_PATH="$reoling_f/loaders${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    export LD_LIBRARY_PATH="$reoling_lib/gtk-fallback${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    reoling_gst=$reoling_bundled_gst
    reoling_what="GTK $reoling_bundled_gtk, GStreamer $reoling_bundled_gst"
fi
echo "reoling: GTK and GStreamer core: $reoling_mode ($reoling_what${reoling_why:+; $reoling_why})" >&2

# A plugin registry of its own, one per GTK choice and GStreamer core
# version: neither GStreamer is necessarily the one ~/.cache/gstreamer-1.0
# belongs to, and sharing a registry would make them rescan at every start.
GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/reoling/gstreamer-registry-$reoling_mode-${reoling_gst:-unknown}.bin"
export GST_REGISTRY_1_0
mkdir -p "$(dirname "$GST_REGISTRY_1_0")" 2>/dev/null || true

# Fallback symbolic icons for the UI, tried after the desktop's own theme.
export REOLING_ICON_PATH="$APPDIR/usr/share/reoling/icons"

unset reoling_lib reoling_bundled_gtk reoling_bundled_gst reoling_plugins reoling_f reoling_host reoling_va_missing \
      reoling_va_dirs reoling_va_names reoling_gtk_file reoling_gst_file reoling_gtk reoling_gst reoling_mode \
      reoling_why reoling_note reoling_problems_host reoling_problems_bundled reoling_new reoling_what
unset -f reoling_trace reoling_resolved reoling_missing reoling_problems reoling_version reoling_at_least
