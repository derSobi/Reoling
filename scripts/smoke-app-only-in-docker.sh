#!/usr/bin/env bash
# Starts the app-only AppImage (Reoling-<version>-system-x86_64.AppImage,
# scripts/build-appimage-app-only.sh), which takes GTK 4, GStreamer, its
# plugins and the decoders from the system, in containers of the
# distributions it is for, as the release workflow does (the Ubuntu 24.04
# scenarios in its packages job, each other distribution in a job of its
# own) and scripts/validate-in-docker.sh before anything is pushed:
#
#   ubuntu-24.04-bare      the distribution's image as it comes, no GTK 4, no
#   debian-13              GStreamer. First the AppImage must refuse to start
#   ubuntu-26.04           (status 127) and print that distribution's install
#   fedora-44              command, then, with exactly that command run
#   opensuse-tumbleweed    (without recommends or weak dependencies), the
#   archlinux              desktop's sound plugin and the distribution's own
#                          GTK 4 sink where it has one, it must start on the
#                          system's GTK and GStreamer, with its own sink ahead
#                          of the distribution's
#   ubuntu-24.04-desktop   what an Ubuntu 24.04 desktop installs (GTK 4 and
#                          GStreamer without the -bad and libav plugins): it
#                          starts, and reports the parsers and decoders it
#                          lacks
#   ubuntu-24.04-deb-deps  the .deb's dependencies ($DEB): nothing missing
#   fedora-44-noopenh264   a stock Fedora desktop: GTK 4 and GStreamer with
#                          openh264dec on noopenh264, Fedora's stand-in for
#                          the OpenH264 library that decodes nothing (its
#                          fedora-cisco-openh264 repository left out): it
#                          starts, and reports neither H.264 nor H.265
#   ubuntu-24.04-notify    the refusal started from the desktop (stderr no
#                          terminal, a display, a session bus, no dialog
#                          program): a desktop notification, which a
#                          stand-in notification service receives
#   ubuntu-24.04-zenity-fails  the same with a zenity that is there but does
#                          not start (zenity 4 needs GTK 4, which may be what
#                          is missing): the hook goes on to the notification
#   ubuntu-24.04-zenity-closed  a zenity whose dialog the user closes with the
#                          window's button or Esc (status 1): no notification
#                          after it
#   ubuntu-24.04-zenity-no-display  a zenity that cannot open the display,
#                          which GTK also ends with 1: the notification
#
# Where the app starts, it runs for 20 s on a virtual display with a session
# bus and an empty configuration (no devices, no network), as the smoke tests
# do: it must still be running when the time is up (124 from timeout), print
# none of the loading failures they look for, name the system's GTK stack
# ("host") in exactly one line, have mapped the system's libgtk-4, and have
# written a plugin registry of its own whose one gtk4paintablesink is the
# AppImage's. Then its requirements report (scripts/assert-requirements.sh,
# `reoling --check-requirements`) must end with the scenario's status, report
# exactly the scenario's missing features and name the AppImage's
# gtk4paintablesink, in the version scripts/build-gst-gtk4-plugin.sh builds;
# the line the app prints at start for what is missing must be there exactly
# where something is.
#
#   scripts/smoke-app-only-in-docker.sh <AppImage> [<scenario>...]   (default: all)
#
# The images are built with a network (the package managers) and tagged
# $SMOKE_IMAGE_PREFIX:<scenario>, <scenario>-bare and <scenario>-installed,
# which a second run takes from docker's cache; the containers run without
# a network.
#
# Environment:
#   DEB                 the .deb whose dependencies ubuntu-24.04-deb-deps
#                       installs (needed for that scenario only)
#   SMOKE_IMAGE_PREFIX  the images' name (default reoling-smoke-app-only)
#   SINK_VERSION        gtk4paintablesink's version (default: the one
#                       scripts/build-gst-gtk4-plugin.sh builds)
set -euo pipefail
export LC_ALL=C.UTF-8

all_scenarios="ubuntu-24.04-bare ubuntu-24.04-desktop ubuntu-24.04-deb-deps ubuntu-24.04-notify
    ubuntu-24.04-zenity-fails ubuntu-24.04-zenity-closed ubuntu-24.04-zenity-no-display debian-13 ubuntu-26.04
    fedora-44 fedora-44-noopenh264 opensuse-tumbleweed archlinux"

