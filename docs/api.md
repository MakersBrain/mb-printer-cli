<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Loopback API v1

The service binds both IPv4 and IPv6 loopback by default and uses port 9847.
Use `--bind` only to select one explicit loopback address. Run
`mb-printer api pair`, copy the displayed secret into the editor, and exchange
it at `POST /v1/pair`. The camelCase response contains `token`, `grantId`, and
RFC 3339 `expiresAt`. Tokens are hashed at rest, origin-bound, expiring, and
revocable with `mb-printer api revoke ID`. `mb-printer api rotate ID` replaces
a grant's bearer token atomically while preserving its bound origin. Browsers
can inspect, rotate, or revoke only their own grant through `/v1/grants/me` and
its `/rotate` and `/revoke` subroutes.

Every non-pairing request needs browser `Origin`, loopback `Host`, and
`Authorization: Bearer TOKEN`. `POST /v1/jobs` accepts canonical SDK v4 or the
editor v4 representation, `printerId`, copies, density, `rotation` (0/90/180/270)
and opt-in `fit`. Loaded-media and head-width mismatches fail before transport
creation unless fitting was requested. A print must provide
exactly one explicit `transport` or persisted `connectionId`; capture is never
an implicit UI default:

```json
{"kind":"capture"}
{"kind":"tcp","address":"printer.local:9100"}
{"kind":"ipp","uri":"ipps://brother.local:631/ipp/print"}
{"kind":"serial","path":"/dev/ttyUSB0","baud":115200}
{"kind":"rfcomm","address":"D3:8C:9F:86:F4:AA","channel":1}
{"kind":"file","path":"/tmp/job.bin"}
```

Job JSON is camelCase and exposes `terminal`, `outcome`, `lastCompletedAction`,
`bytesSent`, `action`, `actions`, `totalBytes`, `phase`, and `error`. Poll
`GET /v1/jobs/ID`, stream `GET /v1/jobs/ID/events`, or cooperatively cancel at
`POST /v1/jobs/ID/cancel`.

Clients may send an ASCII `Idempotency-Key` (maximum 128 bytes). The service
persists the origin-scoped key and exact request digest with the job. An exact
retry returns the original job with HTTP 200, including after restart; reuse
for different request bytes returns HTTP 409 and never starts a write.

Bounded job state and resumable request metadata persist at `jobs_path`.
Interrupted non-terminal work is restored as `outcome-unknown`; completed and
cancelled jobs remain queryable after restart. Saved connections dispatch the
same TCP, IPP/IPPS, serial, RFCOMM, file, feature-gated USB, or feature-gated BLE adapter
as an explicit request.

Browser preflights receive `Access-Control-Allow-Origin` only for an exact
configured origin. GET, POST, OPTIONS, Authorization, Content-Type and
Idempotency-Key are
allowed; bearer tokens are used instead of ambient credentials.
`Access-Control-Allow-Private-Network: true` is emitted only for a valid
allowlisted PNA preflight. Loopback Host and origin checks remain independent
of CORS response headers.

`POST /v1/connection` persists a bounded connection definition. Discovery and
`GET /v1/status?connection=ID` expose live backend-reported transport, status
and media data. A persisted `kind: "ipp"` connection uses the same endpoint for
Get-Printer-Attributes status and Brother raster Print-Job submission. `ipps://`
uses mandatory hostname and certificate verification; private or self-signed
printer certificates can be added explicitly with `certificatePem`, and there
is no insecure TLS or silent downgrade mode. Brother TCP uses IPP by default (or raster status when
`statusMode` is `raster`); readable serial, USB, BLE and RFCOMM transports use
the Brother status frame. Test-only injected probes remain available without
claiming physical hardware acceptance.

Document routes are `POST /v1/documents/validate`, `/preview`, and
`/export?format=png|pdf`. Other authenticated routes list printer definitions,
discovery and private catalogues, extract La Poste PDF slots (optionally
filtered by repeatable one-based `page:slot` selectors), and expose job
polling, SSE progress and cooperative cancellation. The complete route and
content-type contract is in `openapi.yaml`.
