<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Changelog

This project follows Semantic Versioning. The loopback API is independently
versioned by its `/v1` path; breaking API or document changes require a new
versioned path and a migration note.

## Unreleased

No changes yet.

## 0.1.0

- Initial Rust CLI, SDK renderer and protocol-plan integration.
- Authenticated dual-stack loopback API with origin-bound pairing, persistent
  self-service grants, durable idempotent jobs, SSE and safe cancellation.
- File, TCP, configured serial and RFCOMM transports, with feature-gated USB
  and BLE discovery, status and print execution.
- Canonical SDK rendering, validation, PNG/PDF/SVG export, v3 migration,
  deterministic captures and typed configuration defaults.
- Pure-Rust exact-geometry La Poste PDF normalization, occupied-slot
  extraction, one-based slot selection, export and printing.
- Private split-APK/ADB asset inventory with strict credential and paid-content
  filtering and persistent `.mb-assets` round trips.
- Brother raster/IPP status, media and Wi-Fi command support plus mockable
  hardware boundaries.
- Authenticated OpenAPI 3.1 contract, CORS/PNA enforcement, release checksums,
  CycloneDX SBOMs and multi-platform feature-complete builds.
- Rust 1.92 minimum toolchain aligned with the SDK deterministic PDF renderer.
