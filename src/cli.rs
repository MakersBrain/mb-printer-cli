// SPDX-License-Identifier: AGPL-3.0-or-later
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::laposte::LaposteFormat;

#[derive(Debug, Parser)]
#[command(name = "mb-printer", version, about = "Makers' Brain printer platform")]
pub struct Cli {
    #[arg(long, global = true, env = "MB_PRINTER_CONFIG")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Inspect {
        input: PathBuf,
    },
    Validate {
        input: PathBuf,
    },
    Document {
        #[command(subcommand)]
        command: DocumentCommand,
    },
    Render(RenderArgs),
    Export(RenderArgs),
    Printers,
    Discover,
    Usb {
        #[command(subcommand)]
        command: UsbCommand,
    },
    Wifi {
        #[command(subcommand)]
        command: WifiCommand,
    },
    Status(PrinterTarget),
    Print(PrintArgs),
    #[command(name = "density-test", alias = "test")]
    DensityTest {
        #[command(flatten)]
        options: PrintOptions,
    },
    #[command(name = "print-pdf")]
    PrintPdf(LapostePrintArgs),
    #[command(name = "extract-pdf")]
    ExtractPdf(LaposteExtractArgs),
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Assets {
        #[command(subcommand)]
        command: AssetCommand,
    },
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum UsbCommand {
    List,
    Info {
        address: String,
    },
    Report {
        #[arg(short, long)]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum WifiCommand {
    Scan {
        /// Parse a captured AVAILABLEWLAN reply instead of contacting hardware.
        #[arg(long)]
        input: Option<PathBuf>,
    },
    Status {
        /// Parse a captured OBJBRNET reply instead of contacting hardware.
        #[arg(long)]
        input: Option<PathBuf>,
    },
    Encode {
        ssid: String,
        #[arg(long)]
        password: Option<String>,
    },
    Decode {
        input: PathBuf,
    },
    Configure {
        ssid: String,
        #[arg(long)]
        password_stdin: bool,
        #[arg(long, default_value = "tkip-aes")]
        encryption: String,
        #[arg(long, default_value = "wpa-psk")]
        authentication: String,
        #[arg(long)]
        no_reboot: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        capture: Option<PathBuf>,
        #[arg(long)]
        transport: Option<String>,
        #[arg(long, default_value_t = 115_200)]
        baud: u32,
    },
}

#[derive(Debug, Subcommand)]
pub enum DocumentCommand {
    Fields {
        input: PathBuf,
    },
    #[command(name = "import-svg")]
    ImportSvg {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        width_mm: f64,
        #[arg(long)]
        height_mm: f64,
        #[arg(long, default_value_t = 203)]
        dpi: u16,
    },
}

#[derive(Debug, Args)]
pub struct RenderArgs {
    pub input: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long, default_value_t = 203)]
    pub dpi: u16,
    /// Split output into numbered PNG tiles of this width in pixels.
    #[arg(long, requires = "tile_height")]
    pub tile_width: Option<u32>,
    /// Split output into numbered PNG tiles of this height in pixels.
    #[arg(long, requires = "tile_width")]
    pub tile_height: Option<u32>,
    /// Lay labels out on a paper sheet (currently `a4`).
    #[arg(long)]
    pub paper: Option<String>,
    #[arg(long, default_value_t = 5.0)]
    pub margin_mm: f64,
    #[arg(long, default_value_t = 2.0)]
    pub gap_mm: f64,
    #[arg(long, default_value_t = 1)]
    pub columns: u16,
    #[arg(long, default_value_t = 1)]
    pub rows: u16,
    #[arg(long)]
    pub cut_marks: bool,
    /// Preview scale while retaining the media-sized viewport.
    #[arg(long, default_value_t = 1.0)]
    pub zoom: f64,
    /// Horizontal preview translation in millimetres.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub offset_x_mm: f64,
    /// Vertical preview translation in millimetres.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    pub offset_y_mm: f64,
}

