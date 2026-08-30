// SPDX-License-Identifier: AGPL-3.0-or-later
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, broadcast},
};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
};
use uuid::Uuid;

use crate::{
    VERSION,
    auth::{AuthStore, loopback_host},
    config::Config,
    jobs::{Job, JobState},
    raster,
    transport::{CaptureTransport, PhysicalEvent, SerialTransport, TcpTransport, WriteTransport},
};
use mb_printer_core::{
    Document, capabilities,
    protocol::{self, Options},
};

#[derive(Clone)]
pub struct ApiState {
    pub auth: Arc<RwLock<AuthStore>>,
    pub config: Config,
    pub jobs: Arc<RwLock<HashMap<Uuid, Job>>>,
    events: Arc<RwLock<HashMap<Uuid, broadcast::Sender<Job>>>>,
    cancellations: Arc<RwLock<HashMap<Uuid, Arc<AtomicBool>>>>,
    connections: Arc<RwLock<HashMap<String, Connection>>>,
    connection_executions: Arc<RwLock<HashMap<String, Arc<std::sync::Mutex<()>>>>>,
    injected_devices: Arc<RwLock<Vec<crate::transport::NativeDevice>>>,
    injected_probes: Arc<RwLock<HashMap<String, ProbeResult>>>,
}
impl ApiState {
    pub fn new(auth: AuthStore, config: Config) -> Self {
        let connections = config
            .connections_path
            .as_deref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Connection>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|connection| (connection.id.clone(), connection))
            .collect();
        let mut restored_jobs = config
            .jobs_path
            .as_deref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Job>>(&bytes).ok())
            .unwrap_or_default();
        restored_jobs.sort_by_key(|job| (job.updated_at_ms, job.id));
        if restored_jobs.len() > config.max_recent_jobs {
            restored_jobs.drain(..restored_jobs.len() - config.max_recent_jobs);
        }
        let jobs = restored_jobs
            .into_iter()
            .map(|mut job| {
                if !job.terminal() {
                    job.state = JobState::OutcomeUnknown;
                    job.error = Some("service restarted before terminal outcome".into());
                }
                (job.id, job)
            })
            .collect();
        Self {
            auth: Arc::new(RwLock::new(auth)),
            config,
            jobs: Arc::new(RwLock::new(jobs)),
            events: Arc::new(RwLock::new(HashMap::new())),
            cancellations: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(RwLock::new(connections)),
            connection_executions: Arc::new(RwLock::new(HashMap::new())),
            injected_devices: Arc::new(RwLock::new(Vec::new())),
            injected_probes: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub async fn inject_devices(&self, devices: Vec<crate::transport::NativeDevice>) {
        *self.injected_devices.write().await = devices;
    }
    pub async fn inject_connection_status(
        &self,
        id: &str,
        status: &str,
        media: Option<serde_json::Value>,
    ) -> bool {
        let mut connections = self.connections.write().await;
        let Some(connection) = connections.get_mut(id) else {
            return false;
        };
        connection.status = status.to_owned();
        connection.media = media;
        save_connections(self, &connections).is_ok()
    }
    pub async fn inject_probe(&self, id: &str, result: ProbeResult) {
        self.injected_probes
            .write()
            .await
            .insert(id.to_owned(), result);
    }

    pub(crate) async fn submit_cloud_job(
        &self,
        id: Uuid,
        connection_id: &str,
        request: &crate::cloud::store::CloudPrintRequest,
        request_hash: &str,
    ) -> Result<Job, ApiError> {
        let mut value = serde_json::to_value(request).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "cloud job request is invalid",
            )
        })?;
        value
            .as_object_mut()
            .expect("cloud print request serializes as an object")
            .insert(
                "connectionId".into(),
                serde_json::Value::String(connection_id.to_owned()),
            );
        let body = Bytes::from(serde_json::to_vec(&value).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "cloud job request is invalid",
            )
        })?);
        JobExecutor::new(self.clone())
            .submit(
                body,
                Some(format!("cloud\0{id}")),
                request_hash.to_owned(),
                Some(id),
            )
            .await
            .map(|outcome| outcome.job)
    }

    pub(crate) async fn cloud_job(&self, id: Uuid) -> Option<Job> {
        self.jobs.read().await.get(&id).cloned()
    }

    pub(crate) async fn cancel_cloud_job(&self, id: Uuid) -> Option<Job> {
        if let Some(cancel) = self.cancellations.read().await.get(&id) {
            cancel.store(true, Ordering::Release);
        }
        let mut jobs = self.jobs.write().await;
        let job = jobs.get_mut(&id)?;
        job.request_cancel();
        let result = job.clone();
        let _ = save_jobs(self, &jobs);
        Some(result)
    }
}
fn save_jobs(state: &ApiState, jobs: &HashMap<Uuid, Job>) -> Result<(), ApiError> {
    let Some(path) = &state.config.jobs_path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "job persistence failed"))?;
    }
    let mut values = jobs.values().cloned().collect::<Vec<_>>();
    values.sort_by_key(|job| (job.updated_at_ms, job.id));
    if values.len() > state.config.max_recent_jobs {
        values.drain(..values.len() - state.config.max_recent_jobs);
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&values).unwrap())
        .and_then(|_| std::fs::rename(temporary, path))
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "job persistence failed"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub status: String,
    pub media: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection {
    id: String,
    model: String,
    transport: serde_json::Value,
    status: String,
    media: Option<serde_json::Value>,
}
fn save_connections(
    state: &ApiState,
    connections: &HashMap<String, Connection>,
) -> Result<(), ApiError> {
    let Some(path) = &state.config.connections_path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "connection persistence failed",
            )
        })?;
    }
    let mut values = connections.values().cloned().collect::<Vec<_>>();
    values.sort_by(|a, b| a.id.cmp(&b.id));
    std::fs::write(path, serde_json::to_vec_pretty(&values).unwrap()).map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "connection persistence failed",
        )
    })
}

#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobView {
    id: Uuid,
    state: JobState,
    terminal: bool,
    outcome: Option<JobState>,
    last_completed_action: i64,
    bytes_sent: u64,
    action: usize,
    actions: usize,
    total_bytes: u64,
    phase: String,
    error: Option<String>,
}
impl From<&Job> for JobView {
    fn from(job: &Job) -> Self {
        let terminal = job.terminal();
        Self {
            id: job.id,
            state: job.state,
            terminal,
            outcome: terminal.then_some(job.state),
            last_completed_action: job.last_completed_action.map_or(-1, i64::from),
            bytes_sent: job.bytes_written,
            action: job
                .last_completed_action
                .map_or(0, |action| action as usize + 1),
            actions: job.action_count,
            total_bytes: job.total_bytes,
            phase: format!("{:?}", job.state).to_ascii_lowercase(),
            error: job.error.clone(),
        }
    }
}

fn origin(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError(StatusCode::FORBIDDEN, "missing origin"))
}
fn validate_host(headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "missing host"))?;
    if loopback_host(host) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::MISDIRECTED_REQUEST,
            "non-loopback host rejected",
        ))
    }
}
fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "missing bearer token"))
}
async fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    validate_host(headers)?;
    let o = origin(headers)?;
    let token = bearer(headers)?;
    if let Some(grant) = state.auth.read().await.authenticate(token, o) {
        Ok(grant.id)
    } else {
        Err(ApiError(StatusCode::UNAUTHORIZED, "invalid grant"))
    }
}

async fn current_grant(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = authorize(&state, &headers).await?;
    let grant = state
        .auth
        .read()
        .await
        .grants()
        .into_iter()
        .find(|grant| grant.id == id)
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid grant"))?;
    Ok(Json(
        serde_json::json!({"id":grant.id,"origin":grant.origin,"createdAt":grant.created_at,"expiresAt":grant.expires_at,"revokedAt":grant.revoked_at}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RotateGrantRequest {
    #[serde(default = "default_grant_ttl")]
    expires_seconds: u64,
}
const fn default_grant_ttl() -> u64 {
    30 * 24 * 3600
}
async fn rotate_current_grant(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<RotateGrantRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = authorize(&state, &headers).await?;
    if request.expires_seconds == 0 || request.expires_seconds > 31_536_000 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "expiresSeconds must be between 1 and 31536000",
        ));
    }
    let token = state
        .auth
        .write()
        .await
        .rotate(id, Duration::from_secs(request.expires_seconds))
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "grant persistence failed",
            )
        })?
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "invalid grant"))?;
    Ok(Json(
        serde_json::json!({"grantId":id,"token":token,"expiresIn":request.expires_seconds.min(31_536_000)}),
    ))
}
async fn revoke_current_grant(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let id = authorize(&state, &headers).await?;
    state.auth.write().await.revoke(id).map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "grant persistence failed",
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn preflight_guard(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    if request.method() == http::Method::OPTIONS {
        validate_host(request.headers())?;
        let request_origin = origin(request.headers())?;
        if !state
            .config
            .allowed_origins
            .iter()
            .any(|allowed| allowed == request_origin)
        {
            return Err(ApiError(StatusCode::FORBIDDEN, "origin not allowed"));
        }
    }
    Ok(next.run(request).await)
}

#[derive(Deserialize)]
struct PairRequest {
    secret: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairResponse {
    grant_id: Uuid,
    token: String,
    expires_at: String,
}
async fn pair(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    validate_host(&headers)?;
    let o = origin(&headers)?;
    if !state
        .config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == o)
    {
        return Err(ApiError(StatusCode::FORBIDDEN, "origin not allowed"));
    }
    let (grant_id, token) = state
        .auth
        .write()
        .await
        .exchange(&request.secret, o, Duration::from_secs(30 * 24 * 3600))
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "grant persistence failed",
            )
        })?
        .ok_or(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired pairing secret",
        ))?;
    Ok(Json(PairResponse {
        grant_id,
        token,
        expires_at: (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339(),
    }))
}

