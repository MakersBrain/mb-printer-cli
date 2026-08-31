<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Printer-centric CLI simplification plan

## Goal

Make `mb-printer` follow the tasks users perform instead of exposing USB,
network, and Wi-Fi implementation boundaries as top-level commands. Preserve
the existing capabilities under a smaller hierarchy, make discovery cover all
available transports, and make a named printer the reusable target for status,
administration, testing, printing, and cloud publication.

## Command hierarchy

```text
mb-printer discover

mb-printer printer list
mb-printer printer add <name> [--model <model>] [--endpoint <uri>...]
mb-printer printer show <printer>
mb-printer printer rename <printer> <new-name>
mb-printer printer remove <printer>
mb-printer printer default [<printer>] [--clear]
mb-printer printer status [<printer>]
mb-printer printer test [<printer>] --pattern density
mb-printer printer report [<printer>] --output <path>
mb-printer printer wifi scan [<printer>]
mb-printer printer wifi status [<printer>]
mb-printer printer wifi configure [<printer>] ...
mb-printer printer endpoint list|add|remove|prefer ...
mb-printer printer settings show|set|unset ...

mb-printer print <input> [--printer <printer>]

mb-printer document inspect|validate|fields|import-svg|render ...
mb-printer document laposte print|extract ...

mb-printer model list
mb-printer config ...
mb-printer asset ...
mb-printer service ...
mb-printer cloud ...
```

Old protocol-oriented top-level commands are not retained as aliases. Stored
data is migrated because losing configured printers is unacceptable, but the
new command hierarchy stands on its own.

## Happy path

Interactive setup discovers printers and asks the user to choose one:

```sh
mb-printer printer add office-label
mb-printer print shipping-label.mb-label.json
mb-printer printer status
```

If only one printer is saved, it is selected automatically. When several are
saved, an explicit `--printer` or configured default is required.

Non-interactive setup contains all required information in one command:

```sh
mb-printer printer add warehouse \
  --model tspl-generic \
  --endpoint tcp://warehouse-printer.local:9100
```

Multiple endpoints require an explicit preferred endpoint:

```sh
mb-printer printer add office-label \
  --model ql-1110nwb \
  --endpoint usb-device:04f9:209b:001:007 \
  --endpoint ipps://office-label.local/ipp/print \
  --preferred ipps://office-label.local/ipp/print
```

## Managed printers

The CLI and loopback service share a versioned, typed printer store. Each
printer has:

- a stable ID and unique friendly name;
- a supported model ID;
- one or more typed endpoints;
- exactly one preferred print endpoint when several endpoints exist;
- persistent print settings;
- optional description and last-known status/media.

Endpoints are typed variants for file, TCP, serial, USB, BLE, RFCOMM, and
IPP/IPPS. Unknown fields and malformed endpoints are rejected. Writes are
atomic, owner-only where supported, and protected by a cross-process file lock.
The previous connection-array representation is read and migrated into the new
schema. Unknown future schema versions are never overwritten.

Printer resolution order is:

1. the explicit printer;
2. the configured default;
3. the sole saved printer;
4. otherwise, an actionable error.

Endpoint selection is operation-aware. Printing uses the preferred endpoint.
Brother Wi-Fi and system-report operations select the saved USB endpoint even
when IPPS is preferred for printing. Status selects a compatible endpoint.
Printing never fails over after bytes may have been sent, and IPPS is never
retried as plaintext IPP.

## Unified discovery

`mb-printer discover` runs the compiled discovery backends independently and
concurrently:

- USB printer-class discovery;
- likely printer serial devices;
- IPP and IPPS DNS-SD discovery;
- BLE discovery;
- Bluetooth Classic/RFCOMM on supported Linux builds.

Useful options are:

```text
--via usb,serial,network,ble,rfcomm
--timeout 3s
--probe
--include-unknown
--strict
```

Default output excludes unrelated USB, serial, and BLE devices. Exact endpoints
are deduplicated and sorted deterministically. A display name alone is never
used to merge physical printers. Successful backends retain their results when
another backend fails. Partial failures become structured warnings; `--strict`
makes them fatal. Explicitly requesting a backend distinguishes not compiled,
not supported, permission denied, timeout, and scan failure.

## Result output

Every command accepts the global option:

```text
--format auto|pretty|text|json
```

`auto` selects pretty output on a terminal and stable tab-delimited/text output
when redirected. Explicit `pretty` and `text` avoid context-dependent output.
JSON is the supported automation contract and uses one envelope:

```json
{
  "schemaVersion": 1,
  "data": {},
  "warnings": []
}
```

Command results are written only to stdout. Warnings, progress, tracing, and
errors are written only to stderr. File destinations continue to use
`--output`; `--format` never means a path.

Exit status is `0` for success, including non-strict partial discovery and a
graceful long-running service shutdown; `1` for an operational failure; `2` for
Clap usage errors; and the platform's conventional signal status when a
short-lived command is terminated externally.

## Tracing and safety

Operational tracing is independent of result formatting:

```text
--log-level error|warn|info|debug|trace
--log-format pretty|json
-v / -vv
-q
```

`--log-level`, verbosity flags, and `-q` are mutually exclusive. `RUST_LOG`
remains available for advanced filtering, and `MB_PRINTER_LOG_FORMAT` controls
serialization. Structured spans include operation, protocol/transport, printer
or job identity where safe, elapsed time, and outcome.

Passwords, bearer and pairing secrets, document content, raw print payloads,
private certificate material, and unredacted reports must never enter tracing.
JSON tracing is newline-delimited and never contaminates JSON command results.

## Implementation phases

1. Shared foundations: typed printer store, atomic/locked persistence, result
   renderer, error categories, exit behavior, and layered tracing.
2. Printer management: add/list/show/rename/remove/default, endpoint and setting
   management, status, reports, Wi-Fi operations, tests, and saved-printer print
   resolution.
3. Discovery aggregation: independent bounded backends, normalization,
   filtering, deterministic ordering, partial warnings, and API reuse.
4. Command regrouping: document, model, asset, service, and cloud operations in
   the reduced hierarchy without old top-level aliases.
5. Verification and documentation: parser/process tests, store migration and
   permission tests, feature-matrix tests, output/tracing separation, redaction,
   quick start, and command reference updates.

## Definition of done

- Top-level help contains only task-oriented commands.
- One discovery command covers every transport compiled into the binary.
- A user can discover, save, select, inspect, and print without repeatedly
  entering a model or transport URI.
- One saved printer is selected automatically; multiple printers have an
  explicit, understandable selection rule.
- Existing capabilities have intentional locations in the new hierarchy.
- Pretty, text, and versioned JSON results behave consistently.
- Tracing is structured, independently formatted, stderr-only, and redacted.
- The previous persisted connection data migrates safely.
- Default, network, USB, and all-feature test configurations pass.
