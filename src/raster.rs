// SPDX-License-Identifier: AGPL-3.0-or-later
//! Thin native wrappers around the authoritative SDK renderer and exporters.
use mb_printer_core::{
    Document, export,
    protocol::Raster,
    raster::MonoRaster,
    render::{self, RenderOptions},
};
use std::{io, path::Path};

pub fn render(document: &Document, dpi: u16) -> io::Result<MonoRaster> {
    render_with_dither(document, dpi, None)
}
pub fn render_with_dither(
    document: &Document,
    dpi: u16,
    dither: Option<&str>,
) -> io::Result<MonoRaster> {
    let mut document = document.clone();
    document.media.dpi = dpi;
    let dither = match dither.unwrap_or("threshold") {
        "threshold" => mb_printer_core::raster::Dither::Threshold(128),
        "floyd-steinberg" => mb_printer_core::raster::Dither::FloydSteinberg,
        "atkinson" => mb_printer_core::raster::Dither::Atkinson,
        "bayer2" => mb_printer_core::raster::Dither::Bayer2,
        "bayer4" => mb_printer_core::raster::Dither::Bayer4,
        value => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown dither {value}"),
            ));
        }
    };
    render::render(
        &document,
        RenderOptions {
            dither,
            max_pixels: 64 * 1024 * 1024,
        },
    )
    .map_err(io::Error::other)
}
pub fn render_for_printer(
    document: &Document,
    printer: &mb_printer_core::capabilities::PrinterDefinition,
    dpi: u16,
) -> io::Result<Raster> {
    let mut document = document.clone();
    document.media.dpi = dpi;
    render::render_for_printer(
        &document,
        printer,
        RenderOptions {
            max_pixels: 64 * 1024 * 1024,
            ..RenderOptions::default()
        },
    )
    .map_err(io::Error::other)
}
pub fn png(image: &MonoRaster, dpi: u16) -> io::Result<Vec<u8>> {
    export::png(image, dpi).map_err(io::Error::other)
}
pub fn pdf(image: &MonoRaster, dpi: u16) -> io::Result<Vec<u8>> {
    export::pdf(image, dpi).map_err(io::Error::other)
}
pub fn svg(image: &MonoRaster, dpi: u16) -> io::Result<Vec<u8>> {
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD.encode(png(image, dpi)?);
    let width_mm = f64::from(image.width) * 25.4 / f64::from(dpi);
    let height_mm = f64::from(image.height) * 25.4 / f64::from(dpi);
    Ok(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width_mm:.6}mm" height="{height_mm:.6}mm" viewBox="0 0 {} {}"><image width="{}" height="{}" href="data:image/png;base64,{}" image-rendering="pixelated"/></svg>"#, image.width, image.height, image.width, image.height, png).into_bytes())
}

/// Preserve a full-media source SVG as nested vector XML when it is safe and
/// untransformed; otherwise use the deterministic portable raster fallback.
pub fn svg_document(document: &Document, image: &MonoRaster, dpi: u16) -> io::Result<Vec<u8>> {
    let _ = document;
    // Embedded source markup is never copied into output: without a complete
    // namespace-aware XML sanitizer, raster export is the only provably safe path.
    svg(image, dpi)
}

pub fn preview_transform(
    image: &MonoRaster,
    zoom: f64,
    offset_x: f64,
    offset_y: f64,
) -> io::Result<MonoRaster> {
    if !zoom.is_finite() || zoom <= 0.0 || !offset_x.is_finite() || !offset_y.is_finite() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zoom must be positive and preview offsets finite",
        ));
    }
    let mut pixels = vec![0; image.pixels.len()];
    for y in 0..image.height {
        for x in 0..image.width {
            let source_x = (f64::from(x) - offset_x) / zoom;
            let source_y = (f64::from(y) - offset_y) / zoom;
            if source_x >= 0.0
                && source_y >= 0.0
                && source_x < f64::from(image.width)
                && source_y < f64::from(image.height)
            {
                let source = source_y.floor() as u32 * image.width + source_x.floor() as u32;
                pixels[(y * image.width + x) as usize] = image.pixels[source as usize];
            }
        }
    }
    Ok(MonoRaster {
        width: image.width,
        height: image.height,
        pixels,
    })
}

