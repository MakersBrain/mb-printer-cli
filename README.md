<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# mb-printer

`mb-printer` is the native command and authenticated loopback service for the
Makers' Brain printer platform. This repository is a clean-room Rust
implementation and does not import the retired Python package.

The service binds only to a loopback address. Run `mb-printer api pair` to
create a short-lived pairing secret, then exchange it from an explicitly
allowed web origin. Tokens are origin-bound, expiring, revocable, and stored
only as salted hashes.

The CLI validates strict v4 documents through `mb-printer-core`, creates PNG
previews plus PDF/SVG/tiled exports, generates the SDK's timed protocol plans, and executes them through
capture, file, TCP, verified IPPS/IPP, or serial-device transports. For example:

```sh
mb-printer print label.mb-label.json --model m110 --dry-run --capture job.json
mb-printer print label.mb-label.json --model tspl-generic --transport tcp://printer:9100
mb-printer print label.mb-label.json --model ql-1110nwb --transport ipps://brother.local:631/ipp/print
```

Build with `--features usb` for CLI and SDK libusb bulk discovery/execution,
`--features network` for SDK Brother Wi-Fi/IPP helpers and bounded DNS-SD
discovery of both `_ipp._tcp` and `_ipps._tcp`, or
`--features brother-admin` for both. `--features bluetooth` adds portable BLE
discovery, serialized writes and notification waits without pulling vendored
D-Bus into macOS builds. On Linux, use `--features bluetooth-linux` for the
complete BLE, vendored D-Bus and Bluetooth Classic RFCOMM stack. Tagged full
artifacts combine `brother-admin` with the platform Bluetooth feature. Minimal
source builds retain an empty default feature set.

Brother read-only administration uses stable USB identity rather than the
first matching VID/PID. List devices, then pass the emitted selector (or the
exact USB serial number) when more than one Brother printer is attached:

```sh
mb-printer usb list
mb-printer status --device usb-device:04f9:209b:001:007
mb-printer wifi status --device usb-device:04f9:209b:001:007
mb-printer wifi scan --device usb-device:04f9:209b:001:007
mb-printer usb report --device usb-device:04f9:209b:001:007 --output report.json
```

System reports are redacted by default and written with owner-only permissions.
Use `--unsafe-unredacted` only for a protected local diagnostic artifact.

Network discovery preserves the advertised secure scheme and never retries an
IPPS endpoint as plaintext. The CLI and loopback discovery endpoint apply hard
bounds to browse time, service count, TXT bytes and advertised addresses:

```sh
mb-printer network discover --timeout-ms 3000 --max-services 64
mb-printer network status --timeout-ms 3000 --max-services 64
```

La Poste PDF commands use the SDK's shared pure-Rust PDF normalizer, validate A4
media, extract occupied grid cells with provenance, and export exact
63.5 x 33.9 mm stamp pages. Android imports enumerate every APK split through
ADB and emit registered private-only `.mb-assets` bundles with strict path,
size, and credential filters. `document fields`, `document import-svg`,
`density-test`, `wifi`, and `usb` expose document preparation and pure
device-protocol/reporting workflows.

The service persists bounded connection definitions and catalogue metadata.
Discovery and status/media results use injectable backend boundaries for
deterministic tests. Browser access uses an exact origin allowlist, independent
loopback Host validation and valid-preflight-only Private Network Access. See
`docs/openapi.yaml` for the versioned wire contract.

USB, network administration and BLE implementations are feature-gated.
Bluetooth Classic uses the Linux-only `bluetooth-linux` feature and BlueZ's
RFCOMM tooling; actual hardware discovery and
printing still depend on platform permissions, drivers, and device-specific
endpoint/characteristic values. Dry-run captures preserve the logical plan,
physical writes, timing, and concatenated byte stream.

Licensed AGPL-3.0-or-later.
