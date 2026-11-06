#!/usr/bin/env bash
# Rehearses the release workflow (.github/workflows/release.yml) in a plain
# Ubuntu 24.04 container before anything is pushed: the same packages
# (scripts/noble-packages.txt), rustup, build, tests, cargo deb, the two
# AppImages (named by scripts/appimage-common.sh) and the smoke tests (the
# embed AppImage first, on the container's GTK and GStreamer and then on its
# own, then the system AppImage on the container's, then the .deb, as there),
# each with its requirements report (scripts/assert-requirements.sh). Leaves
# the packages, their SHA256SUMS, the .deb's control file and the list of
# installed packages in the output directory.
#
#   mkdir -p /some/output
#   docker run --rm -v "$PWD":/src:ro -v /some/output:/out ubuntu:24.04 \
#       bash /src/scripts/validate-in-docker.sh
#
# The workflow then starts the AppImages in containers for other systems
# (scripts/smoke-appimage-in-docker.sh for the embed AppImage,
# scripts/smoke-app-only-in-docker.sh for the system AppImage) and the .deb
# beside a distribution's own GTK 4 sink (scripts/smoke-deb-in-docker.sh),
# which cannot be done from inside a container. Run on the host with --host,
# this script does both:
#
#   scripts/validate-in-docker.sh --host /some/output
#       the rehearsal above in a fresh ubuntu:24.04 container, then the
#       container smoke tests on the AppImages and the .deb it built
#   scripts/validate-in-docker.sh --host /some/output <AppImage>...
#       the smoke tests of one or both AppImages built elsewhere: in an
#       Ubuntu 24.04 container with the workflow's packages (the embed one
#       on both GTK stacks, as on the runner; no build, no .deb), then in
#       the other containers
#
# Inside the container, --appimage <file> (what the second form passes, once
# per AppImage) skips the build and the .deb.
#
# Environment:
#   SKIP_TESTS=1   leave out cargo test (docker run -e, or set for --host)
#   IMAGE=<image>  the Ubuntu 24.04 image --host starts (default ubuntu:24.04)
#   ALL_DISTROS=1  --host runs the system AppImage on every distribution of
#                  scripts/smoke-app-only-in-docker.sh, not only Ubuntu 24.04
set -euo pipefail

