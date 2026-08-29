<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Migration and troubleshooting

The binary is `mb-printer`; Python imports and `mbprint` are intentionally not
retained. The CLI reads legacy v3 JSON through the SDK importer and writes only
strict portable v4; use `validate` or `inspect` to check the normalized input.

- Pairing rejected: configure the exact HTTPS editor origin, create a fresh
  secret, and ensure browser Local Network Access is permitted.
- `401`: the grant expired or was revoked; pair again.
- Suspected token exposure: run `mb-printer api rotate GRANT_ID`; the old token
  is invalid as soon as the one-time replacement token is printed.
- `421`: a non-loopback `Host` was rejected independently of CORS.
- Serial open failure: check device permissions and `--baud`.
- BLE build failure on Linux: install D-Bus development headers and
  `pkg-config`; runtime acceptance also requires BlueZ and adapter permission.
- `outcome-unknown` or `cancelled-partial`: inspect the physical printer before
  explicitly deciding whether to retry. Jobs are never replayed automatically.
