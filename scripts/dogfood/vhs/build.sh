#!/usr/bin/env bash
# Builds the vhs the tapes under scripts/dogfood/tapes/ are recorded with:
# vhs v0.11.0 with view-capture.patch applied, stamped with the version
# record_gif in scripts/dogfood/lib.sh requires. Stock vhs reads the cursor
# layer and the text layer of each frame in two browser calls, and a frame
# the terminal draws between them records the old cursor over the new
# text; the patch reads both layers in one call.
#
# Usage: scripts/dogfood/vhs/build.sh [INSTALL_DIR]   (default ~/.local/bin)
set -euo pipefail

VHS_MODULE=github.com/charmbracelet/vhs
VHS_RELEASE=v0.11.0
VIEW_VHS_VERSION=v0.11.0-view1

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/lib/scratch.sh
. "$HERE/../../lib/scratch.sh"
DEST=${1:-$HOME/.local/bin}

for tool in go patch; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "build.sh: $tool is not on PATH" >&2
        exit 2
    }
done

WORK=$(mktemp -d "$(scratch_root)/vhs-build-XXXXXX")
cleanup_build() {
    if [ -d "$WORK" ]; then
        chmod -R u+w "$WORK"
    fi
    rm -rf -- "$WORK"
}
trap cleanup_build EXIT

# go verifies the download against the checksum database
(cd "$WORK" && go mod download "$VHS_MODULE@$VHS_RELEASE")
cp -R "$(go env GOMODCACHE)/$VHS_MODULE@$VHS_RELEASE" "$WORK/src"
chmod -R u+w "$WORK/src"
patch -p1 -d "$WORK/src" <"$HERE/view-capture.patch"
(cd "$WORK/src" && go build -ldflags "-X main.Version=$VIEW_VHS_VERSION" -o "$WORK/vhs" .)

mkdir -p -- "$DEST"
cp -- "$WORK/vhs" "$DEST/vhs.new"
chmod 755 "$DEST/vhs.new"
mv -- "$DEST/vhs.new" "$DEST/vhs"
"$DEST/vhs" --version

found=$(command -v vhs || true)
if [ "$found" != "$DEST/vhs" ]; then
    echo "build.sh: installed $DEST/vhs, and PATH resolves vhs to" \
        "${found:-nothing}. Put $DEST ahead of it on PATH" >&2
    exit 1
fi
