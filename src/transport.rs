// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native file, TCP, serial-device and capture transports.
use mb_printer_core::protocol::Plan;
use mb_printer_native::{Transport, WaitOutcome};
use serde::Serialize;
use std::{
    fs::File,
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    thread,
    time::Duration,
};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PhysicalEvent {
    Subscribe,
    Write { bytes: Vec<u8> },
    Delay { milliseconds: u64 },
    Wait { timeout_ms: u64 },
}

pub struct CaptureTransport {
    pub payload_limit: usize,
    pub events: Vec<PhysicalEvent>,
    pub response: Option<Vec<u8>>,
}
impl CaptureTransport {
    pub fn new(payload_limit: usize) -> Self {
        Self {
            payload_limit: payload_limit.max(1),
            events: vec![],
            response: None,
        }
    }
}
impl Transport for CaptureTransport {
    fn payload_limit(&self) -> usize {
        self.payload_limit
    }
    fn subscribe_notifications(&mut self) -> Result<(), String> {
        self.events.push(PhysicalEvent::Subscribe);
        Ok(())
    }
    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.events.push(PhysicalEvent::Write { bytes: b.to_vec() });
        Ok(())
    }
    fn delay_monotonic(&mut self, milliseconds: u64) {
        self.events.push(PhysicalEvent::Delay { milliseconds });
    }
    fn wait_response(&mut self, timeout_ms: u64) -> Result<WaitOutcome, String> {
        self.events.push(PhysicalEvent::Wait { timeout_ms });
        Ok(self
            .response
            .clone()
            .map_or(WaitOutcome::Timeout, WaitOutcome::Response))
    }
}

