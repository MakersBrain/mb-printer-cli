#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

release_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-$release_root/dist}
features=${MB_PRINTER_RELEASE_FEATURES:-usb,bluetooth}
cyclonedx_version=0.5.7
version=$(sed -n '0,/^version = "/s/^version = "\([^"]*\)"/\1/p' "$release_root/Cargo.toml")
target=$(rustc -vV | sed -n 's/^host: //p')
artifact=mb-printer-$target-v$version

if git -C "$release_root" status --porcelain | grep -q .; then
  echo "release candidate requires a clean tracked and untracked worktree" >&2
  exit 1
fi
"$release_root/scripts/check_release_version.sh" "v$version"
mkdir -p "$output"
output=$(CDPATH= cd -- "$output" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/mb-printer-release.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

cargo build --manifest-path "$release_root/Cargo.toml" --release --locked --features "$features"
cp "$release_root/target/release/mb-printer" "$output/$artifact"
(cd "$output" && sha256sum "$artifact" >"$artifact.sha256")

if ! command -v cargo-cyclonedx >/dev/null 2>&1 ||
   ! cargo-cyclonedx cyclonedx --version 2>/dev/null | grep -Fq " $cyclonedx_version"; then
  cargo install cargo-cyclonedx --version "$cyclonedx_version" --locked --root "$work/tools"
  cyclonedx=$work/tools/bin/cargo-cyclonedx
else
  cyclonedx=$(command -v cargo-cyclonedx)
fi
(cd "$release_root" && "$cyclonedx" cyclonedx --format json --spec-version 1.5 --override-filename "$artifact.sbom")
mv "$release_root/$artifact.sbom.json" "$output/$artifact.sbom.json"

git -C "$release_root" archive --format=tar --prefix="mb-printer-cli-$version/" HEAD |
  gzip -n >"$output/mb-printer-cli-$version-source.tar.gz"
for notice in LICENSE NOTICE.md THIRD_PARTY_LICENSES.md; do
  cp "$release_root/$notice" "$output/$notice"
done

mkdir -p "$work/mb-printer-cli" "$work/mb-printer-sdk" "$work/install"
git -C "$release_root" archive HEAD | tar -xf - -C "$work/mb-printer-cli"
git -C "$release_root/../mb-printer-sdk" archive HEAD | tar -xf - -C "$work/mb-printer-sdk"
cargo install --path "$work/mb-printer-cli" --root "$work/install" --locked --features "$features"
test "$("$work/install/bin/mb-printer" --version)" = "mb-printer $version"

(cd "$output" && sha256sum "$artifact" "$artifact.sbom.json" "mb-printer-cli-$version-source.tar.gz" LICENSE NOTICE.md THIRD_PARTY_LICENSES.md >SHA256SUMS)
echo "release candidate: $output"