pub fn scale_to_width(image: &MonoRaster, width: u32) -> io::Result<MonoRaster> {
    if width == 0 || image.width == 0 || image.height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid raster dimensions",
        ));
    }
    let height = ((u64::from(image.height) * u64::from(width) + u64::from(image.width) / 2)
        / u64::from(image.width))
    .max(1) as u32;
    let mut pixels = vec![0; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let source_x = u64::from(x) * u64::from(image.width) / u64::from(width);
            let source_y = u64::from(y) * u64::from(image.height) / u64::from(height);
            pixels[(y * width + x) as usize] =
                image.pixels[(source_y as u32 * image.width + source_x as u32) as usize];
        }
    }
    Ok(MonoRaster {
        width,
        height,
        pixels,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn sheet_pdf(
    label: &MonoRaster,
    dpi: u16,
    paper: &str,
    margin_mm: f64,
    gap_mm: f64,
    columns: u16,
    rows: u16,
    cut_marks: bool,
) -> io::Result<Vec<u8>> {
    let (paper_width_um, paper_height_um) = match paper.to_ascii_lowercase().as_str() {
        "a4" => (210_000_i64, 297_000_i64),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "paper must be a4",
            ));
        }
    };
    if margin_mm < 0.0 || gap_mm < 0.0 || columns == 0 || rows == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid sheet geometry",
        ));
    }
    let dots = |mm: f64| (mm * f64::from(dpi) / 25.4).round() as u32;
    let width = dots(paper_width_um as f64 / 1000.0);
    let height = dots(paper_height_um as f64 / 1000.0);
    let margin = dots(margin_mm);
    let gap = dots(gap_mm);
    let used_width = margin * 2 + u32::from(columns) * label.width + u32::from(columns - 1) * gap;
    let used_height = margin * 2 + u32::from(rows) * label.height + u32::from(rows - 1) * gap;
    if used_width > width || used_height > height {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "label grid does not fit sheet",
        ));
    }
    let mut pixels = vec![0; (width * height) as usize];
    for row in 0..u32::from(rows) {
        for column in 0..u32::from(columns) {
            let left = margin + column * (label.width + gap);
            let top = margin + row * (label.height + gap);
            for y in 0..label.height {
                let destination = ((top + y) * width + left) as usize;
                let source = (y * label.width) as usize;
                pixels[destination..destination + label.width as usize]
                    .copy_from_slice(&label.pixels[source..source + label.width as usize]);
            }
            if cut_marks {
                let length = dots(2.0).max(1);
                for delta in 0..length {
                    for (x, y) in [
                        (left.saturating_sub(delta), top),
                        (left, top.saturating_sub(delta)),
                        (left + label.width + delta, top),
                        (left, top + label.height + delta),
                    ] {
                        if x < width && y < height {
                            pixels[(y * width + x) as usize] = 1;
                        }
                    }
                }
            }
        }
    }
    export::pdf_physical(
        &MonoRaster {
            width,
            height,
            pixels,
        },
        paper_width_um,
        paper_height_um,
    )
    .map_err(io::Error::other)
}
pub fn tiles(image: &MonoRaster, width: u32, height: u32) -> io::Result<Vec<MonoRaster>> {
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tile dimensions must be positive",
        ));
    }
    let mut output = Vec::new();
    for y in (0..image.height).step_by(height as usize) {
        for x in (0..image.width).step_by(width as usize) {
            let tile_width = width.min(image.width - x);
            let tile_height = height.min(image.height - y);
            let mut pixels = Vec::with_capacity((tile_width * tile_height) as usize);
            for row in y..y + tile_height {
                let start = (row * image.width + x) as usize;
                pixels.extend_from_slice(&image.pixels[start..start + tile_width as usize]);
            }
            output.push(MonoRaster {
                width: tile_width,
                height: tile_height,
                pixels,
            });
        }
    }
    Ok(output)
}
pub fn save_png(image: &MonoRaster, dpi: u16, path: &Path) -> io::Result<()> {
    std::fs::write(path, png(image, dpi)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_export_is_deterministic_and_valid_png() {
        let raster = MonoRaster {
            width: 9,
            height: 1,
            pixels: vec![1, 0, 0, 0, 0, 0, 0, 0, 1],
        };
        let first = png(&raster, 203).unwrap();
        assert_eq!(first, png(&raster, 203).unwrap());
        assert!(first.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn scaling_and_a4_sheet_preserve_geometry() {
        let raster = MonoRaster {
            width: 10,
            height: 5,
            pixels: vec![1; 50],
        };
        assert_eq!(scale_to_width(&raster, 4).unwrap().height, 2);
        let pdf = sheet_pdf(&raster, 203, "a4", 5.0, 2.0, 2, 2, true).unwrap();
        assert!(String::from_utf8_lossy(&pdf).contains("/MediaBox [0 0 595.275591 841.889764]"));
    }

    #[test]
    fn preview_transform_and_safe_vector_export_are_explicit() {
        let raster = MonoRaster {
            width: 3,
            height: 2,
            pixels: vec![1, 0, 0, 0, 1, 0],
        };
        assert_eq!(
            preview_transform(&raster, 1.0, 1.0, 0.0).unwrap().pixels,
            vec![0, 1, 0, 0, 0, 1]
        );
        assert!(preview_transform(&raster, 0.0, 0.0, 0.0).is_err());
        let document = Document::from_json(r#"{"version":4,"name":"vector","media":{"width":10000,"height":5000,"unit":"micrometre","dpi":203,"orientation":"portrait","printableBounds":{"x":0,"y":0,"width":10000,"height":5000},"shape":"rectangle","continuous":false,"zones":[]},"coordinateSystem":{"unit":"micrometre","origin":"top-left","rounding":"half-away-from-zero"},"elements":[{"type":"svg","id":"art","transform":{"x":0,"y":0,"width":10000,"height":5000},"zOrder":0,"resource":"svg"}],"resources":[{"id":"svg","mediaType":"image/svg+xml","sha256":"0229f116e8648388eeb7cc0745f5dd4fd2c54392db57376ced9f3b05582153ce","dataBase64":"PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAxMCA1Ij48cGF0aCBkPSJNMCAwaDEwdjV6Ii8+PC9zdmc+"}]}"#).unwrap();
        let exported = String::from_utf8(svg_document(&document, &raster, 203).unwrap()).unwrap();
        assert!(!exported.contains("<path d=\"M0 0h10v5z\""));
        assert!(exported.contains("data:image/png"));
    }
}
