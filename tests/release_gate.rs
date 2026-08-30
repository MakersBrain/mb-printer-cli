// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{fs, path::Path, process::Command};

fn fixture(root: &Path, version: &str, changelog: &str, locked: &str) {
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"mb-printer-cli\"\nversion = \"{version}\"\n\
             [dependencies]\nmb-printer-core = {{ version = \"{version}\", path = \"../core\" }}\n\
             mb-printer-native = {{ version = \"{version}\", path = \"../native\" }}\n"
        ),
    )
    .unwrap();
    fs::write(root.join("CHANGELOG.md"), changelog).unwrap();
    fs::write(
        root.join("Cargo.lock"),
        format!("[[package]]\nname = \"mb-printer-cli\"\nversion = \"{locked}\"\n"),
    )
    .unwrap();
}

fn gate(root: &Path, tag: &str) -> bool {
    Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check_release_version.sh"
        ))
        .arg(tag)
        .env("RELEASE_ROOT", root)
        .status()
        .unwrap()
        .success()
}

#[test]
fn release_gate_accepts_only_consistent_finalized_version() {
    let directory = tempfile::tempdir().unwrap();
    fixture(
        directory.path(),
        "0.1.0",
        "# Changelog\n\n## Unreleased\n\nNo changes yet.\n\n## 0.1.0\n\n- Ready.\n",
        "0.1.0",
    );
    assert!(gate(directory.path(), "v0.1.0"));
    assert!(!gate(directory.path(), "v0.1.1"));

    fixture(
        directory.path(),
        "0.1.0",
        "# Changelog\n\n## Unreleased\n\n- Not finalized.\n\n## 0.1.0\n\n- Ready.\n",
        "0.1.0",
    );
    assert!(!gate(directory.path(), "v0.1.0"));

    fixture(
        directory.path(),
        "0.1.0",
        "# Changelog\n\n## Unreleased\n\nNo changes yet.\n\n## 0.1.0\n\n- Ready.\n",
        "0.0.9",
    );
    assert!(!gate(directory.path(), "v0.1.0"));
}

#[test]
fn sdk_pin_is_a_reachable_main_commit_used_by_every_build_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sdk = root.join("../mb-printer-sdk");
    let pin = fs::read_to_string(root.join(".github/sdk-ref")).unwrap();
    let pin = pin.trim();
    assert_eq!(pin.len(), 40);
    assert!(
        pin.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let checked = Command::new("sh")
        .arg(root.join("scripts/check_sdk_pin.sh"))
        .arg(&sdk)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(String::from_utf8(checked.stdout).unwrap().trim(), pin);

    let malformed = tempfile::tempdir().unwrap();
    fs::create_dir(malformed.path().join(".github")).unwrap();
    fs::write(malformed.path().join(".github/sdk-ref"), "not-a-commit\n").unwrap();
    let rejected = Command::new("sh")
        .arg(root.join("scripts/check_sdk_pin.sh"))
        .arg(&sdk)
        .env("RELEASE_ROOT", malformed.path())
        .output()
        .unwrap();
    assert!(!rejected.status.success());

    for workflow in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let contents = fs::read_to_string(root.join(workflow)).unwrap();
        serde_yaml::from_str::<serde_yaml::Value>(&contents).unwrap();
        assert!(contents.contains(".github/sdk-ref"));
        assert!(contents.contains("ref: '${{ steps.sdk.outputs.ref }}'"));
        assert!(contents.contains("scripts/check_sdk_pin.sh"));
    }
    let candidate = fs::read_to_string(root.join("scripts/build_release_candidate.sh")).unwrap();
    assert!(candidate.contains("scripts/check_sdk_pin.sh"));
    assert!(candidate.contains("git -C \"$sdk_root\" archive \"$sdk_ref\""));
}