async fn capabilities(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    Ok(Json(
        serde_json::json!({"service":"mb-printer","version":VERSION,"api":"v1","features":["documents","preview-png","jobs","job-idempotency","job-fit","self-service-grants","dual-stack-loopback","assets","laposte","file-transport","tcp-transport","serial-transport","ipp-transport","ipps-transport"],"max_document_bytes":state.config.max_document_bytes,"printer_definition_count":capabilities::bundled().len()}),
    ))
}
async fn printers(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    let devices = discovered_devices(&state).await;
    let discovered = devices
        .into_iter()
        .map(|device| {
            let haystack = format!(
                "{} {}",
                device.name.as_deref().unwrap_or(""),
                device.address
            )
            .to_ascii_lowercase();
            let model = capabilities::bundled()
                .into_iter()
                .find(|definition| {
                    haystack.contains(&definition.id.to_ascii_lowercase())
                        || haystack.contains(&definition.name.to_ascii_lowercase())
                })
                .map(|definition| definition.id);
            serde_json::json!({"source":"discovery","device":device,"matchedModel":model})
        })
        .collect::<Vec<_>>();
    let configured = state
        .connections
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"printers":{"discovered":discovered,"configured":configured},"definitions":capabilities::bundled()}),
    ))
}
async fn discovery(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    let devices = discovered_devices(&state).await;
    Ok(Json(
        serde_json::json!({"devices":devices,"supportedTransports":["file","tcp","serial","usb","ble","rfcomm","ipp"]}),
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionRequest {
    id: String,
    model: String,
    transport: serde_json::Value,
}
async fn connection(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<ConnectionRequest>,
) -> Result<Json<Connection>, ApiError> {
    authorize(&state, &headers).await?;
    if request.id.trim().is_empty() || capabilities::by_id(&request.model).is_none() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid connection definition",
        ));
    }
    let kind = request
        .transport
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "transport kind is required",
        ))?;
    if !matches!(
        kind,
        "file" | "tcp" | "serial" | "usb" | "ble" | "rfcomm" | "ipp"
    ) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported transport",
        ));
    }
    let probe = if let Some(probe) = state.injected_probes.read().await.get(&request.id).cloned() {
        probe
    } else {
        probe_transport_async(request.transport.clone()).await?
    };
    let configured = Connection {
        id: request.id,
        model: request.model,
        transport: request.transport,
        status: probe.status,
        media: probe.media,
    };
    let mut connections = state.connections.write().await;
    if !connections.contains_key(&configured.id)
        && connections.len() >= state.config.max_recent_jobs
    {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "connection limit reached",
        ));
    }
    connections.insert(configured.id.clone(), configured.clone());
    save_connections(&state, &connections)?;
    Ok(Json(configured))
}
fn probe_transport(transport: &serde_json::Value) -> Result<ProbeResult, ApiError> {
    let kind = transport["kind"].as_str().unwrap_or_default();
    if kind == "ipp" {
        return ipp_status(transport);
    }
    let result = match kind {
        "tcp" => transport["address"]
            .as_str()
            .ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "TCP address required",
            ))
            .and_then(|address| {
                TcpTransport::connect(address, 128, Duration::from_secs(2))
                    .map(|_| ())
                    .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "TCP probe failed"))
            }),
        "serial" => {
            let path = transport["path"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "serial path required",
            ))?;
            let baud = transport["baud"].as_u64().unwrap_or(115_200) as u32;
            SerialTransport::open(std::path::Path::new(path), baud, 128)
                .map(|_| ())
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "serial probe failed"))
        }
        #[cfg(all(feature = "bluetooth", target_os = "linux"))]
        "rfcomm" => {
            let address = transport["address"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "RFCOMM address required",
            ))?;
            let channel = transport["channel"].as_u64().unwrap_or(1) as u8;
            mb_printer_native::transports::rfcomm::RfcommTransport::bind(0, address, channel, 128)
                .map(|_| ())
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "RFCOMM probe failed"))
        }
        #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
        "rfcomm" => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "RFCOMM unavailable in this build",
            ));
        }
        "file" => {
            let path = transport["path"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "file path required",
            ))?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map(|_| ())
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "file probe failed"))
        }
        #[cfg(feature = "usb")]
        "usb" => crate::transport::usb::UsbTransport::open(
            transport["vid"].as_u64().unwrap_or(0) as u16,
            transport["pid"].as_u64().unwrap_or(0) as u16,
            transport["interface"].as_u64().unwrap_or(0) as u8,
            transport["out"].as_u64().unwrap_or(0) as u8,
            transport["input"].as_u64().map(|value| value as u8),
            128,
        )
        .map(|_| ())
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB probe failed")),
        #[cfg(not(feature = "usb"))]
        "usb" => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "USB unavailable in this build",
            ));
        }
        "ble" => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "BLE probe requires asynchronous discovery/connection",
            ));
        }
        _ => {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported transport",
            ));
        }
    };
    result?;
    Ok(ProbeResult {
        status: "ready".into(),
        media: None,
    })
}

async fn discovered_devices(state: &ApiState) -> Vec<crate::transport::NativeDevice> {
    let mut devices = state.injected_devices.read().await.clone();
    devices.extend(
        tokio::task::spawn_blocking(crate::transport::discover_native)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
    );
    #[cfg(feature = "bluetooth")]
    devices.extend(
        crate::transport::bluetooth::discover()
            .await
            .unwrap_or_default(),
    );
    devices.sort_by(|left, right| {
        (&left.transport, &left.address).cmp(&(&right.transport, &right.address))
    });
    devices
        .dedup_by(|left, right| left.transport == right.transport && left.address == right.address);
    devices
}

async fn probe_transport_async(transport: serde_json::Value) -> Result<ProbeResult, ApiError> {
    if transport["kind"] == "ble" {
        #[cfg(feature = "bluetooth")]
        {
            let address = transport["address"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "BLE address required",
            ))?;
            crate::transport::bluetooth::BleTransport::connect(address, 128)
                .await
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "BLE probe failed"))?;
            return Ok(ProbeResult {
                status: "ready".into(),
                media: None,
            });
        }
        #[cfg(not(feature = "bluetooth"))]
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "BLE unavailable in this build",
        ));
    }
    tokio::task::spawn_blocking(move || probe_transport(&transport))
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "connection probe crashed"))?
}

fn brother_transport_status<T: mb_printer_native::Transport>(
    transport: &mut T,
) -> Result<ProbeResult, ApiError> {
    transport
        .subscribe_notifications()
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "status subscription failed"))?;
    transport
        .write(b"\x1biS")
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "status request failed"))?;
    let bytes = match transport
        .wait_response(3_000)
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "status read failed"))?
    {
        mb_printer_native::WaitOutcome::Response(bytes) => bytes,
        mb_printer_native::WaitOutcome::Timeout => {
            return Err(ApiError(StatusCode::GATEWAY_TIMEOUT, "status timed out"));
        }
        mb_printer_native::WaitOutcome::Unavailable => {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "status reads unavailable",
            ));
        }
    };
    let status = crate::device::brother_status(&bytes)
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "invalid Brother status"))?;
    Ok(ProbeResult {
        status: if status.errors.is_empty() {
            status.phase.clone()
        } else {
            "error".into()
        },
        media: Some(serde_json::json!({
            "widthMm":status.media_width_mm,
            "lengthMm":status.media_length_mm,
            "type":status.media_type,
            "statusType":status.status_type,
            "phase":status.phase,
            "errors":status.errors
        })),
    })
}

fn brother_ipp_status(address: &str) -> Result<ProbeResult, ApiError> {
    let (host, port) = address
        .rsplit_once(':')
        .map_or((address, 631), |(host, port)| {
            (host, port.parse().unwrap_or(631))
        });
    let attributes = crate::device::ipp_query(host, port, Duration::from_secs(3))
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "Brother IPP status failed"))?;
    let text = |name: &str| {
        attributes
            .get(name)
            .and_then(|values| values.first())
            .and_then(|value| match value {
                crate::device::IppValue::Text(value) => Some(value.clone()),
                _ => None,
            })
    };
    let keyword = text("media-ready").or_else(|| text("media-default"));
    let size = keyword.as_deref().and_then(crate::device::ipp_media_size);
    Ok(ProbeResult {
        status: text("printer-state").unwrap_or_else(|| "ipp-response".into()),
        media: Some(serde_json::json!({
            "keyword":keyword,
            "widthMm":size.map(|size|size.0),
            "lengthMm":size.map(|size|size.1),
            "reasons":attributes.get("printer-state-reasons")
        })),
    })
}

fn ipp_endpoint(transport: &serde_json::Value) -> Result<crate::device::IppEndpoint, ApiError> {
    let uri = transport["uri"].as_str().ok_or(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "IPP URI required",
    ))?;
    let certificate_pem = transport["certificatePem"].as_str().map(str::to_owned);
    crate::device::IppEndpoint::new(uri, certificate_pem).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid IPP/IPPS endpoint",
        )
    })
}

