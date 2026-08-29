#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

release_root=${RELEASE_ROOT:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}
manifest=$release_root/Cargo.toml
changelog=$release_root/CHANGELOG.md
lockfile=$release_root/Cargo.lock
version=$(sed -n '0,/^version = "/s/^version = "\([^"]*\)"/\1/p' "$manifest")
tag=${1:-${GITHUB_REF_NAME:-}}
if [ "$tag" = "--manifest-only" ]; then
  tag=
fi

case "$version" in
  ''|*[!0-9.]*|.*|*.|*..*) echo "invalid package version: $version" >&2; exit 1 ;;
esac
test "$(printf '%s' "$version" | awk -F. '{print NF}')" -eq 3 || {
  echo "release version must be MAJOR.MINOR.PATCH: $version" >&2
  exit 1
}
if [ -n "$tag" ] && [ "$tag" != "v$version" ]; then
  echo "tag $tag does not match package version v$version" >&2
  exit 1
fi
test "$(grep -Fxc "## $version" "$changelog")" -eq 1 || {
  echo "CHANGELOG.md must contain exactly one '## $version' heading" >&2
  exit 1
}
if awk '/^## Unreleased$/{inside=1;next} /^## /{inside=0} inside && /^- /{found=1} END{exit !found}' "$changelog"; then
  echo "CHANGELOG.md Unreleased still contains release entries" >&2
  exit 1
fi
locked_version=$(awk '
  $0 == "name = \"mb-printer-cli\"" { package=1; next }
  package && /^version = / { gsub(/"/, "", $3); print $3; exit }
' "$lockfile")
test "$locked_version" = "$version" || {
  echo "Cargo.lock mb-printer-cli version $locked_version does not match $version" >&2
  exit 1
}
for dependency in mb-printer-core mb-printer-native; do
  grep -Eq "^$dependency = \\{ version = \"$version\", path = \"" "$manifest" || {
    echo "$dependency path dependency must pin version $version" >&2
    exit 1
  }
done
echo "release gate: v$version is consistent"
