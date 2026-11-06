#!/usr/bin/env bash
# Installs the .deb (reoling_<version>-1_amd64.deb, cargo deb) in containers
# of the distributions that have a GTK 4 sink of their own,
# gstreamer1.0-gtk4 (Debian 13, Ubuntu 26.04 and later; not the Ubuntu 24.04
# the release workflow's smoke tests run the .deb on), as the release
# workflow does after those and scripts/validate-in-docker.sh --host before
# anything is pushed:
#
#   ubuntu-26.04-gtk4  Ubuntu 26.04 with gstreamer1.0-gtk4 (0.14.4 there)
#   debian-13-gtk4     Debian 13 with gstreamer1.0-gtk4 (0.13.5 there)
#
# The .deb brings its own sink, /usr/lib/reoling/gstreamer-1.0/libgstgtk4.so
# in the version scripts/build-gst-gtk4-plugin.sh builds, and that one, not
# the distribution's, must be the one the app uses. Each container installs
# the .deb with apt (its dependencies are in the image), then runs the app
# for 20 s on a virtual display (24-bit) with a session bus and an empty
# configuration, as the smoke tests do: it must still be running when the
# time is up (124 from timeout), print none of the loading failures,
# critical warnings and the app's line at start for a missing part they
# look for, and write no thumbnailer and no per-user desktop entry (only an
# AppImage writes one). Its requirements report
# (scripts/assert-requirements.sh, `reoling --check-requirements`) must then
# say that nothing is missing (status 0), with the .deb's gtk4paintablesink.
# The registry must be Reoling's own, in ~/.cache/reoling
# (gstreamer-registry-deb-<GStreamer version>.bin); the one other GStreamer
# programs share (~/.cache/gstreamer-1.0) must not be written. The report
# must be the same with GST_REGISTRY_FORK=no, where GStreamer loads the
# plugins in the app's own process while it scans them, with no registry
# and with the one that start wrote.
#
#   scripts/smoke-deb-in-docker.sh <.deb> [<scenario>...]   (default: both)
#
# The images (the distribution's GTK 4 sink, the .deb's dependencies, the
# first of each alternative, and what a run needs) are built with a network
# and tagged $SMOKE_IMAGE_PREFIX:<scenario> (default reoling-smoke-deb),
# which a second run reuses as long as the dependencies are the same; the
# containers run without a network. SINK_VERSION is gtk4paintablesink's
# version (default: the one scripts/build-gst-gtk4-plugin.sh builds).
set -euo pipefail
export LC_ALL=C.UTF-8

all_scenarios="ubuntu-26.04-gtk4 debian-13-gtk4"
# What a run needs besides the .deb's dependencies: a virtual display,
# xauth, a session bus, fonts, Mesa.
harness="xvfb xauth dbus fonts-dejavu-core libgl1 libegl1 libgles2"
# The screen xvfb-run gives on Debian and Ubuntu, as
# scripts/smoke-app-only-in-docker.sh passes it.
xvfb_screen='-screen 0 1280x1024x24'

# What the smoke tests of the release workflow look for: the loading
# failures, a critical warning from GTK, GLib or GStreamer, and the app's
# line at start for a part this system lacks.
failures='Unable to load image-loading module|Failed to load module|Failed to load plugin'
failures+='|cannot open shared object file|symbol lookup error|undefined symbol|Missing decoder|CRITICAL'
failures+='|^reoling: missing on this system: '

fail() { echo "FAIL $scenario: $*" >&2; exit 1; }