fn ipp_status(transport: &serde_json::Value) -> Result<ProbeResult, ApiError> {
    let endpoint = ipp_endpoint(transport)?;
    let attributes = crate::device::ipp_query_endpoint(&endpoint, Duration::from_secs(3))
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "IPP/IPPS status failed"))?;
    let text = |name: &str| {
        attributes.get(name).and_then(|values| {
            values.iter().find_map(|value| match value {
                crate::device::IppValue::Text(value) => Some(value.clone()),
                crate::device::IppValue::Integer(_) => None,
            })
        })
    };
    let integer = |name: &str| {
        attributes.get(name).and_then(|values| {
            values.iter().find_map(|value| match value {
                crate::device::IppValue::Integer(value) => Some(*value),
                crate::device::IppValue::Text(_) => None,
            })
        })
    };
    let keyword = text("media-ready").or_else(|| text("media-default"));
    let size = keyword.as_deref().and_then(crate::device::ipp_media_size);
    let state = match integer("printer-state") {
        Some(3) => "idle",
        Some(4) => "processing",
        Some(5) => "stopped",
        _ => "ready",
    };
    let reasons = attributes
        .get("printer-state-reasons")
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            crate::device::IppValue::Text(value) => Some(value.clone()),
            crate::device::IppValue::Integer(_) => None,
        })
        .collect::<Vec<_>>();
    Ok(ProbeResult {
        status: state.into(),
        media: Some(serde_json::json!({
            "keyword":keyword,
            "widthMm":size.map(|size|size.0),
            "lengthMm":size.map(|size|size.1),
            "printerState":state,
            "reasons":reasons,
            "makeAndModel":text("printer-make-and-model")
        })),
    })
}

async fn printer_status_async(connection: &Connection) -> Result<ProbeResult, ApiError> {
    if connection.transport["kind"] == "ipp" {
        let transport = connection.transport.clone();
        return tokio::task::spawn_blocking(move || ipp_status(&transport))
            .await
            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "IPP status task crashed"))?;
    }
    let Some(definition) = capabilities::by_id(&connection.model) else {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown printer model",
        ));
    };
    if definition.protocol != mb_printer_core::capabilities::Protocol::Brother {
        return probe_transport_async(connection.transport.clone()).await;
    }
    let transport = connection.transport.clone();
    match transport["kind"].as_str().unwrap_or_default() {
        "tcp" if transport["statusMode"] != "raster" => {
            let address = transport["statusAddress"]
                .as_str()
                .or_else(|| transport["address"].as_str())
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "TCP address required",
                ))?
                .to_owned();
            tokio::task::spawn_blocking(move || brother_ipp_status(&address))
                .await
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "IPP status task crashed"))?
        }
        "tcp" => {
            let address = transport["address"]
                .as_str()
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "TCP address required",
                ))?
                .to_owned();
            tokio::task::spawn_blocking(move || {
                let mut target = TcpTransport::connect(&address, 128, Duration::from_secs(3))
                    .map_err(|_| {
                        ApiError(StatusCode::BAD_GATEWAY, "TCP status connection failed")
                    })?;
                brother_transport_status(&mut target)
            })
            .await
            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "TCP status task crashed"))?
        }
        "serial" => tokio::task::spawn_blocking(move || {
            let path = transport["path"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "serial path required",
            ))?;
            let mut target = SerialTransport::open(
                std::path::Path::new(path),
                transport["baud"].as_u64().unwrap_or(115_200) as u32,
                128,
            )
            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "serial status connection failed"))?;
            brother_transport_status(&mut target)
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "serial status task crashed"))?,
        #[cfg(feature = "usb")]
        "usb" => tokio::task::spawn_blocking(move || {
            let mut target = crate::transport::usb::UsbTransport::open(
                transport["vid"].as_u64().unwrap_or(0) as u16,
                transport["pid"].as_u64().unwrap_or(0) as u16,
                transport["interface"].as_u64().unwrap_or(0) as u8,
                transport["out"].as_u64().unwrap_or(0) as u8,
                transport["input"].as_u64().map(|value| value as u8),
                128,
            )
            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB status connection failed"))?;
            brother_transport_status(&mut target)
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB status task crashed"))?,
        #[cfg(feature = "bluetooth")]
        "ble" => {
            let address = transport["address"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "BLE address required",
            ))?;
            let mut target = crate::transport::bluetooth::BleTransport::connect(address, 128)
                .await
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "BLE status connection failed"))?;
            brother_transport_status(&mut target)
        }
        #[cfg(all(feature = "bluetooth", target_os = "linux"))]
        "rfcomm" => tokio::task::spawn_blocking(move || {
            let address = transport["address"].as_str().ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "RFCOMM address required",
            ))?;
            let channel = transport["channel"].as_u64().unwrap_or(1) as u8;
            let mut target = mb_printer_native::transports::rfcomm::RfcommTransport::bind(
                0, address, channel, 128,
            )
            .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "RFCOMM status connection failed"))?;
            brother_transport_status(&mut target)
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "RFCOMM status task crashed"))?,
        _ => probe_transport_async(transport).await,
    }
}
#[derive(Deserialize)]
struct StatusQuery {
    connection: Option<String>,
}
async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<StatusQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    let connections = state.connections.read().await;
    let selected = query
        .connection
        .as_ref()
        .and_then(|id| connections.get(id))
        .cloned();
    let configured = connections.values().cloned().collect::<Vec<_>>();
    drop(connections);
    Ok(Json(match selected {
        Some(value) => {
            let live =
                if let Some(probe) = state.injected_probes.read().await.get(&value.id).cloned() {
                    Ok(probe)
                } else {
                    printer_status_async(&value).await
                };
            let (connected, status, media) = match live {
                Ok(probe) => (true, probe.status, probe.media.or(value.media.clone())),
                Err(_) => (false, "unavailable".into(), value.media.clone()),
            };
            serde_json::json!({"connection":value,"connected":connected,"status":status,"media":media})
        }
        None => {
            serde_json::json!({"connections":configured,"connected":false,"status":"not-connected","media":null})
        }
    }))
}
async fn validate_document(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    if body.len() > state.config.max_document_bytes {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "document too large",
        ));
    }
    let value = serde_json::from_slice(&body)
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "document JSON is invalid"))?;
    let document = canonical_document(&value)?;
    let errors = document
        .validate()
        .err()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"valid":errors.is_empty(),"errors":errors}),
    ))
}

