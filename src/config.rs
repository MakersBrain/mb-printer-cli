// SPDX-License-Identifier: AGPL-3.0-or-later
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_port")]
    pub api_port: u16,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_request_limit")]
    pub max_request_bytes: usize,
    #[serde(default = "default_document_limit")]
    pub max_document_bytes: usize,
    #[serde(default = "default_jobs")]
    pub max_recent_jobs: usize,
    #[serde(default)]
    pub catalogue_path: Option<PathBuf>,
    #[serde(default)]
    pub connections_path: Option<PathBuf>,
    #[serde(default)]
    pub jobs_path: Option<PathBuf>,
    #[serde(default)]
    pub printer_defaults: BTreeMap<String, serde_json::Value>,
}

const fn default_port() -> u16 {
    9847
}
const fn default_request_limit() -> usize {
    8 * 1024 * 1024
}
const fn default_document_limit() -> usize {
    6 * 1024 * 1024
}
const fn default_jobs() -> usize {
    100
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_port: default_port(),
            allowed_origins: vec![],
            max_request_bytes: default_request_limit(),
            max_document_bytes: default_document_limit(),
            max_recent_jobs: default_jobs(),
            catalogue_path: None,
            connections_path: None,
            jobs_path: None,
            printer_defaults: BTreeMap::new(),
        }
    }
}

pub fn default_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mb-printer/config.json")
}

pub fn load(path: &Path) -> io::Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

pub fn save(path: &Path, config: &Config) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(config).map_err(io::Error::other)?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_config_fields() {
        assert!(serde_json::from_str::<Config>(r#"{"surprise":true}"#).is_err());
    }
}
