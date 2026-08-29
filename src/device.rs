// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pure protocol codecs used by hardware backends and fixture tests.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

const PJL_HEADER: &[u8] = b"\x1b%-12345X@PJL\r\n";
const PJL_FOOTER: &[u8] = b"\x1b%-12345X";
const PASSWORD_KEY: [u8; 16] = [
    0x0d, 0xae, 0xe4, 0xa1, 0x8b, 0x7f, 0x26, 0x5e, 0x72, 0x5b, 0x17, 0x7a, 0x71, 0xcd, 0xec, 0x4d,
];
const REBOOT_COMMAND: [u8; 14] = [
    0x1b, 0x69, 0x58, 0x2a, 0x31, 0x03, 0, 0x01, 0x2e, 0, 0, 0, 0x2c, 0,
];

fn pjl(command: &[u8]) -> Vec<u8> {
    [PJL_HEADER, b"@PJL ", command, b"\r\n", PJL_FOOTER].concat()
}
pub fn wifi_scan_start() -> Vec<u8> {
    pjl(b"DEFAULT OBJBRNET=\"458845:31-3a\"")
}
pub fn wifi_scan_results() -> Vec<u8> {
    pjl(b"INFO AVAILABLEWLAN")
}
pub fn wifi_inquire(oid: &str) -> Result<Vec<u8>, &'static str> {
    if oid.is_empty()
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err("invalid Brother OBJBRNET OID");
    }
    Ok([
        PJL_HEADER,
        format!("@PJL DEFAULT OBJBRNET=\"{oid}\"\r\n@PJL INQUIRE OBJBRNET\r\n").as_bytes(),
        PJL_FOOTER,
    ]
    .concat())
}
pub fn wifi_status(data: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(data);
    let (_, tail) = text.split_once("458867")?;
    let value = tail.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    match value.as_bytes().first() {
        Some(b'1') => Some(true),
        Some(b'0') => Some(false),
        _ => None,
    }
}
pub fn wifi_ip(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let (_, tail) = text.split_once("458967.2")?;
    let encoded = tail
        .trim_start_matches(|c: char| c == ':' || c == '"' || c.is_whitespace())
        .split(|c: char| c == '"' || c.is_whitespace())
        .next()?;
    let octets = encoded
        .trim_matches('-')
        .split('-')
        .map(|part| u8::from_str_radix(part, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    (octets.len() == 4).then(|| {
        octets
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    })
}
pub fn encode_ssid(value: &str) -> String {
    value.bytes().map(|byte| format!("-{byte:x}")).collect()
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WifiAccessPoint {
    pub ssid: String,
    pub signal: Option<i16>,
    pub channel: Option<u16>,
    pub security: Option<String>,
}

fn decode_ssid(value: &str) -> String {
    let decoded = value
        .trim_matches(['"', ' '])
        .trim_matches('-')
        .split('-')
        .map(|part| u8::from_str_radix(part, 16).ok())
        .collect::<Option<Vec<_>>>();
    decoded
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| value.trim_matches(['"', ' ']).to_owned())
}

/// Parse Brother AVAILABLEWLAN fixture lines (`ssid,signal,channel,security`).
/// PJL framing and unrelated response lines are ignored.
pub fn wifi_access_points(data: &[u8]) -> Result<Vec<WifiAccessPoint>, csv::Error> {
    let text = String::from_utf8_lossy(data);
    let mut output = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('@') || line.starts_with('\u{1b}') {
            continue;
        }
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(line.as_bytes());
        for record in reader.records() {
            let record = record?;
            let Some(ssid) = record.get(0) else { continue };
            if record.len() < 2 {
                continue;
            }
            output.push(WifiAccessPoint {
                ssid: decode_ssid(ssid),
                signal: record.get(1).and_then(|value| value.trim().parse().ok()),
                channel: record.get(2).and_then(|value| value.trim().parse().ok()),
                security: record.get(3).map(|value| value.trim().to_owned()),
            });
        }
    }
    output.sort_by(|left, right| {
        right
            .signal
            .cmp(&left.signal)
            .then_with(|| left.ssid.cmp(&right.ssid))
    });
    Ok(output)
}
pub fn xor_password(value: &[u8]) -> Vec<u8> {
    value
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ PASSWORD_KEY[index % PASSWORD_KEY.len()])
        .collect()
}
pub fn wifi_configure(
    ssid: &str,
    password: &str,
    encryption: &str,
    authentication: &str,
    reboot: bool,
) -> Result<Vec<u8>, &'static str> {
    let encryption = match encryption {
        "none" => 1,
        "wep" => 2,
        "tkip" => 3,
        "aes" => 4,
        "ckip" => 5,
        "cmic" => 6,
        "ckip-cmic" => 7,
        "tkip-aes" => 8,
        _ => return Err("unknown Wi-Fi encryption"),
    };
    let authentication = match authentication {
        "open" => 1,
        "shared-key" => 2,
        "wpa-psk" => 3,
        "leap" => 7,
        "eap-fast" => 13,
        "peap" => 15,
        "eap-ttls" => 16,
        "eap-tls" => 17,
        "wpa-only" => 18,
        "wpa2-only" => 19,
        _ => return Err("unknown Wi-Fi authentication"),
    };
    if ssid.is_empty() {
        return Err("SSID must not be empty");
    }
    if authentication != 1 && password.is_empty() {
        return Err("authentication requires a password");
    }
    if authentication == 1 && encryption != 1 {
        return Err("open authentication requires no encryption");
    }
    let mut params: Vec<(&str, Vec<u8>)> = vec![
        ("458867", b"0".to_vec()),
        ("458878", b"1".to_vec()),
        ("458877", encode_ssid(ssid).into_bytes()),
    ];
    if matches!(authentication, 3 | 18 | 19) {
        params.push(("99458890", xor_password(password.as_bytes())));
    } else if encryption == 2 {
        params.push(("99458889.1", xor_password(password.as_bytes())));
    }
    params.extend([
        ("458880", encryption.to_string().into_bytes()),
        ("458881", authentication.to_string().into_bytes()),
        ("459138.2", b"1".to_vec()),
        ("459138.3", b"0".to_vec()),
        ("458865", b"1".to_vec()),
    ]);
    let mut body = PJL_HEADER.to_vec();
    for (oid, value) in params {
        body.extend(b"@PJL DEFAULT OBJBRNET=\"");
        body.extend(oid.as_bytes());
        body.push(b':');
        body.extend(value);
        body.extend(b"\"\r\n");
    }
    body.extend(PJL_FOOTER);
    if reboot {
        body.extend(REBOOT_COMMAND);
    }
    Ok(body)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrotherStatus {
    pub media_width_mm: u8,
    pub media_length_mm: u8,
    pub media_type: String,
    pub status_type: String,
    pub phase: String,
    pub errors: Vec<String>,
}
pub fn brother_status(data: &[u8]) -> Result<BrotherStatus, &'static str> {
    if data.len() < 32 {
        return Err("short Brother status response");
    }
    if data[..3] != [0x80, 0x20, 0x42] {
        return Err("invalid Brother status header");
    }
    let mut errors = Vec::new();
    for (byte, table) in [
        (
            data[8],
            &[
                (0, "no media"),
                (1, "end of media"),
                (2, "cutter jam"),
                (4, "unit in use"),
                (5, "printer off"),
                (7, "fan failure"),
            ][..],
        ),
        (
            data[9],
            &[
                (0, "replace media"),
                (1, "expansion buffer full"),
                (2, "transmission error"),
                (4, "cover opened while printing"),
                (6, "media cannot be fed"),
                (7, "system error"),
            ][..],
        ),
    ] {
        errors.extend(
            table
                .iter()
                .filter(|(bit, _)| byte & (1 << bit) != 0)
                .map(|(_, message)| (*message).to_owned()),
        );
    }
    let name = |value, table: &[(u8, &str)]| {
        table.iter().find(|(key, _)| *key == value).map_or_else(
            || format!("unknown 0x{value:02x}"),
            |(_, name)| (*name).into(),
        )
    };
    Ok(BrotherStatus {
        media_width_mm: data[10],
        media_length_mm: data[17],
        media_type: name(
            data[11],
            &[(0, "no media"), (0x0a, "continuous"), (0x0b, "die-cut")],
        ),
        status_type: name(
            data[18],
            &[
                (0, "reply to status request"),
                (1, "printing completed"),
                (2, "error"),
                (5, "notification"),
                (6, "phase change"),
            ],
        ),
        phase: name(data[19], &[(0, "waiting to receive"), (1, "printing")]),
        errors,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum IppValue {
    Integer(i32),
    Text(String),
}
pub fn parse_ipp(data: &[u8]) -> BTreeMap<String, Vec<IppValue>> {
    let mut result = BTreeMap::new();
    let mut offset = 8usize;
    let mut current = None::<String>;
    while offset < data.len() {
        let tag = data[offset];
        offset += 1;
        if matches!(tag, 1..=5) {
            if tag == 3 {
                break;
            }
            continue;
        }
        if offset + 2 > data.len() {
            break;
        }
        let name_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        if offset + name_len + 2 > data.len() {
            break;
        }
        if name_len > 0 {
            current = Some(String::from_utf8_lossy(&data[offset..offset + name_len]).into_owned());
        }
        offset += name_len;
        let value_len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        offset += 2;
        if offset + value_len > data.len() {
            break;
        }
        let raw = &data[offset..offset + value_len];
        offset += value_len;
        let Some(name) = current.clone() else {
            continue;
        };
        let value = if matches!(tag, 0x21 | 0x23) && raw.len() == 4 {
            Some(IppValue::Integer(i32::from_be_bytes(
                raw.try_into().unwrap(),
            )))
        } else if matches!(tag, 0x41 | 0x42 | 0x44..=0x49) {
            Some(IppValue::Text(String::from_utf8_lossy(raw).into_owned()))
        } else {
            None
        };
        if let Some(value) = value {
            result.entry(name).or_insert_with(Vec::new).push(value);
        }
    }
    result
}
fn ipp_attr(tag: u8, name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend((name.len() as u16).to_be_bytes());
    out.extend(name.as_bytes());
    out.extend((value.len() as u16).to_be_bytes());
    out.extend(value.as_bytes());
    out
}
pub fn ipp_request(uri: &str) -> Vec<u8> {
    let mut body = vec![2, 0, 0, 0x0b, 0, 0, 0, 1, 1];
    body.extend(ipp_attr(0x47, "attributes-charset", "utf-8"));
    body.extend(ipp_attr(0x48, "attributes-natural-language", "en"));
    body.extend(ipp_attr(0x45, "printer-uri", uri));
    for (index, name) in [
        "media-ready",
        "media-default",
        "printer-state",
        "printer-state-reasons",
        "printer-make-and-model",
    ]
    .iter()
    .enumerate()
    {
        body.push(0x44);
        body.extend(if index == 0 {
            ("requested-attributes".len() as u16).to_be_bytes()
        } else {
            0u16.to_be_bytes()
        });
        if index == 0 {
            body.extend(b"requested-attributes");
        }
        body.extend((name.len() as u16).to_be_bytes());
        body.extend(name.as_bytes());
    }
    body.push(3);
    body
}
pub fn ipp_query(
    host: &str,
    port: u16,
    timeout: Duration,
) -> std::io::Result<BTreeMap<String, Vec<IppValue>>> {
    let address = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "IPP host did not resolve")
    })?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let uri = format!("ipp://{host}:{port}/ipp/print");
    let body = ipp_request(&uri);
    write!(
        stream,
        "POST /ipp/print HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(length) => {
                response.extend_from_slice(&chunk[..length]);
                if response.len() > 1024 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "IPP HTTP response exceeds 1 MiB",
                    ));
                }
                if http_content_complete(&response) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) && http_content_complete(&response) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid IPP HTTP response")
        })?
        + 4;
    if !response.starts_with(b"HTTP/1.1 200") && !response.starts_with(b"HTTP/1.0 200") {
        return Err(std::io::Error::other("IPP HTTP request failed"));
    }
    Ok(parse_ipp(&response[split..]))
}

