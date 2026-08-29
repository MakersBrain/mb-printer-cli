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

## Remaining locally actionable work

| Priority | Incomplete requirement | Required evidence |
|---|---|---|
| High | Durable submission idempotency is not exposed to API clients. A repeated HTTP submission can intentionally create a second job. | Persist and reject/replay an explicit idempotency key across restart without ever replaying an ambiguous printer write. |
| High | API jobs do not yet expose CLI-equivalent explicit rotation and opt-in aspect-preserving fit/media rejection. | Cross-contract fixtures for 0/90/180/270 degrees, oversize rejection before connection, and `fit=true`. |
| High | OpenAPI describes every route but still uses abbreviated request/response schemas for connections, validation, La Poste and jobs. | Validate the checked-in OpenAPI document and generated client fixtures in CI. |
| Medium | A single service invocation binds one loopback address; serving IPv4 and IPv6 loopback simultaneously needs a dual-listener supervisor. | External-process tests against both `127.0.0.1` and `::1`, including Host rejection. |
| Medium | La Poste CLI/API selection is page-level; per-slot user deselection is not a native surface. | Stable one-based `page:slot` selectors and empty-selection rejection before transport. |
| Medium | Grant revoke/rotate are local administrative CLI operations only. | Decide whether authenticated browser grant-management routes are required; if added, prevent a grant escalating beyond its own origin. |
| Low | Preview API has document DPI only and no explicit zoom/pan parameters; CLI render has them. | Preview query schema and fidelity tests, if the editor cannot perform viewport transforms itself. |
| Low | Public catalogue search/pagination/favourites/downloads are not CLI-owned; `/v1/assets` currently exposes installed private catalogue metadata only. | Define repository ownership before adding a public catalogue API here. |

## External acceptance blockers

Physical BLE, RFCOMM, USB, serial, Wi-Fi and every printer family still need
hardware traces. Browser/WASM byte equivalence, editor behavior, offline PWA
acceptance, public-asset licensing and SDK registry publication cannot be
closed in this repository. Cargo package verification remains dependent on the
versioned SDK crates being available to Cargo's package verifier.
