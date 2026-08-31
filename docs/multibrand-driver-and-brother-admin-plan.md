<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Lean protocol organization and Brother administration plan

## Purpose

This plan covers two related changes:

1. restore Brother diagnostics and network-management features that existed in
   the retired Python CLI; and
2. organize protocol code so another printer family can be added without
   copying CLI and transport logic.

This is an incremental refactor, not a new driver framework. The existing SDK
already has the important common interfaces:

- `PrinterDefinition` and `Protocol` identify models and protocol families;
- `Plan` and `Action` describe transport-neutral printer operations;
- `Transport` executes those actions over USB, BLE, TCP, serial or files; and
- protocol-specific parsers return typed results.

The implementation should extend these types rather than introduce a second
driver registry, transaction-plan system or capability-trait hierarchy.

## Outcomes

When complete:

- manufacturer byte protocols live in `mb-printer-sdk`, not the CLI;
- each protocol family has a focused module containing its command builders and
  parsers;
- adding a protocol means adding a `Protocol` variant, model data, a protocol
  module, fixtures and supported operations;
- existing print plans remain byte-for-byte compatible;
- Brother status, USB information, system reports, wireless status and wireless
  scans work over USB on supported models;
- DNS-SD discovers IPP/IPPS printers and feeds the existing bounded IPP status
  probing path;
- the authenticated loopback API exposes discovery and Brother administration
  to the browser without publishing the hardware service through Cloudflare;
- wireless configuration is dry-run by default, secret-safe and hardware
  gated; and
- the CLI only selects a device, calls SDK operations, asks for confirmation and
  formats results.

## Non-goals

- A dynamic plugin ABI or runtime-loaded third-party drivers.
- A general-purpose `PrinterDriver` trait hierarchy.
- A second transaction-plan type alongside `Plan`.
- Moving every protocol into new files before Brother support ships.
- A universal status schema that hides brand-specific information.
- Arbitrary Brother OID, EEPROM, NVRAM or raw memory access.
- Proving the architecture by moving unrelated Phomemo code before delivering
  Brother functionality.

## Current foundation

The SDK audit used `mb-printer-sdk` `origin/main` commit `19d4341`.

Already available:

- the `Protocol` enum used by printer model definitions and planning;
- transport-neutral `Plan` and `Action` types;
- a common native `Transport` trait;
- fixed-frame response assembly;
- Brother `ESC i S` status construction and strict 32-byte parsing;
- structured USB identity, descriptors and endpoint selection;
- Brother Wi-Fi command encoding;
- parsers for connected state, IPv4 and captured WLAN `VAP` rows;
- redacted `Debug` implementations for Wi-Fi credentials; and
- IPP endpoint status queries.

Missing for the requested feature:

- variable-length bounded response collection;
- Brother system-report request and parsing;
- live WLAN scan execution;
- full wireless status decoding;
- safe selection when identical USB printers are attached;
- standard USB Printer Class device ID and port status in the SDK;
- typed wireless settings instead of string values; and
- one authoritative Brother implementation shared by SDK and CLI.

## Design rules

### Keep the existing dispatch type

`Protocol` remains the built-in protocol selector. Do not add a parallel
`BuiltinDriver`, `DriverId` or `ProtocolFamily`.

Protocol behavior is dispatched explicitly:

```rust
match printer.protocol {
    Protocol::Brother => brother::status_plan(printer),
    Protocol::M110 => phomemo::status_plan(printer),
    _ => Err(PlanError::Unsupported("status")),
}
```

Exhaustive matching makes a new protocol visible to the compiler and keeps the
extension path easy to understand.

Manufacturer/brand can be added later as optional catalogue metadata for UI
grouping. It must not become another protocol-dispatch layer.

### Keep one execution-plan model

`Plan` and `Action` remain the only transport-neutral execution format.
Existing `WaitForResponse` behavior remains compatible with frozen print
fixtures.

Add one action for responses whose size is not known in advance:

