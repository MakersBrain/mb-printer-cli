<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Native CLI/API platform-plan audit

Audited against `mb-cli-printer/docs/rust-printer-platform-plan.md` on
2026-08-29. This file covers the `mb-printer-cli` repository only; SDK, WASM,
editor and physical-hardware gates remain owned by their respective projects.

## Verified locally

- One `mb-printer` binary covers validation/inspection, v3 normalization,
  rendering/export, printing, discovery/status, typed configuration, private
  assets, Wi-Fi, USB reporting, La Poste and the loopback API.
- The API independently enforces loopback Host, exact origin, bearer auth,
  CORS/PNA and body limits. Hashed origin-bound grants, jobs, resumable
  metadata, connections and catalogue metadata persist with bounded retention.
- Jobs require an explicit transport or saved connection, stream progress,
  expose byte/action counts, cooperatively cancel, preserve ambiguous outcomes
  and never automatically retry writes.
- PNG/PDF export, live printer/media status, native transport dispatch,
  deterministic captures, split-APK privacy filtering and La Poste extraction
  have unit and external-process coverage.
- Empty La Poste sheets now fail before transport creation, grant rotation is
  atomic and persistent, and saved RFCOMM jobs use the same MAC/channel contract
  as discovery and connection probes.
- Release jobs generate a pinned CycloneDX 1.5 JSON SBOM instead of labelling
  raw Cargo metadata as an SBOM; artifacts retain SHA-256 checksums and notices.
- Origin-scoped, request-bound idempotency persists across restart and exact
  replay returns the original job; conflicting reuse fails before execution.
- API jobs expose 0/90/180/270 rotation, opt-in aspect-preserving fit and
  loaded-media/head-width preflight. Preview exposes zoom and pixel offsets.
- The checked-in OpenAPI 3.1 contract covers all routes, transport variants,
  grant/job/document/La Poste shapes and is parsed/reference-checked in tests.
- Default serving supervises IPv4 and IPv6 loopback listeners together, with
  external-process coverage. An explicit `--bind` narrows this deliberately.
- CLI and API La Poste workflows accept stable one-based `page:slot`
  selectors and reject an empty occupied selection before transport.
- Authenticated browser grant management is self-scoped: a grant can inspect,
  rotate or revoke itself but cannot enumerate or mutate other origins.

## Remaining locally actionable work

None identified in the CLI/API/release ownership defined by the platform plan.

## External acceptance blockers

Physical BLE, RFCOMM, USB, serial, Wi-Fi and every printer family still need
hardware traces. Browser/WASM byte equivalence, editor behavior, offline PWA
acceptance, the separately owned public catalogue/search service and its asset
licensing, and SDK registry publication cannot be
closed in this repository. Cargo package verification remains dependent on the
versioned SDK crates being available to Cargo's package verifier.