# The loading failures the smoke tests of the release workflow look for.
failures='Unable to load image-loading module|Failed to load module|Failed to load plugin'
failures+='|cannot open shared object file|symbol lookup error|undefined symbol|Missing decoder'

fail() { echo "FAIL $scenario: $*" >&2; exit 1; }

# --- In the container ------------------------------------------------------------
# A home and XDG directories of its own; the AppImage's runtime unpacks it
# (no FUSE in a container).
fresh_home() {
    export APPIMAGE_EXTRACT_AND_RUN=1 HOME=/tmp/smoke/home XDG_CONFIG_HOME=/tmp/smoke/config \
           XDG_CACHE_HOME=/tmp/smoke/cache XDG_DATA_HOME=/tmp/smoke/data
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME"
}

# Checks the hook's refusal in /tmp/smoke/err.txt, with the status $1: 127,
# the libraries named, exactly the expected install command, the embed
# AppImage, and no GTK stack named.
check_refusal() {
    local missing so lines
    [ "$1" -eq 127 ] || fail "the AppImage ended with status $1, not 127"
    missing=$(grep '^reoling: cannot start (missing: ' /tmp/smoke/err.txt || true)
    for so in libgtk-4.so.1 libgstreamer-1.0.so.0; do
        case " ${missing%)} " in
            *" $so "*) ;;
            *) fail "the refusal does not name $so as missing: ${missing:-no 'cannot start' line}" ;;
        esac
    done
    lines=$(grep '^reoling: sudo ' /tmp/smoke/err.txt || true)
    [ "$lines" = "reoling: $EXPECT_LINE" ] \
        || fail "expected exactly the install line 'reoling: $EXPECT_LINE', got: ${lines:-none}"
    grep -qF "Or use $BUNDLED_NAME" /tmp/smoke/err.txt || fail "the refusal does not point at $BUNDLED_NAME"
    if grep -q 'GTK and GStreamer core:' /tmp/smoke/err.txt; then
        fail "the hook named a GTK stack: it did not refuse"
    fi
}

# The bare system: the AppImage refuses, on stderr alone (no display).
refuse() {
    local rc=0
    fresh_home
    unset DISPLAY WAYLAND_DISPLAY
    echo "=== $scenario, as it comes: $(. /etc/os-release && echo "$PRETTY_NAME")"
    timeout 60 /smoke/Reoling.AppImage > /tmp/smoke/out.txt 2> /tmp/smoke/err.txt || rc=$?
    cat /tmp/smoke/out.txt /tmp/smoke/err.txt
    echo "--- end of the output, status $rc"
    check_refusal "$rc"
    echo "PASS $scenario: refused, with the install line"
}

# The refusal started from the desktop: a display, a session bus with a
# stand-in notification service, stderr no terminal.
notify() {
    local rc calls expected=1 fake_status="" fake_error=""
    fresh_home
    echo "=== $scenario: $(. /etc/os-release && echo "$PRETTY_NAME"), the refusal on a desktop"
    # A notification service that prints each Notify call it gets and
    # answers it, until the first call or a minute has passed.
    cat > /tmp/smoke/notifyd.py <<'PY'
import gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib
XML = """<node><interface name="org.freedesktop.Notifications">
<method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
<arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
<arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
</interface></node>"""
node = Gio.DBusNodeInfo.new_for_xml(XML)
loop = GLib.MainLoop()
def call(conn, sender, path, iface, method, params, invocation):
    print("NOTIFY", repr(params.unpack()), flush=True)
    invocation.return_value(GLib.Variant("(u)", (1,)))
    GLib.timeout_add(100, loop.quit)
def acquired(conn, name):
    conn.register_object("/org/freedesktop/Notifications", node.interfaces[0], call, None, None)
    print("READY", flush=True)
Gio.bus_own_name(Gio.BusType.SESSION, "org.freedesktop.Notifications", 0, acquired, None, None)
GLib.timeout_add(60000, loop.quit)
loop.run()
PY
    # A zenity that is there, in place of the dialog: what it ends with, and
    # what it says on stderr.
    case $scenario in
        ubuntu-24.04-zenity-fails)
            # Failing as zenity 4 does without GTK 4: in the loader, before
            # it could show anything.
            fake_status=127
            fake_error="zenity: error while loading shared libraries: libgtk-4.so.1: cannot open shared object file" ;;
        ubuntu-24.04-zenity-closed)
            # Shown, and closed with the window's button or Esc: nothing
            # after it.
            fake_status=1 expected=0 ;;
        ubuntu-24.04-zenity-no-display)
            # GTK could not open the display, which zenity ends with 1 too.
            fake_status=1 fake_error="(zenity:42): Gtk-WARNING **: 12:00:00.000: Failed to open display" ;;
    esac
    if [ -n "$fake_status" ]; then
        mkdir -p /tmp/smoke/bin
        cat > /tmp/smoke/bin/zenity <<SH