```rust
pub enum Action {
    // Existing variants remain unchanged.

    CollectResponse {
        timeout_ms: u64,
        idle_timeout_ms: u64,
        maximum_bytes: usize,
        validation: ResponseValidation,
    },
}
```

`CollectResponse` means:

1. wait up to `timeout_ms` for the first packet;
2. append packets while they continue arriving;
3. finish after `idle_timeout_ms` without another packet;
4. fail before allocating or reading beyond `maximum_bytes`; and
5. validate the assembled response.

Add only the validators needed by observed behavior:

```rust
pub enum ResponseValidation {
    // Existing variants.
    AnyNotification,
    PhomemoNotification,
    BrotherStatus32,

    // New variants.
    BrotherObjbrnet,
    BrotherWifiScan,
    BrotherSystemReport,
}
```

If a real response has a reliable terminator, validation may finish collection
early. Do not create a general response-policy DSL until multiple protocols
need more completion modes.

### Organize protocol-specific code by protocol

Use a small module split:

```text
mb-printer-core/src/
|-- protocol.rs
`-- protocol/
    |-- brother.rs
    `-- brother/
        |-- status.rs
        |-- wifi.rs
        `-- report.rs
```

- `protocol.rs` retains common `Plan`, `Action`, response validation and
  top-level dispatch.
- `brother/status.rs` owns `ESC i S` and `BrotherStatus`.
- `brother/wifi.rs` owns PJL/OBJBRNET commands, settings and parsers.
- `brother/report.rs` owns the configuration-report command, parser and
  redaction.

Do not move other protocol code merely to match this layout. Split another
protocol when it receives meaningful new work or its current module becomes
difficult to maintain.

### Keep results brand-specific

Use typed protocol results:

```rust
pub enum DeviceStatus {
    Brother(BrotherStatus),
    Phomemo(PhomemoStatus),
}
```

The CLI can display common concepts, but the SDK should preserve all fields in
the brand-specific result. Extract a shared status structure only after two or
more consumers demonstrate stable common requirements.

### Add a small operation capability list

Add an operation enum to model data:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrinterOperation {
    Status,
    SystemReport,
    WifiStatus,
    WifiScan,
    WifiConfigure,
    IppStatus,
}
```

`PrinterDefinition` stores a list of supported operations and exposes:

```rust
pub fn supports(&self, operation: PrinterOperation) -> bool;
```

This list answers whether the current release allows an operation. Evidence
quality remains in the existing hardware-acceptance matrix; do not duplicate
`Experimental`, `Provisional` and `Verified` states in runtime model data.

Initial entries must be conservative:

| Model | Initially enabled |
|---|---|
| QL-1110NWB | status; USB system report, Wi-Fi status, scan and configuration |
| QL-1115NWB | status; USB system report, Wi-Fi status, scan and configuration |
| QL-1100 | status; report only after testing; no wireless operations |

`WifiConfigure` is advertised only for the two Wi-Fi-capable models over USB
when the local default-off `enable_brother_wifi_configuration` flag is set.
It remains protected by the separate administrator-grant and local approval
flow; the administrator pairing lifecycle has its own default-off
`enable_brother_wifi_configuration_pairing` flag. Network, Bluetooth and
serial endpoints never advertise it. Physical
testing is strongly recommended validation, not an enablement gate, because
USB or Bluetooth remains the recovery path.

### Keep transport independent of protocol

The native `Transport` trait remains the common I/O interface. Brother modules
must not open USB devices or TCP sockets. USB modules must not contain Brother
PJL/OID constants.

The executor owns:

- complete writes;
- response timeouts and limits;
- multipart assembly;
- monotonic delays;
- progress reporting; and
- transport errors.

Protocol modules own:

- command bytes;
- command ordering;
- expected response validation;
- parsers; and
- typed protocol errors.

### Improve errors only where touched

Use `thiserror` for new Brother parsing and transaction errors. Preserve the
existing error structure unless a touched `Result<_, String>` prevents useful
classification.

Required distinctions:

