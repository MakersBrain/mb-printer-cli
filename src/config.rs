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
    /// Permit the locally approved Brother USB Wi-Fi mutation API. This is
    /// deliberately off by default; read-only wireless diagnostics remain
    /// available for supported USB models.
    #[serde(default)]
    pub enable_brother_wifi_configuration: bool,
    /// Permit issuing and exchanging short-lived administrator pairing secrets
    /// for Brother Wi-Fi administration. It is intentionally a separate,
    /// default-off switch from the mutation feature itself.
    #[serde(default)]
    pub enable_brother_wifi_configuration_pairing: bool,
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
    pub cloud: Option<CloudConfig>,
    #[serde(default)]
    pub printer_defaults: PrinterDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudConfig {
    pub server: String,
    pub agent_id: uuid::Uuid,
    pub token_path: PathBuf,
    pub jobs_path: PathBuf,
    #[serde(default)]
    pub printers: Vec<CloudPrinter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudPrinter {
    pub id: uuid::Uuid,
    pub connection_id: String,
    pub name: String,
    pub model: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

const fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrinterDefaults {
    pub model: Option<String>,
    pub transport: Option<String>,
    pub address: Option<String>,
    pub device: Option<String>,
    pub density: Option<u8>,
    pub dpi: Option<u16>,
    pub baud: Option<u32>,
    pub payload_limit: Option<usize>,
    pub feed: Option<u32>,
    pub speed: Option<u8>,
    pub offset_x: Option<f64>,
    pub offset_y: Option<f64>,
    pub align: Option<String>,
    pub dither: Option<String>,
    pub continuous: Option<bool>,
    pub gap_mm: Option<f64>,
    pub tspl_offset_mm: Option<f64>,
    #[serde(default)]
    pub data: BTreeMap<String, String>,
}
impl PrinterDefaults {
    pub fn set_text(&mut self, key: &str, raw: &str) -> Result<(), serde_json::Error> {
        let mut value = serde_json::to_value(&*self)?;
        let parsed =
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()));
        if let Some(field) = key.strip_prefix("data.") {
            value["data"]
                .as_object_mut()
                .expect("data serializes as an object")
                .insert(field.to_owned(), parsed);
        } else {
            value
                .as_object_mut()
                .expect("defaults serialize as an object")
                .insert(key.to_owned(), parsed);
        }
        *self = serde_json::from_value(value)?;
        Ok(())
    }
    pub fn unset(&mut self, key: &str) -> Result<(), serde_json::Error> {
        let mut value = serde_json::to_value(&*self)?;
        if let Some(field) = key.strip_prefix("data.") {
            value["data"]
                .as_object_mut()
                .expect("data serializes as an object")
                .remove(field);
        } else {
            value
                .as_object_mut()
                .expect("defaults serialize as an object")
                .remove(key);
        }
        *self = serde_json::from_value(value)?;
        Ok(())
    }
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
            enable_brother_wifi_configuration: false,
            enable_brother_wifi_configuration_pairing: false,
            max_request_bytes: default_request_limit(),
            max_document_bytes: default_document_limit(),
            max_recent_jobs: default_jobs(),
            catalogue_path: None,
            connections_path: None,
            jobs_path: None,
            cloud: None,
            printer_defaults: PrinterDefaults::default(),
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
    #[test]
    fn brother_wifi_configuration_requires_explicit_opt_in() {
        assert!(!Config::default().enable_brother_wifi_configuration);
        assert!(!Config::default().enable_brother_wifi_configuration_pairing);
        assert!(
            serde_json::from_str::<Config>(
                r#"{"enable_brother_wifi_configuration":true,"enable_brother_wifi_configuration_pairing":true}"#,
            )
                .unwrap()
                .enable_brother_wifi_configuration
        );
        assert!(serde_json::from_str::<Config>(
            r#"{"enable_brother_wifi_configuration_pairing":true}"#,
        )
        .unwrap()
        .enable_brother_wifi_configuration_pairing);
    }
    #[test]
    fn printer_defaults_are_typed() {
        assert!(
            serde_json::from_str::<Config>(r#"{"printer_defaults":{"density":"hot"}}"#).is_err()
        );
        let config: Config = serde_json::from_str(
            r#"{"printer_defaults":{"density":4,"continuous":true,"data":{"name":"Ada"}}}"#,
        )
        .unwrap();
        assert_eq!(config.printer_defaults.density, Some(4));
        assert_eq!(config.printer_defaults.data["name"], "Ada");
    }
}