#!/bin/sh
echo "\$*" >> /tmp/smoke/zenity-calls
printf '%s' '$fake_error' >&2
exit $fake_status
SH
        chmod +x /tmp/smoke/bin/zenity
        export PATH="/tmp/smoke/bin:$PATH"
    elif command -v zenity >/dev/null || command -v kdialog >/dev/null; then
        fail "the image has a dialog program"
    fi
    command -v gdbus >/dev/null || fail "the image has no gdbus"
    # The service ends after the first call; where none is to come, once the
    # AppImage has ended.
    dbus-run-session -- xvfb-run -a bash -c '
        python3 -W ignore /tmp/smoke/notifyd.py > /tmp/smoke/notify.txt 2>&1 &
        notifyd=$!
        for _ in $(seq 1 100); do grep -q "^READY" /tmp/smoke/notify.txt && break; sleep 0.1; done
        rc=0
        timeout 60 /smoke/Reoling.AppImage > /tmp/smoke/out.txt 2> /tmp/smoke/err.txt || rc=$?
        echo "$rc" > /tmp/smoke/rc
        sleep 1
        kill "$notifyd" 2>/dev/null || true
        wait'
    rc=$(cat /tmp/smoke/rc)
    cat /tmp/smoke/out.txt /tmp/smoke/err.txt
    echo "--- end of the output, status $rc; the notification service:"
    cat /tmp/smoke/notify.txt
    echo "--- end of the notification service's output"
    check_refusal "$rc"
    grep -q '^READY' /tmp/smoke/notify.txt || fail "the stand-in notification service did not start"
    calls=$(grep -c '^NOTIFY ' /tmp/smoke/notify.txt || true)
    [ "$calls" -eq "$expected" ] || fail "$calls notifications instead of $expected"
    if [ "$expected" -eq 1 ]; then
        for text in "'Reoling', 0, 'dialog-error', 'Reoling cannot start'" "$EXPECT_LINE" "$BUNDLED_NAME" \
                    "{'urgency': 2}"; do
            grep '^NOTIFY ' /tmp/smoke/notify.txt | grep -qF -- "$text" || fail "the notification lacks: $text"
        done
    fi
    if [ -n "$fake_status" ]; then
        grep -q -- '--error' /tmp/smoke/zenity-calls 2>/dev/null || fail "the hook did not try zenity first"
        echo "zenity was tried ($(tr '\n' ' ' < /tmp/smoke/zenity-calls | cut -c1-60)...), status $fake_status;" \
             "notifications after it: $calls"
    fi
    echo "PASS $scenario: refused, with $calls notification(s)"
}