fn parse_document(body: &[u8]) -> Result<Document, ApiError> {
    let value = serde_json::from_slice(body)
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "document JSON is invalid"))?;
    let document = canonical_document(&value)?;
    if document.validate().is_err() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "document validation failed",
        ));
    }
    Ok(document)
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewQuery {
    #[serde(default = "default_zoom")]
    zoom: f64,
    #[serde(default)]
    offset_x: f64,
    #[serde(default)]
    offset_y: f64,
}
const fn default_zoom() -> f64 {
    1.0
}
async fn preview_document(
    State(state): State<ApiState>,
    Query(query): Query<PreviewQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    if body.len() > state.config.max_document_bytes {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "document too large",
        ));
    }
    let document = parse_document(&body)?;
    let image = raster::render(&document, document.media.dpi).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "document cannot be rasterized",
        )
    })?;
    let image = raster::preview_transform(&image, query.zoom, query.offset_x, query.offset_y)
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid preview viewport"))?;
    let png = raster::png(&image, document.media.dpi)
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "PNG encoding failed"))?;
    Ok(([(http::header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportFormat {
    Png,
    Pdf,
}
#[derive(Deserialize)]
struct ExportQuery {
    format: ExportFormat,
}
async fn export_document(
    State(state): State<ApiState>,
    Query(query): Query<ExportQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    if body.len() > state.config.max_document_bytes {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "document too large",
        ));
    }
    let document = parse_document(&body)?;
    let image = raster::render(&document, document.media.dpi).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "document cannot be rasterized",
        )
    })?;
    let (content_type, bytes) = match query.format {
        ExportFormat::Png => (
            "image/png",
            raster::png(&image, document.media.dpi)
                .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "PNG encoding failed"))?,
        ),
        ExportFormat::Pdf => (
            "application/pdf",
            mb_printer_core::export::pdf_physical(
                &image,
                document.media.width,
                document.media.height,
            )
            .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "PDF encoding failed"))?,
        ),
    };
    Ok(([(http::header::CONTENT_TYPE, content_type)], bytes).into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobRequest {
    document: serde_json::Value,
    #[serde(alias = "printerId")]
    model: Option<String>,
    #[serde(default)]
    connection_id: Option<String>,
    #[serde(default)]
    dpi: Option<u16>,
    #[serde(default)]
    rotation: u16,
    #[serde(default)]
    fit: bool,
    #[serde(default = "default_density")]
    density: u8,
    #[serde(default = "default_copies")]
    copies: u16,
    #[serde(default = "default_payload")]
    payload_limit: usize,
    #[serde(default)]
    transport: Option<ApiTransport>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ApiTransport {
    Capture,
    File {
        path: String,
    },
    Tcp {
        address: String,
    },
    Ipp {
        uri: String,
        #[serde(default, rename = "certificatePem")]
        certificate_pem: Option<String>,
    },
    Serial {
        path: String,
        #[serde(default = "default_baud")]
        baud: u32,
    },
    Rfcomm {
        address: String,
        #[serde(default = "default_rfcomm_channel")]
        channel: u8,
    },
    Usb {
        vid: u16,
        pid: u16,
        interface: u8,
        out: u8,
        input: Option<u8>,
    },
    Ble {
        address: String,
    },
}
const fn default_baud() -> u32 {
    115_200
}
const fn default_rfcomm_channel() -> u8 {
    1
}
fn canonical_document(value: &serde_json::Value) -> Result<Document, ApiError> {
    if value
        .pointer("/media/unit")
        .and_then(serde_json::Value::as_str)
        == Some("micrometre")
    {
        return Document::from_json(&value.to_string()).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document schema is invalid",
            )
        });
    }
    if value
        .pointer("/media/unit")
        .and_then(serde_json::Value::as_str)
        != Some("mm")
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "document must be canonical v4 or editor v4",
        ));
    }
    let mm = |value: &serde_json::Value| (value.as_f64().unwrap_or(0.0) * 1000.0).round() as i64;
    let bounds = |value: &serde_json::Value| serde_json::json!({"x":mm(&value["x"]),"y":mm(&value["y"]),"width":mm(&value["width"]),"height":mm(&value["height"])});
    let mut resources = value["resources"].as_array().cloned().unwrap_or_default();
    for font in value["fonts"].as_array().into_iter().flatten() {
        if !resources
            .iter()
            .any(|resource| resource["id"] == font["id"])
        {
            resources.push(font.clone());
        }
    }
    let resources=resources.into_iter().map(|resource|serde_json::json!({"id":resource["id"],"mediaType":resource["mimeType"],"sha256":resource["sha256"],"dataBase64":resource["data"]})).collect::<Vec<_>>();
    let elements=value["elements"].as_array().into_iter().flatten().map(|element|{let transform=&element["transform"];let mut object=serde_json::json!({"id":element["id"],"transform":{"x":mm(&transform["x"]),"y":mm(&transform["y"]),"width":mm(&transform["width"]),"height":mm(&transform["height"]),"rotationMillidegrees":(transform["rotation"].as_f64().unwrap_or(0.0)*1000.0).round()as i64},"zOrder":element["zIndex"],"visible":element["visible"],"locked":element["locked"],"groupId":element.get("groupId").cloned().unwrap_or(serde_json::Value::Null)}).as_object().unwrap().clone();let kind=element["type"].as_str().unwrap_or("");object.insert("type".into(),serde_json::json!(if kind=="qr"{"qr-code"}else{kind}));match kind{"text"=>{object.insert("text".into(),element["text"].clone());object.insert("fontResource".into(),serde_json::Value::Null);object.insert("fontSize".into(),serde_json::json!(mm(&element["fontSize"])));for key in ["horizontalAlign","verticalAlign","overflow"]{object.insert(key.into(),element[key].clone());}},"image"|"svg"=>{object.insert("resource".into(),element["resourceId"].clone());if kind=="image"&&element.get("crop").is_some(){object.insert("crop".into(),bounds(&element["crop"]));}},"line"=>{object.insert("strokeWidth".into(),serde_json::json!(mm(&element["strokeWidth"])));},"rectangle"|"ellipse"|"triangle"=>{object.insert("strokeWidth".into(),serde_json::json!(mm(&element["strokeWidth"])));object.insert("fill".into(),element["filled"].clone());},"barcode"=>{object.insert("data".into(),element["value"].clone());object.insert("symbology".into(),serde_json::json!(if element["symbology"]=="upca"{"upc-a"}else{element["symbology"].as_str().unwrap_or("code128")}));object.insert("humanReadable".into(),element["showText"].clone());},"qr"=>{object.insert("data".into(),element["value"].clone());object.insert("errorCorrection".into(),element["errorCorrection"].clone());},"group"=>{object.insert("children".into(),element["childIds"].clone());},_=>{}}serde_json::Value::Object(object)}).collect::<Vec<_>>();
    let media = &value["media"];
    let canonical = serde_json::json!({"version":4,"name":value.get("title").and_then(serde_json::Value::as_str).unwrap_or("Untitled label"),"media":{"width":mm(&media["width"]),"height":mm(&media["height"]),"unit":"micrometre","dpi":media["dpi"],"orientation":media["orientation"],"printableBounds":bounds(&media["printableBounds"]),"shape":if media["shape"]=="round"{"round"}else{"rectangle"},"continuous":media["shape"]=="continuous","zones":[]},"coordinateSystem":{"unit":"micrometre","origin":"top-left","rounding":"half-away-from-zero"},"elements":elements,"resources":resources,"fields":[],"extensions":{"makersbrain.editor:state":{"id":value["id"],"createdAt":value["createdAt"],"modifiedAt":value["modifiedAt"]}}});
    Document::from_json(&canonical.to_string()).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "editor document conversion failed",
        )
    })
}
fn api_render_for_printer(
    document: &Document,
    printer: &mb_printer_core::capabilities::PrinterDefinition,
    dpi: u16,
    rotation: u16,
    fit: bool,
    media_box: Option<(u32, u32)>,
) -> Result<mb_printer_core::protocol::Raster, ApiError> {
    use mb_printer_core::raster::{Fit, Rotation};
    if printer.width_px().is_none() && media_box.is_none() {
        if rotation != 0 || fit {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "rotation/fit require a fixed-width printer model",
            ));
        }
        return raster::render_for_printer(document, printer, dpi).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document cannot be rasterized",
            )
        });
    }
    let mut mono = raster::render(document, dpi).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "document cannot be rasterized",
        )
    })?;
    mono = match rotation {
        0 => mono,
        90 => mono.rotate(Rotation::Clockwise90),
        180 => mono.rotate(Rotation::Half),
        270 => mono.rotate(Rotation::CounterClockwise90),
        _ => {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid rotation",
            ));
        }
    };
    if printer.rotated {
        mono = mono.rotate(Rotation::Clockwise90);
    }
    let head = printer
        .width_px()
        .or_else(|| media_box.map(|(width, _)| width))
        .expect("fixed model or loaded media checked above");
    if fit
        && let Some((media_width, media_height)) = media_box
        && (mono.width > media_width || mono.height > media_height)
    {
        mono = raster::fit_to_box(&mono, media_width.min(head), media_height).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document cannot be fitted to loaded media",
            )
        })?;
    }
    if mono.width > head {
        if !fit {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "rendered document exceeds printer head; set fit=true",
            ));
        }
        mono = raster::scale_to_width(&mono, head).map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document cannot be fitted",
            )
        })?;
    }
    let alignment = match printer.alignment {
        mb_printer_core::capabilities::Alignment::Left => Fit::Left,
        mb_printer_core::capabilities::Alignment::Center => Fit::Center,
        mb_printer_core::capabilities::Alignment::Right => Fit::Right,
    };
    let mono = mono
        .place_on_head_byte_aligned(head, alignment, 0, 0)
        .map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document cannot be placed",
            )
        })?;
    Ok(mb_printer_core::protocol::Raster {
        width_bytes: u16::try_from(mono.width.div_ceil(8))
            .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "raster is too wide"))?,
        height: mono.height,
        data: mono
            .pack_msb()
            .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "raster packing failed"))?,
    })
}
const fn default_density() -> u8 {
    6
}
const fn default_copies() -> u16 {
    1
}
const fn default_payload() -> usize {
    512
}
struct Cancellable<T> {
    inner: T,
    cancel: Arc<AtomicBool>,
}
impl<T: mb_printer_native::Transport> mb_printer_native::Transport for Cancellable<T> {
    fn payload_limit(&self) -> usize {
        self.inner.payload_limit()
    }
    fn subscribe_notifications(&mut self) -> Result<(), String> {
        if self.cancel.load(Ordering::Acquire) {
            Err("cancelled".into())
        } else {
            mb_printer_native::Transport::subscribe_notifications(&mut self.inner)
        }
    }
    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if self.cancel.load(Ordering::Acquire) {
            Err("cancelled".into())
        } else {
            mb_printer_native::Transport::write(&mut self.inner, bytes)
        }
    }
    fn delay_monotonic(&mut self, milliseconds: u64) {
        mb_printer_native::Transport::delay_monotonic(&mut self.inner, milliseconds)
    }
    fn wait_response(&mut self, timeout_ms: u64) -> Result<mb_printer_native::WaitOutcome, String> {
        if self.cancel.load(Ordering::Acquire) {
            Err("cancelled".into())
        } else {
            mb_printer_native::Transport::wait_response(&mut self.inner, timeout_ms)
        }
    }
}
fn error_progress(error: &mb_printer_native::ExecuteError) -> Option<&mb_printer_native::Progress> {
    match error {
        mb_printer_native::ExecuteError::Transport { progress, .. }
        | mb_printer_native::ExecuteError::Timeout { progress }
        | mb_printer_native::ExecuteError::Response { progress, .. } => Some(progress),
        mb_printer_native::ExecuteError::AtomicTooLarge { .. }
        | mb_printer_native::ExecuteError::InvalidPlan { .. }
        | mb_printer_native::ExecuteError::Replay(_)
        | mb_printer_native::ExecuteError::ReplayStore(_) => None,
    }
}
fn execute_cancellable<T: mb_printer_native::Transport>(
    plan: &mb_printer_core::protocol::Plan,
    inner: T,
    cancel: Arc<AtomicBool>,
) -> Result<mb_printer_native::Progress, (String, Option<mb_printer_native::Progress>)> {
    mb_printer_native::execute(plan, &mut Cancellable { inner, cancel }).map_err(|error| {
        let progress = error_progress(&error).cloned();
        (error.to_string(), progress)
    })
}

