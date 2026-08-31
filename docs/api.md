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

Changing printer settings needs a distinct, short-lived administrator grant.
Administrator pairing has its own default-off switch. Before using the browser
administration flow, set both `enable_brother_wifi_configuration` and
`enable_brother_wifi_configuration_pairing` to `true`, then restart the
service. After a local person intentionally runs
`mb-printer api pair-admin`, the
browser exchanges that one-time secret at `POST /v1/admin/pair`; it cannot use
`POST /v1/pair` and normal pairing secrets cannot use the administrator route.
The administrator token is origin-bound, capped at ten minutes, and returned
with `Cache-Control: no-store`. Paste the one-time secret directly into the
editor and do not log or save it.

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
same TCP, IPP/IPPS, serial, RFCOMM, file, feature-gated USB, or feature-gated
BLE adapter as an explicit request. RFCOMM requires the Linux-only
`bluetooth-linux` feature; the portable `bluetooth` feature enables BLE
without vendored D-Bus.

Browser preflights receive `Access-Control-Allow-Origin` only for an exact
configured origin. GET, POST, OPTIONS, Authorization, Content-Type and
Idempotency-Key are
allowed; bearer tokens are used instead of ambient credentials.
`Access-Control-Allow-Private-Network: true` is emitted only for a valid
allowlisted PNA preflight. Loopback Host and origin checks remain independent
of CORS response headers.

For the dev1 hosted editor, configure the exact origin rather than a wildcard:

```sh
mb-printer config set allowed_origins \
  'http://127.0.0.1:4173,http://localhost:4173,https://labels.dev1.makersbrain.net'
```

Open the HTTPS editor and create a fresh pairing secret. Grants are bound to
the origin that exchanges the secret, so a token paired from the loopback
editor cannot be reused by `https://labels.dev1.makersbrain.net` or another
workspace. Keep the API on loopback; never add port 9847 to Cloudflare Tunnel.

`POST /v1/connection` persists a bounded connection definition. Discovery and
`GET /v1/status?connection=ID` expose live backend-reported transport, status
and media data. Their response-only `operations` array is derived from the
model and concrete transport; it is never written to connection files. A USB
QL-1110NWB or QL-1115NWB advertises `wifi-status`, `wifi-scan`,
`system-report`, and—only when the local administrator has opted in—
`wifi-configure` when attached by USB, while a USB QL-1100 advertises
`system-report` only. Network, Bluetooth, and serial
connections do not advertise these USB administration operations. The
`wifi-configure` route still requires a separate short-lived administrator
grant and a local, one-time approval. It is disabled by default. To opt in on
the printer host, set the flag and restart the service:

```sh
mb-printer config set enable_brother_wifi_configuration true
mb-printer config set enable_brother_wifi_configuration_pairing true
# restart `mb-printer api serve`
```

Disable either independently with the corresponding `config unset` command.
Network-enabled builds aggregate bounded DNS-SD `_ipp._tcp` and
`_ipps._tcp` advertisements into `POST /v1/discovery`. Each IPP candidate keeps
the full advertised URI in `address` and typed scheme, host, resource and
address metadata in `network`; arbitrary TXT properties are not returned.
IPPS candidates are never retried as plaintext. A persisted `kind: "ipp"` connection uses the same endpoint for
Get-Printer-Attributes status and Brother raster Print-Job submission. `ipps://`
uses mandatory hostname and certificate verification; private or self-signed
printer certificates can be added explicitly with `certificatePem`, and there
is no insecure TLS or silent downgrade mode. Brother TCP uses IPP by default (or raster status when
`statusMode` is `raster`); readable serial, USB, BLE and RFCOMM transports use
the Brother status frame. Test-only injected probes remain available without
claiming physical hardware acceptance.

Brother USB diagnostics use `GET /v1/printers/ID/brother/wifi/status`,
`POST /v1/printers/ID/brother/wifi/scan`, and
`GET /v1/printers/ID/brother/report`. Their successful responses are always
`Cache-Control: no-store`; reports are redacted. Wireless configuration is
USB-only and requires a separate short-lived **administrator** grant, not a
normal print grant. The browser sends the complete settings (including its
password) to `POST /v1/printers/ID/brother/wifi/prepare`. The API validates
them and retains only a cryptographic fingerprint, returning a non-secret
review and `approvalId`. A local person must approve that exact request within
120 seconds with `mb-printer api approve-wifi APPROVAL_ID` (or `--yes` for an
intentional non-interactive local workflow). The browser then repeats the same settings and `approvalId` at
`POST /v1/printers/ID/brother/wifi/configure`. The approval is origin-,
administrator-, printer- and settings-bound, consumed once before the USB
command is attempted, and cannot be replayed. Neither response includes the
password; do not log either request. Keep USB or Bluetooth connected as the
recovery path while the printer reboots.

Document routes are `POST /v1/documents/validate`, `/preview`, and
`/export?format=png|pdf`. Other authenticated routes list printer definitions,
discovery and private catalogues, extract La Poste PDF slots (optionally
filtered by repeatable one-based `page:slot` selectors), and expose job
polling, SSE progress and cooperative cancellation. The complete route and
content-type contract is in `openapi.yaml`.