#[derive(Debug, Args)]
pub struct PrinterTarget {
    #[arg(long)]
    pub printer: Option<String>,
    /// Decode a captured 32-byte Brother raster status reply.
    #[arg(long)]
    pub response: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PrintArgs {
    pub input: PathBuf,
    #[command(flatten)]
    pub options: PrintOptions,
}

#[derive(Debug, Args, Clone)]
pub struct PrintOptions {
    #[arg(long)]
    pub printer: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub dpi: Option<u16>,
    #[arg(long, default_value_t = 1)]
    pub copies: u16,
    #[arg(long)]
    pub page: Vec<u32>,
    #[arg(long, value_parser = clap::value_parser!(u16).range(0..=359))]
    pub rotation: Option<u16>,
    #[arg(long)]
    pub fit: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub capture: Option<PathBuf>,
    /// Transport URI: file:/path, tcp://host:port, or serial:/dev/ttyUSB0.
    #[arg(long)]
    pub transport: Option<String>,
    #[arg(long, default_value_t = 512)]
    pub payload_limit: usize,
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub density: u8,
    /// Raster dithering: threshold, floyd-steinberg, atkinson, bayer2, or bayer4.
    #[arg(long)]
    pub dither: Option<String>,
    /// Feed after each label, in dots.
    #[arg(long)]
    pub feed: Option<u8>,
    /// Protocol print-speed setting.
    #[arg(long)]
    pub speed: Option<u8>,
    /// Disable gap detection for continuous media.
    #[arg(long)]
    pub continuous: bool,
    /// Override the model's head alignment.
    #[arg(long, value_parser = ["left", "center", "right"])]
    pub align: Option<String>,
    /// Horizontal roller nudge across the head, in dots.
    #[arg(long, default_value_t = 0)]
    pub offset_x: u16,
    /// Feed-direction nudge, in dots.
    #[arg(long, default_value_t = 0)]
    pub offset_y: u16,
    /// TSPL gap between labels, in millimetres.
    #[arg(long)]
    pub gap_mm: Option<f64>,
    /// TSPL OFFSET value, in millimetres.
    #[arg(long, allow_hyphen_values = true)]
    pub tspl_offset_mm: Option<f64>,
    #[arg(long)]
    pub no_cut: bool,
    #[arg(long)]
    pub cut_every: Option<u8>,
    #[arg(long)]
    pub no_compress: bool,
    #[arg(long, default_value_t = 115_200)]
    pub baud: u32,
    #[arg(long = "data", value_name = "KEY=VALUE")]
    pub data: Vec<String>,
    #[arg(long)]
    pub csv: Option<PathBuf>,
    #[arg(long = "map", value_name = "TARGET=SOURCE")]
    pub mappings: Vec<String>,
    #[arg(long, value_name = "FIELD=VALUE")]
    pub filter: Option<String>,
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub copies_from: Option<String>,
    #[arg(long)]
    pub width_mm: Option<f64>,
    #[arg(long)]
    pub height_mm: Option<f64>,
}

#[derive(Debug, Args)]
pub struct LapostePrintArgs {
    pub input: PathBuf,
    #[arg(long)]
    pub laposte_format: LaposteFormat,
    /// Print only occupied one-based `page:slot` entries (repeatable).
    #[arg(long = "slot")]
    pub slots: Vec<String>,
    #[command(flatten)]
    pub options: PrintOptions,
}

#[derive(Debug, Args)]
pub struct LaposteExtractArgs {
    pub input: PathBuf,
    #[arg(long)]
    pub laposte_format: LaposteFormat,
    #[arg(short, long, default_value = "labels.pdf")]
    pub output: PathBuf,
    #[arg(long, default_value_t = 300)]
    pub dpi: u16,
    #[arg(long)]
    pub page: Vec<u32>,
    /// Export only occupied one-based `page:slot` entries (repeatable).
    #[arg(long = "slot")]
    pub slots: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Show,
    Path,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
    Migrate { input: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum AssetCommand {
    List,
    #[command(name = "import-android")]
    ImportAndroid {
        #[arg(long, default_value = "com.project.aimotech.printmaster")]
        package: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    #[command(name = "import-apk")]
    ImportApk {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ApiCommand {
    Serve {
        /// Bind one explicit loopback address; by default both IPv4 and IPv6 are served.
        #[arg(long)]
        bind: Option<std::net::IpAddr>,
        #[arg(long, default_value_t = 9847)]
        port: u16,
    },
    Pair {
        #[arg(long, default_value_t = 120)]
        expires_seconds: u64,
    },
    Grants,
    Revoke {
        id: String,
    },
    Rotate {
        id: String,
        #[arg(long, default_value_t = 2_592_000)]
        expires_seconds: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_laposte_compatibility_surface() {
        let cli = Cli::try_parse_from([
            "mb-printer",
            "print-pdf",
            "sheet.pdf",
            "--laposte-format",
            "SHEET",
            "--slot",
            "1:4",
            "--dry-run",
        ])
        .unwrap();
        let Command::PrintPdf(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.laposte_format, LaposteFormat::L24ASheet);
        assert_eq!(args.slots, ["1:4"]);
        assert!(args.options.dry_run);
    }

    #[test]
    fn rejects_unknown_laposte_format() {
        assert!(
            Cli::try_parse_from([
                "mb-printer",
                "extract-pdf",
                "x.pdf",
                "--laposte-format",
                "L99A"
            ])
            .is_err()
        );
    }
}
