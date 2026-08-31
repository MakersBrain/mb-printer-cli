<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# mb-printer

`mb-printer` is the native command and authenticated loopback service for the
Makers' Brain printer platform. This repository is a clean-room Rust
implementation and does not import the retired Python package.

The service binds only to a loopback address. Run `mb-printer service pair` to
create a short-lived pairing secret, then exchange it from an explicitly
allowed web origin. Tokens are origin-bound, expiring, revocable, and stored
only as salted hashes.

The CLI is organized around saved physical printers. Discover over every
compiled transport, save a friendly name, and then reuse it for printing,
status, diagnostics, Wi-Fi administration, and cloud publication:

```sh
mb-printer discover
mb-printer printer add office-label
mb-printer print label.mb-label.json
mb-printer printer status
```

`printer add` is interactive on a terminal. Automation supplies the endpoint
and model explicitly:

```sh
mb-printer printer add warehouse \
  --model tspl-generic \
  --endpoint tcp://warehouse-printer.local:9100
mb-printer print label.mb-label.json --printer warehouse
```

The sole saved printer is selected automatically. With several printers, pass
`--printer` or run `mb-printer printer default NAME`. Direct `--model` and
`--transport` overrides remain available for one-off and dry-run workflows.

Build with `--features usb` for CLI and SDK libusb bulk discovery/execution,
`--features network` for SDK Brother Wi-Fi/IPP helpers and bounded DNS-SD
discovery of both `_ipp._tcp` and `_ipps._tcp`, or
`--features brother-admin` for both. `--features bluetooth` adds portable BLE
discovery, serialized writes and notification waits without pulling vendored
D-Bus into macOS builds. On Linux, use `--features bluetooth-linux` for the
complete BLE, vendored D-Bus and Bluetooth Classic RFCOMM stack. Tagged full
artifacts combine `brother-admin` with the platform Bluetooth feature. Minimal
source builds retain an empty default feature set.

Brother read-only administration uses the saved stable USB endpoint rather than
the first matching VID/PID:

```sh
mb-printer printer add office-label \
  --model ql-1110nwb \
  --endpoint usb-device:04f9:209b:001:007
mb-printer printer status office-label
mb-printer printer wifi status office-label
mb-printer printer wifi scan office-label
mb-printer printer report office-label --output report.json
```

System reports are redacted by default and written with owner-only permissions.
Use `--unsafe-unredacted` only for a protected local diagnostic artifact.

Network discovery preserves the advertised secure scheme and never retries an
IPPS endpoint as plaintext. The CLI and loopback discovery endpoint apply hard
bounds to browse time, service count, TXT bytes and advertised addresses:

```sh
mb-printer discover --via network --timeout 3s
mb-printer discover --via network --timeout 3s --probe
```

La Poste PDF commands use the SDK's shared pure-Rust PDF normalizer, validate A4
media, extract occupied grid cells with provenance, and export exact
63.5 x 33.9 mm stamp pages. Android imports enumerate every APK split through
ADB and emit registered private-only `.mb-assets` bundles with strict path,
size, and credential filters. `document`, `printer test`, `printer wifi`, and
`printer report` expose document preparation and device workflows without
adding protocol-specific top-level commands.

The CLI and service share typed, versioned printer definitions and catalogue
metadata. Printer-store writes are atomic, owner-only, and cross-process locked.
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

All commands support `--format auto|pretty|text|json`. Command results go to
stdout; warnings, progress, and tracing go to stderr. Use `--log-level`, `-v`,
`-q`, and `--log-format pretty|json` to control Rust tracing independently.
Exit status is 0 for success (including non-strict partial discovery), 1 for an
operational failure, and 2 for command-line usage errors.

Licensed AGPL-3.0-or-later.