# The system with what it needs (or the scenario's part of it): the app runs
# for 20 s, then reports its requirements.
run() {
    local log=/tmp/smoke/run.log maps=/tmp/smoke/maps.txt rc=0 job pid lines count gtk registry sinks element
    local texts=()
    fresh_home
    echo "=== $scenario: $(. /etc/os-release && echo "$PRETTY_NAME"), with GTK 4 and GStreamer"
    # Meanwhile, once the app has mapped GTK (the runtime unpacks the image
    # first), the libraries it has mapped: from a process still there a
    # second later, not one of the hook's short probes, which carry the
    # same name. xvfb-run may send the program's stderr to its stdout, so
    # the log takes both.
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
    lines=$(grep '^reoling: GTK and GStreamer core: ' "$log" || true)
    [ "$(printf '%s' "$lines" | grep -c '^')" -eq 1 ] || fail "not exactly one line names the GTK stack"
    case $lines in
        "reoling: GTK and GStreamer core: host ("*) ;;
        *) fail "expected the system's GTK stack (host)" ;;
    esac
    gtk=$(grep -m1 '/libgtk-4\.so\.1' "$maps" 2>/dev/null || true)
    echo "libgtk-4 mapped from: ${gtk:-nowhere}"
    case $gtk in
        */appimage_extracted_*|"") fail "libgtk-4 is not the system's" ;;
    esac
    # The app's line at start for what this system lacks: there exactly when
    # something is missing.
    count=$(grep -c '^reoling: missing on this system: ' "$log" || true)
    if [ "$EXPECT_STATUS" = 0 ]; then
        [ "$count" -eq 0 ] || fail "the app says something is missing on a system that has everything"
    else
        [ "$count" -eq 1 ] || fail "the app printed $count lines 'reoling: missing on this system: ...' instead of one"
    fi
    # The registry of its own, whose one GTK 4 sink is the AppImage's, ahead
    # of the distribution's where it has one; the elements the scenario has.
    registry=$(ls "$XDG_CACHE_HOME"/reoling/gstreamer-registry-system-*.bin 2>/dev/null || true)
    case $registry in
        *$'\n'*|"") fail "not exactly one registry gstreamer-registry-system-*.bin: ${registry:-none}" ;;
    esac
    sinks=$(grep -aoE '/[^[:cntrl:][:space:]]*/libgstgtk4\.so' "$registry" | sed 's|^/*|/|' | sort -u)
    case $sinks in
        *$'\n'*|"") fail "the registry names not exactly one GTK 4 sink: ${sinks:-none}" ;;
        */appimage_extracted_*/usr/lib/gstreamer-1.0/libgstgtk4.so) echo "GTK 4 sink in the registry: $sinks" ;;
        *) fail "the registry's GTK 4 sink is not the AppImage's: $sinks" ;;
    esac
    for element in $ELEMENTS; do
        grep -qa "$element" "$registry" || fail "$element is not in the registry ($registry)"
    done
    echo "registry $(basename "$registry"): $ELEMENTS"
    # The report, with the scenario's status, gaps and texts, and the plugins
    # the system's, as the hook told the app; where something is missing, it
    # points at the embed AppImage by the release's own file name.
    if [ -n "${EXPECT_TEXTS:-}" ]; then
        IFS=';' read -r -a texts <<< "$EXPECT_TEXTS"
    fi
    texts+=("Plugins: the system's, under the AppImage")
    [ "$EXPECT_STATUS" = 0 ] || texts+=("Or use $BUNDLED_NAME")
    bash /smoke/assert-requirements.sh /smoke/Reoling.AppImage "$EXPECT_STATUS" "$EXPECT_GAPS" \
        '*/appimage_extracted_*/usr/lib/gstreamer-1.0/libgstgtk4.so' "${texts[@]}" \
        || fail "the requirements report (above)"
    echo "PASS $scenario: started, requirements as expected"
}

if [ "${1:-}" = --inside ]; then
    scenario=$3
    case $2 in
        refuse) refuse ;;
        notify) notify ;;
        run) run ;;
        *) fail "no such phase: $2" ;;
    esac
    exit 0
fi

