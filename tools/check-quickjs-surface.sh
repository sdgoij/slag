#!/usr/bin/env bash
# Run the surface ledger for both mutually-exclusive C skins. Each skin has
# its own vendored header and gap list, so the ledger must run once per skin.
set -euo pipefail

cd "$(dirname "$0")/.."

PY=""
for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 \
        && "$candidate" -c 'import tomllib' >/dev/null 2>&1; then
        PY="$candidate"
        break
    fi
done
if [ -z "$PY" ]; then
    echo "check-quickjs-surface: no python with tomllib (3.11+) found" >&2
    exit 1
fi

"$PY" tools/quickjs-surface.py ng
"$PY" tools/quickjs-surface.py bellard
