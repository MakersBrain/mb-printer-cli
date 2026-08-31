// SPDX-License-Identifier: AGPL-3.0-or-later
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::laposte::LaposteFormat;

pub use crate::discovery::DiscoveryTransport;

#[derive(Debug, Parser)]
#[command(name = "mb-printer", version, about = "Makers' Brain printer platform")]
pub struct Cli {
    #[arg(long, global = true, env = "MB_PRINTER_CONFIG")]
    pub config: Option<PathBuf>,
    /// Command result format. `auto` uses pretty output on a terminal and text in a pipe.
    #[arg(
        long,
        global = true,
        env = "MB_PRINTER_FORMAT",
        value_enum,
        default_value_t = crate::output::OutputFormat::Auto
    )]
    pub format: crate::output::OutputFormat,
    /// Operational tracing level. Logs are always written to stderr.
    #[arg(long, global = true, value_enum, conflicts_with_all = ["verbose", "quiet"])]
    pub log_level: Option<LogLevel>,
    /// Operational tracing serialization.
    #[arg(
        long,
        global = true,
        env = "MB_PRINTER_LOG_FORMAT",
        value_enum,
        default_value_t = LogFormat::Pretty
    )]
    pub log_format: LogFormat,
    /// Increase operational tracing verbosity (`-vv` enables trace output).
    #[arg(short, long, global = true, action = ArgAction::Count, conflicts_with_all = ["log_level", "quiet"])]
    pub verbose: u8,
    /// Only emit errors on stderr.
    #[arg(short, long, global = true, conflicts_with_all = ["log_level", "verbose"])]
    pub quiet: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Find printers over every available transport.
    Discover(DiscoverArgs),
    /// Manage saved physical printers and their settings.
    Printer {
        #[command(subcommand)]
        command: PrinterCommand,
    },
    /// Print a document using a saved printer or explicit overrides.
    Print(PrintArgs),
    /// Inspect, validate, render, and transform documents.
    Document {
        #[command(subcommand)]
        command: DocumentCommand,
    },
    /// List supported printer models.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Manage configuration values.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage private printer assets.
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
    /// Run and administer the authenticated loopback service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Connect saved printers to Makers' Brain cloud printing.
    Cloud {
        #[command(subcommand)]
        command: CloudCommand,
    },
}

#[derive(Debug, Args, Clone)]
pub struct DiscoverArgs {
    /// Restrict discovery to a comma-separated set of transports.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub via: Vec<DiscoveryTransport>,
    /// Overall discovery deadline, such as `3s` or `750ms`.
    #[arg(long, default_value = "3s", value_parser = parse_duration)]
    pub timeout: std::time::Duration,
    /// Probe live status after finding candidates.
    #[arg(long)]
    pub probe: bool,
    /// Include USB and serial devices that cannot be classified as printers.
    #[arg(long)]
    pub include_unknown: bool,
    /// Fail if any requested discovery backend fails.
    #[arg(long)]
    pub strict: bool,
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub max_services: u16,
}

fn parse_duration(value: &str) -> Result<std::time::Duration, String> {
    let (amount, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else {
        return Err("duration must end in ms or s".into());
    };
    let millis = amount
        .parse::<u64>()
        .map_err(|_| "duration must contain a positive integer".to_string())?
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())?;
    if !(1..=10_000).contains(&millis) {
        return Err("duration must be between 1ms and 10s".into());
    }
    Ok(std::time::Duration::from_millis(millis))
}

