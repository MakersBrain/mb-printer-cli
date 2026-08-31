// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared bounded, partial-failure printer discovery for the CLI and API.

use crate::{
    output::Warning,
    transport::{self, NativeDevice},
};
use mb_printer_core::capabilities;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiscoveryTransport {
    Usb,
    Serial,
    Network,
    Ble,
    Rfcomm,
}

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub via: Vec<DiscoveryTransport>,
    pub timeout: Duration,
    pub probe: bool,
    pub include_unknown: bool,
    pub max_services: u16,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            via: Vec::new(),
            timeout: Duration::from_secs(3),
            probe: false,
            include_unknown: false,
            max_services: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryCandidate {
    #[serde(skip)]
    pub device: NativeDevice,
    pub transport: String,
    pub endpoint: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub confidence: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<serde_json::Value>,
}

#[derive(Debug, Default)]
pub struct DiscoveryReport {
    pub candidates: Vec<DiscoveryCandidate>,
    pub warnings: Vec<Warning>,
}

fn requested(requested: &[DiscoveryTransport], transport: DiscoveryTransport) -> bool {
    requested.is_empty() || requested.contains(&transport)
}

pub fn model_for_device(device: &NativeDevice) -> Option<String> {
    let haystack = format!(
        "{} {} {}",
        device.name.as_deref().unwrap_or_default(),
        device.address,
        device.ieee1284_device_id.as_deref().unwrap_or_default()
    );
    capabilities::detect(&haystack).map(|definition| definition.id)
}

fn candidate(device: NativeDevice, status: Option<serde_json::Value>) -> DiscoveryCandidate {
    let model = model_for_device(&device);
    let endpoint = match device.transport.as_str() {
        "serial" => format!("serial:{}", device.address),
        "ble" => format!("ble:{}", device.address),
        "rfcomm" => format!("rfcomm:{}", device.address),
        _ => device.address.clone(),
    };
    DiscoveryCandidate {
        transport: device.transport.clone(),
        endpoint,
        name: device.name.clone(),
        confidence: if model.is_some() {
            "model-match"
        } else {
            "printer-class"
        },
        model,
        device,
        status,
    }
}