- unsupported operation;
- ambiguous device;
- device disappeared or changed identity;
- first-response timeout;
- incomplete response;
- oversized response;
- invalid response; and
- transport write/read failure.

Avoid designing an all-encompassing error taxonomy before these cases exist.

## Implementation phases

### Phase 1: Freeze behavior and remove false parity claims

1. Record SDK and CLI baseline commits.
2. Run the current SDK and CLI tests.
3. Freeze Brother print-plan and 32-byte status fixtures.
4. Import relevant Python wireless/report fixtures without live credentials or
   identifiers.
5. Update parity documentation to distinguish:
   - pure codec support;
   - capture parsing;
   - simulated transport;
   - live read-only hardware support; and
   - live mutation support.

Completion:

- current print bytes are reproducible;
- fixtures contain no secrets;
- later byte changes are reviewable.

### Phase 2: Extract the Brother protocol module

1. Move existing Brother status construction and parsing into
   `protocol::brother::status`.
2. Move SDK Wi-Fi commands and parsers into `protocol::brother::wifi`.
3. Preserve compatibility re-exports so existing SDK, WASM and CLI builds keep
   working.
4. Keep behavior unchanged during the move.
5. Remove CLI duplicates only after all CLI callers use the SDK implementation.

Completion:

- there is one authoritative copy of every Brother command/parser;
- all existing fixtures remain byte-identical;
- the CLI contains no Brother constants.

### Phase 3: Add bounded multipart collection

1. Add `Action::CollectResponse`.
2. Implement it in the native executor.
3. Implement equivalent behavior or an explicit unsupported result in WASM
   execution; do not silently ignore the new action.
4. Stop on idle timeout, total timeout or size limit.
5. Retain partial bytes in internal progress where safe, but do not include
   sensitive data in normal errors.
6. Add fake-transport tests for:
   - one packet;
   - every split boundary;
   - timeout before the first packet;
   - timeout after partial data;
   - responses exactly at the maximum;
   - oversized responses; and
   - a transport that never becomes idle.

Initial bounds:

| Operation | First packet | Idle timeout | Maximum |
|---|---:|---:|---:|
| OBJBRNET field | 2 s | 200 ms | 4 KiB |
| WLAN scan | 8 s | 300 ms | 16 KiB |
| System report | 5 s | 300 ms | 64 KiB |

Tune these only from retained hardware captures. No user option may make them
unbounded.

Completion:

- existing `WaitForResponse` fixtures remain unchanged;
- multipart reads are bounded and deterministic;
- malformed responses cannot panic or allocate without limit.

### Phase 4: Make SDK USB selection authoritative

1. Replace CLI VID/PID-only opening with SDK USB discovery.
2. Select by serial number when present.
3. Support bus/address as a session-only selector when serial is unavailable.
4. List candidates and refuse when a selector is ambiguous.
5. Revalidate VID, PID, bus/address and expected serial immediately before open.
6. Keep deterministic printer-class bulk endpoint selection.
7. Add bounded USB Printer Class helpers for:
   - IEEE-1284 `GET_DEVICE_ID`; and
   - `GET_PORT_STATUS`.
8. Parse device ID into manufacturer, model and command-set fields without using
   it as the sole authorization for mutation.

Suggested selectors:

```text
usb:04f9:209b:serial=E12345
usb:04f9:209b:bus=1:address=7
```

Completion:

- identical attached printers cannot cause accidental first-match mutation;
- `mb-printer discover --via usb --include-unknown` exposes identity details;
- non-Brother devices are refused by Brother administration operations.

### Phase 5: Implement read-only Brother administration

#### Status

Keep the existing `ESC i S` request and strict 32-byte parser. Expose it using
the resolved USB device:

```text
mb-printer printer status <printer> --format json
```

Return the existing Brother fields: media size/type, status type, phase and
hardware errors.

#### Wireless status

Query these allowlisted OBJBRNET fields separately:

| Field | OID |
|---|---|
| Connected | `458867` |
| IPv4 | `458967.2` |
| SSID | `458877` |
| Encryption | `458880` |
| Authentication | `458881` |
| Infrastructure | `459138.2` |
| Wireless Direct | `459138.3` |

Validate that every reply names the requested OID. Return field-level errors
for partial status rather than treating malformed data as disconnected.

```text
mb-printer printer wifi status <printer> --format json
```

#### Wireless scan

Execute the observed Python sequence:

1. send the AP scan-start command;
2. wait the observed scan delay;
3. send `INFO AVAILABLEWLAN`;
4. collect the multipart response;
5. validate and parse `VAP` rows; and
6. return SSID, channel, power, enterprise and encryption indicators.

```text
mb-printer printer wifi scan <printer> --format json
```

Retain `--input` for offline fixture parsing, clearly labelled as
capture-derived. Raw hardware capture requires an explicit output file with
owner-only permissions.

#### System configuration report

Add the observed `ESC i X G` request and require the
`<<PRINTER CONFIGURATION>>` marker.

The parser should:

- strip the observed binary prefix;
- preserve unknown sections and fields;
- produce raw text or section-based JSON;
- reject a missing report marker;
- reject oversized/incomplete responses; and
- redact serial, SSID, IP, MAC and other local identifiers by default.

```text
mb-printer printer report <printer> --report-format text --output report.txt
mb-printer printer report <printer> --report-format json --output report.json
```

Unredacted/raw output requires an explicit unsafe option and owner-only output
file. Report contents must not enter telemetry or normal debug logs.

Completion:

- all four read-only operations work on the QL-1110NWB;
- offline fixtures and live parsers share the same SDK code;
- each operation is independently represented in model capabilities and
  hardware evidence.

### Phase 6: Add guarded wireless configuration

Start only after status, scan and report have passed live hardware testing.

1. Replace string encryption/authentication fields with typed Brother enums.
2. Preserve the existing numeric mappings with golden tests.
3. Accept a password only from stdin or a dedicated file descriptor.
4. Never accept passwords in argv, URLs, environment variables or persistent
   config.
5. Redact password-bearing types from `Debug`.
6. Use `zeroize` for the password and encoded password buffers if practical.
7. Keep USB as the only initial mutation transport.
8. Default to a non-secret configuration summary/dry run.
9. Require `--apply` before sending.
10. Require `--yes` for noninteractive execution.
11. Show exact printer identity, SSID, security and reboot choice before
    interactive confirmation.
12. Never retry a credential-bearing write or reboot automatically.
13. Report a complete write as “accepted”, not “configured”.
14. After an optional reboot, observe USB reconnection and query status.
15. Keep `WifiConfigure` disabled by default behind
    `enable_brother_wifi_configuration`, and restricted to the USB QL-1110NWB
    and QL-1115NWB when explicitly enabled.
    Run a real USB configure, reboot and verification test as recommended
    validation. USB or Bluetooth remains the recovery path if the printer
    cannot rejoin its prior network.

```text
printf '%s' "$WIFI_PASSWORD" | mb-printer printer wifi configure <printer> --ssid <ssid> \
  --authentication wpa-psk \
  --encryption tkip-aes \
  --password-stdin

printf '%s' "$WIFI_PASSWORD" | mb-printer printer wifi configure <printer> --ssid <ssid> \
  --authentication wpa-psk \
  --encryption tkip-aes \
  --password-stdin \
  --no-reboot
```

Completion:

- configuration succeeds on a disposable network;
- post-reboot status matches the requested settings;
- printing works on the new network;
- USB or Bluetooth recovery remains available if network recovery is needed;
- the test password is absent from argv, output, errors, traces and fixtures.

### Phase 7: Add DNS-SD discovery

DNS-SD is part of this delivery and should reuse the existing IPP endpoint
status implementation rather than introduce another printer abstraction.

1. Add a native discovery backend for `_ipp._tcp` and `_ipps._tcp`.
2. Keep discovery and probing distinct:
   - discovery resolves service name, host, port, resource path and TXT data;
   - the existing IPP client probes a resolved endpoint for state and media.
