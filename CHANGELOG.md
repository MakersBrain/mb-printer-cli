<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Changelog

## Unreleased

- Unified native PDF and La Poste normalization on the SDK/WASM code path.
- Required explicit or saved print transports and persisted bounded jobs.
- Added authenticated La Poste extraction API, release SBOM/checksums, and
  multi-platform pinned-action builds.

This project follows Semantic Versioning. The loopback API is independently
versioned by its `/v1` path; breaking API or document changes require a new
versioned path and a migration note.

## 0.1.0

- Initial Rust CLI, SDK renderer and protocol-plan integration.
- Authenticated loopback API with pairing, grants, jobs, SSE and cancellation.
- File, TCP, configured serial, optional USB and optional BLE transports.
- Pure-Rust La Poste PDF rasterization, extraction, export and printing.
- Private split-APK/ADB asset inventory.
