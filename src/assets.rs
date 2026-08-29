// SPDX-License-Identifier: AGPL-3.0-or-later
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io,
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueEntry {
    pub path: PathBuf,
    pub source: String,
    pub entries: usize,
    pub private: bool,
}

pub fn load_catalogue(path: &Path) -> io::Result<Vec<CatalogueEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
}
pub fn register_catalogue(
    index: &Path,
    bundle_path: &Path,
    bundle: &PrivateBundle,
) -> io::Result<()> {
    validate_bundle(bundle)?;
    let mut catalogue = load_catalogue(index)?;
    let entry = CatalogueEntry {
        path: bundle_path.to_path_buf(),
        source: bundle.source.clone(),
        entries: bundle.entries.len(),
        private: true,
    };
    catalogue.retain(|existing| existing.path != entry.path);
    catalogue.push(entry);
    catalogue.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(parent) = index.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = index.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&catalogue).map_err(io::Error::other)?,
    )?;
    std::fs::rename(temporary, index)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrivateBundle {
    pub schema: u8,
    pub private: bool,
    pub source: String,
    pub entries: Vec<PrivateEntry>,
    pub remote_references_fetched: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct PrivateEntry {
    pub archive: PathBuf,
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub redistributable: bool,
    pub data_base64: String,
}

const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
fn safe_kind(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if [
        "credential",
        "auth",
        "token",
        "account",
        "cookie",
        "database",
        "shared_prefs",
        "private/",
        "paid/",
    ]
    .iter()
    .any(|part| n.contains(part))
    {
        return None;
    }
    if n.ends_with(".ttf") || n.ends_with(".otf") {
        Some("font")
    } else if n.ends_with(".png")
        || n.ends_with(".webp")
        || n.ends_with(".jpg")
        || n.ends_with(".svg")
    {
        Some("image")
    } else if n.ends_with(".json") || n.ends_with(".xml") {
        Some("metadata")
    } else {
        None
    }
}

pub fn scan_apks(paths: &[PathBuf]) -> io::Result<PrivateBundle> {
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    for path in paths {
        let mut zip = zip::ZipArchive::new(File::open(path)?)?;
        if zip.len() > MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "APK contains too many entries",
            ));
        }
        for index in 0..zip.len() {
            let mut item = zip.by_index(index)?;
            let name = item.name().to_owned();
            if item.enclosed_name().is_none() || item.size() > MAX_ENTRY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe APK entry",
                ));
            }
            total_bytes = total_bytes
                .checked_add(item.size())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "APK size overflow"))?;
            if total_bytes > MAX_TOTAL_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "APK inventory exceeds size limit",
                ));
            }
            let Some(kind) = safe_kind(&name) else {
                continue;
            };
            if item.is_dir() {
                continue;
            }
            let mut bytes = Vec::with_capacity(item.size() as usize);
            io::copy(&mut item, &mut bytes)?;
            let mut h = Sha256::new();
            h.update(&bytes);
            entries.push(PrivateEntry {
                archive: path
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("split.apk")),
                path: name,
                kind: kind.into(),
                sha256: format!("{:x}", h.finalize()),
                redistributable: false,
                data_base64: STANDARD.encode(bytes),
            });
        }
    }
    Ok(PrivateBundle {
        schema: 1,
        private: true,
        source: "local-apk-clean-room-inventory".into(),
        entries,
        remote_references_fetched: false,
    })
}

pub fn android_split_paths(package: &str) -> io::Result<Vec<String>> {
    if package != "com.project.aimotech.printmaster" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only the explicitly scoped Print Master package is accepted",
        ));
    }
    let output = Command::new("adb")
        .args(["shell", "pm", "path", package])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("adb pm path failed"));
    }
    let paths: BTreeSet<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("package:"))
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no APK splits found",
        ));
    }
    Ok(paths.into_iter().collect())
}