fn execute_ipp_cancellable(
    plan: &mb_printer_core::protocol::Plan,
    uri: String,
    certificate_pem: Option<String>,
    payload_limit: usize,
    cancel: Arc<AtomicBool>,
) -> Result<mb_printer_native::Progress, (String, Option<mb_printer_native::Progress>)> {
    if plan.protocol != mb_printer_core::capabilities::Protocol::Brother {
        return Err((
            "IPP octet-stream printing currently requires a Brother raster plan".into(),
            None,
        ));
    }
    let endpoint = crate::device::IppEndpoint::new(uri, certificate_pem)
        .map_err(|error| (error.to_string(), None))?;
    let attributes = crate::device::ipp_query_endpoint(&endpoint, Duration::from_secs(5))
        .map_err(|error| (error.to_string(), None))?;
    let media = attributes
        .get("media-ready")
        .or_else(|| attributes.get("media-default"))
        .and_then(|values| {
            values.iter().find_map(|value| match value {
                crate::device::IppValue::Text(value) => Some(value.clone()),
                crate::device::IppValue::Integer(_) => None,
            })
        })
        .ok_or_else(|| ("IPP printer did not report loaded media".into(), None))?;
    if cancel.load(Ordering::Acquire) {
        return Err(("cancelled".into(), None));
    }
    let mut capture = CaptureTransport::new(payload_limit);
    let mut response = vec![0; 32];
    response[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
    capture.response = Some(response);
    let progress = mb_printer_native::execute(plan, &mut capture)
        .map_err(|error| (error.to_string(), None))?;
    let document = capture
        .events
        .iter()
        .filter_map(|event| match event {
            PhysicalEvent::Write { bytes } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    if cancel.load(Ordering::Acquire) {
        return Err(("cancelled".into(), None));
    }
    crate::device::ipp_print_job_endpoint(&endpoint, &document, &media, Duration::from_secs(15))
        .map_err(|error| (error.to_string(), Some(progress.clone())))?;
    Ok(progress)
}
async fn submit_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<JobView>), ApiError> {
    authorize(&state, &headers).await?;
    if body.len() > state.config.max_document_bytes {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "document too large",
        ));
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid idempotency key"))
        })
        .transpose()?;
    if idempotency_key
        .as_deref()
        .is_some_and(|key| key.is_empty() || key.len() > 128 || !key.is_ascii())
    {
        return Err(ApiError(StatusCode::BAD_REQUEST, "invalid idempotency key"));
    }
    let request_hash = format!("{:x}", Sha256::digest(&body));
    let scoped_key = idempotency_key.as_ref().map(|key| {
        format!(
            "{}\0{key}",
            origin(&headers).expect("authorization already validated origin")
        )
    });
    let outcome = JobExecutor::new(state)
        .submit(body, scoped_key, request_hash, None)
        .await?;
    Ok((outcome.status, Json(JobView::from(&outcome.job))))
}

pub(crate) struct SubmitOutcome {
    pub(crate) status: StatusCode,
    pub(crate) job: Job,
}

/// One execution boundary shared by loopback HTTP submissions and cloud jobs.
///
/// Callers select an already configured connection in the request. Only this
/// boundary resolves that connection into a native transport, validates and
/// plans the document, persists the job, and starts physical execution.
#[derive(Clone)]
pub(crate) struct JobExecutor {
    state: ApiState,
}

impl JobExecutor {
    pub(crate) fn new(state: ApiState) -> Self {
        Self { state }
    }

