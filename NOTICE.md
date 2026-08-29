<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# Notices and provenance

The `mb-printer-cli` source is an original clean-room implementation licensed
AGPL-3.0-or-later. It links to the sibling `mb-printer-sdk`, also
AGPL-3.0-or-later.

Protocol behavior and timing are compatibility facts derived from the frozen
Python implementation and reviewed against `transcriptionstream/phomymo`
commit `1f58d3f0e7f941b9143277cda828380149e56855`. No Ateliera or vendor APK code,
assets, credentials, paid material, or private API responses are included.

PDF rasterization and La Poste normalization use the shared
`mb_printer_core::pdf_import` implementation, keeping native and WASM callers
on one raster contract. Other Rust dependencies and exact versions are
recorded in `Cargo.lock` and summarized in `THIRD_PARTY_LICENSES.md`.
Imported `.mb-assets` content is always private, non-redistributable local data
and is excluded from source control.