pub struct WriteTransport {
    writer: Box<dyn Write + Send>,
    payload_limit: usize,
}
impl WriteTransport {
    pub fn file(path: &Path, payload_limit: usize) -> io::Result<Self> {
        Ok(Self {
            writer: Box::new(File::create(path)?),
            payload_limit,
        })
    }
}
impl Transport for WriteTransport {
    fn payload_limit(&self) -> usize {
        self.payload_limit
    }
    fn subscribe_notifications(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.writer.write_all(b).map_err(|e| e.to_string())
    }
    fn delay_monotonic(&mut self, ms: u64) {
        thread::sleep(Duration::from_millis(ms))
    }
    fn wait_response(&mut self, _: u64) -> Result<WaitOutcome, String> {
        Ok(WaitOutcome::Unavailable)
    }
}

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    payload_limit: usize,
}
impl SerialTransport {
    pub fn open(path: &Path, baud: u32, payload_limit: usize) -> io::Result<Self> {
        let port = serialport::new(path.to_string_lossy(), baud)
            .timeout(Duration::from_millis(500))
            .open()
            .map_err(io::Error::other)?;
        Ok(Self {
            port,
            payload_limit,
        })
    }
}
impl Transport for SerialTransport {
    fn payload_limit(&self) -> usize {
        self.payload_limit
    }
    fn subscribe_notifications(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.port.write_all(b).map_err(|e| e.to_string())
    }
    fn delay_monotonic(&mut self, ms: u64) {
        thread::sleep(Duration::from_millis(ms))
    }
    fn wait_response(&mut self, timeout: u64) -> Result<WaitOutcome, String> {
        self.port
            .set_timeout(Duration::from_millis(timeout))
            .map_err(|e| e.to_string())?;
        let mut b = vec![0; 4096];
        match self.port.read(&mut b) {
            Ok(0) => Ok(WaitOutcome::Unavailable),
            Ok(n) => {
                b.truncate(n);
                Ok(WaitOutcome::Response(b))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(WaitOutcome::Timeout)
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeDevice {
    pub transport: String,
    pub address: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ieee1284_device_id: Option<String>,
}
pub fn discover_native() -> io::Result<Vec<NativeDevice>> {
    #[allow(unused_mut)]
    let mut found = serialport::available_ports()
        .map_err(io::Error::other)?
        .into_iter()
        .map(|p| NativeDevice {
            transport: "serial".into(),
            address: p.port_name,
            name: None,
            vendor_id: None,
            product_id: None,
            serial_number: None,
            ieee1284_device_id: None,
        })
        .collect::<Vec<_>>();
    #[cfg(feature = "usb")]
    found.extend(usb::discover()?);
    #[cfg(all(feature = "bluetooth", target_os = "linux"))]
    found.extend(
        mb_printer_native::transports::rfcomm::discover_paired()
            .map_err(io::Error::other)?
            .into_iter()
            .map(|device| NativeDevice {
                transport: "rfcomm".into(),
                address: device.address,
                name: Some(device.name),
                vendor_id: None,
                product_id: None,
                serial_number: None,
                ieee1284_device_id: None,
            }),
    );
    Ok(found)
}

#[cfg(feature = "bluetooth")]
pub mod bluetooth {
    use super::*;
    use btleplug::{
        api::{
            Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
            ValueNotification, WriteType,
        },
        platform::{Manager, Peripheral},
    };
    use futures_util::{Stream, StreamExt};
    use std::pin::Pin;
    use tokio::runtime::Handle;
    pub async fn discover() -> io::Result<Vec<NativeDevice>> {
        let manager = Manager::new().await.map_err(io::Error::other)?;
        let mut found = Vec::new();
        for adapter in manager.adapters().await.map_err(io::Error::other)? {
            adapter
                .start_scan(ScanFilter::default())
                .await
                .map_err(io::Error::other)?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            for peripheral in adapter.peripherals().await.map_err(io::Error::other)? {
                let properties = peripheral.properties().await.map_err(io::Error::other)?;
                found.push(NativeDevice {
                    transport: "ble".into(),
                    address: peripheral.address().to_string(),
                    name: properties.and_then(|p| p.local_name),
                    vendor_id: None,
                    product_id: None,
                    serial_number: None,
                    ieee1284_device_id: None,
                });
            }
            adapter.stop_scan().await.map_err(io::Error::other)?;
        }
        Ok(found)
    }
    pub struct BleTransport {
        handle: Handle,
        peripheral: Peripheral,
        write: Characteristic,
        notify: Option<Characteristic>,
        notifications: Option<Pin<Box<dyn Stream<Item = ValueNotification> + Send>>>,
        payload_limit: usize,
    }
    impl BleTransport {
        pub async fn connect(address: &str, user_cap: usize) -> io::Result<Self> {
            Self::connect_with_reported_limit(address, user_cap, None).await
        }
        /// `reported_write_limit` is supplied by platform adapters that expose
        /// the negotiated write size. Btleplug does not expose it uniformly.
        pub async fn connect_with_reported_limit(
            address: &str,
            user_cap: usize,
            reported_write_limit: Option<usize>,
        ) -> io::Result<Self> {
            let manager = Manager::new().await.map_err(io::Error::other)?;
            for adapter in manager.adapters().await.map_err(io::Error::other)? {
                adapter
                    .start_scan(ScanFilter::default())
                    .await
                    .map_err(io::Error::other)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                for peripheral in adapter.peripherals().await.map_err(io::Error::other)? {
                    if peripheral
                        .address()
                        .to_string()
                        .eq_ignore_ascii_case(address)
                    {
                        peripheral.connect().await.map_err(io::Error::other)?;
                        peripheral
                            .discover_services()
                            .await
                            .map_err(io::Error::other)?;
                        let chars = peripheral.characteristics();
                        let write = chars
                            .iter()
                            .find(|c| {
                                c.properties.intersects(
                                    CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE,
                                )
                            })
                            .cloned()
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::NotFound,
                                    "BLE write characteristic not found",
                                )
                            })?;
                        let notify = chars
                            .iter()
                            .find(|c| c.properties.contains(CharPropFlags::NOTIFY))
                            .cloned();
                        return Ok(Self {
                            handle: Handle::current(),
                            peripheral,
                            write,
                            notify,
                            notifications: None,
                            payload_limit: crate::device::ble_payload_limit(
                                user_cap,
                                reported_write_limit,
                                None,
                            ),
                        });
                    }
                }
            }
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "BLE peripheral not found",
            ))
        }
        fn block<F: std::future::Future>(&self, future: F) -> F::Output {
            tokio::task::block_in_place(|| self.handle.block_on(future))
        }
    }
    impl Transport for BleTransport {
        fn payload_limit(&self) -> usize {
            self.payload_limit
        }
        fn subscribe_notifications(&mut self) -> Result<(), String> {
            let Some(characteristic) = self.notify.clone() else {
                return Ok(());
            };
            self.block(self.peripheral.subscribe(&characteristic))
                .map_err(|e| e.to_string())?;
            self.notifications = Some(
                self.block(self.peripheral.notifications())
                    .map_err(|e| e.to_string())?,
            );
            Ok(())
        }
        fn write(&mut self, b: &[u8]) -> Result<(), String> {
            let kind = if self
                .write
                .properties
                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
            {
                WriteType::WithoutResponse
            } else {
                WriteType::WithResponse
            };
            self.block(self.peripheral.write(&self.write, b, kind))
                .map_err(|e| e.to_string())
        }
        fn delay_monotonic(&mut self, ms: u64) {
            thread::sleep(Duration::from_millis(ms))
        }
        fn wait_response(&mut self, timeout: u64) -> Result<WaitOutcome, String> {
            let handle = self.handle.clone();
            let Some(stream) = self.notifications.as_mut() else {
                return Ok(WaitOutcome::Unavailable);
            };
            match tokio::task::block_in_place(|| {
                handle.block_on(tokio::time::timeout(
                    Duration::from_millis(timeout),
                    stream.next(),
                ))
            }) {
                Ok(Some(value)) => Ok(WaitOutcome::Response(value.value)),
                Ok(None) => Ok(WaitOutcome::Unavailable),
                Err(_) => Ok(WaitOutcome::Timeout),
            }
        }
    }
}

