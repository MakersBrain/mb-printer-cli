<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Release process

`mb-printer-cli` uses one SemVer value across `Cargo.toml`, `Cargo.lock`, the
SDK dependency requirements, the changelog heading and the Git tag. Release
tags are exactly `vMAJOR.MINOR.PATCH`. The release workflow rejects a tag that
does not match, a missing or duplicated changelog heading, pending bullet
entries under `Unreleased`, or mismatched SDK dependency requirements.

Before creating a tag, finalize the changelog and run:

```sh
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

The candidate script uses locked dependencies and the feature-complete local
profile (`usb,bluetooth` by default), installs pinned `cargo-cyclonedx` 0.5.7
in an isolated temporary tool root when needed, and emits:

- the host binary and its individual SHA-256 file;
- a CycloneDX 1.5 JSON SBOM;
- a deterministic gzip-compressed source archive from committed `HEAD`;
- `LICENSE`, `NOTICE.md`, `THIRD_PARTY_LICENSES.md`, and aggregate
  `SHA256SUMS`.

It reconstructs clean CLI and SDK source trees from each repository's `HEAD`,
installs the CLI into a new temporary Cargo root, and executes the installed
binary's `--version`. Any dirty worktree, build, checksum, SBOM, archive,
installation, or version failure stops the candidate build. Set
`MB_PRINTER_RELEASE_FEATURES` only when deliberately producing a documented
platform-specific feature variant.

Inspect every file and verify `sha256sum -c SHA256SUMS` before signing a tag.
Push the SDK and wait for its release and registry publication before pushing
the CLI tag; the CLI package and workflows require version `0.1.0` of the SDK.
Repository creation, registry credentials, branch protection, tag signing,
pushes and GitHub release publication are deliberately outside the script.
