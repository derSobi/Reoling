#!/usr/bin/env bash
# Starts the AppImage in containers that stand for the systems its two GTK
# stacks are for (scripts/appimage-hook.sh chooses one at every start), as
# the release workflow does after the smoke tests on its Ubuntu 24.04
# runner, and scripts/validate-in-docker.sh before anything is pushed:
#
#   ubuntu-26.04-yaru-dark     Ubuntu 26.04 with its GTK 4.22, libva and
#                              libva-x11, the Yaru theme and
#                              GTK_THEME=Yaru-dark: the AppImage takes the
#                              system's GTK and GStreamer core ("host"), and
#                              the theme, written for that GTK, parses
#                              without a single "Theme parser error"
#   ubuntu-26.04-no-libva-x11  Ubuntu 26.04 with its GTK 4.22 and libva but
#                              no libva-x11, which the FFmpeg libraries
#                              behind the software decoders link: the
#                              AppImage adds its own libva-x11.so.2 alone,
#                              and avdec_h264 and avdec_aac register
#   ubuntu-24.04-no-gtk        Ubuntu 24.04 with the libraries the AppImage
#                              takes from the system but no GTK 4 (and no
#                              libva): it takes its own GTK and GStreamer
#                              core ("bundled") and starts
#
# Each container runs the AppImage (unpacked by its runtime, no FUSE in
# there) for 20 s on a virtual display with a session bus and an empty
# configuration, as the smoke tests do: it must still be running when the
# time is up (124 from timeout), print none of the loading failures they
# look for, name the expected GTK stack in exactly one line and have
# mapped libgtk-4 from where that stack keeps it.
#
#   scripts/smoke-appimage-in-docker.sh <AppImage> [<scenario>...]   (default: all three)
#
# The images are built from ubuntu:26.04 and ubuntu:24.04 (apt, so with a
# network) and tagged reoling-smoke:<scenario>, which a second run reuses;
# the containers run without a network.
set -euo pipefail
export LC_ALL=C.UTF-8

all_scenarios="ubuntu-26.04-yaru-dark ubuntu-26.04-no-libva-x11 ubuntu-24.04-no-gtk"
# What the AppImage takes from the system and the smoke run needs (a
# virtual display, a session bus, pgrep), besides GTK.
resolute_base="libglib2.0-0t64 fonts-dejavu-core shared-mime-info libgl1 libegl1 libgles2 xvfb xauth dbus procps
    libasound2t64"
noble_base="libglib2.0-0t64 libharfbuzz0b libfribidi0 libfontconfig1 libfreetype6 fonts-dejavu-core libx11-6 libxcb1
    libwayland-client0 libgl1 libegl1 libgles2 libxfixes3 shared-mime-info xvfb xauth dbus procps libasound2t64"

# The loading failures the smoke tests of the release workflow look for.
failures='Unable to load image-loading module|Failed to load module|Failed to load plugin'
failures+='|cannot open shared object file|symbol lookup error|undefined symbol|Missing decoder'

fail() { echo "FAIL $scenario: $*" >&2; exit 1; }

