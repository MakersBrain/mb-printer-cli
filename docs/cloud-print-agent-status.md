<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Cloud print implementation status

Date: 2026-08-30

The software baseline in
[`cloud-print-agent-plan.md`](cloud-print-agent-plan.md) is implemented for a
capture-only pilot.

## Implemented

- `mb-printer-cli` retains its loopback API and uses the same internal executor
  for cloud jobs.
- The agent has owner-only enrollment credentials, explicit publication of
  saved local connections, an outbound versioned gRPC stream, reconnect
  backoff, durable local job records, and `(job_id, digest)` deduplication.
- Restart after a possible write becomes `outcome-unknown`; it is never
  automatically re-executed.
- `mb-print-cloud` is an independent Rust repository with one binary, one TOML
  config, and one owner-only SQLite database. It has no control-plane or Odoo
  dependency.
- The cloud service implements static tenant-scoped `print` and
  `manage-printers` credentials, single-use ten-minute enrollment, revocation,
  printer publication/presence, idempotent job submission, polling,
  cancellation, progress/results, seven-day payload cleanup, and OpenAPI 3.1.
- The broker sends at most one unacknowledged job per agent and never reassigns
  jobs.

## Verification performed

- `mb-print-cloud`: 8 unit/integration tests and strict all-target Clippy pass,
  including generated OpenAPI, exact-origin CORS, and agent progress fields.
- `mb-printer-cli`: 52 library tests, 11 process/contract/release tests, doc
  tests, and strict all-target Clippy pass.
- A real process-level capture exercised config initialization, enrollment,
  publication, the gRPC handshake, HTTPS-API submission, durable receipt,
  execution, result polling, cloud restart, agent reconnect, and revocation.
- The capture contained 1,947 bytes. Reconnect and revocation did not change
  its size or modification time, providing evidence that the terminal job was
  not physically replayed.
- The label-editor cloud route was qualified through the same real broker and
  agent path. The completed response preserved all 13 actions
  (`lastCompletedAction` 12), and an idempotent replay returned the same job
  without changing the 1,947-byte capture.
- After cloud restart, the terminal job remained readable and its printer was
  offline until the agent reconnected. Revocation closed authentication and
  left the printer disabled/offline.

## Not enabled

No physical printer family is enabled by this work. Physical disconnect and
cancellation evidence must be collected per printer family before a production
pilot. Odoo cloud-route UI work remains optional; the label editor now uses the
standalone JSON API without receiving agent credentials.