    pub(crate) async fn submit(
        &self,
        body: Bytes,
        scoped_key: Option<String>,
        request_hash: String,
        forced_job_id: Option<Uuid>,
    ) -> Result<SubmitOutcome, ApiError> {
        let state = self.state.clone();
        if let Some(key) = &scoped_key
            && let Some(existing) = state
                .jobs
                .read()
                .await
                .values()
                .find(|job| job.idempotency_key.as_deref() == Some(key))
        {
            if existing.request_hash.as_deref() != Some(&request_hash) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "idempotency key was used for a different request",
                ));
            }
            return Ok(SubmitOutcome {
                status: StatusCode::OK,
                job: existing.clone(),
            });
        }
        let request: JobRequest = serde_json::from_slice(&body)
            .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid job request"))?;
        if !(1..=8).contains(&request.density) || request.copies == 0 || request.payload_limit == 0
        {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid print options",
            ));
        }
        if request.connection_id.is_some() == request.transport.is_some() {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "provide exactly one of connectionId or transport",
            ));
        }
        let (model, transport, loaded_media) = if let Some(id) = &request.connection_id {
            let connections = state.connections.read().await;
            let connection = connections.get(id).ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "saved connection not found",
            ))?;
            if request
                .model
                .as_deref()
                .is_some_and(|model| model != connection.model)
            {
                return Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "printer model does not match saved connection",
                ));
            }
            let transport = serde_json::from_value(connection.transport.clone()).map_err(|_| {
                ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "saved connection transport is invalid",
                )
            })?;
            (
                connection.model.clone(),
                transport,
                connection.media.clone(),
            )
        } else {
            (
                request.model.clone().ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "printerId/model is required",
                ))?,
                request.transport.clone().unwrap(),
                None,
            )
        };
        #[cfg(not(feature = "usb"))]
        if matches!(transport, ApiTransport::Usb { .. }) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "USB transport is unavailable in this build",
            ));
        }
        #[cfg(not(feature = "bluetooth"))]
        if matches!(transport, ApiTransport::Ble { .. }) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "BLE transport is unavailable in this build",
            ));
        }
        #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
        if matches!(transport, ApiTransport::Rfcomm { .. }) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "RFCOMM transport is unavailable in this build",
            ));
        }
        let document = canonical_document(&request.document)?;
        if document.validate().is_err() {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document validation failed",
            ));
        }
        let printer = capabilities::by_id(&model).ok_or(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown printer model",
        ))?;
        if !matches!(request.rotation, 0 | 90 | 180 | 270) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid rotation",
            ));
        }
        let (document_width_mm, document_height_mm) = if matches!(request.rotation, 90 | 270) {
            (
                document.media.height as f64 / 1000.0,
                document.media.width as f64 / 1000.0,
            )
        } else {
            (
                document.media.width as f64 / 1000.0,
                document.media.height as f64 / 1000.0,
            )
        };
        if !request.fit
            && loaded_media.as_ref().is_some_and(|media| {
                let width = media.get("widthMm").and_then(serde_json::Value::as_f64);
                let length = media.get("lengthMm").and_then(serde_json::Value::as_f64);
                width.is_some_and(|width| document_width_mm > width + 0.5)
                    || (!document.media.continuous
                        && length.is_some_and(|length| document_height_mm > length + 0.5))
            })
        {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "document exceeds loaded media; set fit=true",
            ));
        }
        let target_dpi = request.dpi.unwrap_or(printer.dpi);
        let loaded_box = loaded_media.as_ref().and_then(|media| {
            let width = media.get("widthMm")?.as_f64()?;
            let height = media.get("lengthMm")?.as_f64()?;
            if width <= 0.0 || height <= 0.0 || document.media.continuous {
                return None;
            }
            Some((
                (width * f64::from(target_dpi) / 25.4).round() as u32,
                (height * f64::from(target_dpi) / 25.4).round() as u32,
            ))
        });
        let packed = api_render_for_printer(
            &document,
            &printer,
            target_dpi,
            request.rotation,
            request.fit,
            loaded_box,
        )?;
        let brother_media = if printer.protocol == mb_printer_core::capabilities::Protocol::Brother
        {
            let width = loaded_media
                .as_ref()
                .and_then(|media| media.get("widthMm"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(document_width_mm);
            let length = loaded_media
                .as_ref()
                .and_then(|media| media.get("lengthMm"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(document_height_mm);
            let preset = mb_printer_core::media::match_media(&printer, width, length);
            Some(mb_printer_core::protocol::BrotherMedia {
                width_mm: u8::try_from(width.round() as i64).map_err(|_| {
                    ApiError(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Brother media width is invalid",
                    )
                })?,
                length_mm: if document.media.continuous {
                    0
                } else {
                    u8::try_from(length.round() as i64).map_err(|_| {
                        ApiError(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Brother media length is invalid",
                        )
                    })?
                },
                continuous: document.media.continuous,
                feed_margin: preset
                    .and_then(|media| media.feed_margin_dots)
                    .and_then(|margin| u16::try_from(margin).ok())
                    .unwrap_or_default(),
            })
        } else {
            None
        };
        let plan = protocol::plan(
            &printer,
            &packed,
            &Options {
                density: request.density,
                copies: request.copies,
                continuous: document.media.continuous,
                brother_media,
                ..Options::default()
            },
        )
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "protocol plan failed"))?;
        let mut job = Job::new();
        if let Some(id) = forced_job_id {
            job.id = id;
        }
        job.request_hash = scoped_key.as_ref().map(|_| request_hash);
        job.idempotency_key = scoped_key;
        job.protocol = Some(format!("{:?}", printer.protocol).to_ascii_lowercase());
        job.action_count = plan.actions.len();
        job.total_bytes = plan
            .actions
            .iter()
            .map(|action| match action {
                mb_printer_core::protocol::Action::CommandWrite { bytes, .. }
                | mb_printer_core::protocol::Action::RasterWrite { bytes, .. } => {
                    bytes.len() as u64
                }
                _ => 0,
            })
            .sum();
        job.resumable = Some(
            serde_json::json!({"model":model,"connectionId":request.connection_id,"transport":request.transport,"document":request.document,"dpi":request.dpi,"rotation":request.rotation,"fit":request.fit,"density":request.density,"copies":request.copies,"payloadLimit":request.payload_limit}),
        );
        let (events, _) = broadcast::channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        let id = job.id;
        {
            let mut jobs = state.jobs.write().await;
            if let Some(key) = &job.idempotency_key
                && let Some(existing) = jobs
                    .values()
                    .find(|existing| existing.idempotency_key.as_ref() == Some(key))
            {
                if existing.request_hash != job.request_hash {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        "idempotency key was used for a different request",
                    ));
                }
                return Ok(SubmitOutcome {
                    status: StatusCode::OK,
                    job: existing.clone(),
                });
            }
            if jobs.len() >= state.config.max_recent_jobs {
                let removable = jobs
                    .values()
                    .filter(|job| job.terminal())
                    .min_by_key(|job| (job.updated_at_ms, job.id))
                    .map(|job| job.id);
                if let Some(old_id) = removable {
                    jobs.remove(&old_id);
                } else {
                    return Err(ApiError(
                        StatusCode::TOO_MANY_REQUESTS,
                        "all retained jobs are active",
                    ));
                }
            }
            jobs.insert(id, job.clone());
            save_jobs(&state, &jobs)?;
        }
        state.events.write().await.insert(id, events.clone());
        state.cancellations.write().await.insert(id, cancel.clone());
        let execution_lock = if let Some(connection_id) = &request.connection_id {
            let mut locks = state.connection_executions.write().await;
            Some(
                locks
                    .entry(connection_id.clone())
                    .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
                    .clone(),
            )
        } else {
            None
        };
        let accepted = job.clone();
        let worker_state = state.clone();
        #[cfg(feature = "bluetooth")]
        let worker_runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let _connection_guard = execution_lock.as_ref().map(|lock| {
                lock.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            });
            let mut running = job;
            running.state = JobState::Running;
            running.updated_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let _ = events.send(running.clone());
            let execution = match transport {
                ApiTransport::Capture => {
                    let mut target = CaptureTransport::new(request.payload_limit);
                    if matches!(
                        printer.protocol,
                        mb_printer_core::capabilities::Protocol::Brother
                    ) {
                        let mut response = vec![0; 32];
                        response[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
                        target.response = Some(response);
                    }
                    execute_cancellable(&plan, target, cancel.clone())
                }
                ApiTransport::File { path } => {
                    WriteTransport::file(std::path::Path::new(&path), request.payload_limit)
                        .map_err(|error| (error.to_string(), None))
                        .and_then(|target| execute_cancellable(&plan, target, cancel.clone()))
                }
                ApiTransport::Tcp { address } => {
                    TcpTransport::connect(&address, request.payload_limit, Duration::from_secs(5))
                        .map_err(|error| (error.to_string(), None))
                        .and_then(|target| execute_cancellable(&plan, target, cancel.clone()))
                }
                ApiTransport::Ipp {
                    uri,
                    certificate_pem,
                } => execute_ipp_cancellable(
                    &plan,
                    uri,
                    certificate_pem,
                    request.payload_limit,
                    cancel.clone(),
                ),
                ApiTransport::Serial { path, baud } => {
                    SerialTransport::open(std::path::Path::new(&path), baud, request.payload_limit)
                        .map_err(|error| (error.to_string(), None))
                        .and_then(|target| execute_cancellable(&plan, target, cancel.clone()))
                }
                #[cfg(all(feature = "bluetooth", target_os = "linux"))]
                ApiTransport::Rfcomm { address, channel } => {
                    mb_printer_native::transports::rfcomm::RfcommTransport::bind(
                        0,
                        &address,
                        channel,
                        request.payload_limit,
                    )
                    .map_err(|error| (error, None))
                    .and_then(|target| execute_cancellable(&plan, target, cancel.clone()))
                }
                #[cfg(not(all(feature = "bluetooth", target_os = "linux")))]
                ApiTransport::Rfcomm { .. } => {
                    Err(("RFCOMM support is unavailable in this build".into(), None))
                }
                #[cfg(feature = "usb")]
                ApiTransport::Usb {
                    vid,
                    pid,
                    interface,
                    out,
                    input,
                } => crate::transport::usb::UsbTransport::open(
                    vid,
                    pid,
                    interface,
                    out,
                    input,
                    request.payload_limit,
                )
                .map_err(|error| (error.to_string(), None))
                .and_then(|target| execute_cancellable(&plan, target, cancel.clone())),
                #[cfg(not(feature = "usb"))]
                ApiTransport::Usb { .. } => {
                    Err(("USB support is unavailable in this build".into(), None))
                }
                #[cfg(feature = "bluetooth")]
                ApiTransport::Ble { address } => worker_runtime
                    .block_on(crate::transport::bluetooth::BleTransport::connect(
                        &address,
                        request.payload_limit,
                    ))
                    .map_err(|error| (error.to_string(), None))
                    .and_then(|target| execute_cancellable(&plan, target, cancel.clone())),
                #[cfg(not(feature = "bluetooth"))]
                ApiTransport::Ble { .. } => {
                    Err(("BLE support is unavailable in this build".into(), None))
                }
            };
            match execution {
                Ok(progress) => {
                    running.state = if cancel.load(Ordering::Acquire) {
                        if progress.potentially_accepted_write {
                            JobState::CancelledPartial
                        } else {
                            JobState::CancelledBeforeSend
                        }
                    } else {
                        JobState::Completed
                    };
                    running.last_completed_action =
                        progress.last_completed_action.map(|n| n as u32);
                    running.bytes_written = progress.bytes_written;
                    running.potentially_accepted_write = progress.potentially_accepted_write
                }
                Err((error, progress)) => {
                    if let Some(progress) = progress {
                        running.last_completed_action =
                            progress.last_completed_action.map(|n| n as u32);
                        running.bytes_written = progress.bytes_written;
                        running.potentially_accepted_write = progress.potentially_accepted_write;
                    }
                    running.state = if cancel.load(Ordering::Acquire) {
                        if running.potentially_accepted_write {
                            JobState::CancelledPartial
                        } else {
                            JobState::CancelledBeforeSend
                        }
                    } else if running.potentially_accepted_write {
                        JobState::OutcomeUnknown
                    } else {
                        JobState::Failed
                    };
                    running.error = Some(error)
                }
            }
            running.updated_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let _ = events.send(running.clone());
            let mut jobs = worker_state.jobs.blocking_write();
            jobs.insert(id, running);
            let _ = save_jobs(&worker_state, &jobs);
        });
        Ok(SubmitOutcome {
            status: StatusCode::ACCEPTED,
            job: accepted,
        })
    }
}
async fn get_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<JobView>, ApiError> {
    authorize(&state, &headers).await?;
    state
        .jobs
        .read()
        .await
        .get(&id)
        .cloned()
        .map(|job| Json(JobView::from(&job)))
        .ok_or(ApiError(StatusCode::NOT_FOUND, "job not found"))
}
async fn cancel_job(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<JobView>, ApiError> {
    authorize(&state, &headers).await?;
    if let Some(cancel) = state.cancellations.read().await.get(&id) {
        cancel.store(true, Ordering::Release);
    }
    let events = state.events.read().await.get(&id).cloned();
    let mut jobs = state.jobs.write().await;
    let job = jobs
        .get_mut(&id)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "job not found"))?;
    job.request_cancel();
    let job = job.clone();
    save_jobs(&state, &jobs)?;
    if let Some(events) = events {
        let _ = events.send(job.clone());
    }
    Ok(Json(JobView::from(&job)))
}
async fn job_events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    authorize(&state, &headers).await?;
    let current = state
        .jobs
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError(StatusCode::NOT_FOUND, "job not found"))?;
    let sender = state
        .events
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or(ApiError(StatusCode::NOT_FOUND, "job events not found"))?;
    let initial = tokio_stream::once(current);
    let updates = BroadcastStream::new(sender.subscribe()).filter_map(|item| item.ok());
    let stream = initial.chain(updates).map(|job| {
        Ok(Event::default()
            .event("progress")
            .json_data(JobView::from(&job))
            .expect("job serialization cannot fail"))
    });
    Ok(Sse::new(stream))
}
async fn assets(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    let catalogues = if let Some(path) = &state.config.catalogue_path {
        crate::assets::load_catalogue(path).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "asset catalogue cannot be read",
            )
        })?
    } else {
        Vec::new()
    };
    Ok(Json(
        serde_json::json!({"catalogues":catalogues,"visibility":"private-local-only"}),
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaposteRequest {
    pdf_base64: String,
    format: String,
    #[serde(default = "laposte_dpi")]
    dpi: u16,
    #[serde(default)]
    pages: Vec<u32>,
    /// Optional one-based `page:slot` selectors applied after occupancy detection.
    #[serde(default)]
    slots: Vec<String>,
}
const fn laposte_dpi() -> u16 {
    300
}
async fn laposte_extract(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<LaposteRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use base64::Engine as _;
    authorize(&state, &headers).await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(request.pdf_base64)
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "invalid PDF base64"))?;
    if bytes.len() > state.config.max_document_bytes {
        return Err(ApiError(StatusCode::PAYLOAD_TOO_LARGE, "PDF too large"));
    }
    let format = request
        .format
        .parse()
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "unknown La Poste format"))?;
    let mut stamps = crate::laposte::extract_bytes(bytes, format, request.dpi, &request.pages)
        .map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "La Poste extraction failed",
            )
        })?;
    if !request.slots.is_empty() {
        let selectors = parse_slot_selectors(&request.slots)?;
        stamps.retain(|stamp| selectors.contains(&(stamp.page, stamp.slot)));
    }
    if stamps.is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "La Poste sheet contains no occupied stamps",
        ));
    }
    Ok(Json(
        serde_json::json!({"stamps":stamps.iter().map(|stamp|serde_json::json!({"page":stamp.page,"slot":stamp.slot,"widthUm":stamp.width_um,"heightUm":stamp.height_um,"raster":{"width":stamp.raster.width,"height":stamp.raster.height,"pixelsBase64":base64::engine::general_purpose::STANDARD.encode(&stamp.raster.pixels)}})).collect::<Vec<_>>() }),
    ))
}

fn parse_slot_selectors(
    values: &[String],
) -> Result<std::collections::HashSet<(u32, u16)>, ApiError> {
    values
        .iter()
        .map(|value| {
            let (page, slot) = value.split_once(':').ok_or(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "slot selector must be page:slot",
            ))?;
            let page = page
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "slot selector must be page:slot",
                ))?;
            let slot = slot
                .parse::<u16>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "slot selector must be page:slot",
                ))?;
            Ok((page, slot))
        })
        .collect()
}