# --- In the container ------------------------------------------------------------
inside() {
    scenario=$1
    local expect theme_check=0 libav_check=0
    case $scenario in
        ubuntu-26.04-yaru-dark)
            expect=host theme_check=1
            export GTK_THEME=Yaru-dark
            [ -f /usr/share/themes/Yaru-dark/gtk-4.0/gtk.css ] || fail "the image has no Yaru-dark theme for GTK 4"
            ;;
        ubuntu-26.04-no-libva-x11)
            expect=host libav_check=1
            ldconfig -p | grep -q '^[[:space:]]*libva\.so\.2 ' || fail "the image has no libva.so.2"
            if ldconfig -p | grep -q '^[[:space:]]*libva-x11\.so\.2 '; then fail "the image has libva-x11.so.2"; fi
            ;;
        ubuntu-24.04-no-gtk)
            expect=bundled libav_check=1
            if ldconfig -p | grep -q '^[[:space:]]*libgtk-4\.so\.1 '; then fail "the image has a GTK 4"; fi
            ;;
        *) fail "no such scenario" ;;
    esac
    echo "=== $scenario: $(. /etc/os-release && echo "$PRETTY_NAME"), expecting the $expect GTK stack"
    export APPIMAGE_EXTRACT_AND_RUN=1 HOME=/tmp/smoke/home XDG_CONFIG_HOME=/tmp/smoke/config \
           XDG_CACHE_HOME=/tmp/smoke/cache XDG_DATA_HOME=/tmp/smoke/data
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
    local log=/tmp/smoke/run.log maps=/tmp/smoke/maps.txt rc=0 job pid
    # Ubuntu's xvfb-run sends the program's stderr to its stdout, so the
    # log takes both. Meanwhile, once the app has mapped GTK (the runtime
    # unpacks the image first, and a slow machine takes a while), the
    # libraries it has mapped: from a process still there a second later,
    # not one of the hook's short probes, which carry the same name.
    dbus-run-session -- xvfb-run -a timeout 20 /smoke/Reoling.AppImage > "$log" 2>&1 &
    job=$!
    : > "$maps"
    for _ in $(seq 1 18); do
        sleep 1
        pid=$(pgrep -x reoling | head -n1 || true)
        if [ -n "$pid" ] && grep -q '/libgtk-4\.so\.1' "/proc/$pid/maps" 2>/dev/null; then
            sleep 1
            if awk '{ print $6 }' "/proc/$pid/maps" 2>/dev/null | grep '\.so' | sort -u > "$maps.new" \
                && grep -q '/libgtk-4\.so\.1' "$maps.new"; then
                mv "$maps.new" "$maps"
                break
            fi
        fi
    done
    wait "$job" || rc=$?
    cat "$log"
    echo "--- end of the output"
    [ "$rc" -eq 124 ] || fail "the AppImage ended early with status $rc"
    if grep -E "$failures" "$log"; then
        fail "the AppImage printed a loading failure (above)"
    fi
    local lines
    lines=$(grep '^reoling: GTK and GStreamer core: ' "$log" || true)
    [ "$(printf '%s' "$lines" | grep -c '^')" -eq 1 ] || fail "not exactly one line names the GTK stack"
    case $lines in
        "reoling: GTK and GStreamer core: $expect ("*) ;;
        *) fail "expected the $expect GTK stack" ;;
    esac
    local gtk
    gtk=$(grep -m1 '/libgtk-4\.so\.1' "$maps" 2>/dev/null || true)
    echo "libgtk-4 mapped from: ${gtk:-nowhere}"
    case $expect:$gtk in
        host:*/usr/lib/gtk-fallback/*|host:*appimage_extracted*) fail "libgtk-4 is the AppImage's, not the system's" ;;
        host:/*|bundled:*/usr/lib/gtk-fallback/libgtk-4.so.1) ;;
        *) fail "libgtk-4 is not the $expect stack's" ;;
    esac
    if [ "$theme_check" = 1 ]; then
        local errors
        errors=$(grep -c 'Theme parser error' "$log" || true)
        [ "$errors" -eq 0 ] || fail "$errors 'Theme parser error' lines with $GTK_THEME"
        echo "$GTK_THEME: no theme parser error"
    fi
    if [ "$libav_check" = 1 ]; then
        local registry element
        registry=$(ls "$XDG_CACHE_HOME"/reoling/gstreamer-registry-*.bin 2>/dev/null | head -n1 || true)
        [ -n "$registry" ] || fail "the app wrote no GStreamer registry"
        for element in avdec_h264 avdec_aac; do
            grep -qa "$element" "$registry" || fail "$element is not in the registry ($registry)"
        done
        echo "registry $(basename "$registry"): avdec_h264 and avdec_aac present"
    fi
    echo "PASS $scenario"
}

if [ "${1:-}" = --inside ]; then
    inside "$2"
    exit 0
fi

# --- On the host ---------------------------------------------------------------------
if [ $# -lt 1 ]; then
    echo "usage: $0 <AppImage> [<scenario>...]   scenarios: $all_scenarios" >&2
    exit 2
fi
appimage=$(realpath "$1")
shift
[ -f "$appimage" ] || { echo "$appimage is not a file" >&2; exit 1; }
chmod +x "$appimage" 2>/dev/null || true
self=$(realpath "$0")
# shellcheck disable=SC2206
scenarios=(${*:-$all_scenarios})

dockerfile() {
    local base packages
    case $1 in
        ubuntu-26.04-yaru-dark)
            base=ubuntu:26.04 packages="$resolute_base libgtk-4-1 libva2 libva-x11-2 yaru-theme-gtk" ;;
        ubuntu-26.04-no-libva-x11)
            base=ubuntu:26.04 packages="$resolute_base libgtk-4-1 libva2 libva-drm2" ;;
        ubuntu-24.04-no-gtk) base=ubuntu:24.04 packages="$noble_base" ;;
        *) echo "no such scenario: $1 (scenarios: $all_scenarios)" >&2; return 1 ;;
    esac
    printf 'FROM %s\nENV DEBIAN_FRONTEND=noninteractive\n' "$base"
    # shellcheck disable=SC2086
    printf 'RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends %s\n' "$(echo $packages)"
}

for scenario in "${scenarios[@]}"; do
    echo
    echo "=== Container smoke test: $scenario"
    dockerfile "$scenario" > /dev/null
    dockerfile "$scenario" | docker build -t "reoling-smoke:$scenario" -
    docker run --rm --network none \
        -v "$appimage:/smoke/Reoling.AppImage:ro" -v "$self:/smoke/smoke-appimage-in-docker.sh:ro" \
        "reoling-smoke:$scenario" bash /smoke/smoke-appimage-in-docker.sh --inside "$scenario"
done
echo
echo "container smoke tests passed: ${scenarios[*]}"
