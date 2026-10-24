#!/usr/bin/env bash
# Downloads the tools scripts/build-appimage.sh runs, at pinned versions,
# and checks every file against its recorded SHA-256 before it is made
# executable. Files already present with the right hash are kept.
#
#   scripts/fetch-appimage-tools.sh <directory>
#
# The directory is meant to live outside the source tree (for example
# $RUNNER_TEMP/appimage-tools in CI). Point APPIMAGE_TOOLS at it afterwards.
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <directory>" >&2
    exit 2
fi
dir=$1
mkdir -p "$dir"

# name  sha256  url
tools='
linuxdeploy-x86_64.AppImage c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage
appimagetool-x86_64.AppImage ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
linuxdeploy-plugin-gtk.sh b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171 https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/7a3fbc31a9e5075073ff8790f26effbac5f84453/linuxdeploy-plugin-gtk.sh
linuxdeploy-plugin-gstreamer.sh d1cf90701047aa9583c274bb66dd82db352255374fbfb8f3c4c89a05fe23bce0 https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/432af639725bc8b99df12351e86bb2e8a6b3585f/linuxdeploy-plugin-gstreamer.sh
'

check() { echo "$2  $1" | sha256sum --check --status; }

while read -r name sha url; do
    [ -n "$name" ] || continue
    target="$dir/$name"
    if [ -f "$target" ] && check "$target" "$sha"; then
        echo "$name: present"
    else
        echo "$name: downloading"
        curl -fsSL --retry 3 -o "$target.part" "$url"
        if ! check "$target.part" "$sha"; then
            echo "$name: checksum mismatch, expected $sha, got $(sha256sum "$target.part" | cut -d' ' -f1)" >&2
            rm -f "$target.part"
            exit 1
        fi
        mv "$target.part" "$target"
    fi
    chmod +x "$target"
done <<< "$tools"
echo "AppImage tools ready in $dir"
