#!/usr/bin/env bash
#
# Compile the orchestrator's Tailwind v4 source into the static asset
# that the running binary serves at `/static/app.css`.
#
# Run locally before `cargo run` whenever you touch `styles/input.css`
# or any HTML template. CI / Docker handles this automatically.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORCH="${ROOT}/crates/brarr-orchestrator"
BIN="${ROOT}/tools/tailwindcss"

if [[ ! -x "${BIN}" ]]; then
    echo "Tailwind binary missing at ${BIN}." >&2
    echo "Run scripts/install-tailwind.sh first." >&2
    exit 1
fi

INPUT="${ORCH}/styles/input.css"
OUTPUT="${ORCH}/static/app.css"

MODE="${1:-build}"
case "${MODE}" in
    build)
        echo "Building ${OUTPUT} (minified) ..."
        "${BIN}" --input "${INPUT}" --output "${OUTPUT}" --minify
        ;;
    watch)
        echo "Watching ${INPUT} → ${OUTPUT} ..."
        "${BIN}" --input "${INPUT}" --output "${OUTPUT}" --watch
        ;;
    *)
        echo "Usage: $0 [build|watch]" >&2
        exit 1
        ;;
esac