/// Submit an already encoded printer-language document through IPP Print-Job.
/// Brother QL network models advertise `application/octet-stream` for this path.
pub fn ipp_print_job(
    host: &str,
    port: u16,
    document: &[u8],
    media: &str,
    timeout: Duration,
) -> std::io::Result<BTreeMap<String, Vec<IppValue>>> {
    let address = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "IPP host did not resolve")
    })?;
    let uri = format!("ipp://{host}:{port}/ipp/print");
    let mut body = vec![2, 0, 0, 2, 0, 0, 0, 1, 1];
    body.extend(ipp_attr(0x47, "attributes-charset", "utf-8"));
    body.extend(ipp_attr(0x48, "attributes-natural-language", "en"));
    body.extend(ipp_attr(0x45, "printer-uri", &uri));
    body.extend(ipp_attr(0x42, "requesting-user-name", "mb-printer"));
    body.extend(ipp_attr(0x42, "job-name", "mb-printer-label"));
    body.extend(ipp_attr(
        0x49,
        "document-format",
        "application/octet-stream",
    ));
    body.push(2); // job-attributes-tag
    body.extend(ipp_attr(0x44, "media", media));
    body.push(0x21); // integer
    body.extend(("copies".len() as u16).to_be_bytes());
    body.extend(b"copies");
    body.extend(4_u16.to_be_bytes());
    body.extend(1_i32.to_be_bytes());
    body.push(3);
    body.extend(document);

    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "POST /ipp/print HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(length) => {
                response.extend_from_slice(&chunk[..length]);
                if response.len() > 1024 * 1024 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "IPP HTTP response exceeds 1 MiB",
                    ));
                }
                if http_content_complete(&response) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) && http_content_complete(&response) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    let split = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid IPP HTTP response")
        })?
        + 4;
    if !response.starts_with(b"HTTP/1.1 200") && !response.starts_with(b"HTTP/1.0 200") {
        return Err(std::io::Error::other("IPP HTTP request failed"));
    }
    let ipp = &response[split..];
    if ipp.len() < 8 || u16::from_be_bytes([ipp[2], ipp[3]]) > 0x00ff {
        return Err(std::io::Error::other("IPP Print-Job was rejected"));
    }
    Ok(parse_ipp(ipp))
}
fn http_content_complete(response: &[u8]) -> bool {
    let Some(split) = response.windows(4).position(|part| part == b"\r\n\r\n") else {
        return false;
    };
    let header_end = split + 4;
    let headers = String::from_utf8_lossy(&response[..split]);
    let Some(length) = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    }) else {
        return false;
    };
    response.len() >= header_end.saturating_add(length)
}
pub fn ipp_media_size(keyword: &str) -> Option<(u16, u16)> {
    keyword.split('_').rev().find_map(|part| {
        let value = part.strip_suffix("mm")?;
        let (width, height) = value.split_once('x')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    })
}

