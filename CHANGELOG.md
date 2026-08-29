<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Changelog

## Unreleased

- Unified native PDF and La Poste normalization on the SDK/WASM code path.
- Required explicit or saved print transports and persisted bounded jobs.
- Added authenticated La Poste extraction API, release SBOM/checksums, and
  multi-platform pinned-action builds.
- Added persistent bearer-grant rotation and aligned saved RFCOMM jobs with
  the native MAC/channel transport contract.
- Reject empty La Poste sheets before opening a transport and generate a real
  pinned CycloneDX 1.5 release SBOM.
- Added durable request-bound API idempotency, rotation/fit/media preflight,
  dual-stack loopback serving, self-service browser grants, preview viewport
  controls and per-slot La Poste selection.
- Completed and test-validated the OpenAPI 3.1 route and payload contract.

This project follows Semantic Versioning. The loopback API is independently
versioned by its `/v1` path; breaking API or document changes require a new
versioned path and a migration note.

## 0.1.0

- Initial Rust CLI, SDK renderer and protocol-plan integration.
- Authenticated loopback API with pairing, grants, jobs, SSE and cancellation.
- File, TCP, configured serial, optional USB and optional BLE transports.
- Pure-Rust La Poste PDF rasterization, extraction, export and printing.
- Private split-APK/ADB asset inventory.