# --- In the container ------------------------------------------------------------
# $1 the scenario, $2 the .deb's path in the container.
inside() {
    scenario=$1
    local deb=$2 log=/tmp/smoke/run.log rc=0 sink registry state
    echo "=== $scenario: $(. /etc/os-release && echo "$PRETTY_NAME"), the .deb beside the distribution's GTK 4 sink"
    # The distribution's sink, which plain GStreamer takes from its plugin
    # directory: without it, this scenario would prove nothing.
    dpkg-query -W gstreamer1.0-gtk4 || fail "the image has no gstreamer1.0-gtk4"
    sink=$(dpkg -L gstreamer1.0-gtk4 | grep '/gstreamer-1\.0/libgstgtk4\.so$' || true)
    if [ -z "$sink" ] || [ ! -f "$sink" ]; then
        fail "gstreamer1.0-gtk4 has no libgstgtk4.so"
    fi
    echo "the distribution's GTK 4 sink: $sink"
    # The .deb, from the file; apt finds every dependency installed, so it
    # needs no network.
    apt-get install -y -qq --no-install-recommends "$deb" || fail "the .deb did not install"
    dpkg-query -W reoling
    [ -f /usr/lib/reoling/gstreamer-1.0/libgstgtk4.so ] || fail "the .deb has no GTK 4 sink of its own"
    export HOME=/tmp/smoke/home XDG_CONFIG_HOME=/tmp/smoke/config XDG_CACHE_HOME=/tmp/smoke/cache \
           XDG_DATA_HOME=/tmp/smoke/data
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
    [ "$(reoling --version 2>/dev/null)" = "reoling $VERSION" ] || fail "reoling --version does not say $VERSION"
    # Debian's and Ubuntu's xvfb-run send the program's stderr to its
    # stdout, so the log takes both.
    dbus-run-session -- xvfb-run -a -s "$xvfb_screen" timeout 20 reoling > "$log" 2>&1 || rc=$?
    cat "$log"
    echo "--- end of the output"
    [ "$rc" -eq 124 ] || fail "reoling ended early with status $rc"
    if grep -E "$failures" "$log"; then
        fail "reoling printed a loading failure, a critical warning or a missing part (above)"
    fi
    [ ! -e "$XDG_DATA_HOME/thumbnailers" ] || fail "reoling wrote a thumbnailer entry; Reoling installs none"
    if [ -e "$XDG_DATA_HOME/applications/de.dersobi.reoling.desktop" ]; then
        fail "the .deb wrote a per-user desktop entry"
    fi
    echo "reoling ran for 20 s"
    bash /smoke/assert-requirements.sh reoling 0 none /usr/lib/reoling/gstreamer-1.0/libgstgtk4.so \
        || fail "the requirements report (above)"
    registry=$(ls "$XDG_CACHE_HOME"/reoling/gstreamer-registry-deb-*.bin 2>/dev/null || true)
    case $registry in
        *$'\n'*|"") fail "not exactly one registry gstreamer-registry-deb-*.bin: ${registry:-none}" ;;
    esac
    echo "registry: $registry"
    [ ! -e "$XDG_CACHE_HOME/gstreamer-1.0" ] || fail "reoling wrote the registry other GStreamer programs share"
    rm -f "$registry"
    for state in none written; do
        echo "--- GST_REGISTRY_FORK=no, registry: $state"
        GST_REGISTRY_FORK=no bash /smoke/assert-requirements.sh reoling 0 none \
            /usr/lib/reoling/gstreamer-1.0/libgstgtk4.so \
            || fail "the requirements report with GST_REGISTRY_FORK=no, registry: $state (above)"
    done
    echo "PASS $scenario"
}

if [ "${1:-}" = --inside ]; then
    inside "$2" "$3"
    exit 0
fi

# --- On the host ---------------------------------------------------------------------
if [ $# -lt 1 ]; then
    echo "usage: $0 <.deb> [<scenario>...]   scenarios: $all_scenarios" >&2
    exit 2
fi
deb=$(realpath "$1")
shift
[ -f "$deb" ] || { echo "$deb is not a file" >&2; exit 1; }
case ${deb##*/} in
    reoling_*.deb) ;;
    *) echo "$deb is not named reoling_<version>_<architecture>.deb" >&2; exit 1 ;;
esac
self=$(realpath "$0")
here=$(dirname "$self")
sink_version=${SINK_VERSION:-$(sed -n 's/^default_version=//p' "$here/build-gst-gtk4-plugin.sh")}
[ -n "$sink_version" ] || { echo "no gtk4paintablesink version (SINK_VERSION)" >&2; exit 1; }
# The app's version: the package's, without the Debian revision.
version=$(dpkg-deb -f "$deb" Version)
version=${version%-*}
# The .deb's Depends, the first of each alternative, versions dropped.
depends=$(dpkg-deb -f "$deb" Depends | tr ',' '\n' | sed -e 's/|.*//' -e 's/(.*)//' -e 's/[[:space:]]//g' \
    | sed '/^$/d' | sort -u | xargs)
prefix=${SMOKE_IMAGE_PREFIX:-reoling-smoke-deb}
# shellcheck disable=SC2206
scenarios=(${*:-$all_scenarios})

dockerfile() {
    local base
    case $1 in
        ubuntu-26.04-gtk4) base=ubuntu:26.04 ;;
        debian-13-gtk4) base=debian:13 ;;
        *) echo "no such scenario: $1 (scenarios: $all_scenarios)" >&2; return 1 ;;
    esac
    printf 'FROM %s\nENV DEBIAN_FRONTEND=noninteractive\n' "$base"
    printf 'RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends %s\n' \
        "gstreamer1.0-gtk4 $harness $depends"
}

for scenario in "${scenarios[@]}"; do
    echo
    echo "=== Container smoke test of the .deb: $scenario"
    dockerfile "$scenario" > /dev/null
    dockerfile "$scenario" | docker build -t "$prefix:$scenario" -
    docker run --rm --network none -e SINK_VERSION="$sink_version" -e VERSION="$version" \
        -v "$deb:/smoke/${deb##*/}:ro" -v "$self:/smoke/smoke-deb-in-docker.sh:ro" \
        -v "$here/assert-requirements.sh:/smoke/assert-requirements.sh:ro" \
        "$prefix:$scenario" bash /smoke/smoke-deb-in-docker.sh --inside "$scenario" "/smoke/${deb##*/}"
done
echo
echo "container smoke tests of the .deb passed: ${scenarios[*]}"
