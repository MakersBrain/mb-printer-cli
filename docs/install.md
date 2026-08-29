<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Installation

Build the default portable binary with `cargo build --release --locked`.
`--features usb` adds vendored-libusb bulk transport. `--features bluetooth`
adds BLE and requires the platform Bluetooth stack; Linux builds need D-Bus
development headers and `pkg-config`. The combined release build uses
`--all-features`.

Copy `target/release/mb-printer` onto `PATH`. Configure an origin before
starting the service:

```sh
mb-printer config set allowed_origins https://labels.example
mb-printer api serve
```

There is no `mbprint` or Python compatibility command.

Release maintainers should follow [the release process](release.md); local
candidate generation never creates a tag or publishes an artifact.
