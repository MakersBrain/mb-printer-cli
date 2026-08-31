// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{fs, process::Command};

fn document() -> String {
    serde_json::json!({
        "version":4,"name":"CLI integration","media":{"width":10000,"height":5000,"unit":"micrometre","dpi":203,"orientation":"portrait","printableBounds":{"x":0,"y":0,"width":10000,"height":5000},"shape":"rectangle","continuous":false,"zones":[]},
        "coordinateSystem":{"unit":"micrometre","origin":"top-left","rounding":"half-away-from-zero"},
        "elements":[{"type":"rectangle","id":"box","transform":{"x":1000,"y":1000,"width":8000,"height":3000},"zOrder":0,"strokeWidth":125,"fill":false}],"resources":[],"fields":[],"extensions":{}
    }).to_string()
}

#[test]
fn render_list_and_dry_run_are_real_commands() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("label.mb-label.json");
    let png = directory.path().join("preview.png");
    let capture = directory.path().join("job.capture.json");
    fs::write(&input, document()).unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let validate = Command::new(binary)
        .args(["validate", input.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let render = Command::new(binary)
        .args([
            "render",
            input.to_str().unwrap(),
            "-o",
            png.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        render.status.success(),
        "{}",
        String::from_utf8_lossy(&render.stderr)
    );
    assert!(fs::read(&png).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
    let printers = Command::new(binary).arg("printers").output().unwrap();
    assert!(printers.status.success());
    assert!(String::from_utf8(printers.stdout).unwrap().contains("m110"));
    let print = Command::new(binary)
        .args([
            "print",
            input.to_str().unwrap(),
            "--model",
            "m110",
            "--dry-run",
            "--capture",
            capture.to_str().unwrap(),
            "--payload-limit",
            "20",
        ])
        .output()
        .unwrap();
    assert!(
        print.status.success(),
        "{}",
        String::from_utf8_lossy(&print.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    assert_eq!(value["schema"], 1);
    assert!(value["plan"]["actions"].as_array().unwrap().len() > 5);
    assert!(!value["concatenated_bytes"].as_array().unwrap().is_empty());
}

#[test]
fn document_svg_and_tiled_export_surfaces_execute() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.svg");
    let document = directory.path().join("imported.json");
    let svg = directory.path().join("preview.svg");
    let tiles = directory.path().join("tile.png");
    fs::write(&source, r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5"><rect width="10" height="5" fill="black"/></svg>"#).unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    for args in [
        vec![
            "document",
            "import-svg",
            source.to_str().unwrap(),
            "-o",
            document.to_str().unwrap(),
            "--width-mm",
            "10",
            "--height-mm",
            "5",
        ],
        vec!["document", "fields", document.to_str().unwrap()],
        vec![
            "export",
            document.to_str().unwrap(),
            "-o",
            svg.to_str().unwrap(),
        ],
        vec![
            "export",
            document.to_str().unwrap(),
            "-o",
            tiles.to_str().unwrap(),
            "--tile-width",
            "8",
            "--tile-height",
            "8",
        ],
    ] {
        let output = Command::new(binary).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(fs::read_to_string(svg).unwrap().starts_with("<svg"));
    assert!(directory.path().join("tile.0001.png").exists());
}

#[test]
fn legacy_direct_inputs_rotation_fit_and_sheet_export_execute() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let legacy = directory.path().join("legacy.json");
    fs::write(
        &legacy,
        r#"{"version":3,"name":"Legacy","widthMm":20,"heightMm":10,"dotsPerMm":8,"elements":[]}"#,
    )
    .unwrap();
    let validation = Command::new(binary)
        .args(["validate", legacy.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        validation.status.success(),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );

    let input = directory.path().join("label.json");
    let sheet = directory.path().join("sheet.pdf");
    fs::write(&input, document()).unwrap();
    let export = Command::new(binary)
        .args([
            "export",
            input.to_str().unwrap(),
            "-o",
            sheet.to_str().unwrap(),
            "--paper",
            "a4",
            "--columns",
            "2",
            "--rows",
            "2",
            "--cut-marks",
        ])
        .output()
        .unwrap();
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert!(
        String::from_utf8_lossy(&fs::read(&sheet).unwrap())
            .contains("/MediaBox [0 0 595.275591 841.889764]")
    );
    assert!(
        String::from_utf8_lossy(&fs::read(&sheet).unwrap()).contains("/Width 1678 /Height 2374")
    );

    let high_dpi_sheet = directory.path().join("sheet-300dpi.pdf");
    let high_dpi_export = Command::new(binary)
        .args([
            "export",
            input.to_str().unwrap(),
            "-o",
            high_dpi_sheet.to_str().unwrap(),
            "--dpi",
            "300",
            "--paper",
            "a4",
            "--columns",
            "2",
            "--rows",
            "2",
            "--cut-marks",
        ])
        .output()
        .unwrap();
    assert!(
        high_dpi_export.status.success(),
        "{}",
        String::from_utf8_lossy(&high_dpi_export.stderr)
    );
    assert!(
        String::from_utf8_lossy(&fs::read(high_dpi_sheet).unwrap())
            .contains("/Width 2480 /Height 3508")
    );

    let svg = directory.path().join("wide.svg");
    let capture = directory.path().join("svg.json");
    fs::write(&svg, r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="10"><rect width="100" height="10" fill="black"/></svg>"#).unwrap();
    let failure = Command::new(binary)
        .args([
            "print",
            svg.to_str().unwrap(),
            "--model",
            "m110",
            "--width-mm",
            "100",
            "--height-mm",
            "10",
            "--rotation",
            "180",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("pass --fit"));
    let fitted = Command::new(binary)
        .args([
            "print",
            svg.to_str().unwrap(),
            "--model",
            "m110",
            "--width-mm",
            "100",
            "--height-mm",
            "10",
            "--rotation",
            "180",
            "--fit",
            "--dry-run",
            "--capture",
            capture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        fitted.status.success(),
        "{}",
        String::from_utf8_lossy(&fitted.stderr)
    );
    assert!(capture.exists());
}

#[test]
fn csv_batch_mapping_filter_limit_copies_and_config_defaults_are_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let input = directory.path().join("batch.json");
    let mut value: serde_json::Value = serde_json::from_str(&document()).unwrap();
    value["fields"] = serde_json::json!([
        {"key":"name","label":"Name"},
        {"key":"copies","label":"Copies"}
    ]);
    fs::write(&input, serde_json::to_vec(&value).unwrap()).unwrap();
    let csv = directory.path().join("data.csv");
    fs::write(
        &csv,
        "Customer Name,Copies,Keep\nAlice,2,yes\nBob,3,no\nCarol,4,yes\n",
    )
    .unwrap();
    let config = directory.path().join("config.json");
    fs::write(
        &config,
        r#"{"printer_defaults":{"model":"m110","density":4}}"#,
    )
    .unwrap();
    let capture = directory.path().join("batch.json.capture");
    let run = || {
        Command::new(binary)
            .args([
                "--config",
                config.to_str().unwrap(),
                "print",
                input.to_str().unwrap(),
                "--csv",
                csv.to_str().unwrap(),
                "--map",
                "name=Customer Name",
                "--filter",
                "keep=yes",
                "--limit",
                "1",
                "--copies-from",
                "copies",
                "--dry-run",
                "--capture",
                capture.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let record = capture.with_extension("record-1.json");
    let first_bytes = fs::read(&record).unwrap();
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first_bytes, fs::read(record).unwrap());
    assert!(!capture.with_extension("record-2.json").exists());
}

#[test]
fn mocked_brother_and_wifi_status_workflows_decode_without_hardware() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let brother = directory.path().join("brother.bin");
    let mut reply = [0_u8; 32];
    reply[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
    reply[10] = 62;
    reply[11] = 0x0b;
    reply[17] = 29;
    fs::write(&brother, reply).unwrap();
    let status = Command::new(binary)
        .args(["status", "--response", brother.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("die-cut"));

    let wifi = directory.path().join("wifi.txt");
    fs::write(
        &wifi,
        "VAP,-43-61-66-65,x,x,6,-42,0,2\nVAP,-47-75-65-73-74,x,x,11,-70,0,1\n",
    )
    .unwrap();
    let scan = Command::new(binary)
        .args(["wifi", "scan", "--input", wifi.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let text = String::from_utf8(scan.stdout).unwrap();
    assert!(text.contains("Cafe"));
    assert!(text.find("Cafe").unwrap() < text.find("Guest").unwrap());
    let wifi_status = directory.path().join("wifi-status.txt");
    fs::write(
        &wifi_status,
        "OBJBRNET\r\n\"458867:1\"\r\n\"458967.2:-c0-a8-01-64\"\r\n\"458877:-43-61-66-65\"\r\n\"458880:8\"\r\n\"458881:3\"\r\n\"459138.2:1\"\r\n\"459138.3:0\"\r\n",
    )
    .unwrap();
    let decoded_status = Command::new(binary)
        .args(["wifi", "status", "--input", wifi_status.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(decoded_status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&decoded_status.stdout).unwrap();
    assert_eq!(status["connected"], true);
    assert_eq!(status["ipAddress"], "192.168.1.100");
    assert_eq!(status["ssid"], "Cafe");
    assert_eq!(status["encryption"], "tkip-aes");
    assert_eq!(status["authentication"], "wpa-psk");
}

#[test]
fn typed_nested_config_and_test_alias_are_exposed() {
    let directory = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let config = directory.path().join("config.json");
    for args in [
        vec![
            "--config",
            config.to_str().unwrap(),
            "config",
            "set",
            "printer_defaults.density",
            "4",
        ],
        vec![
            "--config",
            config.to_str().unwrap(),
            "config",
            "get",
            "printer_defaults.density",
        ],
    ] {
        let output = Command::new(binary).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.stdout.is_empty() {
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
        }
    }
    let help = Command::new(binary)
        .args(["test", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("density"));
}

#[test]
fn empty_laposte_sheet_fails_before_transport_or_capture() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("empty-a4.pdf");
    let capture = directory.path().join("must-not-exist.json");
    let raster = mb_printer_core::raster::MonoRaster {
        width: 248,
        height: 351,
        pixels: vec![0; 248 * 351],
    };
    fs::write(
        &input,
        mb_printer_core::export::pdf_physical(&raster, 210_000, 297_000).unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mb-printer"))
        .args([
            "print-pdf",
            input.to_str().unwrap(),
            "--laposte-format",
            "L24A",
            "--model",
            "m110",
            "--dpi",
            "30",
            "--dry-run",
            "--capture",
            capture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no occupied stamps"));
    assert!(!capture.exists());
}
