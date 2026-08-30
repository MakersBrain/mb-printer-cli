<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Release process

`mb-printer-cli` uses one SemVer value across `Cargo.toml`, `Cargo.lock`, the
SDK dependency requirements, the changelog heading and the Git tag. Release
tags are exactly `vMAJOR.MINOR.PATCH`. The release workflow rejects a tag that
does not match, a missing or duplicated changelog heading, pending bullet
entries under `Unreleased`, or mismatched SDK dependency requirements.

Before creating a tag, finalize the changelog and run:

```sh
scripts/pin_sdk.sh <merged-sdk-commit>
scripts/check_sdk_pin.sh ../mb-printer-sdk
scripts/check_release_version.sh v0.1.0
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Build a local candidate from a clean commit into a directory outside the
working tree:

```sh
scripts/build_release_candidate.sh /tmp/mb-printer-cli-0.1.0
```

Linux feature-complete candidates require `pkg-config` and D-Bus development
headers. The same platform prerequisites documented for Bluetooth installation
must be available before running the script.

The committed `.github/sdk-ref` is the single SDK source revision used by CI,
tagged releases, and local release candidates. The pin must be a full commit on
`mb-printer-sdk`'s `origin/main`; update it only after the required SDK change is
merged. The sheet-export migration must not land until this pin is advanced to the
merged SDK commit that provides `mb_printer_core::sheet`; a local uncommitted SDK
worktree is never a valid substitute for that pin.

The candidate script uses locked dependencies and the feature-complete local
profile (`usb,bluetooth` by default), installs pinned `cargo-cyclonedx` 0.5.7
in an isolated temporary tool root when needed, and emits:

- the host binary and its individual SHA-256 file;
- a CycloneDX 1.5 JSON SBOM;
- a deterministic gzip-compressed source archive from committed `HEAD`;
- `LICENSE`, `NOTICE.md`, `THIRD_PARTY_LICENSES.md`, and aggregate
  `SHA256SUMS`.

It reconstructs the CLI from its committed `HEAD` and the SDK from the pinned
commit, installs the CLI into a new temporary Cargo root, and executes the
installed binary's `--version`. Any dirty CLI worktree, invalid SDK pin, build,
checksum, SBOM, archive, installation, or version failure stops the candidate
build. Set
`MB_PRINTER_RELEASE_FEATURES` only when deliberately producing a documented
platform-specific feature variant.

Inspect every file and verify `sha256sum -c SHA256SUMS` before signing a tag.
Push the SDK and wait for its release and registry publication before pushing
the CLI tag; the CLI package and workflows require version `0.1.0` of the SDK.
Repository creation, registry credentials, branch protection, tag signing,
pushes and GitHub release publication are deliberately outside the script.
