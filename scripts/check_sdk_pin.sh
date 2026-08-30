#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

root=${RELEASE_ROOT:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}
sdk=${1:-$root/../mb-printer-sdk}
pin=$(tr -d '[:space:]' <"$root/.github/sdk-ref")

if [ "${#pin}" -ne 40 ]; then
    echo ".github/sdk-ref must contain exactly one full lowercase commit SHA" >&2
    exit 1
fi
case $pin in
    *[!0-9a-f]*)
        echo ".github/sdk-ref must contain exactly one full lowercase commit SHA" >&2
        exit 1
        ;;
esac

git -C "$sdk" cat-file -e "$pin^{commit}" 2>/dev/null || {
    echo "pinned SDK commit $pin is unavailable in $sdk; fetch mb-printer-sdk" >&2
    exit 1
}
git -C "$sdk" rev-parse --verify origin/main^{commit} >/dev/null 2>&1 || {
    echo "mb-printer-sdk origin/main is unavailable in $sdk; fetch it before pinning" >&2
    exit 1
}
git -C "$sdk" merge-base --is-ancestor "$pin" origin/main || {
    echo "pinned SDK commit $pin is not on mb-printer-sdk origin/main" >&2
    exit 1
}
printf '%s\n' "$pin"
