// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{fs, path::Path};

#[test]
fn shipped_sources_and_metadata_declare_agpl() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("license = \"AGPL-3.0-or-later\""));
    assert!(manifest.starts_with("# SPDX-License-Identifier: AGPL-3.0-or-later"));
    let license = fs::read_to_string(root.join("LICENSE")).unwrap();
    assert!(license.contains("GNU AFFERO GENERAL PUBLIC LICENSE"));
    assert!(
        license.contains("END OF TERMS AND CONDITIONS"),
        "LICENSE must contain the complete text"
    );
    assert!(root.join("NOTICE.md").exists());
    assert!(
        fs::read_to_string(root.join("docs/openapi.yaml"))
            .unwrap()
            .starts_with("# SPDX-License-Identifier: AGPL-3.0-or-later")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(root.join("tests/fixtures/editor-job.json")).unwrap()
        )
        .unwrap()["spdxLicenseIdentifier"],
        "AGPL-3.0-or-later"
    );
    for directory in ["src", "tests"] {
        for entry in fs::read_dir(root.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                assert!(
                    fs::read_to_string(&path)
                        .unwrap()
                        .contains("SPDX-License-Identifier: AGPL-3.0-or-later"),
                    "missing SPDX: {}",
                    path.display()
                );
            }
        }
    }
}