# --- On the host ---------------------------------------------------------------------
if [ $# -lt 1 ]; then
    echo "usage: $0 <AppImage> [<scenario>...]   scenarios: $(xargs <<< "$all_scenarios")" >&2
    exit 2
fi
appimage=$(realpath "$1")
shift
[ -f "$appimage" ] || { echo "$appimage is not a file" >&2; exit 1; }
chmod +x "$appimage" 2>/dev/null || true
self=$(realpath "$0")
here=$(dirname "$self")
. "$here/appimage-common.sh"
read -r variant version <<< "$(appimage_variant_of "$appimage" || true)"
if [ "${variant:-}" != app-only ]; then
    echo "$appimage is not named $(appimage_file_name app-only '<version>')" >&2
    exit 1
fi
bundled_name=$(appimage_file_name bundled "$version")
sink_version=${SINK_VERSION:-$(sed -n 's/^default_version=//p' "$here/build-gst-gtk4-plugin.sh")}
[ -n "$sink_version" ] || { echo "no gtk4paintablesink version (SINK_VERSION)" >&2; exit 1; }
prefix=${SMOKE_IMAGE_PREFIX:-reoling-smoke-app-only}
# shellcheck disable=SC2206
scenarios=(${*:-$all_scenarios})

# Per package family: the install command the hook prints (scripts/
# appimage-app-only-hook.sh), the same without questions, without recommends
# or weak dependencies, what the bare image adds to its base (ps, and awk
# where the base has none), and what a run needs besides (a virtual display,
# xauth, a session bus, pgrep, fonts, Mesa).
family_of() {
    case $1 in
        ubuntu-*|debian-*) echo apt ;;
        fedora-*) echo dnf ;;
        opensuse-*) echo zypper ;;
        archlinux) echo pacman ;;
    esac
}
expected_line() {
    case $1 in
        apt) echo "sudo apt install libgtk-4-1 libgstreamer-gl1.0-0 gstreamer1.0-plugins-base" \
                  "gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav" ;;
        dnf) echo "sudo dnf install gtk4 gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free" \
                  "gstreamer1-plugin-libav gstreamer1-plugin-openh264 openh264" ;;
        zypper) echo "sudo zypper install libgtk-4-1 libgstgl-1_0-0 gstreamer-plugins-base gstreamer-plugins-good" \
                     "gstreamer-plugins-bad gstreamer-plugins-libav" ;;
        pacman) echo "sudo pacman -S --needed gtk4 gst-plugins-base gst-plugins-good gst-plugins-bad-libs gst-libav" \
                     "gst-plugin-va" ;;
    esac
}
installer() {
    case $1 in
        apt) echo "apt-get update -qq && apt-get install -y -qq --no-install-recommends" ;;
        dnf) echo "dnf install -y -q --setopt=install_weak_deps=False" ;;
        zypper) echo "zypper --non-interactive install --no-recommends" ;;
        pacman) echo "pacman -Syu --noconfirm --needed" ;;
    esac
}
bare_packages() {
    case $1 in
        apt) echo "procps" ;;
        dnf) echo "procps-ng gawk" ;;
        zypper) echo "procps gawk" ;;
        pacman) echo "procps-ng gawk" ;;
    esac
}
harness() {
    case $1 in
        apt) echo "xvfb xauth dbus procps fonts-dejavu-core libgl1 libegl1 libgles2" ;;
        dnf) echo "xorg-x11-server-Xvfb xorg-x11-xauth dbus-daemon procps-ng mesa-dri-drivers mesa-libEGL" \
                  "libglvnd-gles dejavu-sans-fonts" ;;
        zypper) echo "xvfb-run xorg-x11-server-Xvfb xauth dbus-1 dbus-1-daemon procps Mesa-dri Mesa-libEGL1" \
                     "dejavu-fonts" ;;
        pacman) echo "xorg-server-xvfb xorg-xauth dbus procps-ng mesa ttf-dejavu" ;;
    esac
}

