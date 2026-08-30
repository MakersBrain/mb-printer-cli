#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sdk=$root/../mb-printer-sdk
requested=${1:-main}

pin=$(git -C "$sdk" rev-parse --verify "$requested^{commit}" 2>/dev/null) || {
    echo "cannot resolve SDK commit $requested in $sdk" >&2
    exit 1
}
git -C "$sdk" rev-parse --verify origin/main^{commit} >/dev/null 2>&1 || {
    echo "mb-printer-sdk origin/main is unavailable; fetch it before pinning" >&2
    exit 1
}
git -C "$sdk" merge-base --is-ancestor "$pin" origin/main || {
    echo "$pin is not on mb-printer-sdk origin/main; merge the SDK change first" >&2
    exit 1
}

printf '%s\n' "$pin" >"$root/.github/sdk-ref"
echo "Pinned mb-printer-sdk to $pin"