pub fn router(state: ApiState) -> Router {
    let limit = state.config.max_request_bytes;
    let allowed_origins = state.config.allowed_origins.clone();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            origin
                .to_str()
                .is_ok_and(|origin| allowed_origins.iter().any(|allowed| allowed == origin))
        }))
        .allow_methods([http::Method::GET, http::Method::POST, http::Method::OPTIONS])
        .allow_headers([
            http::header::AUTHORIZATION,
            http::header::CONTENT_TYPE,
            http::HeaderName::from_static("idempotency-key"),
        ])
        .allow_private_network(true);
    Router::new()
        .route("/v1/pair", post(pair))
        .route("/v1/grants/me", get(current_grant))
        .route("/v1/grants/me/rotate", post(rotate_current_grant))
        .route("/v1/grants/me/revoke", post(revoke_current_grant))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/printers", get(printers))
        .route("/v1/discovery", post(discovery))
        .route("/v1/connection", post(connection))
        .route("/v1/status", get(status))
        .route("/v1/documents/validate", post(validate_document))
        .route("/v1/documents/preview", post(preview_document))
        .route("/v1/documents/export", post(export_document))
        .route("/v1/jobs", post(submit_job))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/events", get(job_events))
        .route("/v1/jobs/{id}/cancel", post(cancel_job))
        .route("/v1/assets", get(assets))
        .route("/v1/laposte/extract", post(laposte_extract))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            preflight_guard,
        ))
        .with_state(state)
}

