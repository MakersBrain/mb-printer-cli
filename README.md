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
capture, file, TCP, or serial-device transports. For example:

```sh
mb-printer print label.mb-label.json --model m110 --dry-run --capture job.json
mb-printer print label.mb-label.json --model tspl-generic --transport tcp://printer:9100
```

Build with `--features usb` for libusb bulk discovery/execution, or
`--features bluetooth` for BLE discovery, serialized writes and notification
waits. On Linux, Bluetooth Classic uses BlueZ paired-device discovery and
`rfcomm:MAC[@CHANNEL]`; the SDK binds the selected `/dev/rfcommN` endpoint.
Tagged release artifacts carry the `-full` suffix and are built with both
`usb,bluetooth`; Linux CI installs the D-Bus development headers required by
the BLE backend. Minimal source builds retain an empty default feature set.

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

USB and BLE implementations are feature-gated. Bluetooth Classic uses BlueZ's
RFCOMM tooling; actual hardware discovery and
printing still depend on platform permissions, drivers, and device-specific
endpoint/characteristic values. Dry-run captures preserve the logical plan,
physical writes, timing, and concatenated byte stream.

Licensed AGPL-3.0-or-later.
