// SPDX-License-Identifier: AGPL-3.0-or-later
//! Versioned, typed persistence for user-managed printers.

use crate::config::PrinterDefaults;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const PRINTER_STORE_SCHEMA: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrinterStore {
    pub schema_version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_printer: Option<String>,
    #[serde(default)]
    pub printers: Vec<Printer>,
}

impl Default for PrinterStore {
    fn default() -> Self {
        Self {
            schema_version: PRINTER_STORE_SCHEMA,
            default_printer: None,
            printers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Printer {
    pub id: String,
    pub name: String,
    pub model: String,
    pub endpoints: Vec<PrinterEndpoint>,
    #[serde(default)]
    pub settings: PrinterDefaults,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<serde_json::Value>,
}

impl Printer {
    pub fn new(name: String, model: String, transports: Vec<PrinterTransport>) -> Self {
        let multiple = transports.len() > 1;
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            model,
            endpoints: transports
                .into_iter()
                .enumerate()
                .map(|(index, transport)| PrinterEndpoint {
                    id: Uuid::new_v4().to_string(),
                    preferred: !multiple || index == 0,
                    transport,
                })
                .collect(),
            settings: PrinterDefaults::default(),
            description: None,
            status: None,
            media: None,
        }
    }

    pub fn preferred_endpoint(&self) -> Result<&PrinterEndpoint, StoreError> {
        let mut preferred = self.endpoints.iter().filter(|endpoint| endpoint.preferred);
        match (preferred.next(), preferred.next()) {
            (Some(endpoint), None) => Ok(endpoint),
            (None, _) if self.endpoints.len() == 1 => Ok(&self.endpoints[0]),
            (None, _) => Err(StoreError::NoPreferredEndpoint(self.name.clone())),
            _ => Err(StoreError::MultiplePreferredEndpoints(self.name.clone())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrinterEndpoint {
    pub id: String,
    #[serde(default)]
    pub preferred: bool,
    #[serde(flatten)]
    pub transport: PrinterTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PrinterTransport {
    File {
        path: PathBuf,
    },
    Tcp {
        address: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_address: Option<String>,
    },
    Serial {
        path: PathBuf,
        #[serde(default = "default_baud")]
        baud: u32,
    },
    Usb {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vid: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<u8>,
    },
    Ble {
        address: String,
    },
    Rfcomm {
        address: String,
        #[serde(default = "default_rfcomm_channel")]
        channel: u8,
    },
    Ipp {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        certificate_pem: Option<String>,
    },
}

const fn default_baud() -> u32 {
    115_200
}

const fn default_rfcomm_channel() -> u8 {
    1
}

impl PrinterTransport {
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        if let Some(path) = value.strip_prefix("file:") {
            return nonempty(path, "file path").map(|path| Self::File { path: path.into() });
        }
        if let Some(address) = value.strip_prefix("tcp://") {
            return nonempty(address, "TCP address").map(|address| Self::Tcp {
                address: address.into(),
                status_mode: None,
                status_address: None,
            });
        }
        if let Some(path) = value.strip_prefix("serial:") {
            return nonempty(path, "serial path").map(|path| Self::Serial {
                path: path.into(),
                baud: default_baud(),
            });
        }
        if value.starts_with("ipp://") || value.starts_with("ipps://") {
            return Ok(Self::Ipp {
                uri: value.into(),
                certificate_pem: None,
            });
        }
        if let Some(address) = value.strip_prefix("ble:") {
            return nonempty(address, "BLE address").map(|address| Self::Ble {
                address: address.into(),
            });
        }
        if let Some(spec) = value.strip_prefix("rfcomm:") {
            let (address, channel) = spec
                .rsplit_once(':')
                .and_then(|(address, channel)| channel.parse::<u8>().ok().map(|c| (address, c)))
                .unwrap_or((spec, default_rfcomm_channel()));
            nonempty(address, "RFCOMM address")?;
            return Ok(Self::Rfcomm {
                address: address.into(),
                channel,
            });
        }
        if let Some(device) = value.strip_prefix("usb-device:") {
            nonempty(device, "USB device selector")?;
            return Ok(Self::Usb {
                device: Some(value.into()),
                vid: None,
                pid: None,
                interface: None,
                out: None,
                input: None,
            });
        }
        if let Some(spec) = value.strip_prefix("usb:") {
            let parts = spec.split(':').collect::<Vec<_>>();
            if !(4..=5).contains(&parts.len()) {
                return Err(StoreError::InvalidEndpoint(
                    "USB endpoint must be usb:VID:PID:INTERFACE:OUT[:IN]".into(),
                ));
            }
            let hex = |raw: &str| {
                u16::from_str_radix(raw.trim_start_matches("0x"), 16).map_err(|_| {
                    StoreError::InvalidEndpoint(format!("invalid USB hexadecimal value {raw}"))
                })
            };
            return Ok(Self::Usb {
                device: None,
                vid: Some(hex(parts[0])?),
                pid: Some(hex(parts[1])?),
                interface: Some(u8::try_from(hex(parts[2])?).map_err(|_| {
                    StoreError::InvalidEndpoint("USB interface must fit in one byte".into())
                })?),
                out: Some(u8::try_from(hex(parts[3])?).map_err(|_| {
                    StoreError::InvalidEndpoint("USB OUT endpoint must fit in one byte".into())
                })?),
                input: parts
                    .get(4)
                    .map(|raw| {
                        u8::try_from(hex(raw)?).map_err(|_| {
                            StoreError::InvalidEndpoint(
                                "USB IN endpoint must fit in one byte".into(),
                            )
                        })
                    })
                    .transpose()?,
            });
        }
        Err(StoreError::InvalidEndpoint(format!(
            "unsupported endpoint {value}; expected file:, tcp://, serial:, ipp://, ipps://, ble:, rfcomm:, usb-device:, or usb:"
        )))
    }

    pub fn uri(&self) -> String {
        match self {
            Self::File { path } => format!("file:{}", path.display()),
            Self::Tcp { address, .. } => format!("tcp://{address}"),
            Self::Serial { path, .. } => format!("serial:{}", path.display()),
            Self::Usb {
                device: Some(device),
                ..
            } => device.clone(),
            Self::Usb {
                vid: Some(vid),
                pid: Some(pid),
                interface: Some(interface),
                out: Some(out),
                input,
                ..
            } => {
                let suffix = input.map_or_else(String::new, |value| format!(":{value:02x}"));
                format!("usb:{vid:04x}:{pid:04x}:{interface:02x}:{out:02x}{suffix}")
            }
            Self::Usb { .. } => "usb:incomplete".into(),
            Self::Ble { address } => format!("ble:{address}"),
            Self::Rfcomm { address, channel } => format!("rfcomm:{address}:{channel}"),
            Self::Ipp { uri, .. } => uri.clone(),
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Tcp { .. } => "tcp",
            Self::Serial { .. } => "serial",
            Self::Usb { .. } => "usb",
            Self::Ble { .. } => "ble",
            Self::Rfcomm { .. } => "rfcomm",
            Self::Ipp { .. } => "ipp",
        }
    }
}

fn nonempty<'a>(value: &'a str, what: &str) -> Result<&'a str, StoreError> {
    if value.trim().is_empty() {
        Err(StoreError::InvalidEndpoint(format!("{what} is required")))
    } else {
        Ok(value)
    }
}

impl PrinterStore {
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path)?;
        let store: Self = if bytes
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(b'[')
        {
            let legacy: Vec<LegacyConnection> =
                serde_json::from_slice(&bytes).map_err(StoreError::InvalidStore)?;
            Self {
                schema_version: PRINTER_STORE_SCHEMA,
                default_printer: None,
                printers: legacy
                    .into_iter()
                    .map(|connection| Printer {
                        id: connection.id.clone(),
                        name: connection.id,
                        model: connection.model,
                        endpoints: vec![PrinterEndpoint {
                            id: Uuid::new_v4().to_string(),
                            preferred: true,
                            transport: connection.transport,
                        }],
                        settings: PrinterDefaults::default(),
                        description: None,
                        status: connection.status,
                        media: connection.media,
                    })
                    .collect(),
            }
        } else {
            serde_json::from_slice(&bytes).map_err(StoreError::InvalidStore)?
        };
        store.validate()?;
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_path = path.with_extension("json.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock_exclusive()?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(temporary, path)?;
        lock.unlock()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), StoreError> {
        if self.schema_version != PRINTER_STORE_SCHEMA {
            return Err(StoreError::UnsupportedSchema(self.schema_version));
        }
        for (index, printer) in self.printers.iter().enumerate() {
            validate_name(&printer.name)?;
            if printer.id.trim().is_empty() {
                return Err(StoreError::InvalidPrinter("printer ID is required".into()));
            }
            if printer.model.trim().is_empty() {
                return Err(StoreError::InvalidPrinter(format!(
                    "printer {} has no model",
                    printer.name
                )));
            }
            if printer.endpoints.is_empty() {
                return Err(StoreError::InvalidPrinter(format!(
                    "printer {} has no endpoints",
                    printer.name
                )));
            }
            if self.printers[..index].iter().any(|other| {
                other.id == printer.id || other.name.eq_ignore_ascii_case(&printer.name)
            }) {
                return Err(StoreError::DuplicatePrinter(printer.name.clone()));
            }
            printer.preferred_endpoint()?;
        }
        if let Some(selector) = &self.default_printer {
            self.find(selector)
                .ok_or_else(|| StoreError::UnknownPrinter(selector.clone()))?;
        }
        Ok(())
    }

    pub fn find(&self, selector: &str) -> Option<&Printer> {
        self.printers
            .iter()
            .find(|printer| printer.id == selector || printer.name.eq_ignore_ascii_case(selector))
    }

    pub fn find_mut(&mut self, selector: &str) -> Option<&mut Printer> {
        self.printers
            .iter_mut()
            .find(|printer| printer.id == selector || printer.name.eq_ignore_ascii_case(selector))
    }

    pub fn resolve(&self, selector: Option<&str>) -> Result<&Printer, StoreError> {
        if let Some(selector) = selector {
            return self
                .find(selector)
                .ok_or_else(|| StoreError::UnknownPrinter(selector.into()));
        }
        if let Some(default) = &self.default_printer {
            return self
                .find(default)
                .ok_or_else(|| StoreError::UnknownPrinter(default.clone()));
        }
        if self.printers.len() == 1 {
            return Ok(&self.printers[0]);
        }
        if self.printers.is_empty() {
            Err(StoreError::NoPrinters)
        } else {
            Err(StoreError::PrinterRequired)
        }
    }

    pub fn add(&mut self, printer: Printer) -> Result<(), StoreError> {
        if self.find(&printer.name).is_some()
            || self
                .printers
                .iter()
                .any(|existing| existing.id == printer.id)
        {
            return Err(StoreError::DuplicatePrinter(printer.name));
        }
        self.printers.push(printer);
        self.printers
            .sort_by_key(|printer| printer.name.to_lowercase());
        self.validate()
    }

    pub fn remove(&mut self, selector: &str) -> Result<Printer, StoreError> {
        let index = self
            .printers
            .iter()
            .position(|printer| {
                printer.id == selector || printer.name.eq_ignore_ascii_case(selector)
            })
            .ok_or_else(|| StoreError::UnknownPrinter(selector.into()))?;
        let printer = self.printers.remove(index);
        if self.default_printer.as_deref().is_some_and(|default| {
            default == printer.id || default.eq_ignore_ascii_case(&printer.name)
        }) {
            self.default_printer = None;
        }
        Ok(printer)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyConnection {
    id: String,
    model: String,
    transport: PrinterTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media: Option<serde_json::Value>,
}

pub fn validate_name(name: &str) -> Result<(), StoreError> {
    if name.trim() != name || name.is_empty() || name.len() > 120 {
        return Err(StoreError::InvalidPrinter(
            "printer name must contain 1 to 120 characters without outer whitespace".into(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(StoreError::InvalidPrinter(
            "printer name cannot contain control characters".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("printer store schema {0} is unsupported")]
    UnsupportedSchema(u16),
    #[error("invalid printer store: {0}")]
    InvalidStore(serde_json::Error),
    #[error("invalid printer: {0}")]
    InvalidPrinter(String),
    #[error("invalid printer endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("printer {0} already exists")]
    DuplicatePrinter(String),
    #[error("printer {0} was not found")]
    UnknownPrinter(String),
    #[error("no printers are configured; run `mb-printer printer add NAME`")]
    NoPrinters,
    #[error(
        "select a printer with `--printer NAME` or set one with `mb-printer printer default NAME`"
    )]
    PrinterRequired,
    #[error("printer {0} has no preferred endpoint")]
    NoPreferredEndpoint(String),
    #[error("printer {0} has more than one preferred endpoint")]
    MultiplePreferredEndpoints(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn printer(name: &str) -> Printer {
        Printer::new(
            name.into(),
            "m110".into(),
            vec![PrinterTransport::Tcp {
                address: "printer.local:9100".into(),
                status_mode: None,
                status_address: None,
            }],
        )
    }

    #[test]
    fn resolves_explicit_default_and_only_printer() {
        let mut store = PrinterStore::default();
        store.add(printer("desk")).unwrap();
        assert_eq!(store.resolve(None).unwrap().name, "desk");
        store.add(printer("warehouse")).unwrap();
        assert!(matches!(
            store.resolve(None),
            Err(StoreError::PrinterRequired)
        ));
        store.default_printer = Some("warehouse".into());
        assert_eq!(store.resolve(None).unwrap().name, "warehouse");
        assert_eq!(store.resolve(Some("DESK")).unwrap().name, "desk");
    }

    #[test]
    fn typed_store_round_trips_owner_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("printers.json");
        let mut store = PrinterStore::default();
        store.add(printer("desk")).unwrap();
        store.save(&path).unwrap();
        let loaded = PrinterStore::load(&path).unwrap();
        assert_eq!(loaded.printers[0].name, "desk");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unprintable_and_ambiguous_printers() {
        let mut empty = printer("empty");
        empty.endpoints.clear();
        let mut store = PrinterStore::default();
        assert!(store.add(empty).is_err());

        let mut ambiguous = printer("ambiguous");
        ambiguous.endpoints.push(PrinterEndpoint {
            id: Uuid::new_v4().to_string(),
            preferred: true,
            transport: PrinterTransport::Ble {
                address: "00:11:22:33:44:55".into(),
            },
        });
        assert!(store.add(ambiguous).is_err());
    }

    #[test]
    fn parses_supported_endpoint_uris() {
        for uri in [
            "file:/tmp/output.bin",
            "tcp://printer.local:9100",
            "serial:/dev/ttyUSB0",
            "ipp://printer.local/ipp/print",
            "ipps://printer.local/ipp/print",
            "ble:00:11:22:33:44:55",
            "rfcomm:00:11:22:33:44:55:1",
            "usb-device:04f9:209b:001:007",
            "usb:04f9:209b:00:02:81",
        ] {
            PrinterTransport::parse(uri).unwrap_or_else(|error| panic!("{uri}: {error}"));
        }
        assert!(PrinterTransport::parse("http://printer.local").is_err());
    }

    #[test]
    fn migrates_the_previous_connection_array() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connections.json");
        fs::write(
            &path,
            r#"[{"id":"desk","model":"m110","transport":{"kind":"tcp","address":"printer.local:9100"},"status":"ready","media":null}]"#,
        )
        .unwrap();
        let store = PrinterStore::load(&path).unwrap();
        assert_eq!(store.schema_version, PRINTER_STORE_SCHEMA);
        assert_eq!(store.printers[0].name, "desk");
        assert_eq!(store.printers[0].status.as_deref(), Some("ready"));
    }
}