# --- On the host ---------------------------------------------------------------------
if [ "${1:-}" = --host ]; then
    if [ $# -lt 2 ] || [ $# -gt 4 ]; then
        echo "usage: $0 --host <output directory> [<AppImage>...]" >&2
        exit 2
    fi
    repo=$(cd "$(dirname "$0")/.." && pwd)
    . "$repo/scripts/appimage-common.sh"
    mkdir -p "$2"
    out=$(realpath "$2")
    shift 2
    mounts=()
    inner=()
    bundled=""
    app_only=""
    for f in "$@"; do
        f=$(realpath "$f")
        read -r variant _ <<< "$(appimage_variant_of "$f" || true)"
        case ${variant:-} in
            bundled) bundled=$f ;;
            app-only) app_only=$f ;;
            *) echo "$f is named neither $(appimage_file_name bundled '<version>') nor" \
                    "$(appimage_file_name app-only '<version>')" >&2; exit 2 ;;
        esac
        # Mounted read-only: the container cannot make it executable.
        [ -x "$f" ] || chmod +x "$f"
        mounts+=(-v "$f:/in/${f##*/}:ro")
        inner+=(--appimage "/in/${f##*/}")
    done
    docker run --rm -e SKIP_TESTS="${SKIP_TESTS:-}" -v "$repo:/src:ro" -v "$out:/out" "${mounts[@]}" \
        "${IMAGE:-ubuntu:24.04}" bash /src/scripts/validate-in-docker.sh "${inner[@]}"
    if [ $# -eq 0 ]; then
        version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)
        bundled="$out/$(appimage_file_name bundled "$version")"
        app_only="$out/$(appimage_file_name app-only "$version")"
    fi
    # The .deb, where this run built it.
    deb=""
    if [ $# -eq 0 ]; then
        deb=$(ls "$out"/reoling_*.deb 2>/dev/null | head -n1 || true)
    fi
    if [ -n "$bundled" ]; then
        "$repo/scripts/smoke-appimage-in-docker.sh" "$bundled"
    fi
    if [ -n "$app_only" ]; then
        scenarios=()
        if [ -z "${ALL_DISTROS:-}" ]; then
            scenarios=(ubuntu-24.04-bare ubuntu-24.04-desktop ubuntu-24.04-notify ubuntu-24.04-zenity-fails
                       ubuntu-24.04-zenity-closed ubuntu-24.04-zenity-no-display)
        fi
        # The .deb's dependencies.
        if [ -n "$deb" ]; then
            [ ${#scenarios[@]} -eq 0 ] || scenarios+=(ubuntu-24.04-deb-deps)
            DEB=$deb "$repo/scripts/smoke-app-only-in-docker.sh" "$app_only" "${scenarios[@]}"
        elif [ ${#scenarios[@]} -eq 0 ]; then
            echo "note: no .deb built here, so ubuntu-24.04-deb-deps is left out"
            "$repo/scripts/smoke-app-only-in-docker.sh" "$app_only" debian-13 ubuntu-26.04 fedora-44 \
                fedora-44-noopenh264 opensuse-tumbleweed archlinux ubuntu-24.04-bare ubuntu-24.04-desktop \
                ubuntu-24.04-notify ubuntu-24.04-zenity-fails ubuntu-24.04-zenity-closed ubuntu-24.04-zenity-no-display
        else
            "$repo/scripts/smoke-app-only-in-docker.sh" "$app_only" "${scenarios[@]}"
        fi
    fi
    if [ -n "$deb" ]; then
        "$repo/scripts/smoke-deb-in-docker.sh" "$deb"
    fi
    echo
    echo "validated ${bundled:+$bundled }${app_only:+$app_only}${deb:+ $deb}: the Ubuntu 24.04 smoke tests and the" \
         "container smoke tests passed"
    exit 0
fi

# --- In the container --------------------------------------------------------------
appimages_in=()
while [ "${1:-}" = --appimage ]; do
    appimages_in+=("$2")
    shift 2
done
src=${1:-/src}
out=${2:-/out}
. "$src/scripts/appimage-common.sh"
export DEBIAN_FRONTEND=noninteractive APPIMAGE_EXTRACT_AND_RUN=1 LC_ALL=C.UTF-8
start=$(date +%s)
step() { echo; echo "=== $* [$(( $(date +%s) - start )) s]"; }

step "Ubuntu $(. /etc/os-release && echo "$VERSION_ID"): installing the packages"
apt-get update -qq
# shellcheck disable=SC2046
apt-get install -y -qq --no-install-recommends $(grep -v '^#' "$src/scripts/noble-packages.txt")

if [ ${#appimages_in[@]} -eq 0 ]; then
    step "Rust"
    if ! command -v cargo >/dev/null; then
        curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
        sh /tmp/rustup-init.sh -y --profile minimal --default-toolchain stable >/dev/null
        rm /tmp/rustup-init.sh
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    cargo --version
    rustc --version

    step "Sources (without target/ and .git/)"
    rm -rf /tmp/src
    mkdir -p /tmp/src
    tar -C "$src" --exclude=./target --exclude=./.git -cf - . | tar -C /tmp/src -xf -
    cd /tmp/src

    step "cargo build --release"
    cargo build --release --locked

    if [ -z "${SKIP_TESTS:-}" ]; then
        step "cargo test --release"
        xvfb-run -a cargo test --release --locked
        xvfb-run -a cargo test --release --locked --bin reoling ui::zoom_picture::tests::widget_renders_and_resets -- --ignored --exact
    fi

    step "GTK 4 paintable sink plugin"
    scripts/build-gst-gtk4-plugin.sh target/gst-gtk4

    step "cargo deb"
    cargo install cargo-deb --locked --version 3.8.0 >/dev/null
    cargo deb
    mkdir -p "$out"
    rm -f "$out"/reoling_*.deb
    cp target/debian/reoling_*.deb "$out/"

    step "AppImages"
    scripts/fetch-appimage-tools.sh /tmp/appimage-tools
    export APPIMAGE_TOOLS=/tmp/appimage-tools
    version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
    rm -f "$out/$(appimage_file_name bundled "$version")" "$out/$(appimage_file_name app-only "$version")"
    scripts/build-appimage.sh target/release/reoling "$out"
    scripts/build-appimage-app-only.sh target/release/reoling "$out"
    bundled="$out/$(appimage_file_name bundled "$version")"
    app_only="$out/$(appimage_file_name app-only "$version")"
else
    bundled=""
    app_only=""
    for f in "${appimages_in[@]}"; do
        f=$(realpath "$f")
        read -r variant version <<< "$(appimage_variant_of "$f" || true)"
        case ${variant:-} in
            bundled) bundled=$f ;;
            app-only) app_only=$f ;;
            *) echo "$f is not named $(appimage_file_name bundled '<version>') or" \
                    "$(appimage_file_name app-only '<version>')" >&2; exit 1 ;;
        esac
    done
fi

# The smoke tests of the release workflow. The app runs for 20 s on a
# virtual display with a session bus and an empty configuration (no devices
# to reach): it must still be running when the time is up (124 from
# timeout), and nothing it loads may have failed to (a module, a pixbuf
# loader, a GStreamer plugin, a library, a symbol, a decoder), GTK, GLib and
# GStreamer must not have printed a critical warning (a bug or a broken
# setup), and on this system, which has every package the app uses, the app
# must not say that anything is missing. Ubuntu's xvfb-run (a 24-bit screen
# by default) sends the program's stderr to its stdout, so the log takes
# both. An AppImage names in one line the GTK and GStreamer core it chose,
# the system's ("host") or its own ("bundled"); $2 is the one it must name.
run_for_20s() {
    local rc=0 stack
    dbus-run-session -- xvfb-run -a timeout 20 "$1" > /tmp/smoke.log 2>&1 || rc=$?
    cat /tmp/smoke.log
    [ "$rc" -eq 124 ] || { echo "$1 ended early with status $rc" >&2; exit 1; }
    if grep -E 'Unable to load image-loading module|Failed to load module|Failed to load plugin|cannot open shared object file|symbol lookup error|undefined symbol|Missing decoder|CRITICAL' /tmp/smoke.log \
        || grep '^reoling: missing on this system: ' /tmp/smoke.log; then
        echo "$1 printed a loading failure, a critical warning or a missing part (above)" >&2
        exit 1
    fi
    if [ -n "${2:-}" ]; then
        stack=$(grep '^reoling: GTK and GStreamer core: ' /tmp/smoke.log || true)
        case $stack in
            *$'\n'*|"") echo "$1 did not name its GTK stack in exactly one line" >&2; exit 1 ;;
            "reoling: GTK and GStreamer core: $2 ("*) ;;
            *) echo "$1 should have taken the $2 GTK stack" >&2; exit 1 ;;
        esac
    fi
    echo "$1 ran for 20 s"
    # No build installs a thumbnailer.
    [ ! -e "$XDG_DATA_HOME/thumbnailers" ] || { echo "$1 wrote a thumbnailer entry; Reoling installs none" >&2; exit 1; }
}
# Each AppImage runs with XDG directories of its own ($1), as on a machine
# that has only that AppImage.
xdg_dirs() {
    export XDG_CONFIG_HOME=/tmp/smoke-$1/config XDG_CACHE_HOME=/tmp/smoke-$1/cache XDG_DATA_HOME=/tmp/smoke-$1/data
    rm -rf "/tmp/smoke-$1"
}
# The requirements report of the program $1 on this system, which has every
# package the app uses: nothing missing, gtk4paintablesink from the file $2
# (a pattern), and $3 said as well (where the plugins come from, as the
# AppImage's hook told the app).
requirements() {
    "$src/scripts/assert-requirements.sh" "$1" 0 none "$2" ${3:+"$3"}
}
appimage_sink='*/@(.mount_*|appimage_extracted_*)/usr/lib/gstreamer-1.0/libgstgtk4.so'
# At its start an AppImage, before the .deb is installed, as on a machine
# that has only the AppImage, gives its own file ($1) an icon: it copies its
# icon into the user's icon theme, writes its file's thumbnail-cache entries
# and a per-user desktop entry that starts the file (the file's custom icon
# needs gvfs, which the container lacks). Checked after its first run.
integration() {
    local size png key line
    for size in 16 22 24 32 48 64 128 256 512; do
        png="$XDG_DATA_HOME/icons/hicolor/${size}x${size}/apps/de.dersobi.reoling.png"
        test -s "$png" || { echo "$1 did not copy its $size px icon into the user icon theme ($png)" >&2; exit 1; }
        cmp "$png" "$src/data/icons/hicolor/${size}x${size}/apps/de.dersobi.reoling.png"
    done
    key=$(printf 'file://%s' "$1" | md5sum | cut -c1-32)
    for size in large x-large; do
        png="$XDG_CACHE_HOME/thumbnails/$size/$key.png"
        test -s "$png" || { echo "$1 wrote no $size thumbnail of itself ($png)" >&2; exit 1; }
        file "$png"
    done
    entry="$XDG_DATA_HOME/applications/de.dersobi.reoling.desktop"
    test -s "$entry" || { echo "$1 wrote no desktop entry ($entry)" >&2; exit 1; }
    cat "$entry"
    for line in "Exec=\"$1\"" "TryExec=$1" "Icon=de.dersobi.reoling" "X-Reoling-Generated=true"; do
        grep -qFx "$line" "$entry" || { echo "$1's desktop entry lacks $line" >&2; exit 1; }
    done
    if grep -q '^MimeType=' "$entry"; then
        echo "$1's desktop entry claims MIME types" >&2
        exit 1
    fi
    desktop-file-validate "$entry"
}

# The embed AppImage runs on the container's GTK 4.14 and GStreamer 1.24 (as
# new as its own), then on its own (REOLING_GTK_STACK=bundled); its
# --version is read without xvfb-run, which would add the stack line from
# stderr.
if [ -n "$bundled" ]; then
    step "Smoke test: the embed AppImage"
    xdg_dirs bundled
    [ -x "$bundled" ] || chmod +x "$bundled"
    test "$("$bundled" --version 2>/dev/null)" = "reoling $version"
    run_for_20s "$bundled" host
    integration "$bundled"
    requirements "$bundled" "$appimage_sink" "Plugins: bundled with the AppImage"
    REOLING_GTK_STACK=bundled run_for_20s "$bundled" bundled
    REOLING_GTK_STACK=bundled requirements "$bundled" "$appimage_sink" "Plugins: bundled with the AppImage"
fi
# The system AppImage runs on the container's GTK, GStreamer and plugins
# with its own sink, from a registry of its own that names that sink, from
# the AppImage, and no other.
if [ -n "$app_only" ]; then
    step "Smoke test: the system AppImage"
    xdg_dirs app-only
    [ -x "$app_only" ] || chmod +x "$app_only"
    test "$("$app_only" --version 2>/dev/null)" = "reoling $version"
    run_for_20s "$app_only" host
    integration "$app_only"
    registry=$(ls "$XDG_CACHE_HOME"/reoling/gstreamer-registry-system-*.bin)
    sinks=$(grep -aoE '/[^[:cntrl:][:space:]]*/libgstgtk4\.so' "$registry" | sed 's|^/*|/|' | sort -u)
    case $sinks in
        *$'\n'*|"") echo "the system AppImage's registry names not exactly one GTK 4 sink: $sinks" >&2; exit 1 ;;
        */.mount_*/usr/lib/gstreamer-1.0/libgstgtk4.so|*/appimage_extracted_*/usr/lib/gstreamer-1.0/libgstgtk4.so)
            echo "GTK 4 sink: $sinks" ;;
        *) echo "the system AppImage's registry's GTK 4 sink is not the AppImage's: $sinks" >&2; exit 1 ;;
    esac
    for element in appsrc queue decodebin videoconvert h264parse h265parse avdec_h264 avdec_h265 avdec_aac \
                   gtk4paintablesink; do
        grep -qa "$element" "$registry" || { echo "$element is not in the system AppImage's registry" >&2; exit 1; }
    done
    requirements "$app_only" "$appimage_sink" "Plugins: the system's, under the AppImage"
