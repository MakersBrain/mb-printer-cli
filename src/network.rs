// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bounded DNS-SD discovery and verified IPP/IPPS status aggregation.

use crate::{device, transport::NativeDevice};
use mb_printer_native::transports::dns_sd::{self, DiscoveredIppPrinter, DiscoveryLimits};
use serde::Serialize;
use std::time::Duration;

pub const MAX_DISCOVERY_TIMEOUT_MS: u64 = 10_000;
pub const MAX_DISCOVERY_SERVICES: usize = 256;
const MAX_TXT_BYTES: usize = 4_096;
const MAX_ADDRESSES: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct DiscoveryOptions {
    pub timeout_ms: u64,
    pub maximum_services: usize,
}

impl DiscoveryOptions {
    pub fn limits(self) -> Result<DiscoveryLimits, String> {
        if self.timeout_ms == 0
            || self.timeout_ms > MAX_DISCOVERY_TIMEOUT_MS
            || self.maximum_services == 0
            || self.maximum_services > MAX_DISCOVERY_SERVICES
        {
            return Err(
                "DNS-SD bounds must use timeout-ms 1..=10000 and max-services 1..=256".into(),
            );
        }
        Ok(DiscoveryLimits {
            timeout_ms: self.timeout_ms,
            maximum_services: self.maximum_services,
            maximum_txt_bytes: MAX_TXT_BYTES,
            maximum_addresses: MAX_ADDRESSES,
        })
    }
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            timeout_ms: 3_000,
            maximum_services: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkDiscoveryDetails {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub resource: String,
    pub addresses: Vec<String>,
    pub service_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make_and_model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    pub device: NativeDevice,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer_state: Option<i32>,
    pub reasons: Vec<String>,
    pub media_ready: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn endpoint_uri(printer: &DiscoveredIppPrinter) -> String {
    format!(
        "{}://{}:{}{}",
        printer.endpoint.scheme.as_str(),
        printer.endpoint.host,
        printer.endpoint.port,
        printer.endpoint.resource
    )
}

pub fn candidate(printer: DiscoveredIppPrinter) -> NativeDevice {
    let make_and_model = printer
        .txt_utf8("ty")
        .or_else(|| printer.txt_utf8("product"))
        .map(str::to_owned);
    let name = make_and_model
        .clone()
        .unwrap_or_else(|| printer.fullname.clone());
    let details = NetworkDiscoveryDetails {
        scheme: printer.endpoint.scheme.as_str().into(),
        host: printer.endpoint.host.clone(),
        port: printer.endpoint.port,
        resource: printer.endpoint.resource.clone(),
        addresses: printer.addresses.iter().map(ToString::to_string).collect(),
        service_name: printer.fullname.clone(),
        make_and_model,
    };
    NativeDevice {
        transport: "ipp".into(),
        address: endpoint_uri(&printer),
        name: Some(name),
        vendor_id: None,
        product_id: None,
        serial_number: None,
        ieee1284_device_id: None,
        network: Some(details),
    }
}

pub fn discover(options: DiscoveryOptions) -> Result<Vec<NativeDevice>, String> {
    let limits = options.limits()?;
    dns_sd::discover(limits)
        .map(|printers| printers.into_iter().map(candidate).collect())
        .map_err(|error| error.to_string())
}

pub fn status(options: DiscoveryOptions) -> Result<Vec<NetworkStatus>, String> {
    let timeout = Duration::from_millis(options.timeout_ms.min(MAX_DISCOVERY_TIMEOUT_MS));
    discover(options).map(|devices| {
        devices
            .into_iter()
            .map(|device| {
                let result = device::IppEndpoint::new(device.address.clone(), None)
                    .and_then(|endpoint| device::ipp_query_endpoint(&endpoint, timeout));
                match result {
                    Ok(attributes) => NetworkStatus {
                        printer_state: attributes.get("printer-state").and_then(|values| {
                            values.iter().find_map(|value| match value {
                                device::IppValue::Integer(value) => Some(*value),
                                device::IppValue::Text(_) => None,
                            })
                        }),
                        reasons: text_values(&attributes, "printer-state-reasons"),
                        media_ready: text_values(&attributes, "media-ready"),
                        device,
                        reachable: true,
                        error: None,
                    },
                    Err(error) => NetworkStatus {
                        device,
                        reachable: false,
                        printer_state: None,
                        reasons: Vec::new(),
                        media_ready: Vec::new(),
                        error: Some(error.to_string()),
                    },
                }
            })
            .collect()
    })
}

fn text_values(
    attributes: &std::collections::BTreeMap<String, Vec<device::IppValue>>,
    key: &str,
) -> Vec<String> {
    attributes
        .get(key)
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            device::IppValue::Text(value) => Some(value.clone()),
            device::IppValue::Integer(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_printer_native::transports::dns_sd::DiscoveredIppPrinter;
    use mb_printer_native::transports::wifi::IppScheme;
    use std::{collections::BTreeMap, net::Ipv4Addr};

    #[test]
    fn candidate_preserves_ipps_and_safe_typed_metadata() {
        let printer = DiscoveredIppPrinter {
            fullname: "Labels._ipps._tcp.local.".into(),
            endpoint: mb_printer_native::transports::wifi::IppEndpoint::ipps(
                "labels.local.",
                443,
                "/ipp/print",
            ),
            addresses: [Ipv4Addr::new(192, 0, 2, 10).into()].into_iter().collect(),
            txt: BTreeMap::from([
                ("ty".into(), b"Brother QL-1110NWB".to_vec()),
                ("password".into(), b"must-not-be-returned".to_vec()),
            ]),
        };
        let candidate = candidate(printer);
        assert_eq!(candidate.transport, "ipp");
        assert_eq!(candidate.address, "ipps://labels.local.:443/ipp/print");
        let network = candidate.network.unwrap();
        assert_eq!(network.scheme, IppScheme::Ipps.as_str());
        assert_eq!(
            network.make_and_model.as_deref(),
            Some("Brother QL-1110NWB")
        );
        assert!(
            !serde_json::to_string(&network)
                .unwrap()
                .contains("must-not")
        );
    }

    #[test]
    fn public_bounds_cannot_exceed_hard_limits() {
        assert!(
            DiscoveryOptions {
                timeout_ms: 0,
                maximum_services: 1
            }
            .limits()
            .is_err()
        );
        assert!(
            DiscoveryOptions {
                timeout_ms: 1,
                maximum_services: 257
            }
            .limits()
            .is_err()
        );
        let limits = DiscoveryOptions::default().limits().unwrap();
        assert_eq!(limits.maximum_txt_bytes, 4_096);
        assert_eq!(limits.maximum_addresses, 16);
    }
}
