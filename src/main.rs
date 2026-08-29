// SPDX-License-Identifier: AGPL-3.0-or-later
use clap::Parser;
use mb_printer_cli::{
    api::{self, ApiState},
    assets,
    auth::{self, AuthStore},
    cli::{
        ApiCommand, AssetCommand, Cli, Command, ConfigCommand, DocumentCommand, UsbCommand,
        WifiCommand,
    },
    config, laposte, raster,
    transport::{
        self, CaptureTransport, PhysicalEvent, SerialTransport, TcpTransport, WriteTransport,
    },
};
use mb_printer_core::{
    Document, capabilities,
    protocol::{self, Options, Plan},
};
use serde_json::json;
use std::{fs, path::Path, time::Duration};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("mb-printer: {error}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_path);
    let mut cfg = config::load(&config_path)?;
    let catalogue_path = cfg.catalogue_path.clone().unwrap_or_else(|| {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("catalogues.json")
    });
    cfg.catalogue_path = Some(catalogue_path.clone());
    if cfg.connections_path.is_none() {
        cfg.connections_path = Some(
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("connections.json"),
        );
    }
    if cfg.jobs_path.is_none() {
        cfg.jobs_path = Some(
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("jobs.json"),
        );
    }
    match cli.command {
        Command::Inspect { input } => {
            let data = fs::read_to_string(&input)?;
            let document = load_document(&input)?;
            let errors = document
                .validate()
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"path":input,"bytes":data.len(),"version":document.version,"name":document.name,"media":document.media,"elements":document.elements.len(),"resources":document.resources.len(),"valid":errors.is_empty(),"errors":errors})
                )?
            );
        }
        Command::Validate { input } => {
            validate_document(&input)?;
            println!("valid label document");
        }
        Command::Document { command } => match command {
            DocumentCommand::Fields { input } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&load_document(&input)?.fields)?
                );
            }
            DocumentCommand::ImportSvg {
                input,
                output,
                width_mm,
                height_mm,
                dpi,
            } => {
                use base64::Engine as _;
                use sha2::{Digest, Sha256};
                if width_mm <= 0.0 || height_mm <= 0.0 {
                    return Err("SVG media dimensions must be positive".into());
                }
                let svg = fs::read(&input)?;
                let sha = format!("{:x}", Sha256::digest(&svg));
                let width = (width_mm * 1000.0).round() as i64;
                let height = (height_mm * 1000.0).round() as i64;
                let document = json!({"version":4,"name":input.file_stem().and_then(|v|v.to_str()).unwrap_or("Imported SVG"),"media":{"width":width,"height":height,"unit":"micrometre","dpi":dpi,"orientation":"portrait","printableBounds":{"x":0,"y":0,"width":width,"height":height},"shape":"rectangle","continuous":false,"zones":[]},"coordinateSystem":{"unit":"micrometre","origin":"top-left","rounding":"half-away-from-zero"},"elements":[{"type":"svg","id":"imported-svg","transform":{"x":0,"y":0,"width":width,"height":height},"zOrder":0,"resource":"svg-source"}],"resources":[{"id":"svg-source","mediaType":"image/svg+xml","sha256":sha,"dataBase64":base64::engine::general_purpose::STANDARD.encode(svg)}],"fields":[],"extensions":{}});
                let parsed = Document::from_json(&document.to_string())?;
                parsed.validate().map_err(|errors| {
                    errors
                        .into_iter()
                        .map(|error| error.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                })?;
                fs::write(&output, serde_json::to_vec_pretty(&document)?)?;
                println!("{}", output.display());
            }
        },
        Command::Render(args) | Command::Export(args) => {
            let document = load_document(&args.input)?;
            let mut image = raster::render(&document, args.dpi)?;
            image = raster::preview_transform(
                &image,
                args.zoom,
                args.offset_x_mm * f64::from(args.dpi) / 25.4,
                args.offset_y_mm * f64::from(args.dpi) / 25.4,
            )?;
            if let (Some(width), Some(height)) = (args.tile_width, args.tile_height) {
                for (index, tile) in raster::tiles(&image, width, height)?.iter().enumerate() {
                    let path = args.output.with_extension(format!("{:04}.png", index + 1));
                    raster::save_png(tile, args.dpi, &path)?;
                }
            } else if args
                .output
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
            {
                let bytes = if let Some(paper) = &args.paper {
                    raster::sheet_pdf(
                        &image,
                        args.dpi,
                        paper,
                        args.margin_mm,
                        args.gap_mm,
                        args.columns,
                        args.rows,
                        args.cut_marks,
                    )?
                } else {
                    raster::pdf(&image, args.dpi)?
                };
                fs::write(&args.output, bytes)?;
            } else if args
                .output
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
            {
                fs::write(
                    &args.output,
                    raster::svg_document(&document, &image, args.dpi)?,
                )?;
            } else {
                raster::save_png(&image, args.dpi, &args.output)?;
            }
            println!("{}", args.output.display());
        }
        Command::Printers => println!(
            "{}",
            serde_json::to_string_pretty(&capabilities::bundled())?
        ),
        Command::Discover => {
            #[allow(unused_mut)]
            let mut devices = transport::discover_native()?;
            #[cfg(feature = "bluetooth")]
            devices.extend(transport::bluetooth::discover().await?);
            println!("{}", serde_json::to_string_pretty(&devices)?);
        }
        Command::Usb { command } => {
            let devices = transport::discover_native()?
                .into_iter()
                .filter(|device| device.transport == "usb")
                .collect::<Vec<_>>();
            match command {
                UsbCommand::List => println!("{}", serde_json::to_string_pretty(&devices)?),
                UsbCommand::Info { address } => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &devices
                            .into_iter()
                            .find(|device| device.address == address)
                            .ok_or("USB device not found")?
                    )?
                ),
                UsbCommand::Report { output } => {
                    let report = json!({"schema":1,"capturedAt":chrono::Utc::now().to_rfc3339(),"hardwareClaim":false,"devices":devices});
                    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
                    println!("{}", output.display());
                }
            }
        }
        Command::Wifi { command } => match command {
            WifiCommand::Scan { input } => {
                if let Some(input) = input {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&mb_printer_cli::device::wifi_access_points(
                            &fs::read(input)?
                        )?)?
                    );
                } else {
                    use base64::Engine as _;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "hardwareClaim": false,
                            "commandsBase64": [
                                base64::engine::general_purpose::STANDARD.encode(mb_printer_cli::device::wifi_scan_start()),
                                base64::engine::general_purpose::STANDARD.encode(mb_printer_cli::device::wifi_scan_results())
                            ]
                        }))?
                    );
                }
            }
            WifiCommand::Status { input } => {
                if let Some(input) = input {
                    let data = fs::read(input)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"connected":mb_printer_cli::device::wifi_status(&data),"ipAddress":mb_printer_cli::device::wifi_ip(&data)})
                        )?
                    );
                } else {
                    use base64::Engine as _;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"hardwareClaim":false,"commandsBase64":[base64::engine::general_purpose::STANDARD.encode(mb_printer_cli::device::wifi_inquire("458867")?),base64::engine::general_purpose::STANDARD.encode(mb_printer_cli::device::wifi_inquire("458967.2")?)]})
                        )?
                    );
                }
            }
            WifiCommand::Encode { ssid, password } => {
                let obfuscated = password.as_deref().map(|value| {
                    mb_printer_cli::device::xor_password(value.as_bytes())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"ssid":ssid,"encodedSsid":mb_printer_cli::device::encode_ssid(&ssid),"obfuscatedPasswordHex":obfuscated,"warning":"reversible device obfuscation; treat as a credential"})
                    )?
                );
            }
            WifiCommand::Decode { input } => {
                let data = fs::read(input)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"connected":mb_printer_cli::device::wifi_status(&data),"ipAddress":mb_printer_cli::device::wifi_ip(&data)})
                    )?
                );
            }
            WifiCommand::Configure {
                ssid,
                password_stdin,
                encryption,
                authentication,
                no_reboot,
                dry_run,
                capture,
                transport,
                baud,
            } => {
                let mut password = String::new();
                if password_stdin {
                    use std::io::Read as _;
                    std::io::stdin().read_to_string(&mut password)?;
                    while password.ends_with(['\n', '\r']) {
                        password.pop();
                    }
                } else if authentication != "open" {
                    return Err(
                        "use --password-stdin so credentials never appear in process arguments"
                            .into(),
                    );
                }
                let command = mb_printer_cli::device::wifi_configure(
                    &ssid,
                    &password,
                    &encryption,
                    &authentication,
                    !no_reboot,
                )?;
                if dry_run {
                    let capture = capture.ok_or("--dry-run requires --capture")?;
                    fs::write(&capture, command)?;
                    println!("{}", capture.display());
                } else {
                    use mb_printer_native::Transport as _;
                    let uri = transport
                        .as_deref()
                        .ok_or("Wi-Fi configuration requires --transport")?;
                    if let Some(path) = uri.strip_prefix("file:") {
                        WriteTransport::file(Path::new(path), command.len())?.write(&command)?;
                    } else if let Some(address) = uri.strip_prefix("tcp://") {
                        TcpTransport::connect(address, command.len(), Duration::from_secs(5))?
                            .write(&command)?;
                    } else if let Some(path) = uri.strip_prefix("serial:") {
                        SerialTransport::open(Path::new(path), baud, command.len())?
                            .write(&command)?;
                    } else if let Some(spec) = uri.strip_prefix("rfcomm:") {
                        #[cfg(all(feature = "bluetooth", target_os = "linux"))]
                        {
                            let (address, channel) = parse_rfcomm(spec)?;
                            let mut target =
                                mb_printer_native::transports::rfcomm::RfcommTransport::bind(
                                    0,
                                    address,
                                    channel,
                                    command.len(),
                                )?;
                            target.write(&command)?;
                        }
                        #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
                        {
                            let _ = spec;
                            return Err("RFCOMM requires the bluetooth feature on Linux".into());
                        }
                    } else {
                        return Err(
                            "Wi-Fi transport must use file:, tcp://, serial:, or rfcomm:".into(),
                        );
                    }
                }
            }
        },
        Command::Status(target) => {
            if let Some(response) = &target.response {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&mb_printer_cli::device::brother_status(
                        &fs::read(response)?
                    )?)?
                );
            } else if let Some(address) = target
                .printer
                .as_deref()
                .and_then(|value| value.strip_prefix("tcp://"))
            {
                let (host, port) = address
                    .rsplit_once(':')
                    .map_or((address, 631), |(host, port)| {
                        (host, port.parse().unwrap_or(631))
                    });
                let attributes =
                    mb_printer_cli::device::ipp_query(host, port, Duration::from_secs(5))?;
                let keyword = attributes
                    .get("media-ready")
                    .or_else(|| attributes.get("media-default"))
                    .and_then(|values| values.first())
                    .and_then(|value| match value {
                        mb_printer_cli::device::IppValue::Text(value) => Some(value.as_str()),
                        _ => None,
                    });
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"target":target.printer,"connected":true,"status":"ipp-response","media":{"keyword":keyword,"sizeMm":keyword.and_then(mb_printer_cli::device::ipp_media_size)},"attributes":attributes})
                    )?
                );
            } else if let Some(path) = target
                .printer
                .as_deref()
                .and_then(|value| value.strip_prefix("serial:"))
            {
                let mut transport = SerialTransport::open(Path::new(path), 115_200, 128)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&query_brother_status(&mut transport)?)?
                );
            } else if let Some(spec) = target
                .printer
                .as_deref()
                .and_then(|value| value.strip_prefix("rfcomm:"))
            {
                let _ = spec;
                #[cfg(all(feature = "bluetooth", target_os = "linux"))]
                {
                    let (address, channel) = parse_rfcomm(spec)?;
                    let mut transport =
                        mb_printer_native::transports::rfcomm::RfcommTransport::bind(
                            0, address, channel, 128,
                        )?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&query_brother_status(&mut transport)?)?
                    );
                }
                #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
                return Err("RFCOMM requires the bluetooth feature on Linux".into());
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"target":target.printer,"connected":false,"status":"pass --printer tcp://HOST[:631] for live Brother IPP status"})
                    )?
                );
            }
        }
        Command::Print(args) => {
            let mut options = args.options;
            apply_config_defaults(&mut options, &cfg)?;
            let document = load_print_input(&args.input, &options)?;
            let batch = expand_batch(&document, &options)?;
            for (index, (document, copies)) in batch.into_iter().enumerate() {
                let mut record_options = options.clone();
                record_options.copies = copies;
                if let Some(path) = &options.capture
                    && options.csv.is_some()
                {
                    record_options.capture =
                        Some(path.with_extension(format!("record-{}.json", index + 1)));
                }
                let plan = plan_document(&document, &record_options)?;
                execute_plan(&plan, &record_options).await?;
            }
        }
        Command::DensityTest { mut options } => {
            apply_config_defaults(&mut options, &cfg)?;
            let model = options.model.as_deref().ok_or("--model is required")?;
            let printer = capabilities::by_id(model).ok_or("unknown printer model")?;
            let width_bytes = 40u16;
            let strip = mb_printer_core::protocol::Raster {
                width_bytes,
                height: 30,
                data: vec![0xff; usize::from(width_bytes) * 30],
            };
            for density in 1..=8 {
                let plan = protocol::plan(
                    &printer,
                    &strip,
                    &Options {
                        density,
                        copies: 1,
                        ..Options::default()
                    },
                )?;
                let mut density_options = options.clone();
                density_options.density = density;
                if let Some(path) = &options.capture {
                    density_options.capture =
                        Some(path.with_extension(format!("density-{density}.json")));
                }
                execute_plan(&plan, &density_options).await?;
            }
        }
        Command::PrintPdf(args) => {
            let mut options = args.options;
            apply_config_defaults(&mut options, &cfg)?;
            laposte::validate_a4(&args.input, &options.page)?;
            let model = options
                .model
                .as_deref()
                .ok_or("--model is required for La Poste printing")?;
            let printer = capabilities::by_id(model)
                .ok_or_else(|| format!("unknown printer model {model}"))?;
            let dpi = options.dpi.unwrap_or(printer.dpi);
            let stamps =
                laposte::extract_pdf(&args.input, args.laposte_format, dpi, &options.page)?;
            if stamps.is_empty() {
                return Err("La Poste sheet contains no occupied stamps".into());
            }
            for (index, stamp) in stamps.iter().enumerate() {
                let plan = plan_stamp(stamp, &printer, &options)?;
                let mut stamp_options = options.clone();
                if stamps.len() > 1
                    && let Some(path) = &options.capture
                {
                    stamp_options.capture =
                        Some(path.with_extension(format!("{}.json", index + 1)));
                }
                execute_plan(&plan, &stamp_options).await?;
            }
            eprintln!("processed {} occupied La Poste stamps", stamps.len());
        }
        Command::ExtractPdf(args) => {
            laposte::validate_a4(&args.input, &args.page)?;
            let stamps =
                laposte::extract_pdf(&args.input, args.laposte_format, args.dpi, &args.page)?;
            fs::write(&args.output, laposte::export_stamps_pdf(&stamps, args.dpi)?)?;
            println!(
                "{} ({} occupied stamps)",
                args.output.display(),
                stamps.len()
            );
        }
        Command::Config { command } => match command {
            ConfigCommand::Show => println!("{}", serde_json::to_string_pretty(&cfg)?),
            ConfigCommand::Path => println!("{}", config_path.display()),
            ConfigCommand::Get { key } => {
                let value = serde_json::to_value(&cfg)?;
                let nested = key
                    .split('.')
                    .try_fold(&value, |current, part| current.get(part));
                println!("{}", nested.ok_or("unknown configuration key")?);
            }
            ConfigCommand::Set { key, value } => {
                match key.as_str() {
                    "api_port" => cfg.api_port = value.parse()?,
                    "max_request_bytes" => cfg.max_request_bytes = value.parse()?,
                    "max_document_bytes" => cfg.max_document_bytes = value.parse()?,
                    "max_recent_jobs" => cfg.max_recent_jobs = value.parse()?,
                    "catalogue_path" => cfg.catalogue_path = Some(value.into()),
                    "connections_path" => cfg.connections_path = Some(value.into()),
                    "jobs_path" => cfg.jobs_path = Some(value.into()),
                    key if key.starts_with("printer_defaults.") => {
                        cfg.printer_defaults
                            .set_text(key.trim_start_matches("printer_defaults."), &value)?;
                    }
                    "allowed_origins" => {
                        cfg.allowed_origins = value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                            .collect()
                    }
                    _ => return Err(format!("unknown configuration key {key}").into()),
                };
                config::save(&config_path, &cfg)?;
            }
            ConfigCommand::Unset { key } => {
                let defaults = config::Config::default();
                match key.as_str() {
                    "api_port" => cfg.api_port = defaults.api_port,
                    "allowed_origins" => cfg.allowed_origins.clear(),
                    "max_request_bytes" => cfg.max_request_bytes = defaults.max_request_bytes,
                    "max_document_bytes" => cfg.max_document_bytes = defaults.max_document_bytes,
                    "max_recent_jobs" => cfg.max_recent_jobs = defaults.max_recent_jobs,
                    "catalogue_path" => cfg.catalogue_path = None,
                    "connections_path" => cfg.connections_path = None,
                    "jobs_path" => cfg.jobs_path = None,
                    key if key.starts_with("printer_defaults.") => {
                        cfg.printer_defaults
                            .unset(key.trim_start_matches("printer_defaults."))?;
                    }
                    _ => return Err(format!("unknown configuration key {key}").into()),
                }
                config::save(&config_path, &cfg)?;
            }
            ConfigCommand::Migrate { input } => {
                let legacy: serde_json::Value = serde_json::from_slice(&fs::read(input)?)?;
                if let Some(port) = legacy.get("api_port").and_then(serde_json::Value::as_u64) {
                    cfg.api_port = u16::try_from(port)?;
                }
                if let Some(origins) = legacy
                    .get("allowed_origins")
                    .and_then(serde_json::Value::as_array)
                {
                    cfg.allowed_origins = origins
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                let mut migrated = serde_json::Map::new();
                for key in [
                    "model",
                    "transport",
                    "address",
                    "device",
                    "density",
                    "feed",
                    "speed",
                    "offset_x",
                    "offset_y",
                    "align",
                    "dither",
                    "continuous",
                    "gap_mm",
                    "tspl_offset_mm",
                ] {
                    if let Some(value) = legacy.get(key) {
                        migrated.insert(key.into(), value.clone());
                    }
                }
                if let Some(data) = legacy.get("data").and_then(serde_json::Value::as_object) {
                    migrated.insert("data".into(), serde_json::Value::Object(data.clone()));
                }
                cfg.printer_defaults = serde_json::from_value(serde_json::Value::Object(migrated))?;
                config::save(&config_path, &cfg)?;
                println!(
                    "migrated supported security/service settings to {}",
                    config_path.display()
                );
            }
        },
        Command::Assets { command } => match command {
            AssetCommand::List => println!(
                "{}",
                serde_json::to_string_pretty(&assets::load_catalogue(&catalogue_path)?)?
            ),
            AssetCommand::ImportApk { paths, output } => {
                let bundle = assets::scan_apks(&paths)?;
                let output = output.unwrap_or_else(|| "private.mb-assets".into());
                assets::save_bundle(&output, &bundle)?;
                assets::register_catalogue(&catalogue_path, &output, &bundle)?;
                println!("{}", output.display());
            }
            AssetCommand::ImportAndroid { package, output } => {
                let bundle = assets::import_android(&package)?;
                let output = output.unwrap_or_else(|| "private-android.mb-assets".into());
                assets::save_bundle(&output, &bundle)?;
                assets::register_catalogue(&catalogue_path, &output, &bundle)?;
                println!("{}", output.display());
            }
        },
        Command::Api { command } => {
            let store_path = auth::store_path(&config_path);
            match command {
                ApiCommand::Pair { expires_seconds } => {
                    let mut store = AuthStore::load(store_path)?;
                    let pair = store.begin_pairing(Duration::from_secs(expires_seconds))?;
                    println!(
                        "pairing secret (expires at {}): {}",
                        pair.expires_at, pair.value
                    );
                }
                ApiCommand::Grants => println!(
                    "{}",
                    serde_json::to_string_pretty(&AuthStore::load(store_path)?.grants())?
                ),
                ApiCommand::Revoke { id } => {
                    let mut store = AuthStore::load(store_path)?;
                    if !store.revoke(id.parse()?)? {
                        return Err("grant not found".into());
                    }
                }
                ApiCommand::Rotate {
                    id,
                    expires_seconds,
                } => {
                    let mut store = AuthStore::load(store_path)?;
                    let token = store
                        .rotate(id.parse()?, Duration::from_secs(expires_seconds))?
                        .ok_or("grant not found")?;
                    println!("replacement bearer token (shown once): {token}");
                }
                ApiCommand::Serve { bind, port } => {
                    if cfg.allowed_origins.is_empty() {
                        return Err("configure allowed_origins before serving the API".into());
                    }
                    let state = ApiState::new(AuthStore::load(store_path)?, cfg);
                    api::serve(bind, port, state).await?;
                }
            }
        }
    }
    Ok(())
}

