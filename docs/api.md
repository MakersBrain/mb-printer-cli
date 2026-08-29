<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Loopback API v1

The service binds only to loopback and defaults to port 9847. Run
`mb-printer api pair`, copy the displayed secret into the editor, and exchange
it at `POST /v1/pair`. The camelCase response contains `token`, `grantId`, and
RFC 3339 `expiresAt`. Tokens are hashed at rest, origin-bound, expiring, and
revocable with `mb-printer api revoke ID`. `mb-printer api rotate ID` replaces
a grant's bearer token atomically while preserving its bound origin.

Every non-pairing request needs browser `Origin`, loopback `Host`, and
`Authorization: Bearer TOKEN`. `POST /v1/jobs` accepts canonical SDK v4 or the
editor v4 representation, `printerId`, copies and density. A print must provide
exactly one explicit `transport` or persisted `connectionId`; capture is never
an implicit UI default:

```json
{"kind":"capture"}
{"kind":"tcp","address":"printer.local:9100"}
{"kind":"serial","path":"/dev/ttyUSB0","baud":115200}
{"kind":"rfcomm","address":"D3:8C:9F:86:F4:AA","channel":1}
{"kind":"file","path":"/tmp/job.bin"}
```

Job JSON is camelCase and exposes `terminal`, `outcome`, `lastCompletedAction`,
`bytesSent`, `action`, `actions`, `totalBytes`, `phase`, and `error`. Poll
`GET /v1/jobs/ID`, stream `GET /v1/jobs/ID/events`, or cooperatively cancel at
`POST /v1/jobs/ID/cancel`.

Bounded job state and resumable request metadata persist at `jobs_path`.
Interrupted non-terminal work is restored as `outcome-unknown`; completed and
cancelled jobs remain queryable after restart. Saved connections dispatch the
same TCP, serial, RFCOMM, file, feature-gated USB, or feature-gated BLE adapter
as an explicit request.

Browser preflights receive `Access-Control-Allow-Origin` only for an exact
configured origin. GET, POST, OPTIONS, Authorization and Content-Type are
allowed; bearer tokens are used instead of ambient credentials.
`Access-Control-Allow-Private-Network: true` is emitted only for a valid
allowlisted PNA preflight. Loopback Host and origin checks remain independent
of CORS response headers.

`POST /v1/connection` persists a bounded connection definition. Discovery and
`GET /v1/status?connection=ID` expose live backend-reported transport, status
and media data. Brother TCP uses IPP by default (or raster status when
`statusMode` is `raster`); readable serial, USB, BLE and RFCOMM transports use
the Brother status frame. Test-only injected probes remain available without
claiming physical hardware acceptance.

Document routes are `POST /v1/documents/validate`, `/preview`, and
`/export?format=png|pdf`. Other authenticated routes list printer definitions,
discovery and private catalogues, extract La Poste PDF slots, and expose job
polling, SSE progress and cooperative cancellation. The complete route and
content-type contract is in `openapi.yaml`.
