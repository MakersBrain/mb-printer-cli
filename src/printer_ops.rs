// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared, typed printer operations used by the CLI and loopback API.

use mb_printer_core::protocol::brother::{report, status, wifi};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PrinterOpsError {
    #[error("{0}")]
    Device(String),
    #[error("Brother response is invalid: {0}")]
    InvalidResponse(&'static str),
    #[error(transparent)]
    SystemReport(#[from] report::SystemReportError),
    #[error(transparent)]
    Wireless(#[from] wifi::WirelessError),
    #[cfg(feature = "usb")]
    #[error(transparent)]
    Execute(#[from] mb_printer_native::ExecuteError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WirelessStatus {
    pub connected: Option<bool>,
    pub ip_address: Option<String>,
    pub ssid: Option<String>,
    pub encryption: Option<wifi::WirelessEncryption>,
    pub authentication: Option<wifi::WirelessAuthentication>,
    pub infrastructure: Option<bool>,
    pub wireless_direct: Option<bool>,
}

pub fn parse_brother_status(data: &[u8]) -> Result<status::BrotherStatus, PrinterOpsError> {
    status::parse_status(data).map_err(PrinterOpsError::InvalidResponse)
}

pub fn parse_wireless_scan(data: &[u8]) -> Vec<wifi::AccessPoint> {
    wifi::parse_access_points(data)
}

pub fn parse_wireless_status(data: &[u8]) -> WirelessStatus {
    WirelessStatus {
        connected: wifi::parse_wifi_status(data),
        ip_address: wifi::parse_ip_address(data),
        ssid: wifi::parse_oid_value(data, wifi::WirelessField::Ssid.oid()),
        encryption: wifi::parse_encryption(data),
        authentication: wifi::parse_authentication(data),
        infrastructure: wifi::parse_boolean_field(data, wifi::WirelessField::Infrastructure),
        wireless_direct: wifi::parse_boolean_field(data, wifi::WirelessField::WirelessDirect),
    }
}

pub fn parse_system_report(
    data: &[u8],
    redact: bool,
) -> Result<report::SystemReport, PrinterOpsError> {
    let parsed = report::parse_system_report(data)?;
    Ok(if redact { parsed.redacted() } else { parsed })
}

#[cfg(feature = "usb")]
mod usb {
    use super::*;
    use mb_printer_core::{
        capabilities::{self, PrinterDefinition, Protocol},
        protocol::Plan,
    };
    use mb_printer_native::transports::usb::{
        self, UsbBulkCandidate, UsbIdentity, select_bulk_candidate,
    };
    use mb_printer_native::{Progress, Transport as _};

    const BROTHER_VENDOR_ID: u16 = 0x04f9;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct BrotherUsbDevice {
        pub selector: String,
        pub vendor_id: u16,
        pub product_id: u16,
        pub bus: u8,
        pub address: u8,
        pub manufacturer: Option<String>,
        pub product: Option<String>,
        pub serial_number: Option<String>,
        pub model: Option<String>,
    }

    struct ResolvedUsb {
        candidate: UsbBulkCandidate,
        model: Option<PrinterDefinition>,
    }

    pub fn selector_for(identity: UsbIdentity) -> String {
        format!(
            "usb-device:{:04x}:{:04x}:{:03}:{:03}",
            identity.vendor_id, identity.product_id, identity.bus, identity.address
        )
    }

    fn model_for(candidate: &UsbBulkCandidate) -> Option<PrinterDefinition> {
        candidate
            .product
            .as_deref()
            .and_then(capabilities::detect)
            .filter(|model| model.protocol == Protocol::Brother)
    }

    fn unique_brother_candidates() -> Result<Vec<UsbBulkCandidate>, PrinterOpsError> {
        let discovered = usb::discover_rusb_bulk().map_err(PrinterOpsError::Device)?;
        let mut identities = discovered
            .iter()
            .filter(|candidate| candidate.identity.vendor_id == BROTHER_VENDOR_ID)
            .map(|candidate| candidate.identity)
            .collect::<Vec<_>>();
        identities.sort_by_key(|identity| (identity.bus, identity.address));
        identities.dedup();
        Ok(identities
            .into_iter()
            .filter_map(|identity| select_bulk_candidate(&discovered, identity).cloned())
            .collect())
    }

    pub fn devices() -> Result<Vec<BrotherUsbDevice>, PrinterOpsError> {
        Ok(unique_brother_candidates()?
            .into_iter()
            .map(|candidate| BrotherUsbDevice {
                selector: selector_for(candidate.identity),
                vendor_id: candidate.identity.vendor_id,
                product_id: candidate.identity.product_id,
                bus: candidate.identity.bus,
                address: candidate.identity.address,
                manufacturer: candidate.manufacturer.clone(),
                product: candidate.product.clone(),
                serial_number: candidate.serial_number.clone(),
                model: model_for(&candidate).map(|model| model.id),
            })
            .collect())
    }

    fn resolve(selector: Option<&str>, wireless: bool) -> Result<ResolvedUsb, PrinterOpsError> {
        let mut candidates = unique_brother_candidates()?;
        if let Some(selector) = selector {
            candidates.retain(|candidate| {
                selector_for(candidate.identity) == selector
                    || candidate.serial_number.as_deref() == Some(selector)
            });
        }
        if candidates.len() != 1 {
            let available = candidates
                .iter()
                .map(|candidate| selector_for(candidate.identity))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PrinterOpsError::Device(if available.is_empty() {
                "no matching Brother USB printer was found".into()
            } else if selector.is_some() {
                format!("USB selector is ambiguous: {available}")
            } else {
                format!("multiple Brother USB printers found; pass --device: {available}")
            }));
        }
        let candidate = candidates.pop().expect("one candidate");
        if candidate.in_endpoint.is_none() {
            return Err(PrinterOpsError::Device(
                "selected Brother USB interface has no bulk IN endpoint".into(),
            ));
        }
        let model = model_for(&candidate);
        if wireless
            && !model
                .as_ref()
                .is_some_and(|model| matches!(model.id.as_str(), "ql-1110nwb" | "ql-1115nwb"))
        {
            return Err(PrinterOpsError::Device(
                "wireless administration requires an identified QL-1110NWB or QL-1115NWB".into(),
            ));
        }
        Ok(ResolvedUsb { candidate, model })
    }

    fn execute(
        resolved: &ResolvedUsb,
        plan: &Plan,
        response_limit: usize,
    ) -> Result<Progress, PrinterOpsError> {
        let mut transport = usb::open_rusb_with_limits(
            &resolved.candidate,
            usize::from(resolved.candidate.max_packet_size),
            64 * 1024,
            response_limit,
            5_000,
        )
        .map_err(PrinterOpsError::Device)?;
        // A previous process may have left a complete status frame queued.
        for _ in 0..8 {
            match transport
                .wait_response(50)
                .map_err(PrinterOpsError::Device)?
            {
                mb_printer_native::WaitOutcome::Response(_) => {}
                mb_printer_native::WaitOutcome::Timeout
                | mb_printer_native::WaitOutcome::Unavailable => break,
            }
        }
        mb_printer_native::execute(plan, &mut transport).map_err(Into::into)
    }

    pub fn status(selector: Option<&str>) -> Result<status::BrotherStatus, PrinterOpsError> {
        let resolved = resolve(selector, false)?;
        let model = resolved.model.as_ref().ok_or_else(|| {
            PrinterOpsError::Device(
                "Brother model could not be identified safely from its USB product string".into(),
            )
        })?;
        let progress = execute(&resolved, &status::plan(model), 4 * 1024)?;
        parse_brother_status(
            progress
                .responses
                .last()
                .ok_or(PrinterOpsError::InvalidResponse("missing status response"))?,
        )
    }

    pub fn wireless_scan(
        selector: Option<&str>,
    ) -> Result<Vec<wifi::AccessPoint>, PrinterOpsError> {
        let resolved = resolve(selector, true)?;
        let progress = execute(&resolved, &wifi::wireless_scan_plan(), 16 * 1024)?;
        let response = progress
            .responses
            .last()
            .ok_or(PrinterOpsError::InvalidResponse(
                "missing wireless scan response",
            ))?;
        Ok(parse_wireless_scan(response))
    }

    pub fn wireless_status(selector: Option<&str>) -> Result<WirelessStatus, PrinterOpsError> {
        let resolved = resolve(selector, true)?;
        let progress = execute(&resolved, &wifi::wireless_status_plan(), 4 * 1024)?;
        let response = progress.responses.concat();
        Ok(parse_wireless_status(&response))
    }

    /// Applies typed Brother wireless settings through a directly attached USB
    /// printer. Callers are responsible for authorising this state-changing
    /// operation before invoking it.
    ///
    /// The selector is resolved immediately before opening the device, so this
    /// cannot silently target the first of several attached Brother printers.
    pub fn wireless_configure(
        selector: &str,
        settings: &wifi::WirelessSettings,
    ) -> Result<(), PrinterOpsError> {
        let resolved = resolve(Some(selector), true)?;
        let command = settings.command()?;
        let plan = Plan {
            protocol: Protocol::Brother,
            source_commit: mb_printer_core::protocol::SOURCE_COMMIT.into(),
            actions: vec![mb_printer_core::protocol::Action::CommandWrite {
                name: "Brother wireless configuration".into(),
                bytes: command,
                atomic: true,
            }],
        };
        execute(&resolved, &plan, 4 * 1024)?;
        Ok(())
    }

    pub fn system_report(
        selector: Option<&str>,
        redact: bool,
    ) -> Result<report::SystemReport, PrinterOpsError> {
        let resolved = resolve(selector, false)?;
        if !resolved.model.as_ref().is_some_and(|model| {
            matches!(model.id.as_str(), "ql-1100" | "ql-1110nwb" | "ql-1115nwb")
        }) {
            return Err(PrinterOpsError::Device(
                "system reports require an identified supported Brother QL-1100 family model"
                    .into(),
            ));
        }
        let progress = execute(
            &resolved,
            &report::system_report_plan(),
            report::MAX_SYSTEM_REPORT_BYTES,
        )?;
        parse_system_report(
            progress
                .responses
                .last()
                .ok_or(PrinterOpsError::InvalidResponse(
                    "missing system report response",
                ))?,
            redact,
        )
    }
}

#[cfg(feature = "usb")]
pub use usb::{devices as brother_usb_devices, selector_for as brother_usb_selector};
#[cfg(feature = "usb")]
pub use usb::{status as usb_brother_status, system_report as usb_system_report};
#[cfg(feature = "usb")]
pub use usb::{
    wireless_configure as usb_wireless_configure, wireless_scan as usb_wireless_scan,
    wireless_status as usb_wireless_status,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_wireless_status_fields_from_one_capture() {
        let capture = b"OBJBRNET\r\n\"458867:1\"\r\n\"458967.2:-c0-a8-01-64\"\r\n\"458877:-43-61-66-65\"\r\n\"458880:8\"\r\n\"458881:3\"\r\n\"459138.2:1\"\r\n\"459138.3:0\"\r\n";
        let status = parse_wireless_status(capture);
        assert_eq!(status.connected, Some(true));
        assert_eq!(status.ip_address.as_deref(), Some("192.168.1.100"));
        assert_eq!(status.ssid.as_deref(), Some("Cafe"));
        assert_eq!(status.encryption, Some(wifi::WirelessEncryption::TkipAes));
        assert_eq!(
            status.authentication,
            Some(wifi::WirelessAuthentication::WpaPsk)
        );
        assert_eq!(status.infrastructure, Some(true));
        assert_eq!(status.wireless_direct, Some(false));
    }

    #[test]
    fn report_is_redacted_by_default() {
        let report = parse_system_report(
            b"prefix<<PRINTER CONFIGURATION>>\n[WLAN]\nSSID=Cafe\nChannel=6\n",
            true,
        )
        .unwrap();
        assert_eq!(report.sections["WLAN"]["SSID"], report::REDACTED);
        assert_eq!(report.sections["WLAN"]["Channel"], "6");
        assert!(!report.text.contains("Cafe"));
    }

    #[cfg(feature = "usb")]
    #[test]
    fn stable_usb_selector_contains_physical_identity() {
        let selector = brother_usb_selector(mb_printer_native::transports::usb::UsbIdentity {
            vendor_id: 0x04f9,
            product_id: 0x209b,
            bus: 1,
            address: 7,
        });
        assert_eq!(selector, "usb-device:04f9:209b:001:007");
    }
}
