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
