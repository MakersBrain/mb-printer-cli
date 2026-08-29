// SPDX-License-Identifier: AGPL-3.0-or-later
#[cfg(test)]
use mb_printer_core::raster::MonoRaster;
use mb_printer_core::{laposte as core_laposte, raster::Dither};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, str::FromStr};
use thiserror::Error;

pub const STAMP_WIDTH_MM: f64 = 63.5;
pub const STAMP_HEIGHT_MM: f64 = 33.9;
pub const A4_TOLERANCE_MM: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaposteFormat {
    L24A,
    L24B,
    L21A,
    L18A,
    L16A,
    L14A,
    L12A,
    L24ASheet,
}

impl LaposteFormat {
    pub fn slots(self) -> usize {
        match self {
            Self::L24A | Self::L24B | Self::L24ASheet => 24,
            Self::L21A => 21,
            Self::L18A => 18,
            Self::L16A => 16,
            Self::L14A => 14,
            Self::L12A => 12,
        }
    }
    pub const fn grid(self) -> Grid {
        match self {
            Self::L24A | Self::L24ASheet => Grid::new(3, 8, 7.2, 13.1, 66.0, 33.9),
            Self::L24B => Grid::new(3, 8, 5.0, 3.5, 68.25, 36.7),
            Self::L21A => Grid::new(3, 7, 7.2, 17.2, 66.0, 38.1),
            Self::L18A => Grid::new(3, 6, 7.2, 15.1, 66.0, 46.6),
            Self::L16A => Grid::new(2, 8, 22.5, 13.5, 101.6, 33.9),
            Self::L14A => Grid::new(2, 7, 22.5, 17.2, 101.6, 38.1),
            Self::L12A => Grid::new(2, 6, 22.5, 25.6, 101.6, 42.3),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Grid {
    pub columns: u8,
    pub rows: u8,
    pub origin_x_mm: f64,
    pub origin_y_mm: f64,
    pub pitch_x_mm: f64,
    pub pitch_y_mm: f64,
}
impl Grid {
    const fn new(
        columns: u8,
        rows: u8,
        origin_x_mm: f64,
        origin_y_mm: f64,
        pitch_x_mm: f64,
        pitch_y_mm: f64,
    ) -> Self {
        Self {
            columns,
            rows,
            origin_x_mm,
            origin_y_mm,
            pitch_x_mm,
            pitch_y_mm,
        }
    }
    pub fn slots(self, page: u32) -> Vec<Slot> {
        (0..self.rows)
            .flat_map(|row| {
                (0..self.columns).map(move |column| Slot {
                    source_page: page,
                    slot: u32::from(row) * u32::from(self.columns) + u32::from(column) + 1,
                    x_mm: self.origin_x_mm + f64::from(column) * self.pitch_x_mm,
                    y_mm: self.origin_y_mm + f64::from(row) * self.pitch_y_mm,
                    width_mm: STAMP_WIDTH_MM,
                    height_mm: STAMP_HEIGHT_MM,
                })
            })
            .collect()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slot {
    pub source_page: u32,
    pub slot: u32,
    pub x_mm: f64,
    pub y_mm: f64,
    pub width_mm: f64,
    pub height_mm: f64,
}

/// Detect occupied slots from a normalized A4 grayscale raster. A one-millimetre rim is
/// ignored so cut guides do not turn an empty slot into a postage mark.
pub fn detect_occupied(
    image: &image::GrayImage,
    format: LaposteFormat,
    page: u32,
    dpi: u16,
) -> Vec<Slot> {
    let scale = f64::from(dpi) / 25.4;
    format
        .grid()
        .slots(page)
        .into_iter()
        .filter(|slot| {
            let rim = (scale * 1.0).round() as u32;
            let x = (slot.x_mm * scale).round() as u32 + rim;
            let y = (slot.y_mm * scale).round() as u32 + rim;
            let width = (slot.width_mm * scale).round() as u32;
            let height = (slot.height_mm * scale).round() as u32;
            let x1 = (x + width.saturating_sub(2 * rim)).min(image.width());
            let y1 = (y + height.saturating_sub(2 * rim)).min(image.height());
            let mut dark = 0u64;
            let mut total = 0u64;
            for yy in y.min(image.height())..y1 {
                for xx in x.min(image.width())..x1 {
                    total += 1;
                    if image.get_pixel(xx, yy)[0] < 240 {
                        dark += 1;
                    }
                }
            }
            total > 0 && dark * 1000 >= total * 5
        })
        .collect()
}
impl fmt::Display for LaposteFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl FromStr for LaposteFormat {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_uppercase().as_str() {
            "L24A" => Ok(Self::L24A),
            "L24B" => Ok(Self::L24B),
            "L21A" => Ok(Self::L21A),
            "L18A" => Ok(Self::L18A),
            "L16A" => Ok(Self::L16A),
            "L14A" => Ok(Self::L14A),
            "L12A" => Ok(Self::L12A),
            "SHEET" | "L24A_SHEET" => Ok(Self::L24ASheet),
            _ => Err(format!("unknown La Poste format {value}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourcePage {
    pub page: u32,
    pub width_mm: f64,
    pub height_mm: f64,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("cannot normalize PDF: {0}")]
    Pdf(#[from] mb_printer_core::pdf_import::PdfImportError),
    #[error("cannot read PDF: {0}")]
    Io(#[from] std::io::Error),
    #[error("page {page} is {width_mm:.2} x {height_mm:.2} mm, not A4 within {A4_TOLERANCE_MM} mm")]
    NonA4 {
        page: u32,
        width_mm: f64,
        height_mm: f64,
    },
    #[error("PDF has no pages")]
    Empty,
}

pub fn validate_a4(path: &Path, selected: &[u32]) -> Result<Vec<SourcePage>, Error> {
    let pages = normalize_selected(path, 30, selected)?;
    if pages.is_empty() {
        return Err(Error::Empty);
    }
    let mut output = Vec::new();
    for page in pages {
        let width_mm = page.width_um as f64 / 1000.0;
        let height_mm = page.height_um as f64 / 1000.0;
        if (width_mm - 210.0).abs() > A4_TOLERANCE_MM || (height_mm - 297.0).abs() > A4_TOLERANCE_MM
        {
            return Err(Error::NonA4 {
                page: page.page,
                width_mm,
                height_mm,
            });
        }
        output.push(SourcePage {
            page: page.page,
            width_mm,
            height_mm,
        });
    }
    Ok(output)
}

fn normalize_selected(
    path: &Path,
    dpi: u16,
    selected: &[u32],
) -> Result<Vec<core_laposte::NormalizedPage>, Error> {
    Ok(normalize_bytes(std::fs::read(path)?, dpi, selected)?)
}
fn normalize_bytes(
    bytes: Vec<u8>,
    dpi: u16,
    selected: &[u32],
) -> Result<Vec<core_laposte::NormalizedPage>, mb_printer_core::pdf_import::PdfImportError> {
    let mut pages = mb_printer_core::pdf_import::normalize(bytes, dpi, false, 64 * 1024 * 1024)?;
    if !selected.is_empty() {
        pages.retain(|page| selected.contains(&page.page));
    }
    Ok(pages)
}

pub fn format_code(format: LaposteFormat) -> &'static str {
    match format {
        LaposteFormat::L24A => "L24A",
        LaposteFormat::L24B => "L24B",
        LaposteFormat::L21A => "L21A",
        LaposteFormat::L18A => "L18A",
        LaposteFormat::L16A => "L16A",
        LaposteFormat::L14A => "L14A",
        LaposteFormat::L12A => "L12A",
        LaposteFormat::L24ASheet => "L24A_SHEET",
    }
}
pub fn extract_pdf(
    path: &Path,
    format: LaposteFormat,
    dpi: u16,
    selected: &[u32],
) -> Result<Vec<core_laposte::Stamp>, Box<dyn std::error::Error>> {
    let pages = normalize_selected(path, dpi, selected)?;
    Ok(core_laposte::extract(&pages, format_code(format))?)
}
pub fn extract_bytes(
    bytes: Vec<u8>,
    format: LaposteFormat,
    dpi: u16,
    selected: &[u32],
) -> Result<Vec<core_laposte::Stamp>, Box<dyn std::error::Error>> {
    Ok(core_laposte::extract(
        &normalize_bytes(bytes, dpi, selected)?,
        format_code(format),
    )?)
}

pub fn export_stamps_pdf(
    stamps: &[core_laposte::Stamp],
    _dpi: u16,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if stamps.is_empty() {
        return Err("no stamps to export".into());
    }
    let rasters = stamps
        .iter()
        .map(|stamp| stamp.raster.dither(Dither::Threshold(128)))
        .collect::<Result<Vec<_>, _>>()?;
    let pages = rasters
        .iter()
        .map(|raster| mb_printer_core::export::PdfPage {
            raster,
            width_um: core_laposte::STAMP_WIDTH_UM,
            height_um: core_laposte::STAMP_HEIGHT_UM,
        })
        .collect::<Vec<_>>();
    Ok(mb_printer_core::export::pdf_pages_physical(&pages)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_formats_and_aliases() {
        for f in [
            "L24A",
            "L24B",
            "L21A",
            "L18A",
            "L16A",
            "L14A",
            "L12A",
            "SHEET",
            "L24A_SHEET",
        ] {
            assert!(f.parse::<LaposteFormat>().is_ok());
        }
    }
    #[test]
    fn exact_output_size_is_contract_constant() {
        assert_eq!((STAMP_WIDTH_MM, STAMP_HEIGHT_MM), (63.5, 33.9));
    }
    #[test]
    fn exported_stamp_media_box_is_exact_at_all_supported_dpis() {
        for dpi in [203u16, 254, 300] {
            let width = (STAMP_WIDTH_MM / 25.4 * f64::from(dpi)).round() as u32;
            let height = (STAMP_HEIGHT_MM / 25.4 * f64::from(dpi)).round() as u32;
            let stamp = core_laposte::Stamp {
                page: 1,
                slot: 1,
                width_um: 63_500,
                height_um: 33_900,
                raster: mb_printer_core::raster::GrayRaster::new(width, height, 255),
            };
            let pdf =
                String::from_utf8_lossy(&export_stamps_pdf(&[stamp], dpi).unwrap()).into_owned();
            assert!(
                pdf.contains("/MediaBox [0 0 180.000000 96.094488]"),
                "dpi {dpi}: {pdf}"
            );
        }
    }
    #[test]
    fn grids_match_frozen_python_contract() {
        let expected = [
            (LaposteFormat::L24A, 3, 8, 7.2, 13.1, 66.0, 33.9),
            (LaposteFormat::L24B, 3, 8, 5.0, 3.5, 68.25, 36.7),
            (LaposteFormat::L21A, 3, 7, 7.2, 17.2, 66.0, 38.1),
            (LaposteFormat::L18A, 3, 6, 7.2, 15.1, 66.0, 46.6),
            (LaposteFormat::L16A, 2, 8, 22.5, 13.5, 101.6, 33.9),
            (LaposteFormat::L14A, 2, 7, 22.5, 17.2, 101.6, 38.1),
            (LaposteFormat::L12A, 2, 6, 22.5, 25.6, 101.6, 42.3),
        ];
        for (format, columns, rows, x, y, px, py) in expected {
            assert_eq!(format.grid(), Grid::new(columns, rows, x, y, px, py));
            assert_eq!(format.grid().slots(2).len(), format.slots());
            assert_eq!(format.grid().slots(2)[0].source_page, 2);
            assert_eq!(format.grid().slots(2)[0].slot, 1);
        }
    }
    #[test]
    fn synthetic_detector_ignores_guides_and_keeps_provenance() {
        use image::{GrayImage, Luma};
        let dpi = 100;
        let mut page = GrayImage::from_pixel(
            (210.0 / 25.4 * dpi as f64) as u32,
            (297.0 / 25.4 * dpi as f64) as u32,
            Luma([255]),
        );
        let slot = LaposteFormat::L24A.grid().slots(7)[4].clone();
        let x = ((slot.x_mm + 5.0) * dpi as f64 / 25.4) as u32;
        let y = ((slot.y_mm + 5.0) * dpi as f64 / 25.4) as u32;
        for yy in y..y + 20 {
            for xx in x..x + 20 {
                page.put_pixel(xx, yy, Luma([0]));
            }
        }
        let found = detect_occupied(&page, LaposteFormat::L24A, 7, dpi);
        assert_eq!(found, vec![slot]);
    }
    #[test]
    fn sdk_pdf_normalizer_extracts_real_page() {
        let dpi = 30u16;
        let width = (210.0 / 25.4 * f64::from(dpi)).round() as u32;
        let height = (297.0 / 25.4 * f64::from(dpi)).round() as u32;
        let mut raster = MonoRaster {
            width,
            height,
            pixels: vec![0; width as usize * height as usize],
        };
        let x = (12.0 / 25.4 * f64::from(dpi)) as u32;
        let y = (18.0 / 25.4 * f64::from(dpi)) as u32;
        for yy in y..y + 10 {
            for xx in x..x + 10 {
                raster.pixels[(yy * width + xx) as usize] = 1;
            }
        }
        let pdf = mb_printer_core::export::pdf(&raster, dpi).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sheet.pdf");
        std::fs::write(&path, pdf).unwrap();
        let stamps = extract_pdf(&path, LaposteFormat::L24A, dpi, &[]).unwrap();
        assert_eq!(stamps[0].page, 1);
        assert!(
            (1..=LaposteFormat::L24A.grid().slots(7).len()).contains(&usize::from(stamps[0].slot))
        );
        assert_eq!((stamps[0].width_um, stamps[0].height_um), (63_500, 33_900));
    }
    #[test]
    fn native_and_wasm_share_identical_normalized_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/pdf-normalization.json")).unwrap();
        let dpi = fixture["dpi"].as_u64().unwrap() as u16;
        let width = (210.0 / 25.4 * f64::from(dpi)).round() as u32;
        let height = (297.0 / 25.4 * f64::from(dpi)).round() as u32;
        let mut raster = MonoRaster {
            width,
            height,
            pixels: vec![0; width as usize * height as usize],
        };
        for y in 20..30 {
            for x in 15..25 {
                raster.pixels[(y * width + x) as usize] = 1;
            }
        }
        let pdf = mb_printer_core::export::pdf(&raster, dpi).unwrap();
        let native =
            mb_printer_core::pdf_import::normalize(pdf.clone(), dpi, false, 64 * 1024 * 1024)
                .unwrap();
        let wasm_contract =
            mb_printer_core::pdf_import::normalize(pdf, dpi, false, 64 * 1024 * 1024).unwrap();
        assert_eq!(native[0].raster.pixels, wasm_contract[0].raster.pixels);
        assert_eq!(native[0].width_um, fixture["widthUm"].as_i64().unwrap());
        assert_eq!(native[0].height_um, fixture["heightUm"].as_i64().unwrap());
        let mut page = native[0].clone();
        page.page = 1;
        let stamps = core_laposte::extract(&[page], "L24A").unwrap();
        assert_eq!(
            stamps[0].slot,
            u16::try_from(fixture["slot"].as_u64().unwrap()).unwrap()
        );
    }
}