fi

if [ ${#appimages_in[@]} -ne 0 ]; then
    step "Done (the AppImages only; the container smoke tests run from the host)"
    exit 0
fi

# Then the .deb, installed; it does none of the above, and its start
# removes the per-user desktop entry the AppImage started last wrote, which
# would hide its own.
step "Smoke test: the .deb"
apt-get install -y -qq "$out"/reoling_*.deb
test "$(xvfb-run -a reoling --version)" = "reoling $version"
dpkg -L reoling
run_for_20s reoling
[ ! -e "$entry" ] || { echo "the .deb left the AppImage's per-user desktop entry in place ($entry)" >&2; exit 1; }
requirements reoling /usr/lib/reoling/gstreamer-1.0/libgstgtk4.so

step "Record"
dpkg-query -W -f='${Package} ${Version}\n' > "$out/noble-installed-packages.txt"
dpkg-deb -I "$out"/reoling_*.deb > "$out/deb-control.txt"
dpkg-deb -c "$out"/reoling_*.deb > "$out/deb-contents.txt"
(cd "$out" && sha256sum "${bundled##*/}" "${app_only##*/}" reoling_*.deb) | tee "$out/SHA256SUMS"
{
    echo "built $(date -u +%FT%TZ) on Ubuntu $(. /etc/os-release && echo "$VERSION_ID"), $(rustc --version)"
    echo "glibc $(ldd --version | head -n1 | awk '{print $NF}'), GTK $(pkg-config --modversion gtk4), GStreamer $(pkg-config --modversion gstreamer-1.0)"
    ls -l "$out"/reoling_*.deb "$bundled" "$app_only"
} | tee "$out/summary.txt"
step "Done (the container smoke tests run from the host: scripts/validate-in-docker.sh --host)"