pub async fn discover(options: DiscoveryOptions) -> DiscoveryReport {
    #[allow(unused_variables)]
    let explicit = !options.via.is_empty();
    let mut native_tasks: Vec<(
        &'static str,
        tokio::task::JoinHandle<std::io::Result<Vec<NativeDevice>>>,
    )> = Vec::new();
    if requested(&options.via, DiscoveryTransport::Serial) {
        let include_unknown = options.include_unknown;
        native_tasks.push((
            "serial",
            tokio::task::spawn_blocking(move || transport::discover_serial(include_unknown)),
        ));
    }
    #[cfg(feature = "usb")]
    if requested(&options.via, DiscoveryTransport::Usb) {
        let include_unknown = options.include_unknown;
        native_tasks.push((
            "usb",
            tokio::task::spawn_blocking(move || transport::usb::discover(include_unknown)),
        ));
    }
    #[cfg(all(feature = "bluetooth-linux", target_os = "linux"))]
    if requested(&options.via, DiscoveryTransport::Rfcomm) {
        native_tasks.push((
            "rfcomm",
            tokio::task::spawn_blocking(transport::discover_rfcomm),
        ));
    }

    #[cfg(feature = "network")]
    let network = requested(&options.via, DiscoveryTransport::Network).then(|| {
        let timeout_ms = u64::try_from(options.timeout.as_millis()).unwrap_or(10_000);
        let maximum_services = usize::from(options.max_services);
        let probe = options.probe;
        tokio::task::spawn_blocking(move || {
            let options = crate::network::DiscoveryOptions {
                timeout_ms,
                maximum_services,
            };
            if probe {
                crate::network::status(options).map(|statuses| {
                    statuses
                        .into_iter()
                        .map(|status| {
                            let value = serde_json::json!({
                                "reachable": status.reachable,
                                "printerState": status.printer_state,
                                "reasons": status.reasons,
                                "mediaReady": status.media_ready,
                                "error": status.error,
                            });
                            candidate(status.device, Some(value))
                        })
                        .collect::<Vec<_>>()
                })
            } else {
                crate::network::discover(options).map(|devices| {
                    devices
                        .into_iter()
                        .map(|device| candidate(device, None))
                        .collect::<Vec<_>>()
                })
            }
        })
    });

    #[cfg(feature = "bluetooth")]
    let ble = requested(&options.via, DiscoveryTransport::Ble)
        .then(|| tokio::spawn(async { transport::bluetooth::discover().await }));

    let mut report = DiscoveryReport::default();
    for (source, task) in native_tasks {
        match tokio::time::timeout(options.timeout, task).await {
            Ok(Ok(Ok(devices))) => report
                .candidates
                .extend(devices.into_iter().map(|device| candidate(device, None))),
            Ok(Ok(Err(error))) => report.warnings.push(Warning {
                code: format!("{source}-discovery-failed"),
                message: error.to_string(),
                source: Some(source.into()),
            }),
            Ok(Err(error)) => report.warnings.push(Warning {
                code: "discovery-task-failed".into(),
                message: error.to_string(),
                source: Some(source.into()),
            }),
            Err(_) => report.warnings.push(Warning {
                code: "discovery-timeout".into(),
                message: format!("{source} discovery exceeded {:?}", options.timeout),
                source: Some(source.into()),
            }),
        }
    }

    #[cfg(feature = "network")]
    if let Some(task) = network {
        match tokio::time::timeout(options.timeout + Duration::from_millis(100), task).await {
            Ok(Ok(Ok(found))) => report.candidates.extend(found),
            Ok(Ok(Err(error))) => report.warnings.push(Warning {
                code: "network-discovery-failed".into(),
                message: error,
                source: Some("network".into()),
            }),
            Ok(Err(error)) => report.warnings.push(Warning {
                code: "discovery-task-failed".into(),
                message: error.to_string(),
                source: Some("network".into()),
            }),
            Err(_) => report.warnings.push(Warning {
                code: "discovery-timeout".into(),
                message: format!("network discovery exceeded {:?}", options.timeout),
                source: Some("network".into()),
            }),
        }
    }
    #[cfg(not(feature = "network"))]
    if explicit && requested(&options.via, DiscoveryTransport::Network) {
        report.warnings.push(Warning {
            code: "not-compiled".into(),
            message: "network discovery is not included in this build".into(),
            source: Some("network".into()),
        });
    }

    #[cfg(feature = "bluetooth")]
    if let Some(task) = ble {
        match tokio::time::timeout(options.timeout, task).await {
            Ok(Ok(Ok(devices))) => report.candidates.extend(
                devices
                    .into_iter()
                    .filter(|device| options.include_unknown || model_for_device(device).is_some())
                    .map(|device| candidate(device, None)),
            ),
            Ok(Ok(Err(error))) => report.warnings.push(Warning {
                code: "bluetooth-discovery-failed".into(),
                message: error.to_string(),
                source: Some("ble".into()),
            }),
            Ok(Err(error)) => report.warnings.push(Warning {
                code: "discovery-task-failed".into(),
                message: error.to_string(),
                source: Some("ble".into()),
            }),
            Err(_) => report.warnings.push(Warning {
                code: "discovery-timeout".into(),
                message: format!("Bluetooth discovery exceeded {:?}", options.timeout),
                source: Some("ble".into()),
            }),
        }
    }
    #[cfg(not(feature = "bluetooth"))]
    if explicit && requested(&options.via, DiscoveryTransport::Ble) {
        report.warnings.push(Warning {
            code: "not-compiled".into(),
            message: "BLE discovery is not included in this build".into(),
            source: Some("ble".into()),
        });
    }

    #[cfg(not(feature = "usb"))]
    if explicit && requested(&options.via, DiscoveryTransport::Usb) {
        report.warnings.push(Warning {
            code: "not-compiled".into(),
            message: "USB discovery is not included in this build".into(),
            source: Some("usb".into()),
        });
    }
    #[cfg(not(all(feature = "bluetooth-linux", target_os = "linux")))]
    if explicit && requested(&options.via, DiscoveryTransport::Rfcomm) {
        report.warnings.push(Warning {
            code: "not-supported".into(),
            message: "RFCOMM discovery requires a bluetooth-linux build on Linux".into(),
            source: Some("rfcomm".into()),
        });
    }

    report.candidates.sort_by(|left, right| {
        (&left.transport, &left.endpoint).cmp(&(&right.transport, &right.endpoint))
    });
    report
        .candidates
        .dedup_by(|left, right| left.endpoint == right.endpoint);
    report
}

#[cfg(all(test, not(feature = "network")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicitly_unavailable_backend_is_a_structured_warning() {
        let report = discover(DiscoveryOptions {
            via: vec![DiscoveryTransport::Network],
            timeout: Duration::from_millis(10),
            ..DiscoveryOptions::default()
        })
        .await;
        assert!(report.candidates.is_empty());
        assert_eq!(report.warnings[0].code, "not-compiled");
        assert_eq!(report.warnings[0].source.as_deref(), Some("network"));
    }
}