# Each scenario: its base image, how it is set up, what it adds after the
# install command (the desktop's sound plugin where the command leaves it to
# the desktop, the distribution's GTK 4 sink where it has one), the elements
# its registry must have, and the requirements report it must give: status,
# missing features, texts (';' between them; "Or use <the embed AppImage>"
# is added wherever something is missing). The reports are the app's
# (src/ui/requirements.rs); "measured" rows rest on these containers' plugin
# lists (gst-inspect-1.0 in the hook's environment), the others on the
# packages' contents. A one-image scenario may also give the package
# manager options of its own, and the RPM packages that must and must not be
# installed in its image.
full="appsrc queue decodebin videoconvert h264parse h265parse avdec_h264 avdec_h265 avdec_aac gtk4paintablesink"
openh264="appsrc queue decodebin videoconvert h264parse h265parse openh264dec avdec_aac gtk4paintablesink"
scenario_setup() {
    expect_texts="" installer_options="" rpms_installed="" rpms_absent=""
    case $1 in
        ubuntu-24.04-bare)
            base=ubuntu:24.04 setup=two-phase extras=gstreamer1.0-alsa elements=$full status=0 gaps=none ;;
        ubuntu-24.04-desktop)
            # The Ubuntu 24.04 desktop's GStreamer (its manifest): no -bad
            # (h264parse, h265parse, faad), no libav (avdec_*).
            base=ubuntu:24.04 setup=one-image elements="appsrc queue decodebin videoconvert gtk4paintablesink"
            extras="libgtk-4-1 libgstreamer-gl1.0-0 gstreamer1.0-plugins-base gstreamer1.0-plugins-good"
            extras+=" gstreamer1.0-gl gstreamer1.0-pipewire gstreamer1.0-alsa"
            status=1 gaps=h264,h265,sound expect_texts="sudo apt install gstreamer1.0-plugins-bad gstreamer1.0-libav" ;;
        ubuntu-24.04-deb-deps)
            if [ -z "${DEB:-}" ] || [ ! -f "$DEB" ]; then
                echo "ubuntu-24.04-deb-deps needs DEB=<the .deb>" >&2
                return 1
            fi
            # The .deb's Depends, the first of each alternative, versions
            # dropped.
            base=ubuntu:24.04 setup=one-image elements=$full status=0 gaps=none
            extras=$(dpkg-deb -f "$DEB" Depends | tr ',' '\n' | sed -e 's/|.*//' -e 's/(.*)//' -e 's/[[:space:]]//g' \
                | sed '/^$/d' | tr '\n' ' ') ;;
        ubuntu-24.04-notify|ubuntu-24.04-zenity-*)
            base=ubuntu:24.04 setup=notify extras="xvfb xauth dbus procps libglib2.0-bin python3-gi" elements=""
            status=127 gaps="" ;;
        debian-13)
            base=debian:13 setup=two-phase extras="gstreamer1.0-alsa gstreamer1.0-gtk4" elements=$full
            status=0 gaps=none ;;
        ubuntu-26.04)
            base=ubuntu:26.04 setup=two-phase extras="gstreamer1.0-alsa gstreamer1.0-gtk4" elements=$full
            status=0 gaps=none ;;
        fedora-44)
            # Measured: openh264dec (with Cisco's openh264, not the stub),
            # no avdec_h264 or avdec_h265: FFmpeg without H.264 and H.265
            # until RPM Fusion's.
            base=fedora:44 setup=two-phase extras=gstreamer1-plugin-gtk4 elements=$openh264
            status=1 gaps=h265 expect_texts="dnf swap ffmpeg-free ffmpeg --allowerasing" ;;
        fedora-44-noopenh264)
            # A stock Fedora desktop: the install command's packages with
            # noopenh264 in place of openh264 (fedora-cisco-openh264 left
            # out). openh264dec registers but cannot start, so the app counts
            # no H.264 decoder and names the real library and the stand-in.
            base=fedora:44 setup=one-image elements=$openh264
            installer_options="--disablerepo='fedora-cisco-openh264*'"
            extras="gtk4 gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free"
            extras+=" gstreamer1-plugin-libav gstreamer1-plugin-openh264 noopenh264 gstreamer1-plugin-gtk4"
            rpms_installed=noopenh264 rpms_absent=openh264 status=1 gaps=h264,h265
            expect_texts="gstreamer1-plugin-openh264 openh264;noopenh264;dnf swap ffmpeg-free ffmpeg --allowerasing" ;;
        opensuse-tumbleweed)
            # Measured: openh264dec, no avdec_h264 or avdec_h265 (Packman
            # has them). Which openh264 library is installed is printed; the
            # stub's decoder would make H.264 a gap too, where the app tells
            # it apart.
            base=opensuse/tumbleweed setup=two-phase extras="" elements=$openh264
            status=1 gaps=h265 expect_texts="opi codecs" ;;
        archlinux)
            base=archlinux:latest setup=two-phase extras=gst-plugin-gtk4 elements=$full status=0 gaps=none ;;
        *) echo "no such scenario: $1 (scenarios: $(xargs <<< "$all_scenarios"))" >&2; return 1 ;;
    esac
}