fn validate_document(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 6 * 1024 * 1024 {
        return Err("document exceeds 6 MiB CLI limit".into());
    }
    load_document(path)?;
    Ok(())
}

fn load_document(path: &Path) -> Result<Document, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(path)?;
    let document = Document::from_json(&text).or_else(|_| {
        mb_printer_core::importer::import_v3(&text).and_then(|value| {
            Document::from_json(&value.to_string())
                .map_err(mb_printer_core::importer::ImportError::Json)
        })
    })?;
    document.validate().map_err(|errors| {
        errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(document)
}

fn load_print_input(
    path: &Path,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<Document, Box<dyn std::error::Error>> {
    if path
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("json"))
    {
        return load_document(path);
    }
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path)?;
    let is_svg = path
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("svg"));
    let dpi = options.dpi.unwrap_or(203);
    let (width_mm, height_mm) = if let (Some(w), Some(h)) = (options.width_mm, options.height_mm) {
        (w, h)
    } else if !is_svg {
        let image = image::load_from_memory(&bytes)?;
        (
            f64::from(image.width()) * 25.4 / f64::from(dpi),
            f64::from(image.height()) * 25.4 / f64::from(dpi),
        )
    } else {
        return Err("SVG input requires --width-mm and --height-mm".into());
    };
    if width_mm <= 0.0 || height_mm <= 0.0 {
        return Err("input physical dimensions must be positive".into());
    }
    let width = (width_mm * 1000.0).round() as i64;
    let height = (height_mm * 1000.0).round() as i64;
    let media_type = if is_svg {
        "image/svg+xml"
    } else if path
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("png"))
    {
        "image/png"
    } else {
        "image/jpeg"
    };
    let kind = if is_svg { "svg" } else { "image" };
    let value = json!({"version":4,"name":path.file_stem().and_then(|value|value.to_str()).unwrap_or("Imported input"),"media":{"width":width,"height":height,"unit":"micrometre","dpi":dpi,"orientation":"portrait","printableBounds":{"x":0,"y":0,"width":width,"height":height},"shape":"rectangle","continuous":false,"zones":[]},"coordinateSystem":{"unit":"micrometre","origin":"top-left","rounding":"half-away-from-zero"},"elements":[{"type":kind,"id":"input","transform":{"x":0,"y":0,"width":width,"height":height},"zOrder":0,"resource":"input-resource"}],"resources":[{"id":"input-resource","mediaType":media_type,"sha256":format!("{:x}",Sha256::digest(&bytes)),"dataBase64":base64::engine::general_purpose::STANDARD.encode(bytes)}],"fields":[],"extensions":{}});
    let document = Document::from_json(&value.to_string())?;
    document.validate().map_err(|errors| {
        errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(document)
}

fn parse_assignment(value: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (key, value) = value.split_once('=').ok_or("expected KEY=VALUE")?;
    if key.trim().is_empty() {
        return Err("empty assignment key".into());
    }
    Ok((key.trim().to_owned(), value.to_owned()))
}
fn normalized_header(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}
fn expand_batch(
    document: &Document,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<Vec<(Document, u16)>, Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    let base = options
        .data
        .iter()
        .map(|value| parse_assignment(value))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mappings = options
        .mappings
        .iter()
        .map(|value| parse_assignment(value))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let filter = options
        .filter
        .as_deref()
        .map(parse_assignment)
        .transpose()?;
    let mut records = Vec::new();
    if let Some(path) = &options.csv {
        let mut reader = csv::Reader::from_path(path)?;
        let headers = reader
            .headers()?
            .iter()
            .map(normalized_header)
            .collect::<Vec<_>>();
        for row in reader.records() {
            let row = row?;
            let mut fields = base.clone();
            for (header, value) in headers.iter().zip(row.iter()) {
                fields.insert(header.clone(), value.to_owned());
            }
            for (target, source) in &mappings {
                fields.insert(
                    target.clone(),
                    fields
                        .get(&normalized_header(source))
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            if filter
                .as_ref()
                .is_some_and(|(key, value)| fields.get(key) != Some(value))
            {
                continue;
            }
            records.push(fields);
            if options.limit.is_some_and(|limit| records.len() >= limit) {
                break;
            }
        }
    } else {
        records.push(base)
    }
    let mut output = Vec::new();
    for fields in records {
        for required in &document.fields {
            if !fields.contains_key(&required.key) {
                return Err(format!("missing required field {}", required.key).into());
            }
        }
        let mut value = serde_json::to_value(document)?;
        evaluate_json_templates(&mut value, &fields)?;
        let rendered = Document::from_json(&value.to_string())?;
        let copies = options
            .copies_from
            .as_ref()
            .and_then(|key| fields.get(key))
            .map(|value| value.parse::<u16>())
            .transpose()?
            .unwrap_or(options.copies);
        if copies == 0 {
            return Err("record copies must be positive".into());
        }
        output.push((rendered, copies));
    }
    Ok(output)
}
fn evaluate_json_templates(
    value: &mut serde_json::Value,
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match value {
        serde_json::Value::String(text) if text.contains("{{") => {
            *text = mb_printer_core::template::evaluate(text, fields)?
        }
        serde_json::Value::Array(values) => {
            for value in values {
                evaluate_json_templates(value, fields)?
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                evaluate_json_templates(value, fields)?
            }
        }
        _ => {}
    }
    Ok(())
}
fn apply_config_defaults(
    options: &mut mb_printer_cli::cli::PrintOptions,
    config: &config::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let defaults = &config.printer_defaults;
    if options.model.is_none() {
        options.model = defaults.model.clone()
    }
    if options.transport.is_none() {
        let kind = defaults.transport.as_deref();
        let address = defaults.address.as_deref().or(defaults.device.as_deref());
        if let (Some(kind), Some(address)) = (kind, address) {
            options.transport = Some(match kind {
                "tcp" => format!("tcp://{address}"),
                "file" => format!("file:{address}"),
                "bluetooth" => format!("rfcomm:{address}"),
                _ => format!("{kind}:{address}"),
            });
        }
    }
    if options.density == 6
        && let Some(value) = defaults.density
    {
        options.density = value;
    }
    if options.dither.is_none() {
        options.dither = defaults.dither.clone();
    }
    if options.dpi.is_none() {
        options.dpi = defaults.dpi;
    }
    if options.feed.is_none() {
        options.feed = defaults.feed.and_then(|value| u8::try_from(value).ok());
    }
    if options.speed.is_none() {
        options.speed = defaults.speed;
    }
    if !options.continuous {
        options.continuous = defaults.continuous.unwrap_or(false);
    }
    if options.align.is_none() {
        options.align = defaults.align.clone();
    }
    if options.offset_x == 0 {
        options.offset_x = defaults
            .offset_x
            .and_then(|value| u16::try_from(value.round() as i64).ok())
            .unwrap_or(0);
    }
    if options.offset_y == 0 {
        options.offset_y = defaults
            .offset_y
            .and_then(|value| u16::try_from(value.round() as i64).ok())
            .unwrap_or(0);
    }
    if options.gap_mm.is_none() {
        options.gap_mm = defaults.gap_mm;
    }
    if options.tspl_offset_mm.is_none() {
        options.tspl_offset_mm = defaults.tspl_offset_mm;
    }
    if options.baud == 115_200 {
        options.baud = defaults.baud.unwrap_or(options.baud);
    }
    if options.payload_limit == 512 {
        options.payload_limit = defaults.payload_limit.unwrap_or(options.payload_limit);
    }
    if options.data.is_empty() {
        options.data = defaults
            .data
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
    }
    Ok(())
}

fn plan_document(
    document: &Document,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<Plan, Box<dyn std::error::Error>> {
    let model = options
        .model
        .as_deref()
        .ok_or("--model is required for planning and printing")?;
    let printer =
        capabilities::by_id(model).ok_or_else(|| format!("unknown printer model {model}"))?;
    let mut mono = raster::render_with_dither(
        document,
        options.dpi.unwrap_or(printer.dpi),
        options.dither.as_deref(),
    )?;
    let brother_62x29 = printer.protocol == mb_printer_core::capabilities::Protocol::Brother && {
        let width = document.media.width;
        let height = document.media.height;
        (width.abs_diff(62_000) <= 1_500 && height.abs_diff(29_000) <= 1_500)
            || (width.abs_diff(29_000) <= 1_500 && height.abs_diff(62_000) <= 1_500)
    };
    if brother_62x29 {
        use mb_printer_core::raster::Fit;
        // Brother's DK-11209/62x29 table: a 696x271 printable rectangle,
        // positioned 12 + 44 dots from the right edge on wide QL heads.
        mono = raster::fit_to_box(&mono, 696, 271)?;
        mono = mono.place_on_head(1296, Fit::Right, -56, 0)?;
    } else {
        mono = transform_for_printer(mono, &printer, options)?;
    }
    let head = printer
        .width_px()
        .ok_or("printer has media-dependent head width")?;
    let packed = mb_printer_core::protocol::Raster {
        width_bytes: head.div_ceil(8) as u16,
        height: mono.height,
        data: mono.pack_msb()?,
    };
    let brother_media = if printer.protocol == mb_printer_core::capabilities::Protocol::Brother {
        let millimetres = |value: i64| -> Result<u8, Box<dyn std::error::Error>> {
            Ok(u8::try_from(value.saturating_add(500) / 1000)?)
        };
        Some(mb_printer_core::protocol::BrotherMedia {
            width_mm: if brother_62x29 {
                62
            } else {
                millimetres(document.media.width)?
            },
            length_mm: if document.media.continuous || options.continuous {
                0
            } else if brother_62x29 {
                29
            } else {
                millimetres(document.media.height)?
            },
            continuous: document.media.continuous || options.continuous,
            feed_margin: 0,
        })
    } else {
        None
    };
    let protocol_options = Options {
        density: options.density,
        feed: options.feed.unwrap_or(Options::default().feed),
        speed: options.speed.unwrap_or(Options::default().speed),
        copies: options.copies,
        continuous: options.continuous || document.media.continuous,
        gap_tenths_mm: millimetres_to_tenths(options.gap_mm, Options::default().gap_tenths_mm)?,
        offset_tenths_mm: millimetres_to_tenths(
            options.tspl_offset_mm,
            Options::default().offset_tenths_mm,
        )?,
        offset_x: options.offset_x,
        offset_y: options.offset_y,
        label_width_tenths_mm: u16::try_from(document.media.width / 100).ok(),
        label_height_tenths_mm: u16::try_from(document.media.height / 100).ok(),
        brother_media,
        cut: !options.no_cut,
        cut_every: options.cut_every.unwrap_or(Options::default().cut_every),
        compress: !options.no_compress,
        ..Options::default()
    };
    Ok(protocol::plan(&printer, &packed, &protocol_options)?)
}

fn plan_stamp(
    stamp: &mb_printer_core::laposte::Stamp,
    printer: &mb_printer_core::capabilities::PrinterDefinition,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<Plan, Box<dyn std::error::Error>> {
    use mb_printer_core::raster::Dither;
    let mono = stamp.raster.dither(Dither::Threshold(128))?;
    let fitted = transform_for_printer(mono, printer, options)?;
    let head = printer
        .width_px()
        .ok_or("printer has media-dependent head width")?;
    let packed = mb_printer_core::protocol::Raster {
        width_bytes: head.div_ceil(8) as u16,
        height: fitted.height,
        data: fitted.pack_msb()?,
    };
    Ok(protocol::plan(
        printer,
        &packed,
        &Options {
            density: options.density,
            feed: options.feed.unwrap_or(Options::default().feed),
            speed: options.speed.unwrap_or(Options::default().speed),
            copies: options.copies,
            continuous: options.continuous,
            gap_tenths_mm: millimetres_to_tenths(options.gap_mm, Options::default().gap_tenths_mm)?,
            offset_tenths_mm: millimetres_to_tenths(
                options.tspl_offset_mm,
                Options::default().offset_tenths_mm,
            )?,
            offset_x: options.offset_x,
            offset_y: options.offset_y,
            label_width_tenths_mm: Some(635),
            label_height_tenths_mm: Some(339),
            cut: !options.no_cut,
            cut_every: options.cut_every.unwrap_or(Options::default().cut_every),
            compress: !options.no_compress,
            ..Options::default()
        },
    )?)
}

fn transform_for_printer(
    mut mono: mb_printer_core::raster::MonoRaster,
    printer: &mb_printer_core::capabilities::PrinterDefinition,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<mb_printer_core::raster::MonoRaster, Box<dyn std::error::Error>> {
    use mb_printer_core::raster::{Fit, Rotation};
    mono = match options.rotation.unwrap_or(0) {
        0 => mono,
        90 => mono.rotate(Rotation::Clockwise90),
        180 => mono.rotate(Rotation::Half),
        270 => mono.rotate(Rotation::CounterClockwise90),
        value => return Err(format!("rotation must be 0, 90, 180, or 270 (got {value})").into()),
    };
    if printer.rotated {
        mono = mono.rotate(Rotation::Clockwise90);
    }
    let head = printer
        .width_px()
        .ok_or("printer has media-dependent head width")?;
    if mono.width > head {
        if !options.fit {
            return Err(format!(
                "rendered width {} exceeds printer head {head}; pass --fit",
                mono.width
            )
            .into());
        }
        mono = raster::scale_to_width(&mono, head)?;
    }
    let fit = match options.align.as_deref() {
        Some("left") => Fit::Left,
        Some("center") => Fit::Center,
        Some("right") => Fit::Right,
        Some(value) => return Err(format!("unknown alignment {value}").into()),
        None => match printer.alignment {
            mb_printer_core::capabilities::Alignment::Left => Fit::Left,
            mb_printer_core::capabilities::Alignment::Center => Fit::Center,
            mb_printer_core::capabilities::Alignment::Right => Fit::Right,
        },
    };
    let (offset_x, offset_y) = if printer.protocol == mb_printer_core::capabilities::Protocol::Tspl
    {
        (0, 0)
    } else {
        (i32::from(options.offset_x), i32::from(options.offset_y))
    };
    Ok(mono.place_on_head_byte_aligned(head, fit, offset_x, offset_y)?)
}

fn millimetres_to_tenths(
    value: Option<f64>,
    default: i16,
) -> Result<i16, Box<dyn std::error::Error>> {
    let Some(value) = value else {
        return Ok(default);
    };
    if !value.is_finite() {
        return Err("millimetre protocol option must be finite".into());
    }
    Ok(i16::try_from((value * 10.0).round() as i64)?)
}

async fn execute_plan(
    plan: &Plan,
    options: &mb_printer_cli::cli::PrintOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    if options.dry_run {
        let mut target = CaptureTransport::new(options.payload_limit);
        if matches!(
            plan.protocol,
            mb_printer_core::capabilities::Protocol::Brother
        ) {
            let mut response = vec![0; 32];
            response[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
            target.response = Some(response);
        }
        let progress = mb_printer_native::execute(plan, &mut target)?;
        let encoded = transport::capture_json(plan, &target)?;
        if let Some(path) = &options.capture {
            fs::write(path, encoded)?;
        } else {
            println!("{}", String::from_utf8(encoded)?);
        }
        eprintln!(
            "dry-run: {} bytes through action {:?}",
            progress.bytes_written, progress.last_completed_action
        );
        return Ok(());
    }
    let uri = options
        .transport
        .as_deref()
        .or(options.printer.as_deref())
        .ok_or("--transport is required unless --dry-run is used")?;
    let progress = if let Some(path) = uri.strip_prefix("file:") {
        let mut target = WriteTransport::file(Path::new(path), options.payload_limit)?;
        mb_printer_native::execute(plan, &mut target)?
    } else if let Some(address) = uri.strip_prefix("ipp://") {
        execute_ipp_plan(plan, address, options.payload_limit)?
    } else if let Some(address) = uri.strip_prefix("tcp://") {
        if plan.protocol == mb_printer_core::capabilities::Protocol::Brother
            && address
                .rsplit_once(':')
                .is_some_and(|(_, port)| port == "631")
        {
            execute_ipp_plan(plan, address, options.payload_limit)?
        } else {
            let mut target =
                TcpTransport::connect(address, options.payload_limit, Duration::from_secs(5))?;
            mb_printer_native::execute(plan, &mut target)?
        }
    } else if let Some(path) = uri.strip_prefix("serial:") {
        let mut target =
            SerialTransport::open(Path::new(path), options.baud, options.payload_limit)?;
        mb_printer_native::execute(plan, &mut target)?
    } else if let Some(spec) = uri.strip_prefix("rfcomm:") {
        #[cfg(all(feature = "bluetooth", target_os = "linux"))]
        {
            let (address, channel) = parse_rfcomm(spec)?;
            let mut target = mb_printer_native::transports::rfcomm::RfcommTransport::bind(
                0,
                address,
                channel,
                options.payload_limit,
            )?;
            mb_printer_native::execute(plan, &mut target)?
        }
        #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
        {
            let _ = spec;
            return Err("RFCOMM requires the bluetooth feature on Linux".into());
        }
    } else if let Some(address) = uri.strip_prefix("ble:") {
        #[cfg(feature = "bluetooth")]
        {
            let mut target =
                transport::bluetooth::BleTransport::connect(address, options.payload_limit).await?;
            mb_printer_native::execute(plan, &mut target)?
        }
        #[cfg(not(feature = "bluetooth"))]
        {
            let _ = address;
            return Err("BLE support requires the bluetooth Cargo feature".into());
        }
    } else if let Some(spec) = uri.strip_prefix("usb:") {
        #[cfg(feature = "usb")]
        {
            let parts = spec.split(':').collect::<Vec<_>>();
            if !(4..=5).contains(&parts.len()) {
                return Err("USB transport is usb:VID:PID:INTERFACE:OUT[:IN]".into());
            }
            let hex = |value: &str| u16::from_str_radix(value.trim_start_matches("0x"), 16);
            let vid = hex(parts[0])?;
            let pid = hex(parts[1])?;
            let interface = u8::try_from(hex(parts[2])?)?;
            let out = u8::try_from(hex(parts[3])?)?;
            let input = if let Some(value) = parts.get(4) {
                Some(u8::try_from(hex(value)?)?)
            } else {
                None
            };
            let mut target = transport::usb::UsbTransport::open(
                vid,
                pid,
                interface,
                out,
                input,
                options.payload_limit,
            )?;
            mb_printer_native::execute(plan, &mut target)?
        }
        #[cfg(not(feature = "usb"))]
        {
            let _ = spec;
            return Err("USB support requires the usb Cargo feature".into());
        }
    } else {
        return Err(
            "transport must use file:, tcp://, ipp://, serial:, rfcomm:, ble:, or usb:".into(),
        );
    };
    eprintln!(
        "completed: {} bytes, last action {:?}",
        progress.bytes_written, progress.last_completed_action
    );
    Ok(())
}

fn execute_ipp_plan(
    plan: &Plan,
    address: &str,
    payload_limit: usize,
) -> Result<mb_printer_native::Progress, Box<dyn std::error::Error>> {
    if plan.protocol != mb_printer_core::capabilities::Protocol::Brother {
        return Err("IPP octet-stream printing currently requires a Brother raster plan".into());
    }
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (host, port.parse()?),
        None => (address, 631),
    };
    let attributes = mb_printer_cli::device::ipp_query(host, port, Duration::from_secs(5))?;
    let media = attributes
        .get("media-ready")
        .and_then(|values| values.first())
        .and_then(|value| match value {
            mb_printer_cli::device::IppValue::Text(value) => Some(value.as_str()),
            mb_printer_cli::device::IppValue::Integer(_) => None,
        })
        .ok_or("IPP printer did not report loaded media")?;
    let mut capture = CaptureTransport::new(payload_limit);
    let mut response = vec![0; 32];
    response[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
    capture.response = Some(response);
    let progress = mb_printer_native::execute(plan, &mut capture)?;
    let document = capture
        .events
        .iter()
        .filter_map(|event| match event {
            PhysicalEvent::Write { bytes } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    let result = mb_printer_cli::device::ipp_print_job(
        host,
        port,
        &document,
        media,
        Duration::from_secs(15),
    )?;
    let job_id = result
        .get("job-id")
        .and_then(|values| values.first())
        .and_then(|value| match value {
            mb_printer_cli::device::IppValue::Integer(value) => Some(*value),
            mb_printer_cli::device::IppValue::Text(_) => None,
        });
    eprintln!(
        "IPP job accepted: {}",
        job_id.map_or_else(|| "unknown".into(), |id| id.to_string())
    );
    Ok(progress)
}

#[cfg(all(feature = "bluetooth", target_os = "linux"))]
fn parse_rfcomm(spec: &str) -> Result<(&str, u8), Box<dyn std::error::Error>> {
    let (address, channel) = spec.rsplit_once('@').map_or((spec, "1"), |parts| parts);
    if address.is_empty()
        || !address
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
    {
        return Err("RFCOMM selector is rfcomm:MAC[@CHANNEL]".into());
    }
    let channel = channel.parse::<u8>()?;
    if channel == 0 || channel > 30 {
        return Err("RFCOMM channel must be 1..30".into());
    }
    Ok((address, channel))
}

fn query_brother_status<T: mb_printer_native::Transport>(
    transport: &mut T,
) -> Result<mb_printer_cli::device::BrotherStatus, Box<dyn std::error::Error>> {
    transport.write(b"\x1biS")?;
    match transport.wait_response(3_000)? {
        mb_printer_native::WaitOutcome::Response(bytes) => {
            Ok(mb_printer_cli::device::brother_status(&bytes)?)
        }
        mb_printer_native::WaitOutcome::Timeout => Err("Brother status timed out".into()),
        mb_printer_native::WaitOutcome::Unavailable => {
            Err("transport cannot read Brother status".into())
        }
    }
}