#[cfg(feature = "usb")]
pub mod usb {
    use super::*;
    use rusb::{DeviceHandle, UsbContext as _};
    pub struct UsbTransport {
        handle: DeviceHandle<rusb::Context>,
        out: u8,
        input: Option<u8>,
        payload_limit: usize,
        timeout: Duration,
    }
    impl UsbTransport {
        pub fn open(
            vid: u16,
            pid: u16,
            interface: u8,
            out: u8,
            input: Option<u8>,
            payload_limit: usize,
        ) -> io::Result<Self> {
            let context = rusb::Context::new().map_err(io::Error::other)?;
            let devices = context.devices().map_err(io::Error::other)?;
            let device = devices
                .iter()
                .find(|device| {
                    device.device_descriptor().is_ok_and(|descriptor| {
                        descriptor.vendor_id() == vid && descriptor.product_id() == pid
                    })
                })
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "USB device not found"))?;
            let configuration = device
                .active_config_descriptor()
                .or_else(|_| device.config_descriptor(0))
                .map_err(io::Error::other)?;
            let packet_size = configuration
                .interfaces()
                .flat_map(|candidate| candidate.descriptors())
                .filter(|descriptor| descriptor.interface_number() == interface)
                .flat_map(|descriptor| descriptor.endpoint_descriptors())
                .find(|endpoint| endpoint.address() == out)
                .map(|endpoint| usize::from(endpoint.max_packet_size()))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "USB bulk OUT endpoint not found")
                })?;
            let handle = device.open().map_err(io::Error::other)?;
            let _ = handle.set_auto_detach_kernel_driver(true);
            handle
                .set_active_configuration(configuration.number())
                .map_err(io::Error::other)?;
            handle
                .claim_interface(interface)
                .map_err(io::Error::other)?;
            handle
                .set_alternate_setting(interface, 0)
                .map_err(io::Error::other)?;
            // Match the Python transport: a status block can outlive the
            // process which requested it, so discard stale replies before a
            // new job. A timeout means the endpoint is clean.
            if let Some(endpoint) = input {
                let mut stale = [0_u8; 64];
                for _ in 0..8 {
                    match handle.read_bulk(endpoint, &mut stale, Duration::from_millis(50)) {
                        Ok(0) | Err(rusb::Error::Timeout) => break,
                        Ok(_) => {}
                        Err(error) => return Err(io::Error::other(error)),
                    }
                }
            }
            Ok(Self {
                handle,
                out,
                input,
                payload_limit: payload_limit.max(1).min(packet_size.max(1)),
                timeout: Duration::from_secs(3),
            })
        }
    }
    impl Transport for UsbTransport {
        fn payload_limit(&self) -> usize {
            self.payload_limit
        }
        fn command_limit(&self) -> usize {
            // libusb accepts one logical bulk transfer and packetizes it for
            // the endpoint. The working Python driver relies on this for the
            // 200-byte Brother invalidate command.
            usize::MAX
        }
        fn subscribe_notifications(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn write(&mut self, b: &[u8]) -> Result<(), String> {
            let written = self
                .handle
                .write_bulk(self.out, b, self.timeout)
                .map_err(|e| e.to_string())?;
            if written != b.len() {
                return Err(format!(
                    "short USB bulk write: accepted {written} of {} bytes",
                    b.len()
                ));
            }
            Ok(())
        }
        fn delay_monotonic(&mut self, ms: u64) {
            thread::sleep(Duration::from_millis(ms))
        }
        fn wait_response(&mut self, timeout: u64) -> Result<WaitOutcome, String> {
            let Some(endpoint) = self.input else {
                return Ok(WaitOutcome::Unavailable);
            };
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
            let mut b = vec![0; 64];
            loop {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    return Ok(WaitOutcome::Timeout);
                };
                match self.handle.read_bulk(endpoint, &mut b, remaining) {
                    // QL printers can return zero-length USB packets while a
                    // status reply is being prepared; keep polling.
                    Ok(0) => thread::sleep(Duration::from_millis(50)),
                    Ok(n) => {
                        b.truncate(n);
                        return Ok(WaitOutcome::Response(b));
                    }
                    Err(rusb::Error::Timeout) => return Ok(WaitOutcome::Timeout),
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
    }
    pub fn discover() -> io::Result<Vec<NativeDevice>> {
        let context = rusb::Context::new().map_err(io::Error::other)?;
        let devices = context.devices().map_err(io::Error::other)?;
        let mut out = Vec::new();
        for device in devices.iter() {
            let descriptor = device.device_descriptor().map_err(io::Error::other)?;
            let handle = device.open().ok();
            let name = handle.as_ref().and_then(|handle| {
                handle
                    .read_product_string_ascii(&descriptor)
                    .ok()
                    .or_else(|| handle.read_manufacturer_string_ascii(&descriptor).ok())
            });
            let serial_number = handle
                .as_ref()
                .and_then(|handle| handle.read_serial_number_string_ascii(&descriptor).ok());
            let ieee1284_device_id = handle.as_ref().and_then(|handle| {
                read_ieee1284_device_id(&device, handle, Duration::from_millis(250))
                    .ok()
                    .flatten()
            });
            out.push(NativeDevice {
                transport: "usb".into(),
                address: format!(
                    "usb:{:04x}:{:04x}",
                    descriptor.vendor_id(),
                    descriptor.product_id()
                ),
                name,
                vendor_id: Some(descriptor.vendor_id()),
                product_id: Some(descriptor.product_id()),
                serial_number,
                ieee1284_device_id,
            });
        }
        Ok(out)
    }

    fn read_ieee1284_device_id(
        device: &rusb::Device<rusb::Context>,
        handle: &DeviceHandle<rusb::Context>,
        timeout: Duration,
    ) -> io::Result<Option<String>> {
        let config = device
            .active_config_descriptor()
            .map_err(io::Error::other)?;
        let Some(interface) = config
            .interfaces()
            .flat_map(|interface| interface.descriptors())
            .find(|descriptor| descriptor.class_code() == 7)
            .map(|descriptor| descriptor.interface_number())
        else {
            return Ok(None);
        };
        let mut bytes = vec![0_u8; 2048];
        let length = handle
            .read_control(0xa1, 0, 0, u16::from(interface), &mut bytes, timeout)
            .map_err(io::Error::other)?;
        parse_ieee1284_device_id(&bytes[..length]).map(Some)
    }

    pub fn parse_ieee1284_device_id(bytes: &[u8]) -> io::Result<String> {
        if bytes.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short IEEE-1284 device ID",
            ));
        }
        let declared = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        if declared < 2 || declared > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IEEE-1284 device ID length",
            ));
        }
        let value = std::str::from_utf8(&bytes[2..declared])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .trim_matches(char::from(0))
            .trim()
            .to_owned();
        if value.is_empty() || !value.contains(';') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed IEEE-1284 device ID",
            ));
        }
        Ok(value)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn parses_bounded_ieee1284_identity() {
            let payload = b"MFG:Brother;MDL:QL-1100;CMD:RASTER;";
            let mut bytes = ((payload.len() + 2) as u16).to_be_bytes().to_vec();
            bytes.extend_from_slice(payload);
            assert_eq!(
                super::parse_ieee1284_device_id(&bytes).unwrap(),
                String::from_utf8(payload.to_vec()).unwrap()
            );
            assert!(super::parse_ieee1284_device_id(&[0, 40, b'M']).is_err());
        }
    }
}

