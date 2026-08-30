// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",body.len()).unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    write!(stream, "\r\n{body}").unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("HTTP read: {error}"),
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
        .to_owned();
    (status, text, body)
}
fn ipv6_status(port: u16, host: &str, token: &str) -> u16 {
    let mut stream = TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET /v1/capabilities HTTP/1.1\r\nHost: {host}\r\nOrigin: https://editor.example\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.split_whitespace().nth(1).unwrap().parse().unwrap()
}
// Every returned child is killed and waited below; Clippy cannot follow that
// ownership across this test helper boundary.
#[allow(clippy::zombie_processes)]
fn start(binary: &str, config: &Path, port: u16) -> Child {
    let child = Command::new(binary)
        .args([
            "--config",
            config.to_str().unwrap(),
            "api",
            "serve",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            assert!(
                TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)).is_ok(),
                "default service must bind IPv6 loopback with IPv4"
            );
            return child;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("service did not start")
}
fn pairing(binary: &str, config: &Path) -> String {
    let output = Command::new(binary)
        .args([
            "--config",
            config.to_str().unwrap(),
            "api",
            "pair",
            "--expires-seconds",
            "120",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
}
fn exchange_at_origin(port: u16, secret: &str, origin: &str) -> serde_json::Value {
    let (status, _, body) = request(
        port,
        "POST",
        "/v1/pair",
        &[("Origin", origin), ("Content-Type", "application/json")],
        &serde_json::json!({"secret":secret}).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn exchange(port: u16, secret: &str) -> serde_json::Value {
    exchange_at_origin(port, secret, "https://editor.example")
}

#[test]
fn hosted_editor_preflight_and_grant_are_exact_in_real_service() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.json");
    let origin = "https://labels.dev1.makersbrain.net";
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({"allowed_origins":[origin]})).unwrap(),
    )
    .unwrap();
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let secret = pairing(binary, &config);
    let mut server = start(binary, &config, port);

    let (status, headers, _) = request(
        port,
        "OPTIONS",
        "/v1/status",
        &[
            ("Origin", origin),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Private-Network", "true"),
        ],
        "",
    );
    assert_eq!(status, 200);
    let headers = headers.to_ascii_lowercase();
    assert!(headers.contains("access-control-allow-origin: https://labels.dev1.makersbrain.net"));
    assert!(headers.contains("access-control-allow-private-network: true"));

    let pair = exchange_at_origin(port, &secret, origin);
    let auth = format!("Bearer {}", pair["token"].as_str().unwrap());
    assert_eq!(
        request(
            port,
            "GET",
            "/v1/capabilities",
            &[("Origin", origin), ("Authorization", &auth)],
            ""
        )
        .0,
        200
    );
    assert_eq!(
        request(
            port,
            "GET",
            "/v1/capabilities",
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth)
            ],
            ""
        )
        .0,
        401
    );
    let (status, headers, _) = request(
        port,
        "OPTIONS",
        "/v1/status",
        &[
            ("Origin", "https://labels.dev1.makersbrain.net.evil.example"),
            ("Access-Control-Request-Method", "GET"),
            ("Access-Control-Request-Private-Network", "true"),
        ],
        "",
    );
    assert_eq!(status, 403);
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("access-control-allow-private-network")
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[test]
fn external_service_security_restart_and_job_contract() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(
            &serde_json::json!({"allowed_origins":["https://editor.example"],"max_recent_jobs":20}),
        )
        .unwrap(),
    )
    .unwrap();
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = env!("CARGO_BIN_EXE_mb-printer");
    let secret = pairing(binary, &config);
    let mut server = start(binary, &config, port);
    let pair = exchange(port, &secret);
    let token = pair["token"].as_str().unwrap();
    let grant = pair["grantId"].as_str().unwrap();
    assert_eq!(ipv6_status(port, &format!("[::1]:{port}"), token), 200);
    assert_eq!(ipv6_status(port, "evil.example", token), 421);
    let (preflight, headers, _) = request(
        port,
        "OPTIONS",
        "/v1/jobs",
        &[
            ("Origin", "https://editor.example"),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization,content-type",
            ),
            ("Access-Control-Request-Private-Network", "true"),
        ],
        "",
    );
    assert_eq!(preflight, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("access-control-allow-private-network: true")
    );
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/editor-job.json")).unwrap();
    let document = fixture["document"].to_string();
    let auth = format!("Bearer {token}");
    let saved_path = directory.path().join("saved-connection.bin");
    let connection = serde_json::json!({"id":"saved-file","model":"m110","transport":{"kind":"file","path":saved_path}});
    assert_eq!(
        request(
            port,
            "POST",
            "/v1/connection",
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth),
                ("Content-Type", "application/json")
            ],
            &connection.to_string()
        )
        .0,
        200
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/v1/documents/validate",
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth),
                ("Content-Type", "application/json")
            ],
            &document
        )
        .0,
        200
    );
    let mut saved_job: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/editor-job.json")).unwrap();
    saved_job.as_object_mut().unwrap().remove("transport");
    saved_job["connectionId"] = serde_json::json!("saved-file");
    let (_, _, body) = request(
        port,
        "POST",
        "/v1/jobs",
        &[
            ("Origin", "https://editor.example"),
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ],
        &saved_job.to_string(),
    );
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    let saved_id = saved["id"].as_str().unwrap();
    let mut saved = serde_json::Value::Null;
    for _ in 0..50 {
        let (_, _, body) = request(
            port,
            "GET",
            &format!("/v1/jobs/{saved_id}"),
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth),
            ],
            "",
        );
        saved = serde_json::from_str(&body).unwrap();
        if saved["terminal"] == true {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(saved["outcome"], "completed");
    let capture = directory.path().join("service.bin");
    let mut job = fixture;
    job["transport"] = serde_json::json!({"kind":"file","path":capture});
    let (accepted, _, body) = request(
        port,
        "POST",
        "/v1/jobs",
        &[
            ("Origin", "https://editor.example"),
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
        ],
        &job.to_string(),
    );
    assert_eq!(accepted, 202, "{body}");
    let job: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = job["id"].as_str().unwrap();
    let (sse, _, events) = request(
        port,
        "GET",
        &format!("/v1/jobs/{id}/events"),
        &[
            ("Origin", "https://editor.example"),
            ("Authorization", &auth),
        ],
        "",
    );
    assert_eq!(sse, 200);
    assert!(events.contains("progress"));
    assert_eq!(
        request(
            port,
            "POST",
            &format!("/v1/jobs/{id}/cancel"),
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth)
            ],
            ""
        )
        .0,
        200
    );
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        request(
            port,
            "GET",
            &format!("/v1/jobs/{id}"),
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth)
            ],
            ""
        )
        .0,
        200
    );
    server.kill().unwrap();
    server.wait().unwrap();
    let mut server = start(binary, &config, port);
    assert_eq!(
        request(
            port,
            "GET",
            &format!("/v1/jobs/{id}"),
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth)
            ],
            ""
        )
        .0,
        200
    );
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(
        Command::new(binary)
            .args(["--config", config.to_str().unwrap(), "api", "revoke", grant])
            .status()
            .unwrap()
            .success()
    );
    let mut server = start(binary, &config, port);
    assert_eq!(
        request(
            port,
            "GET",
            "/v1/capabilities",
            &[
                ("Origin", "https://editor.example"),
                ("Authorization", &auth)
            ],
            ""
        )
        .0,
        401
    );
    server.kill().unwrap();
    server.wait().unwrap();
    let replacement = pairing(binary, &config);
    let mut server = start(binary, &config, port);
    let new_pair = exchange(port, &replacement);
    assert_ne!(new_pair["token"], pair["token"]);
    assert_eq!(
        request(
            port,
            "POST",
            "/v1/pair",
            &[
                ("Origin", "https://editor.example"),
                ("Content-Type", "application/json")
            ],
            &serde_json::json!({"secret":replacement}).to_string()
        )
        .0,
        401
    );
    server.kill().unwrap();
    server.wait().unwrap();
}