3. Bound browse duration, number of services, TXT size and address resolution.
4. Deduplicate services by stable DNS-SD identity, not display name alone.
5. Preserve whether an endpoint came from IPP or IPPS.
6. Never silently downgrade IPPS to IPP.
7. Normalize common TXT keys such as `rp`, `ty`, `product`, `UUID` and `TLS`
   while preserving unknown keys within bounds.
8. Make discovery injectable so tests do not require multicast networking.
9. Correlate USB and DNS-SD identities only when serial/UUID/model evidence is
   strong enough; otherwise return separate candidates.

CLI surface:

```text
mb-printer discover --via network --timeout 3s --format json
mb-printer discover --via network --timeout 3s --probe --format json
```

Completion:

- a QL-1110NWB advertised over IPP/IPPS is discovered on macOS and Linux;
- its resolved endpoint passes through the existing IPP status parser;
- discovery terminates deterministically and handles duplicate or malformed
  services without panicking.

### Phase 8: Add authenticated browser APIs

Browser support is part of this delivery. The browser talks to the existing
loopback service on `127.0.0.1`; the Cloudflare tunnel serves the editor but
must not proxy the local hardware API.

1. Extend the versioned OpenAPI document before implementing handlers.
2. Expose authenticated, origin-bound endpoints for:
   - USB and DNS-SD printer discovery;
   - common printer status;
   - Brother wireless status;
   - Brother wireless scan; and
   - redacted Brother system reports.
3. Use the same SDK operations and parsers as the CLI; API handlers contain no
   protocol bytes or OIDs.
4. Preserve exact Host validation, the configured origin allowlist and valid
   Private Network Access preflights.
5. Apply `Cache-Control: no-store` to discovery, status, scan and report
   responses and exclude them from service-worker caches.
6. Add bounded per-origin rate limits for discovery, status, WLAN scans and
   reports.
7. Do not expose unredacted or raw reports through the browser API initially.
8. Add USB-only wireless configuration with these safeguards; complete the
   Phase 6 real-printer exercise as recommended validation rather than an
   enablement prerequisite:
   - use a separate short-lived administration grant rather than the ordinary
     print grant;
   - require a fresh local confirmation for each mutation;
   - accept the password only in the request body over loopback;
   - never echo, persist, log or cache the password or encoded command; and
   - return accepted/verification state rather than claiming immediate success.
9. Update the label editor's local-printer client to use these endpoints and
   present unsupported, timeout and permission states explicitly.
10. Add browser integration tests for allowed and rejected origins, PNA
    preflight, expired/revoked grants, rate limits and no-store behavior.

Completion:

- the tunneled editor can discover and inspect local printers through the
  authenticated loopback API;
- read-only Brother operations work without exposing raw sensitive data;
- browser Wi-Fi mutation is available only for USB QL-1110NWB and QL-1115NWB,
  requires administrator authorization and local confirmation, and is backed
  by recommended real-printer validation; and
- no local hardware endpoint is routed through Cloudflare.

### Phase 9: Clean up and document the extension recipe

1. Remove compatibility re-exports only in a semver-appropriate release.
2. Remove obsolete CLI protocol code and transport duplication.
3. Document the steps for adding a protocol:
   - add a `Protocol` variant;
   - add printer model definitions;
   - add a protocol module;
   - emit existing `Action` variants or the bounded response action;
   - add typed parsers/results;
   - add `PrinterOperation` entries;
   - add byte-level fixtures; and
   - add operation-specific hardware evidence.
4. Apply the pattern to another protocol only when that protocol receives real
   feature work.

Completion:

- the extension guide can be followed without editing CLI protocol code or
  transport implementations;
- no speculative trait or registry is required.

## Testing

### Protocol tests