# Builds the image $1 from the Dockerfile on stdin, once more when that
# fails (a mirror of a rolling distribution, say).
build_image() {
    local dockerfile
    dockerfile=$(cat)
    printf '%s\n' "$dockerfile" | docker build -t "$1" - && return 0
    echo "building $1 failed; once more in 20 s" >&2
    sleep 20
    printf '%s\n' "$dockerfile" | docker build -t "$1" - && return 0
    echo "::error::$scenario: the image $1 did not build, twice; run this scenario again" >&2
    return 1
}
# Runs the phase $2 of the scenario in the image $1, without a network.
in_container() {
    docker run --rm --network none \
        -e EXPECT_LINE="$line" -e BUNDLED_NAME="$bundled_name" -e SINK_VERSION="$sink_version" \
        -e EXPECT_STATUS="$status" -e EXPECT_GAPS="$gaps" -e EXPECT_TEXTS="$expect_texts" -e ELEMENTS="$elements" \
        -v "$appimage:/smoke/Reoling.AppImage:ro" -v "$self:/smoke/smoke-app-only-in-docker.sh:ro" \
        -v "$here/assert-requirements.sh:/smoke/assert-requirements.sh:ro" \
        "$1" bash /smoke/smoke-app-only-in-docker.sh --inside "$2" "$scenario"
}

for scenario in "${scenarios[@]}"; do
    echo
    echo "=== Container smoke test of the app-only AppImage: $scenario"
    scenario_setup "$scenario"
    family=$(family_of "$scenario")
    line=$(expected_line "$family")
    apt_env=""
    [ "$family" != apt ] || apt_env='ENV DEBIAN_FRONTEND=noninteractive'
    case $setup in
        two-phase)
            build_image "$prefix:$scenario-bare" <<EOF
FROM $base
$apt_env
RUN $(installer "$family") $(bare_packages "$family")
EOF
            in_container "$prefix:$scenario-bare" refuse
            # The command as printed, without questions; then what the run
            # needs and the scenario adds.
            packages=${line#sudo apt install }
            packages=${packages#sudo dnf install }
            packages=${packages#sudo zypper install }
            packages=${packages#sudo pacman -S --needed }
            [ "$packages" != "$line" ] || { echo "cannot read the install line: $line" >&2; exit 1; }
            build_image "$prefix:$scenario-installed" <<EOF
FROM $prefix:$scenario-bare
RUN $(installer "$family") $packages
RUN $(installer "$family") $(harness "$family") $extras
EOF
            if [ "$family" = dnf ] || [ "$family" = zypper ]; then
                echo "OpenH264 packages: $(docker run --rm --network none "$prefix:$scenario-installed" \
                    rpm -qa '*openh264*' | sort | xargs)"
            fi
            in_container "$prefix:$scenario-installed" run
            ;;
        one-image)
            build_image "$prefix:$scenario" <<EOF
FROM $base
$apt_env
RUN $(installer "$family") ${installer_options:+$installer_options }$extras $(harness "$family")
EOF
            if [ "$family" = dnf ] || [ "$family" = zypper ]; then
                echo "OpenH264 packages: $(docker run --rm --network none "$prefix:$scenario" \
                    rpm -qa '*openh264*' | sort | xargs)"
            fi
            for package in $rpms_installed; do
                docker run --rm --network none "$prefix:$scenario" rpm -q "$package" \
                    || fail "$package is not installed in the image"
            done
            for package in $rpms_absent; do
                if docker run --rm --network none "$prefix:$scenario" rpm -q "$package"; then
                    fail "$package is installed in the image"
                fi
            done
            in_container "$prefix:$scenario" run
            ;;
        notify)
            build_image "$prefix:ubuntu-24.04-notify" <<EOF
FROM $base
$apt_env
RUN $(installer "$family") $extras
EOF
            in_container "$prefix:ubuntu-24.04-notify" notify
            ;;
    esac
done
echo
echo "container smoke tests of the app-only AppImage passed: ${scenarios[*]}"