pub struct TcpTransport {
    stream: TcpStream,
    payload_limit: usize,
}
impl TcpTransport {
    pub fn connect(address: &str, payload_limit: usize, timeout: Duration) -> io::Result<Self> {
        let addr = address.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "TCP address did not resolve")
        })?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            payload_limit,
        })
    }
}
impl Transport for TcpTransport {
    fn payload_limit(&self) -> usize {
        self.payload_limit
    }
    fn subscribe_notifications(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.stream.write_all(b).map_err(|e| e.to_string())
    }
    fn delay_monotonic(&mut self, ms: u64) {
        thread::sleep(Duration::from_millis(ms))
    }
    fn wait_response(&mut self, timeout: u64) -> Result<WaitOutcome, String> {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(timeout)))
            .map_err(|e| e.to_string())?;
        let mut b = vec![0; 4096];
        match self.stream.read(&mut b) {
            Ok(0) => Ok(WaitOutcome::Unavailable),
            Ok(n) => {
                b.truncate(n);
                Ok(WaitOutcome::Response(b))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(WaitOutcome::Timeout)
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

#[derive(Serialize)]
struct Capture<'a> {
    schema: u8,
    plan: &'a Plan,
    physical_events: &'a [PhysicalEvent],
    concatenated_bytes: Vec<u8>,
}
pub fn save_capture(path: &Path, plan: &Plan, transport: &CaptureTransport) -> io::Result<()> {
    let bytes = transport
        .events
        .iter()
        .filter_map(|e| {
            if let PhysicalEvent::Write { bytes } = e {
                Some(bytes.as_slice())
            } else {
                None
            }
        })
        .flatten()
        .copied()
        .collect();
    let c = Capture {
        schema: 1,
        plan,
        physical_events: &transport.events,
        concatenated_bytes: bytes,
    };
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&c).map_err(io::Error::other)?,
    )
}
pub fn capture_json(plan: &Plan, transport: &CaptureTransport) -> io::Result<Vec<u8>> {
    let bytes = transport
        .events
        .iter()
        .filter_map(|e| {
            if let PhysicalEvent::Write { bytes } = e {
                Some(bytes.as_slice())
            } else {
                None
            }
        })
        .flatten()
        .copied()
        .collect();
    serde_json::to_vec_pretty(&Capture {
        schema: 1,
        plan,
        physical_events: &transport.events,
        concatenated_bytes: bytes,
    })
    .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mb_printer_core::{
        capabilities,
        protocol::{self, Options, Raster},
    };
    #[test]
    fn capture_preserves_physical_chunking_and_bytes() {
        let p = capabilities::by_id("m110").unwrap();
        let plan = protocol::plan(
            &p,
            &Raster {
                width_bytes: 1,
                height: 16,
                data: vec![1; 16],
            },
            &Options::default(),
        )
        .unwrap();
        let mut t = CaptureTransport::new(8);
        let progress = mb_printer_native::execute(&plan, &mut t).unwrap();
        assert_eq!(
            progress.bytes_written,
            t.events
                .iter()
                .filter_map(|e| if let PhysicalEvent::Write { bytes } = e {
                    Some(bytes.len() as u64)
                } else {
                    None
                })
                .sum::<u64>()
        );
    }
}