- exact Brother command bytes and ordering;
- unchanged Brother print fixtures;
- exact 32-byte status validation;
- OBJBRNET field parsing;
- UTF-8/non-ASCII SSID handling;
- VAP parsing with malformed and unknown rows;
- system-report markers, sections and redaction;
- wireless enum-to-wire mappings; and
- Python fixture equivalence.

### Executor tests

- multipart reads across all split boundaries;
- first-response and idle timeouts;
- exact-limit and oversized replies;
- no infinite responder loop;
- complete versus short writes; and
- sensitive response/error handling.

### USB tests

- deterministic endpoint selection;
- serial and bus/address selection;
- ambiguity refusal;
- identity change before open;
- IEEE-1284 length validation;
- port-status bit decoding; and
- Brother vendor/model checks for administration.

### CLI tests

- JSON and text output;
- stable failure exit codes;
- offline input clearly marked as capture-derived;
- dry-run default;
- `--apply` and confirmation behavior;
- password stdin handling;
- owner-only report/capture output; and
- absence of secrets in argv, output, errors and traces.

### Build matrix

- core without native features;
- native minimal build;
- USB build on macOS and Linux;
- Wi-Fi/IPP feature build;
- combined USB and Wi-Fi build; and
- Bluetooth independently, so Brother USB support does not pull macOS-incompatible
  D-Bus dependencies.

Fuzz or property-test device-controlled parsers where practical. Required
invariants are no panic, bounded allocation and deterministic error handling.

## Hardware acceptance

Use QL-1110NWB as the first reference unit:

1. record CLI/SDK commits, platform, model and firmware;
2. resolve and retain protected USB identity evidence;
3. compare 32-byte status with physical media/error state;
4. retrieve and parse the complete system report;
5. compare non-secret report fields with the printer UI;
6. compare wireless status with the printer UI;
7. scan and confirm known nearby access points;
8. configure a disposable SSID over USB;
9. reboot and verify the resulting network state;
10. print through the new network;
11. confirm USB or Bluetooth remains available as the recovery path; and
12. retain redacted traces and acceptance metadata.

Repeat read-only testing on QL-1115NWB. Enable QL-1100 report support only after
testing that exact model. Do not infer wireless support from a related model.

## Security requirements

- Device ambiguity fails closed for mutation.
- All reads and device-derived allocations are bounded.
- Reports are redacted by default and excluded from telemetry.
- Raw/unredacted files use owner-only permissions.
- Password input is stdin/file-descriptor only.
- Reversible password bytes are treated as secrets.
- Mutation is explicit, confirmed and never automatically retried.
- Public commands expose typed operations, not arbitrary OIDs or raw bytes.
- Browser operations retain origin-bound authentication, Host validation and
  valid Private Network Access preflights.
- Browser responses are not cached, and printer administration remains on
  loopback rather than being routed through Cloudflare.

## Deferred follow-ups

Only these speculative architecture changes remain deferred:

- dynamic third-party drivers;
- a universal normalized status model beyond fields required by the CLI/API;
  and
- broader transport-module file reorganization.

## Definition of done

- Existing print fixtures pass without unexplained byte changes.
- `Protocol`, `Plan`, `Action` and `Transport` remain the common
  architecture.
- Brother commands and parsers have one SDK-owned implementation.
- Stable USB selection prevents ambiguous mutation.
- Brother status, wireless status, wireless scan and system report pass retained
  QL-1110NWB hardware tests.
- DNS-SD discovers and probes the QL-1110NWB IPP/IPPS service on macOS and
  Linux.
- The authenticated loopback API and editor expose discovery plus read-only
  Brother administration with origin, PNA, rate-limit and no-store tests.
- Wireless configuration is disabled by default and, when explicitly enabled,
  is advertised only for USB QL-1110NWB and QL-1115NWB; it retains
  administrator-grant plus local approval gates. Disposable-network,
  reboot, verification, printing and recovery-path checks are recommended
  physical validation before operational rollout, not an enablement gate.
- Password and report leakage tests pass.
- macOS builds Brother USB administration without Bluetooth/D-Bus.
- Adding another protocol does not require new CLI or transport protocol logic.
