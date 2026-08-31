<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Installation

Build the default portable binary with `cargo build --release --locked`.
`--features usb` adds vendored-libusb bulk transport in both the CLI and SDK.
`--features network` enables SDK Brother Wi-Fi/IPP helpers, while
`--features brother-admin` enables both USB and network support. The portable
`--features bluetooth` build adds BLE without vendored D-Bus and is suitable
for macOS and Windows. Linux builds that need BLE plus Bluetooth Classic
RFCOMM should use `--features bluetooth-linux`; that feature vendors D-Bus and
requires BlueZ at runtime. A portable `bluetooth` build performed on Linux uses
the system D-Bus development package through `pkg-config` instead.

Typical full builds are:

```sh
# macOS or Windows
cargo build --release --locked --features brother-admin,bluetooth

# Linux, including RFCOMM
cargo build --release --locked --features brother-admin,bluetooth-linux
```

Copy `target/release/mb-printer` onto `PATH`. Configure an origin before
starting the service:

```sh
mb-printer config set allowed_origins https://labels.example
mb-printer api serve
```

Brother USB Wi-Fi configuration is disabled by default. If you need it, opt
in explicitly before starting the service:

```sh
mb-printer config set enable_brother_wifi_configuration true
mb-printer config set enable_brother_wifi_configuration_pairing true
```

The second switch controls administrator-secret issuance and exchange. Keep it
false when Wi-Fi configuration should be available only through non-browser
local workflows.

There is no `mbprint` or Python compatibility command.

Release maintainers should follow [the release process](release.md); local
candidate generation never creates a tag or publishes an artifact.