pub fn save_bundle(path: &Path, bundle: &PrivateBundle) -> io::Result<()> {
    validate_bundle(bundle)?;
    std::fs::write(
        path,
        serde_json::to_vec_pretty(bundle).map_err(io::Error::other)?,
    )
}
pub fn load_bundle(path: &Path) -> io::Result<PrivateBundle> {
    let bundle: PrivateBundle =
        serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    validate_bundle(&bundle)?;
    Ok(bundle)
}
fn validate_bundle(bundle: &PrivateBundle) -> io::Result<()> {
    if bundle.schema != 1
        || !bundle.private
        || bundle.remote_references_fetched
        || bundle.entries.iter().any(|entry| {
            entry.redistributable
                || entry.sha256.len() != 64
                || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                || STANDARD
                    .decode(&entry.data_base64)
                    .map(|bytes| format!("{:x}", Sha256::digest(bytes)) != entry.sha256)
                    .unwrap_or(true)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bundle violates private-only policy",
        ));
    }
    Ok(())
}

pub fn import_android(package: &str) -> io::Result<PrivateBundle> {
    let remote = android_split_paths(package)?;
    let directory = tempfile::tempdir()?;
    let mut local = Vec::new();
    for (index, path) in remote.iter().enumerate() {
        let destination = directory.path().join(format!("split-{index}.apk"));
        let status = Command::new("adb")
            .args(["pull", path])
            .arg(&destination)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("adb pull failed"));
        }
        local.push(destination);
    }
    scan_apks(&local)
}

pub fn inventory_directory(root: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_unscoped_android_packages() {
        assert!(android_split_paths("other.vendor").is_err());
    }
    #[test]
    fn privacy_filter_excludes_credentials_paid_and_tokens() {
        assert_eq!(safe_kind("assets/fonts/free.ttf"), Some("font"));
        for path in [
            "shared_prefs/auth.xml",
            "assets/paid/icon.png",
            "res/raw/token.json",
            "private/account.webp",
        ] {
            assert_eq!(safe_kind(path), None, "{path}");
        }
    }
    #[test]
    fn rejects_bundle_that_could_be_published() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.mb-assets");
        let bad = PrivateBundle {
            schema: 1,
            private: true,
            source: "test".into(),
            entries: vec![PrivateEntry {
                archive: "x.apk".into(),
                path: "icon.png".into(),
                kind: "image".into(),
                sha256: "0".repeat(64),
                redistributable: true,
                data_base64: STANDARD.encode(b"bad"),
            }],
            remote_references_fetched: false,
        };
        assert!(save_bundle(&path, &bad).is_err());
    }
    #[test]
    fn private_bundle_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ok.mb-assets");
        let bundle = PrivateBundle {
            schema: 1,
            private: true,
            source: "test".into(),
            entries: vec![],
            remote_references_fetched: false,
        };
        save_bundle(&path, &bundle).unwrap();
        let loaded = load_bundle(&path).unwrap();
        assert!(loaded.private);
        assert!(!loaded.remote_references_fetched);
    }
    #[test]
    fn catalogue_is_persisted_and_deduplicated() {
        let directory = tempfile::tempdir().unwrap();
        let bundle_path = directory.path().join("private.mb-assets");
        let index = directory.path().join("catalogues.json");
        let bundle = PrivateBundle {
            schema: 1,
            private: true,
            source: "fixture".into(),
            entries: vec![],
            remote_references_fetched: false,
        };
        register_catalogue(&index, &bundle_path, &bundle).unwrap();
        register_catalogue(&index, &bundle_path, &bundle).unwrap();
        assert_eq!(
            load_catalogue(&index).unwrap(),
            vec![CatalogueEntry {
                path: bundle_path,
                source: "fixture".into(),
                entries: 0,
                private: true
            }]
        );
    }
    #[test]
    fn synthetic_split_apks_persist_allowed_bytes() {
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, entry, data) in [
            ("base.apk", "assets/fonts/free.ttf", b"font".as_slice()),
            ("split.apk", "res/drawable/icon.png", b"png".as_slice()),
        ] {
            let path = directory.path().join(name);
            let file = File::create(&path).unwrap();
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file(entry, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(data).unwrap();
            archive.finish().unwrap();
            paths.push(path);
        }
        let bundle = scan_apks(&paths).unwrap();
        assert_eq!(bundle.entries.len(), 2);
        let output = directory.path().join("roundtrip.mb-assets");
        save_bundle(&output, &bundle).unwrap();
        let loaded = load_bundle(&output).unwrap();
        assert!(
            loaded
                .entries
                .iter()
                .any(|entry| STANDARD.decode(&entry.data_base64).unwrap() == b"font")
        );
    }
}
