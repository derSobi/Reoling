#!/usr/bin/env bash
# Asks a Reoling package for its requirements report and checks it against
# what the system it runs on must give, for the smoke tests of the release
# workflow, scripts/validate-in-docker.sh, scripts/smoke-appimage-in-docker.sh
# and scripts/smoke-app-only-in-docker.sh. `reoling --check-requirements`
# prints the report and exits before GTK opens a display (none is needed,
# no device is contacted), with the status
#   0  nothing is missing,
#   1  H.264 or H.265 video, or the picture itself, cannot be shown,
#   2  only another feature (sound, talk, recording, downloads) misses
#      something;
# its "Display:" line names the gtk4paintablesink plugin in use, as
# "Display: gtk4paintablesink <version> (<file>)", each feature that misses
# something has a line "- <feature>: <what it needs>" (the feature as
# src/ui/requirements.rs titles it: Video, H.264 video, H.265 video, Sound,
# Talk, Recording, Downloads), and a complete system says "Missing:
# nothing". The status alone proves nothing: an app without
# the option, or one that fails before its report, also ends with 1. So the
# Display line must be there, name the expected plugin, and the missing
# features must be exactly the expected ones.
#
#   scripts/assert-requirements.sh <program> <status> <gaps> <sink> [<text>...] [--absent <text>...]
#
#   program  the AppImage, or reoling (found in PATH) for the .deb
#   status   the status it must end with: 0, 1 or 2
#   gaps     the features that must be reported missing, and no other: none,
#            or a comma-separated list of picture, h264, h265, sound, talk,
#            recording and downloads
#   sink     the file the Display line's gtk4paintablesink must come from, a
#            bash pattern (extglob: @(a|b) for either)
#   text     what the report must also say, each a fixed string (an install
#            command, the embed AppImage's name)
#   --absent what the report must not say, each text after it a fixed string
#            (the package of a plugin the system has loaded, which installing
#            again would not change)
#
# Environment:
#   SINK_VERSION  the gtk4paintablesink version the Display line must name;
#                 default: the one scripts/build-gst-gtk4-plugin.sh builds
set -euo pipefail
shopt -s extglob
export LC_ALL=C.UTF-8

if [ $# -lt 4 ]; then
    echo "usage: $0 <program> <status> <gaps> <sink> [<text>...] [--absent <text>...]" >&2
    exit 2
fi
program=$1 status=$2 gaps=$3 sink=$4
shift 4
# The texts to find, and after --absent those not to.
texts=()
absent=()
while [ $# -gt 0 ] && [ "$1" != --absent ]; do
    texts+=("$1")
    shift
done
if [ $# -gt 0 ]; then
    shift
    absent=("$@")
fi
fail() { echo "FAIL requirements of $program: $*" >&2; exit 1; }

if [ -z "${SINK_VERSION:-}" ]; then
    SINK_VERSION=$(sed -n 's/^default_version=//p' "$(dirname "$(realpath "$0")")/build-gst-gtk4-plugin.sh" \
        2>/dev/null || true)
    [ -n "$SINK_VERSION" ] || fail "no SINK_VERSION, and no scripts/build-gst-gtk4-plugin.sh beside this script"
fi

# The expectation itself must be one the statuses allow.
expected=()
consistent=0
if [ "$gaps" != none ]; then
    IFS=, read -r -a expected <<< "$gaps"
    for gap in "${expected[@]}"; do
        case $gap in
            picture|h264|h265) consistent=1 ;;
            sound|talk|recording|downloads) [ "$consistent" = 1 ] || consistent=2 ;;
            *) fail "no feature '$gap' (picture, h264, h265, sound, talk, recording, downloads)" ;;
        esac
    done
fi
[ "$status" = "$consistent" ] || fail "the expected gaps '$gaps' make status $consistent, not $status"
expected_list=$(printf '%s\n' "${expected[@]}" | sed '/^$/d' | sort | paste -sd, -)

report=$(mktemp)
errors=$(mktemp)
trap 'rm -f "$report" "$errors"' EXIT
rc=0
timeout 120 env -u DISPLAY -u WAYLAND_DISPLAY "$program" --check-requirements > "$report" 2> "$errors" || rc=$?
echo "--- $program --check-requirements: status $rc"
cat "$report"
sed 's/^/stderr: /' "$errors"
echo "--- end of the report"
[ "$rc" != 124 ] || fail "no report within 120 s"

display=$(grep '^Display: ' "$report" || true)
case $display in
    "") fail "no 'Display:' line: the app wrote no requirements report (status $rc; an app without the option?)" ;;
    *$'\n'*) fail "more than one 'Display:' line" ;;
esac
rest=${display#Display: gtk4paintablesink }
version=${rest%% (*}
file=${rest#* (}
file=${file%)}
if [ "$rest" = "$display" ] || [ "$version" = "$rest" ] || [ "$file" = "$rest" ]; then
    fail "the Display line is not 'Display: gtk4paintablesink <version> (<file>)': $display"
fi
case $version in
    "$SINK_VERSION"|"$SINK_VERSION"-*) ;;
    *) fail "gtk4paintablesink $version is in use, not $SINK_VERSION" ;;
esac
# shellcheck disable=SC2053 # $sink is a pattern
[[ $file == $sink ]] || fail "gtk4paintablesink comes from $file, not from $sink"

[ "$rc" = "$status" ] || fail "status $rc, not $status"

# The features reported missing, one line each, by the words of their names.
found=()
while IFS= read -r line; do
    label=${line#- }
    label=${label%%:*}
    case ${label,,} in
        *h.264*|*h264*) found+=(h264) ;;
        *h.265*|*h265*) found+=(h265) ;;
        *talk*) found+=(talk) ;;
        *sound*) found+=(sound) ;;
        *picture*|*video*) found+=(picture) ;;
        *record*) found+=(recording) ;;
        *download*) found+=(downloads) ;;
        *) fail "a missing feature this check does not know: $line" ;;
    esac
done < <(grep -E '^- [^:]+:' "$report" || true)
found_list=$(printf '%s\n' "${found[@]}" | sed '/^$/d' | sort | paste -sd, -)
if [ "$gaps" = none ]; then
    [ -z "$found_list" ] || fail "missing features reported: $found_list; expected none"
    grep -qx 'Missing: nothing' "$report" || fail "no 'Missing: nothing' line"
else
    [ -n "$found_list" ] || fail "no '- <feature>: ...' line at all (a new report format?); expected $expected_list"
    [ "$found_list" = "$expected_list" ] || fail "missing features reported: $found_list; expected $expected_list"
fi

for text in "${texts[@]}"; do
    grep -qF -- "$text" "$report" || fail "the report does not say: $text"
done
for text in "${absent[@]}"; do
    [ -n "$text" ] || fail "an empty text after --absent"
    match=$(grep -nF -- "$text" "$report" | head -n1 || true)
    [ -z "$match" ] || fail "the report must not say $text, but its line ${match%%:*} does: ${match#*:}"
done
summary="requirements of $program: status $rc, missing ${found_list:-nothing}, gtk4paintablesink $version ($file)"
[ ${#absent[@]} -eq 0 ] || summary+="; not said: $(printf '%s, ' "${absent[@]}" | sed 's/, $//')"
echo "$summary"
