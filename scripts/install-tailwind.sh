#!/usr/bin/env bash
#
# Download the Tailwind v4 standalone binary into ./tools/tailwindcss.
# Idempotent: re-runs are no-ops once the binary exists.
#
# The binary is single-file, ~30MB, has no Node dependency, and is
# the same artifact Tailwind's CI publishes for every release.
#
# Targets recognised: linux-x64, linux-arm64, macos-arm64, macos-x64.
# Windows users must run `scripts/install-tailwind.ps1` from PowerShell.

set -euo pipefail

VERSION="${TAILWIND_VERSION:-v4.1.16}"
REPO="https://github.com/tailwindlabs/tailwindcss/releases/download"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOOLS_DIR="${ROOT}/tools"
mkdir -p "${TOOLS_DIR}"
DEST="${TOOLS_DIR}/tailwindcss"

if [[ -x "${DEST}" ]]; then
    echo "Tailwind binary already present at ${DEST}."
    "${DEST}" --help > /dev/null
    exit 0
fi

UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"

case "${UNAME_S}-${UNAME_M}" in
    Linux-x86_64)   TARGET="tailwindcss-linux-x64" ;;
    Linux-aarch64)  TARGET="tailwindcss-linux-arm64" ;;
    Darwin-arm64)   TARGET="tailwindcss-macos-arm64" ;;
    Darwin-x86_64)  TARGET="tailwindcss-macos-x64" ;;
    *)
        echo "Unsupported platform: ${UNAME_S}-${UNAME_M}." >&2
        echo "Download manually from ${REPO}/${VERSION}/" >&2
        exit 1
        ;;
esac

URL="${REPO}/${VERSION}/${TARGET}"
echo "Downloading Tailwind ${VERSION} (${TARGET}) ..."
curl --fail --location --show-error --silent --output "${DEST}" "${URL}"
chmod +x "${DEST}"
"${DEST}" --help > /dev/null

echo "Installed: ${DEST}"