/// Effective ATT payload: an explicitly reported write limit wins; otherwise MTU-3,
/// with the Bluetooth baseline MTU 23 (20 bytes) as the safe fallback.
pub fn ble_payload_limit(
    user_cap: usize,
    reported_write_limit: Option<usize>,
    mtu: Option<u16>,
) -> usize {
    let physical = reported_write_limit
        .or_else(|| mtu.map(|value| usize::from(value.saturating_sub(3))))
        .unwrap_or(20)
        .max(1);
    user_cap.max(1).min(physical)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wifi_contract() {
        assert_eq!(
            wifi_scan_start(),
            b"\x1b%-12345X@PJL\r\n@PJL DEFAULT OBJBRNET=\"458845:31-3a\"\r\n\x1b%-12345X"
        );
        assert_eq!(wifi_status(b"\"458867:1\""), Some(true));
        assert_eq!(
            wifi_ip(b"\"458967.2:-c0-a8-01-64\"").as_deref(),
            Some("192.168.1.100")
        );
        assert_eq!(encode_ssid("Café"), "-43-61-66-c3-a9");
        assert_eq!(xor_password(&xor_password(b"secret")), b"secret");
        assert!(wifi_inquire("bad-oid").is_err());
        let command = wifi_configure("Cafe", "secret", "tkip-aes", "wpa-psk", true).unwrap();
        let ssid = b"458877:-43-61-66-65";
        assert!(command.windows(ssid.len()).any(|value| value == ssid));
        assert!(command.ends_with(&REBOOT_COMMAND));
        let access_points =
            wifi_access_points(b"-43-61-66-65,-42,6,wpa2\n-47-75-65-73-74,-70,11,open\n").unwrap();
        assert_eq!(access_points[0].ssid, "Cafe");
        assert_eq!(access_points[0].signal, Some(-42));
    }
    #[test]
    fn brother_status_contract() {
        let mut reply = [0u8; 32];
        reply[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
        reply[10] = 62;
        reply[11] = 0x0b;
        reply[17] = 29;
        reply[18] = 0;
        reply[19] = 0;
        let status = brother_status(&reply).unwrap();
        assert_eq!((status.media_width_mm, status.media_length_mm), (62, 29));
        assert_eq!(status.media_type, "die-cut");
    }
    #[test]
    fn ipp_and_ble_fixture_matrix() {
        let mut response = vec![2, 0, 0, 0, 0, 0, 0, 1, 4, 0x44, 0, 11];
        response.extend(b"media-ready");
        response.extend([0, 19]);
        response.extend(b"roll_current_62x0mm");
        response.push(3);
        assert_eq!(
            parse_ipp(&response)["media-ready"],
            vec![IppValue::Text("roll_current_62x0mm".into())]
        );
        assert_eq!(ipp_media_size("om_label_29x90mm"), Some((29, 90)));
        assert_eq!(ble_payload_limit(512, None, None), 20);
        assert_eq!(ble_payload_limit(512, None, Some(23)), 20);
        assert_eq!(ble_payload_limit(512, None, Some(517)), 512);
        assert_eq!(ble_payload_limit(600, None, Some(517)), 514);
        assert_eq!(ble_payload_limit(128, Some(244), Some(517)), 128);
    }
    #[test]
    fn live_ipp_client_uses_http_and_decodes_media() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            assert!(
                request
                    .windows(b"application/ipp".len())
                    .any(|part| part == b"application/ipp")
            );
            let mut body = vec![2, 0, 0, 0, 0, 0, 0, 1, 4, 0x44, 0, 11];
            body.extend(b"media-ready");
            body.extend([0, 19]);
            body.extend(b"roll_current_62x0mm");
            body.push(3);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: Keep-Alive\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let attributes = ipp_query("127.0.0.1", address.port(), Duration::from_millis(50)).unwrap();
        server.join().unwrap();
        assert_eq!(
            attributes["media-ready"],
            vec![IppValue::Text("roll_current_62x0mm".into())]
        );
    }

    #[test]
    fn ipp_print_job_submits_raw_brother_data_with_media() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let document = b"\x1bia\x01BROTHER-RASTER";
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(split) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break split + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&chunk[..count]);
            }
            let body = &request[header_end..header_end + content_length];
            assert_eq!(&body[..4], &[2, 0, 0, 2]);
            let media = b"om_label_62x29mm";
            assert!(body.windows(media.len()).any(|part| part == media));
            assert!(body.ends_with(document));

            let response = [2, 0, 0, 0, 0, 0, 0, 1, 3];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(&response).unwrap();
        });
        ipp_print_job(
            "127.0.0.1",
            address.port(),
            document,
            "om_label_62x29mm",
            Duration::from_secs(1),
        )
        .unwrap();
        server.join().unwrap();
    }
}