pub async fn serve(
    bind: IpAddr,
    port: u16,
    state: ApiState,
) -> Result<(), Box<dyn std::error::Error>> {
    if !bind.is_loopback() {
        return Err("API bind address must be loopback".into());
    }
    let listener = TcpListener::bind(SocketAddr::new(bind, port)).await?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Serve IPv4 and IPv6 loopback concurrently. Binding either listener must
/// succeed so callers never unknowingly lose one browser loopback path.
pub async fn serve_dual(port: u16, state: ApiState) -> Result<(), Box<dyn std::error::Error>> {
    use std::future::IntoFuture as _;
    let ipv4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let ipv6 = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await?;
    let v4 = axum::serve(ipv4, router(state.clone())).into_future();
    let v6 = axum::serve(ipv6, router(state)).into_future();
    tokio::pin!(v4);
    tokio::pin!(v6);
    tokio::select! {
        result = &mut v4 => result?,
        result = &mut v6 => result?,
        _ = tokio::signal::ctrl_c() => {},
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use tower::ServiceExt;
    fn test_state() -> ApiState {
        let dir = tempfile::tempdir().unwrap().keep();
        test_state_at(&dir)
    }
    fn test_state_at(dir: &std::path::Path) -> ApiState {
        ApiState::new(
            AuthStore::load(dir.join("g.json")).unwrap(),
            Config {
                allowed_origins: vec!["https://editor.example".into()],
                connections_path: Some(dir.join("connections.json")),
                catalogue_path: Some(dir.join("catalogues.json")),
                jobs_path: Some(dir.join("jobs.json")),
                ..Config::default()
            },
        )
    }
    async fn test_token(state: &ApiState) -> String {
        let secret = state
            .auth
            .write()
            .await
            .begin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;
        state
            .auth
            .write()
            .await
            .exchange(&secret, "https://editor.example", Duration::from_secs(300))
            .unwrap()
            .unwrap()
            .1
    }

    #[tokio::test]
    async fn persisted_ipp_connection_executes_query_and_print_job() {
        use http_body_util::BodyExt as _;
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for expected_operation in [0x000b_u16, 0x0002] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
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
                assert_eq!(u16::from_be_bytes([body[2], body[3]]), expected_operation);
                if expected_operation == 0x0002 {
                    assert!(body.windows(3).any(|bytes| bytes == [0x1b, b'i', b'a']));
                }
                let response = if expected_operation == 0x000b {
                    let media = b"om_label_62x29mm";
                    let mut body = vec![2, 0, 0, 0, 0, 0, 0, 1, 4, 0x44, 0, 11];
                    body.extend(b"media-ready");
                    body.extend((media.len() as u16).to_be_bytes());
                    body.extend(media);
                    body.push(3);
                    body
                } else {
                    vec![2, 0, 0, 0, 0, 0, 0, 1, 3]
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
            }
        });

        let state = test_state();
        let token = test_token(&state).await;
        state
            .inject_probe(
                "secure",
                ProbeResult {
                    status: "idle".into(),
                    media: Some(serde_json::json!({"widthMm":62,"lengthMm":29})),
                },
            )
            .await;
        let app = router(state.clone());
        let auth = || format!("Bearer {token}");
        let configure = Request::post("/v1/connection")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", auth())
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"id":"secure","model":"ql-1110nwb","transport":{{"kind":"ipp","uri":"ipp://127.0.0.1:{}/ipp/print"}}}}"#,
                address.port()
            )))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(configure).await.unwrap().status(),
            StatusCode::OK
        );
        let mut body: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/editor-job.json")).unwrap();
        body.as_object_mut().unwrap().remove("transport");
        body["printerId"] = serde_json::json!("ql-1110nwb");
        body["connectionId"] = serde_json::json!("secure");
        body["dpi"] = serde_json::json!(300);
        body["document"]["media"]["width"] = serde_json::json!(62);
        body["document"]["media"]["height"] = serde_json::json!(29);
        body["document"]["media"]["printableBounds"]["width"] = serde_json::json!(62);
        body["document"]["media"]["printableBounds"]["height"] = serde_json::json!(29);
        let submit = Request::post("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", auth())
            .header("content-type", "application/json")
            .header("idempotency-key", "ipp-integration")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(submit).await.unwrap();
        let response_status = response.status();
        let submitted: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(response_status, StatusCode::ACCEPTED, "{submitted}");
        let id = submitted["id"].as_str().unwrap();
        let mut finished = None;
        for _ in 0..100 {
            let get = Request::get(format!("/v1/jobs/{id}"))
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", auth())
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(get).await.unwrap();
            let job: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            if job["terminal"] == true {
                finished = Some(job);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let finished = finished.unwrap();
        assert_eq!(finished["outcome"], "completed", "{finished}");
        server.join().unwrap();
    }
    #[tokio::test]
    async fn idempotency_replays_same_persisted_job_after_restart() {
        use http_body_util::BodyExt as _;
        let directory = tempfile::tempdir().unwrap();
        let state = test_state_at(directory.path());
        let token = test_token(&state).await;
        let request = || {
            Request::post("/v1/jobs")
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .header("idempotency-key", "restart-safe")
                .body(Body::from(include_str!(
                    "../tests/fixtures/editor-job.json"
                )))
                .unwrap()
        };
        let response = router(state).oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let first: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let restarted = test_state_at(directory.path());
        let response = router(restarted).oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let replay: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(first["id"], replay["id"]);
    }

    #[tokio::test]
    async fn saved_connection_execution_is_serialized() {
        let state = test_state();
        let capture = tempfile::NamedTempFile::new().unwrap();
        state.connections.write().await.insert(
            "cloud-capture".into(),
            Connection {
                id: "cloud-capture".into(),
                model: "m110".into(),
                transport: serde_json::json!({"kind":"file","path":capture.path()}),
                status: "ready".into(),
                media: None,
            },
        );
        let lock = Arc::new(std::sync::Mutex::new(()));
        state
            .connection_executions
            .write()
            .await
            .insert("cloud-capture".into(), lock.clone());
        let (acquired_sender, acquired_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            acquired_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
        });
        acquired_receiver.recv().unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/editor-job.json")).unwrap();
        let request = crate::cloud::store::CloudPrintRequest {
            document: fixture["document"].clone(),
            model: "m110".into(),
            dpi: None,
            rotation: 0,
            fit: false,
            density: 6,
            copies: 1,
            payload_limit: 512,
        };
        let id = Uuid::new_v4();
        state
            .submit_cloud_job(id, "cloud-capture", &request, "digest")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(state.cloud_job(id).await.unwrap().state, JobState::Queued);
        release_sender.send(()).unwrap();
        blocker.join().unwrap();
        for _ in 0..500 {
            if state.cloud_job(id).await.is_some_and(|job| job.terminal()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("serialized capture did not finish");
    }
    #[tokio::test]
    async fn rejects_non_loopback_host_before_pairing() {
        let app = router(test_state());
        let request = Request::post("/v1/pair")
            .header("host", "evil.example")
            .header("origin", "https://editor.example")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"secret":"x"}"#))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::MISDIRECTED_REQUEST
        );
    }
    #[tokio::test]
    async fn all_non_pairing_routes_require_auth() {
        let app = router(test_state());
        let request = Request::get("/v1/capabilities")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cors_and_private_network_preflight_are_origin_scoped() {
        let app = router(test_state());
        let allowed = Request::options("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,idempotency-key",
            )
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(allowed).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://editor.example"
        );
        assert_eq!(
            response.headers()["access-control-allow-private-network"],
            "true"
        );
        assert!(
            response.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("idempotency-key")
        );
        assert!(
            response.headers()["vary"]
                .to_str()
                .unwrap()
                .contains("origin")
        );

        let rejected = Request::options("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(rejected).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn pairing_issues_an_origin_bound_token_accepted_by_api() {
        let state = test_state();
        let secret = state
            .auth
            .write()
            .await
            .begin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;
        state
            .inject_probe(
                "desk",
                ProbeResult {
                    status: "ready".into(),
                    media: Some(serde_json::json!({"widthMm": 62, "lengthMm": 29})),
                },
            )
            .await;
        state
            .inject_devices(vec![crate::transport::NativeDevice {
                transport: "serial".into(),
                address: "/dev/mock-m110".into(),
                name: Some("Brother m110 fixture".into()),
                vendor_id: None,
                product_id: None,
                serial_number: None,
                ieee1284_device_id: None,
            }])
            .await;
        let app = router(state.clone());
        let request = Request::post("/v1/pair")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        use http_body_util::BodyExt;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let pair = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        assert!(pair["expiresAt"].as_str().is_some());
        let token = pair["token"].as_str().unwrap().to_owned();
        let authorized = Request::get("/v1/capabilities")
            .header("host", "127.0.0.1:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(authorized).await.unwrap().status(),
            StatusCode::OK
        );
        let request = Request::get("/v1/printers")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let injected = listing["printers"]["discovered"]
            .as_array()
            .unwrap()
            .iter()
            .find(|printer| printer["device"]["address"] == "/dev/mock-m110")
            .expect("injected M110 fixture should be discovered");
        assert_eq!(injected["matchedModel"], "m110");
        for (format, content_type, magic) in [
            ("png", "image/png", b"\x89PNG".as_slice()),
            ("pdf", "application/pdf", b"%PDF".as_slice()),
        ] {
            let export = Request::post(format!("/v1/documents/export?format={format}"))
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(include_str!(
                    "../tests/fixtures/canonical-document.json"
                )))
                .unwrap();
            let response = app.clone().oneshot(export).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[http::header::CONTENT_TYPE], content_type);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert!(bytes.starts_with(magic));
        }
        let missing_format = Request::post("/v1/documents/export")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(include_str!(
                "../tests/fixtures/canonical-document.json"
            )))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(missing_format).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let request = Request::post("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(include_str!(
                "../tests/fixtures/editor-job.json"
            )))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(job["state"], "queued");
        assert_eq!(job["terminal"], false);
        assert!(job["actions"].as_u64().unwrap() > 5);
        let id = job["id"].as_str().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let request = Request::get(format!("/v1/jobs/{id}"))
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let finished: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(finished["outcome"], "completed");
        assert_eq!(finished["terminal"], true);
        assert!(finished["bytesSent"].as_u64().unwrap() > 0);

        let idempotent_body = include_str!("../tests/fixtures/editor-job.json");
        let submit = || {
            Request::post("/v1/jobs")
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .header("idempotency-key", "editor-submit-42")
                .body(Body::from(idempotent_body))
                .unwrap()
        };
        let first = app.clone().oneshot(submit()).await.unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let first: serde_json::Value =
            serde_json::from_slice(&first.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let replay = app.clone().oneshot(submit()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        let replay: serde_json::Value =
            serde_json::from_slice(&replay.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(first["id"], replay["id"]);
        let mut conflicting: serde_json::Value = serde_json::from_str(idempotent_body).unwrap();
        conflicting["density"] = serde_json::json!(2);
        let conflict = Request::post("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("idempotency-key", "editor-submit-42")
            .body(Body::from(conflicting.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(conflict).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        let request = Request::post("/v1/connection")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"desk","model":"m110","transport":{"kind":"tcp","address":"printer.local:9100"}}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        assert!(state.config.connections_path.as_ref().unwrap().exists());
        let request = Request::get("/v1/status?connection=desk")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["connection"]["id"], "desk");
        assert_eq!(status["status"], "ready");
        assert_eq!(status["media"]["widthMm"], 62);

        let mut missing_route: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/editor-job.json")).unwrap();
        missing_route.as_object_mut().unwrap().remove("transport");
        let request = Request::post("/v1/jobs")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(missing_route.to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );

        #[cfg(not(feature = "usb"))]
        {
            let mut unavailable: serde_json::Value =
                serde_json::from_str(include_str!("../tests/fixtures/editor-job.json")).unwrap();
            unavailable["transport"] =
                serde_json::json!({"kind":"usb","vid":1,"pid":2,"interface":0,"out":1});
            let request = Request::post("/v1/jobs")
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(unavailable.to_string()))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }

        let rotate = Request::post("/v1/grants/me/rotate")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"expiresSeconds":120}"#))
            .unwrap();
        let response = app.clone().oneshot(rotate).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let rotated: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let replacement = rotated["token"].as_str().unwrap();
        let old = Request::get("/v1/grants/me")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(old).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let revoke = Request::post("/v1/grants/me/revoke")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {replacement}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(revoke).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
    }
    #[test]
    fn cancellation_at_first_write_uses_conservative_ambiguous_outcome() {
        use mb_printer_core::protocol::{Action, Boundary, Plan};
        let cancel = Arc::new(AtomicBool::new(true));
        let mut target = Cancellable {
            inner: CaptureTransport::new(20),
            cancel,
        };
        let plan = Plan {
            protocol: mb_printer_core::capabilities::Protocol::MSeries,
            source_commit: "test".into(),
            actions: vec![
                Action::JobBoundary {
                    kind: Boundary::Start,
                },
                Action::CommandWrite {
                    name: "first".into(),
                    bytes: vec![1],
                    atomic: true,
                },
            ],
        };
        let error = mb_printer_native::execute(&plan, &mut target).unwrap_err();
        let progress = error_progress(&error).unwrap();
        assert!(progress.potentially_accepted_write);
        assert_eq!(progress.bytes_written, 0);
    }
    #[test]
    fn first_write_error_is_reported_as_potentially_accepted() {
        struct FailFirstWrite;
        impl mb_printer_native::Transport for FailFirstWrite {
            fn payload_limit(&self) -> usize {
                128
            }
            fn subscribe_notifications(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn write(&mut self, _: &[u8]) -> Result<(), String> {
                Err("disconnect".into())
            }
            fn delay_monotonic(&mut self, _: u64) {}
            fn wait_response(&mut self, _: u64) -> Result<mb_printer_native::WaitOutcome, String> {
                Ok(mb_printer_native::WaitOutcome::Unavailable)
            }
        }
        use mb_printer_core::protocol::{Action, Plan};
        let plan = Plan {
            protocol: mb_printer_core::capabilities::Protocol::MSeries,
            source_commit: "test".into(),
            actions: vec![Action::CommandWrite {
                name: "first".into(),
                bytes: vec![1],
                atomic: true,
            }],
        };
        let error = execute_cancellable(&plan, FailFirstWrite, Arc::new(AtomicBool::new(false)))
            .unwrap_err();
        let progress = error.1.unwrap();
        assert!(progress.potentially_accepted_write);
        assert_eq!(progress.bytes_written, 0);
    }
    #[test]
    fn api_rotation_and_fit_are_explicit_and_preflight_head_width() {
        let mut document =
            Document::from_json(include_str!("../tests/fixtures/canonical-document.json")).unwrap();
        let printer = capabilities::by_id("m110").unwrap();
        let normal = api_render_for_printer(&document, &printer, 203, 0, false, None).unwrap();
        let rotated = api_render_for_printer(&document, &printer, 203, 90, false, None).unwrap();
        assert_ne!(normal.height, rotated.height);
        document.media.width = 100_000;
        assert!(api_render_for_printer(&document, &printer, 203, 0, false, None).is_err());
        let fitted = api_render_for_printer(&document, &printer, 203, 0, true, None).unwrap();
        assert_eq!(
            u32::from(fitted.width_bytes) * 8,
            printer.width_px().unwrap()
        );
        assert!(api_render_for_printer(&document, &printer, 203, 45, true, None).is_err());
        let media_fitted =
            api_render_for_printer(&document, &printer, 203, 0, true, Some((384, 200))).unwrap();
        assert_eq!(media_fitted.height, 200);
    }
    #[test]
    fn laposte_slot_selectors_are_one_based_and_stable() {
        let selectors = parse_slot_selectors(&["1:4".into(), "3:12".into()]).unwrap();
        assert!(selectors.contains(&(1, 4)));
        assert!(selectors.contains(&(3, 12)));
        assert!(parse_slot_selectors(&["0:1".into()]).is_err());
        assert!(parse_slot_selectors(&["1".into()]).is_err());
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn selected_brother_tcp_status_reports_live_media() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 3];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"\x1biS");
            let mut reply = [0_u8; 32];
            reply[..3].copy_from_slice(&[0x80, 0x20, 0x42]);
            reply[10] = 62;
            reply[11] = 0x0b;
            reply[17] = 29;
            stream.write_all(&reply).unwrap();
        });
        let connection = Connection {
            id: "brother".into(),
            model: "ql-1110nwb".into(),
            transport: serde_json::json!({"kind":"tcp","address":address.to_string(),"statusMode":"raster"}),
            status: "ready".into(),
            media: None,
        };
        let result = printer_status_async(&connection).await.unwrap();
        server.join().unwrap();
        assert_eq!(result.media.unwrap()["widthMm"], 62);
    }
    #[test]
    fn native_file_probe_opens_the_backend() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.bin");
        let result = probe_transport(&serde_json::json!({"kind":"file","path":path})).unwrap();
        assert_eq!(result.status, "ready");
        assert!(path.exists());
    }
    #[test]
    fn persisted_job_boundary_keeps_actual_most_recent_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let jobs_path = directory.path().join("jobs.json");
        let config = Config {
            max_recent_jobs: 2,
            jobs_path: Some(jobs_path.clone()),
            ..Config::default()
        };
        let state = ApiState::new(
            AuthStore::load(directory.path().join("grants.json")).unwrap(),
            config.clone(),
        );
        let mut jobs = HashMap::new();
        for updated in [30u128, 10, 20] {
            let mut job = Job::new();
            job.state = JobState::Completed;
            job.created_at_ms = updated;
            job.updated_at_ms = updated;
            jobs.insert(job.id, job);
        }
        save_jobs(&state, &jobs).unwrap();
        let persisted: Vec<Job> =
            serde_json::from_slice(&std::fs::read(&jobs_path).unwrap()).unwrap();
        assert_eq!(
            persisted
                .iter()
                .map(|job| job.updated_at_ms)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
        let restarted = ApiState::new(
            AuthStore::load(directory.path().join("grants.json")).unwrap(),
            config,
        );
        let loaded = restarted.jobs.blocking_read();
        let mut times = loaded
            .values()
            .map(|job| job.updated_at_ms)
            .collect::<Vec<_>>();
        times.sort();
        assert_eq!(times, vec![20, 30]);
    }
}
