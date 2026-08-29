<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Third-party dependency notices

The authoritative, versioned dependency inventory is `Cargo.lock`. Release CI
generates `cargo metadata` SBOM JSON and SHA-256 checksums beside each binary.

Principal runtime crates include Axum/Tokio/Tower HTTP (MIT), Clap (MIT OR
Apache-2.0), Serde (MIT OR Apache-2.0), image (MIT OR Apache-2.0), serialport
(MPL-2.0), optional rusb/libusb (MIT; libusb LGPL-2.1-or-later), optional
btleplug (MIT OR Apache-2.0), and the Makers' Brain SDK crates
(AGPL-3.0-or-later). Hayro and related PDF-rendering crates arrive through the
SDK and retain their declared Cargo package licenses.

This summary is informational; dependency license files and package metadata
control their terms. Distributors should archive the SBOM and corresponding
crate sources for the exact locked release.