#[derive(Debug, Subcommand)]
pub enum PrinterCommand {
    List,
    Add {
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        endpoint: Vec<String>,
        #[arg(long, requires = "endpoint")]
        preferred: Option<String>,
    },
    Show {
        printer: String,
    },
    Rename {
        printer: String,
        new_name: String,
    },
    Remove {
        printer: String,
    },
    Default {
        printer: Option<String>,
        #[arg(long, conflicts_with = "printer")]
        clear: bool,
    },
    Status(PrinterTarget),
    Test {
        #[arg(value_name = "PRINTER")]
        target: Option<String>,
        #[arg(long, value_enum, default_value_t = TestPattern::Density)]
        pattern: TestPattern,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        capture: Option<PathBuf>,
        #[arg(long)]
        transport: Option<String>,
        #[arg(long, default_value_t = 512)]
        payload_limit: usize,
        #[arg(long, default_value_t = 115_200)]
        baud: u32,
    },
    Report {
        printer: Option<String>,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value_t = ReportFormat::Json)]
        report_format: ReportFormat,
        /// Include network and device identifiers. The output file remains owner-only.
        #[arg(long)]
        unsafe_unredacted: bool,
    },
    Wifi {
        #[command(subcommand)]
        command: PrinterWifiCommand,
    },
    Endpoint {
        #[command(subcommand)]
        command: EndpointCommand,
    },
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TestPattern {
    Density,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ReportFormat {
    Json,
    Text,
}

#[derive(Debug, Subcommand)]
pub enum PrinterWifiCommand {
    Scan {
        printer: Option<String>,
        /// Parse a captured AVAILABLEWLAN reply instead of contacting hardware.
        #[arg(long)]
        input: Option<PathBuf>,
    },
    Status {
        printer: Option<String>,
        /// Parse a captured OBJBRNET reply instead of contacting hardware.
        #[arg(long)]
        input: Option<PathBuf>,
    },
    /// Encode a Brother wireless command value without contacting a printer.
    Encode {
        ssid: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Decode a captured Brother wireless status response.
    Decode { input: PathBuf },
    Configure {
        printer: Option<String>,
        #[arg(long)]
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
    },
}

// Clap owns these values only during dispatch; boxing nested argument structs would
// make every command handler less direct without improving the steady-state footprint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum DocumentCommand {
    Inspect {
        input: PathBuf,
    },
    Validate {
        input: PathBuf,
    },
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
    Render(RenderArgs),
    Laposte {
        #[command(subcommand)]
        command: LaposteCommand,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum LaposteCommand {
    Print(LapostePrintArgs),
    Extract(LaposteExtractArgs),
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    List,
}

#[derive(Debug, Subcommand)]
pub enum EndpointCommand {
    List {
        printer: String,
    },
    Add {
        printer: String,
        endpoint: String,
        #[arg(long)]
        preferred: bool,
    },
    Remove {
        printer: String,
        endpoint: String,
    },
    Prefer {
        printer: String,
        endpoint: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    Show {
        printer: String,
    },
    Set {
        printer: String,
        key: String,
        value: String,
    },
    Unset {
        printer: String,
        key: String,
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
    /// Saved printer name or ID. The default or sole printer is used when omitted.
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
    #[arg(skip)]
    pub result_format: crate::output::OutputFormat,
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

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            result_format: crate::output::OutputFormat::Auto,
            printer: None,
            model: None,
            dpi: None,
            copies: 1,
            page: Vec::new(),
            rotation: None,
            fit: false,
            dry_run: false,
            capture: None,
            transport: None,
            payload_limit: 512,
            density: 6,
            dither: None,
            feed: None,
            speed: None,
            continuous: false,
            align: None,
            offset_x: 0,
            offset_y: 0,
            gap_mm: None,
            tspl_offset_mm: None,
            no_cut: false,
            cut_every: None,
            no_compress: false,
            baud: 115_200,
            data: Vec::new(),
            csv: None,
            mappings: Vec::new(),
            filter: None,
            limit: None,
            copies_from: None,
            width_mm: None,
            height_mm: None,
        }
    }
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
    Import {
        #[command(subcommand)]
        command: AssetImportCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AssetImportCommand {
    Android {
        #[arg(long, default_value = "com.project.aimotech.printmaster")]
        package: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    Apk {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    Run {
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
    /// Create a one-time secret for a short-lived browser administrator grant.
    #[command(name = "pair-admin")]
    PairAdmin {
        /// Lifetime of the one-time secret (1–600 seconds).
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
        expires_seconds: u64,
    },
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
    },
    Wifi {
        #[command(subcommand)]
        command: ServiceWifiCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum GrantCommand {
    List,
    Revoke {
        id: String,
    },
    Rotate {
        id: String,
        #[arg(long, default_value_t = 2_592_000)]
        expires_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceWifiCommand {
    /// Approve one pending browser Wi-Fi configuration request on this machine.
    Approve {
        /// Opaque approval ID returned by the browser prepare request.
        id: String,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CloudCommand {
    Enroll {
        #[arg(long)]
        server: String,
    },
    Publish {
        printer: String,
        #[arg(long)]
        name: String,
    },
    Unpublish {
        printer_id: uuid::Uuid,
    },
    Status,
    Connect,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_printer_centric_laposte_surface() {
        let cli = Cli::try_parse_from([
            "mb-printer",
            "document",
            "laposte",
            "print",
            "sheet.pdf",
            "--laposte-format",
            "SHEET",
            "--slot",
            "1:4",
            "--dry-run",
        ])
        .unwrap();
        let Command::Document {
            command:
                DocumentCommand::Laposte {
                    command: LaposteCommand::Print(args),
                },
        } = cli.command
        else {
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
                "document",
                "laposte",
                "extract",
                "x.pdf",
                "--laposte-format",
                "L99A"
            ])
            .is_err()
        );
    }

    #[test]
    fn unified_discovery_arguments_are_bounded() {
        let cli = Cli::try_parse_from([
            "mb-printer",
            "discover",
            "--via",
            "usb,network",
            "--timeout",
            "2500ms",
            "--max-services",
            "12",
        ])
        .unwrap();
        let Command::Discover(args) = cli.command else {
            panic!()
        };
        assert_eq!(args.timeout, std::time::Duration::from_millis(2500));
        assert_eq!(
            args.via,
            [DiscoveryTransport::Usb, DiscoveryTransport::Network]
        );
        assert_eq!(args.max_services, 12);
        assert!(Cli::try_parse_from(["mb-printer", "discover", "--timeout", "10001ms"]).is_err());
    }

    #[test]
    fn managed_printer_commands_have_consistent_action_order() {
        let cli = Cli::try_parse_from(["mb-printer", "printer", "wifi", "scan", "desk"]).unwrap();
        let Command::Printer {
            command:
                PrinterCommand::Wifi {
                    command: PrinterWifiCommand::Scan { printer, .. },
                },
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(printer.as_deref(), Some("desk"));
    }

    #[test]
    fn service_administrator_pairing_expiry_is_bounded() {
        let cli = Cli::try_parse_from([
            "mb-printer",
            "service",
            "pair-admin",
            "--expires-seconds",
            "300",
        ])
        .unwrap();
        let Command::Service {
            command: ServiceCommand::PairAdmin { expires_seconds },
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(expires_seconds, 300);
        assert!(
            Cli::try_parse_from([
                "mb-printer",
                "service",
                "pair-admin",
                "--expires-seconds",
                "601",
            ])
            .is_err()
        );
    }

    #[test]
    fn result_and_log_controls_are_global_and_unambiguous() {
        let cli = Cli::try_parse_from([
            "mb-printer",
            "printer",
            "list",
            "--format",
            "json",
            "--log-format",
            "json",
            "-v",
        ])
        .unwrap();
        assert_eq!(cli.format, crate::output::OutputFormat::Json);
        assert_eq!(cli.verbose, 1);
        assert!(
            Cli::try_parse_from(["mb-printer", "--log-level", "info", "-v", "printer", "list"])
                .is_err()
        );
    }
}
