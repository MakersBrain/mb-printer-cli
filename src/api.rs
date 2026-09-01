// SPDX-License-Identifier: AGPL-3.0-or-later
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(test)]
use std::collections::HashSet;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, broadcast},
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
};
use uuid::Uuid;

use crate::{
    VERSION,
    auth::{AuthStore, loopback_host},
    config::Config,
    jobs::Job,
    printers::{Printer, PrinterEndpoint, PrinterStore, PrinterTransport},
    raster,
    transport::{SerialTransport, TcpTransport},
};
use mb_printer_core::{Document, capabilities};

mod jobs;

#[cfg(test)]
use crate::jobs::JobState;
#[cfg(test)]
use crate::transport::CaptureTransport;
#[cfg(test)]
use jobs::{
    Cancellable, api_render_for_printer, error_progress, execute_cancellable,
    local_job_execution_span,
};
use jobs::{JobExecutor, cancel_job, get_job, job_events, load_jobs, save_jobs, submit_job};

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
    #[cfg(test)]
    injected_wireless_statuses: Arc<RwLock<HashMap<String, crate::printer_ops::WirelessStatus>>>,
    #[cfg(test)]
    injected_wireless_scans:
        Arc<RwLock<HashMap<String, Vec<mb_printer_core::protocol::brother::wifi::AccessPoint>>>>,
    #[cfg(test)]
    injected_system_reports:
        Arc<RwLock<HashMap<String, mb_printer_core::protocol::brother::report::SystemReport>>>,
    #[cfg(test)]
    injected_wireless_configurations: Arc<RwLock<HashSet<(String, String)>>>,
}
impl ApiState {
    pub fn new(auth: AuthStore, config: Config) -> Self {
        let connections = config
            .printers_path
            .as_deref()
            .and_then(|path| PrinterStore::load(path).ok())
            .unwrap_or_default()
            .printers
            .into_iter()
            .filter_map(|printer| {
                let transport =
                    serde_json::to_value(&printer.preferred_endpoint().ok()?.transport).ok()?;
                let connection = Connection {
                    id: printer.id.clone(),
                    model: printer.model,
                    transport,
                    status: printer.status.unwrap_or_else(|| "unknown".into()),
                    media: printer.media,
                };
                Some((connection.id.clone(), connection))
            })
            .collect();
        let jobs = load_jobs(&config);
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
            #[cfg(test)]
            injected_wireless_statuses: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            injected_wireless_scans: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            injected_system_reports: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(test)]
            injected_wireless_configurations: Arc::new(RwLock::new(HashSet::new())),
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

    #[cfg(test)]
    async fn inject_brother_reads(
        &self,
        id: &str,
        status: crate::printer_ops::WirelessStatus,
        scan: Vec<mb_printer_core::protocol::brother::wifi::AccessPoint>,
        report: mb_printer_core::protocol::brother::report::SystemReport,
    ) {
        self.injected_wireless_statuses
            .write()
            .await
            .insert(id.into(), status);
        self.injected_wireless_scans
            .write()
            .await
            .insert(id.into(), scan);
        self.injected_system_reports
            .write()
            .await
            .insert(id.into(), report);
    }

    #[cfg(test)]
    async fn inject_brother_wireless_configuration(
        &self,
        connection: &str,
        settings: &mb_printer_core::protocol::brother::wifi::WirelessSettings,
    ) {
        self.injected_wireless_configurations
            .write()
            .await
            .insert((connection.into(), wireless_settings_fingerprint(settings)));
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
    let Some(path) = &state.config.printers_path else {
        return Ok(());
    };
    let previous = PrinterStore::load(path).unwrap_or_default();
    let mut printers = connections
        .values()
        .filter_map(|connection| {
            let transport =
                serde_json::from_value::<PrinterTransport>(connection.transport.clone()).ok()?;
            let previous_printer = previous.find(&connection.id);
            let endpoints = previous_printer
                .map(|printer| printer.endpoints.clone())
                .unwrap_or_else(|| {
                    vec![PrinterEndpoint {
                        id: Uuid::new_v4().to_string(),
                        preferred: true,
                        transport,
                    }]
                });
            Some(Printer {
                id: connection.id.clone(),
                name: previous_printer
                    .map(|printer| printer.name.clone())
                    .unwrap_or_else(|| connection.id.clone()),
                model: connection.model.clone(),
                endpoints,
                settings: previous_printer
                    .map(|printer| printer.settings.clone())
                    .unwrap_or_default(),
                description: previous_printer.and_then(|printer| printer.description.clone()),
                status: Some(connection.status.clone()),
                media: connection.media.clone(),
            })
        })
        .collect::<Vec<_>>();
    printers.sort_by(|left, right| left.name.cmp(&right.name));
    let default_printer = previous.default_printer.filter(|selector| {
        printers
            .iter()
            .any(|printer| printer.id == *selector || printer.name.eq_ignore_ascii_case(selector))
    });
    PrinterStore {
        schema_version: crate::printers::PRINTER_STORE_SCHEMA,
        default_printer,
        printers,
    }
    .save(path)
    .map_err(|_| {
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
        let mut response = (self.0, Json(serde_json::json!({"error": self.1}))).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

/// Return sensitive, device-derived JSON without allowing browsers or
/// intermediaries to retain it. Brother administration routes should use the
/// same helper when they are added.
fn no_store_json<T: Serialize>(value: T) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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

/// State-changing Brother administration is deliberately separate from the
/// ordinary, long-lived print grant. An administrator token is short-lived,
/// origin-bound, and is only issued after local confirmation.
async fn authorize_admin(state: &ApiState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    validate_host(headers)?;
    let o = origin(headers)?;
    let token = bearer(headers)?;
    if let Some(grant) = state.auth.read().await.authenticate_admin(token, o) {
        Ok(grant.id)
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "administrator grant required",
        ))
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
    let mut auth = state.auth.write().await;
    // Pairing secrets are created by a separate `mb-printer service pair`
    // process. Refresh while holding the service lock so the exchange and
    // one-time consumption operate on one durable-store generation.
    auth.reload().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "grant persistence failed",
        )
    })?;
    let (grant_id, token) = auth
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

/// Exchanges a locally-created administrator pairing secret. This must remain
/// separate from `/v1/pair`: a normal print secret can never mint a token that
/// is allowed to alter printer state.
async fn pair_admin(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> Result<Response, ApiError> {
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
    require_brother_wifi_configuration_pairing(&state)?;
    let mut auth = state.auth.write().await;
    // See `/v1/pair`: the administrator pairing secret is written by the
    // local CLI after this long-running service may already have started.
    auth.reload().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "grant persistence failed",
        )
    })?;
    let (grant_id, token) = auth
        .exchange_admin(&request.secret, o, crate::auth::ADMIN_GRANT_MAX_TTL)
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "grant persistence failed",
            )
        })?
        .ok_or(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired administrator pairing secret",
        ))?;
    Ok(no_store_json(PairResponse {
        grant_id,
        token,
        expires_at: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
    }))
}

async fn capabilities(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    authorize(&state, &headers).await?;
    Ok(Json(
        serde_json::json!({"service":"mb-printer","version":VERSION,"api":"v1","features":["documents","preview-png","jobs","job-idempotency","job-fit","continuous-options","native-document-batch","self-service-grants","dual-stack-loopback","assets","laposte","file-transport","tcp-transport","serial-transport","ipp-transport","ipps-transport"],"max_document_bytes":state.config.max_document_bytes,"printer_definition_count":capabilities::bundled().len()}),
    ))
}
async fn printers(State(state): State<ApiState>, headers: HeaderMap) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let devices = discovered_devices(&state).await;
    let wifi_configuration_enabled = state.config.enable_brother_wifi_configuration;
    let discovered = devices
        .into_iter()
        .map(|device| discovered_printer_json(device, wifi_configuration_enabled))
        .collect::<Vec<_>>();
    let configured = state
        .connections
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    Ok(no_store_json(
        serde_json::json!({"printers":{"discovered":discovered,"configured":configured.iter().map(|connection| connection_json(connection, wifi_configuration_enabled)).collect::<Vec<_>>()},"definitions":capabilities::bundled()}),
    ))
}
async fn discovery(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let devices = discovered_devices(&state).await;
    let wifi_configuration_enabled = state.config.enable_brother_wifi_configuration;
    Ok(no_store_json(
        serde_json::json!({"devices":devices.into_iter().map(|device| discovery_device_json(device, wifi_configuration_enabled)).collect::<Vec<_>>(),"supportedTransports":["file","tcp","serial","usb","ble","rfcomm","ipp"]}),
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
) -> Result<Response, ApiError> {
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
    Ok(no_store_json(connection_json(
        &configured,
        state.config.enable_brother_wifi_configuration,
    )))
}

/// Return the operations that are safe to advertise for a concrete endpoint.
///
/// The persisted connection is intentionally not changed: operations depend on
/// the *current transport*, while saved connection files remain portable across
/// older CLI versions and across USB/network use. In particular, Brother
/// administration routes only accept locally attached USB devices.
fn connection_operations(connection: &Connection, wifi_configuration_enabled: bool) -> Vec<String> {
    let usb = connection.transport["kind"] == "usb";
    let mut operations: Vec<String> = capabilities::by_id(&connection.model)
        .map(|definition| {
            definition
                .operations
                .into_iter()
                // Model data describes the protocol operation, whereas the
                // API response must also respect this concrete endpoint.
                // Brother administration never leaves the local USB path.
                .filter(|operation| {
                    usb || !matches!(
                        operation,
                        mb_printer_core::capabilities::PrinterOperation::SystemReport
                            | mb_printer_core::capabilities::PrinterOperation::WifiStatus
                            | mb_printer_core::capabilities::PrinterOperation::WifiScan
                            | mb_printer_core::capabilities::PrinterOperation::WifiConfigure
                    )
                })
                .filter(|operation| {
                    wifi_configuration_enabled
                        || !matches!(
                            operation,
                            mb_printer_core::capabilities::PrinterOperation::WifiConfigure
                        )
                })
                .filter_map(|operation| serde_json::to_value(operation).ok())
                .filter_map(|operation| operation.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    operations.sort();
    operations.dedup();
    operations
}

fn connection_json(connection: &Connection, wifi_configuration_enabled: bool) -> serde_json::Value {
    serde_json::json!({
        "id": connection.id,
        "model": connection.model,
        "transport": connection.transport,
        "status": connection.status,
        "media": connection.media,
        "operations": connection_operations(connection, wifi_configuration_enabled),
    })
}

fn matched_model(device: &crate::transport::NativeDevice) -> Option<String> {
    let haystack = format!(
        "{} {} {}",
        device.name.as_deref().unwrap_or(""),
        device.address,
        device.ieee1284_device_id.as_deref().unwrap_or(""),
    )
    .to_ascii_lowercase();
    capabilities::bundled()
        .into_iter()
        .find(|definition| {
            haystack.contains(&definition.id.to_ascii_lowercase())
                || haystack.contains(&definition.name.to_ascii_lowercase())
        })
        .map(|definition| definition.id)
}

fn discovery_device_json(
    device: crate::transport::NativeDevice,
    wifi_configuration_enabled: bool,
) -> serde_json::Value {
    let model = matched_model(&device);
    let connection = Connection {
        id: String::new(),
        model: model.clone().unwrap_or_default(),
        transport: serde_json::json!({"kind": device.transport.clone()}),
        status: String::new(),
        media: None,
    };
    let mut value = serde_json::to_value(device).expect("native discovery device serializes");
    let object = value
        .as_object_mut()
        .expect("native discovery device serializes to object");
    object.insert(
        "matchedModel".into(),
        model.map_or(serde_json::Value::Null, serde_json::Value::String),
    );
    object.insert(
        "operations".into(),
        serde_json::to_value(connection_operations(
            &connection,
            wifi_configuration_enabled,
        ))
        .expect("operations serialize"),
    );
    value
}

fn discovered_printer_json(
    device: crate::transport::NativeDevice,
    wifi_configuration_enabled: bool,
) -> serde_json::Value {
    let model = matched_model(&device);
    let connection = Connection {
        id: String::new(),
        model: model.clone().unwrap_or_default(),
        transport: serde_json::json!({"kind": device.transport.clone()}),
        status: String::new(),
        media: None,
    };
    serde_json::json!({
        "source": "discovery",
        "device": device,
        "matchedModel": model,
        "operations": connection_operations(&connection, wifi_configuration_enabled),
    })
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
        #[cfg(all(feature = "bluetooth-linux", target_os = "linux"))]
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
        #[cfg(not(all(feature = "bluetooth-linux", target_os = "linux")))]
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
        "usb" => {
            if let Some(device) = transport["device"].as_str() {
                crate::printer_ops::usb_brother_status(Some(device))
                    .map(|_| ())
                    .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB probe failed"))
            } else {
                crate::transport::usb::UsbTransport::open(
                    transport["vid"].as_u64().unwrap_or(0) as u16,
                    transport["pid"].as_u64().unwrap_or(0) as u16,
                    transport["interface"].as_u64().unwrap_or(0) as u8,
                    transport["out"].as_u64().unwrap_or(0) as u8,
                    transport["input"].as_u64().map(|value| value as u8),
                    128,
                )
                .map(|_| ())
                .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB probe failed"))
            }
        }
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
    let report = crate::discovery::discover(crate::discovery::DiscoveryOptions::default()).await;
    devices.extend(
        report
            .candidates
            .into_iter()
            .map(|candidate| candidate.device),
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
    let status = crate::printer_ops::parse_brother_status(&bytes)
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "invalid Brother status"))?;
    Ok(brother_status_probe(status))
}

fn brother_status_probe(
    status: mb_printer_core::protocol::brother::status::BrotherStatus,
) -> ProbeResult {
    ProbeResult {
        status: if status.errors.is_empty() {
            status.phase.into()
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
    }
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
            if let Some(device) = transport["device"].as_str() {
                return crate::printer_ops::usb_brother_status(Some(device))
                    .map(brother_status_probe)
                    .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "USB status failed"));
            }
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
        #[cfg(all(feature = "bluetooth-linux", target_os = "linux"))]
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
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let connections = state.connections.read().await;
    let selected = query
        .connection
        .as_ref()
        .and_then(|id| connections.get(id))
        .cloned();
    let configured = connections.values().cloned().collect::<Vec<_>>();
    drop(connections);
    Ok(no_store_json(match selected {
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
            serde_json::json!({"connection":connection_json(&value, state.config.enable_brother_wifi_configuration),"connected":connected,"status":status,"media":media})
        }
        None => {
            serde_json::json!({"connections":configured.iter().map(|connection| connection_json(connection, state.config.enable_brother_wifi_configuration)).collect::<Vec<_>>(),"connected":false,"status":"not-connected","media":null})
        }
    }))
}

#[derive(Debug, Clone, Copy)]
enum BrotherReadOperation {
    Wireless,
    Report,
}

async fn brother_wireless_status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(connection): AxumPath<String>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let (_, selector) =
        brother_usb_connection_async(&state, &connection, BrotherReadOperation::Wireless).await?;
    #[cfg(test)]
    if let Some(status) = state
        .injected_wireless_statuses
        .read()
        .await
        .get(&connection)
        .cloned()
    {
        return Ok(no_store_json(status));
    }
    #[cfg(feature = "usb")]
    {
        let status = tokio::task::spawn_blocking(move || {
            crate::printer_ops::usb_wireless_status(Some(&selector))
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "wireless status task crashed"))?
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "wireless status failed"))?;
        Ok(no_store_json(status))
    }
    #[cfg(not(feature = "usb"))]
    {
        let _ = selector;
        Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "USB support is unavailable in this build",
        ))
    }
}

#[cfg(any(feature = "usb", test))]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrotherWirelessScanResponse {
    access_points: Vec<mb_printer_core::protocol::brother::wifi::AccessPoint>,
}

async fn brother_wireless_scan(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(connection): AxumPath<String>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let (_, selector) =
        brother_usb_connection_async(&state, &connection, BrotherReadOperation::Wireless).await?;
    #[cfg(test)]
    if let Some(access_points) = state
        .injected_wireless_scans
        .read()
        .await
        .get(&connection)
        .cloned()
    {
        return Ok(no_store_json(BrotherWirelessScanResponse { access_points }));
    }
    #[cfg(feature = "usb")]
    {
        let access_points = tokio::task::spawn_blocking(move || {
            crate::printer_ops::usb_wireless_scan(Some(&selector))
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "wireless scan task crashed"))?
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "wireless scan failed"))?;
        Ok(no_store_json(BrotherWirelessScanResponse { access_points }))
    }
    #[cfg(not(feature = "usb"))]
    {
        let _ = selector;
        Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "USB support is unavailable in this build",
        ))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrotherWirelessSettingsRequest {
    ssid: String,
    password: String,
    encryption: mb_printer_core::protocol::brother::wifi::WirelessEncryption,
    authentication: mb_printer_core::protocol::brother::wifi::WirelessAuthentication,
    #[serde(default = "default_true")]
    infrastructure: bool,
    #[serde(default)]
    wireless_direct: bool,
    #[serde(default = "default_true")]
    reboot: bool,
}

const fn default_true() -> bool {
    true
}

impl BrotherWirelessSettingsRequest {
    fn settings(&self) -> mb_printer_core::protocol::brother::wifi::WirelessSettings {
        mb_printer_core::protocol::brother::wifi::WirelessSettings {
            ssid: self.ssid.clone(),
            password: self.password.clone(),
            encryption: self.encryption,
            authentication: self.authentication,
            infrastructure: self.infrastructure,
            wireless_direct: self.wireless_direct,
            reboot: self.reboot,
        }
    }

    fn validate(
        &self,
    ) -> Result<mb_printer_core::protocol::brother::wifi::WirelessSettings, ApiError> {
        let settings = self.settings();
        settings.command().map_err(|_| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid wireless settings",
            )
        })?;
        Ok(settings)
    }
}

fn wireless_settings_fingerprint(
    settings: &mb_printer_core::protocol::brother::wifi::WirelessSettings,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mb-printer-wifi-approval-v1\0");
    for part in [
        settings.ssid.as_bytes(),
        settings.password.as_bytes(),
        &[settings.encryption.code()],
        &[settings.authentication.code()],
        &[u8::from(settings.infrastructure)],
        &[u8::from(settings.wireless_direct)],
        &[u8::from(settings.reboot)],
    ] {
        hash.update((part.len() as u32).to_be_bytes());
        hash.update(part);
    }
    URL_SAFE_NO_PAD.encode(hash.finalize())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrotherWirelessPrepareRequest {
    #[serde(flatten)]
    settings: BrotherWirelessSettingsRequest,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrotherWirelessPrepareResponse {
    approval_id: Uuid,
    expires_at: u64,
    connection: String,
    device: String,
    ssid: String,
    encryption: mb_printer_core::protocol::brother::wifi::WirelessEncryption,
    authentication: mb_printer_core::protocol::brother::wifi::WirelessAuthentication,
    infrastructure: bool,
    wireless_direct: bool,
    reboot: bool,
    recovery: &'static str,
}

/// Returns a non-secret review of a pending wireless change and records an
/// opaque, short-lived manifest for local approval. Raw settings and the
/// password are never persisted in the approval store.
async fn brother_wireless_prepare(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(connection): AxumPath<String>,
    Json(request): Json<BrotherWirelessPrepareRequest>,
) -> Result<Response, ApiError> {
    let grant_id = authorize_admin(&state, &headers).await?;
    require_brother_wifi_configuration(&state)?;
    let request_origin = origin(&headers)?;
    let (_, selector) =
        brother_usb_connection_async(&state, &connection, BrotherReadOperation::Wireless).await?;
    let settings = request.settings.validate()?;
    let fingerprint = wireless_settings_fingerprint(&settings);
    let approval = state
        .auth
        .write()
        .await
        .prepare_wifi_approval(grant_id, request_origin, &connection, &fingerprint)
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "approval persistence failed",
            )
        })?
        .ok_or(ApiError(
            StatusCode::UNAUTHORIZED,
            "administrator grant required",
        ))?;
    Ok(no_store_json(BrotherWirelessPrepareResponse {
        approval_id: approval.id,
        expires_at: approval.expires_at,
        connection,
        device: selector,
        ssid: settings.ssid,
        encryption: settings.encryption,
        authentication: settings.authentication,
        infrastructure: settings.infrastructure,
        wireless_direct: settings.wireless_direct,
        reboot: settings.reboot,
        recovery: "Keep the printer connected over USB or Bluetooth while changing wireless settings.",
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrotherWirelessConfigureRequest {
    approval_id: Uuid,
    #[serde(flatten)]
    settings: BrotherWirelessSettingsRequest,
}

#[cfg(any(feature = "usb", test))]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrotherWirelessConfigureResponse {
    connection: String,
    device: String,
    applied: bool,
    reboot: bool,
}

/// Applies an exact, locally approved mutation once. The one-time approval
/// binds the originating administrator grant, browser origin, USB connection,
/// and a hash of all settings including the password.
async fn brother_wireless_configure(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(connection): AxumPath<String>,
    Json(request): Json<BrotherWirelessConfigureRequest>,
) -> Result<Response, ApiError> {
    let grant_id = authorize_admin(&state, &headers).await?;
    require_brother_wifi_configuration(&state)?;
    let request_origin = origin(&headers)?;
    let (_, selector) =
        brother_usb_connection_async(&state, &connection, BrotherReadOperation::Wireless).await?;
    let settings = request.settings.validate()?;
    let fingerprint = wireless_settings_fingerprint(&settings);
    let mut auth = state.auth.write().await;
    // Local approval is recorded by a separate CLI process. Reload the full
    // store before checking the admin grant and consuming the one-time
    // approval, so the approval cannot be missed or replayed from stale RAM.
    auth.reload().map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "approval persistence failed",
        )
    })?;
    let approved = auth
        .consume_wifi_approval(
            request.approval_id,
            grant_id,
            request_origin,
            &connection,
            &fingerprint,
        )
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "approval persistence failed",
            )
        })?;
    if !approved {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "local wireless approval is missing, expired, or already used",
        ));
    }
    #[cfg(test)]
    if state
        .injected_wireless_configurations
        .read()
        .await
        .contains(&(connection.clone(), fingerprint))
    {
        return Ok(no_store_json(BrotherWirelessConfigureResponse {
            connection,
            device: selector,
            applied: true,
            reboot: settings.reboot,
        }));
    }
    #[cfg(feature = "usb")]
    {
        let reboot = settings.reboot;
        let configure_selector = selector.clone();
        tokio::task::spawn_blocking(move || {
            crate::printer_ops::usb_wireless_configure(&configure_selector, &settings)
        })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "wireless configuration task crashed",
            )
        })?
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "wireless configuration failed"))?;
        Ok(no_store_json(BrotherWirelessConfigureResponse {
            connection,
            device: selector,
            applied: true,
            reboot,
        }))
    }
    #[cfg(not(feature = "usb"))]
    {
        let _ = (selector, settings);
        Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "USB support is unavailable in this build",
        ))
    }
}

/// Wi-Fi credentials are a state-changing operation. The API stays present so
/// clients can keep a stable contract, but it is unavailable until the local
/// operator explicitly opts in through the service configuration.
fn require_brother_wifi_configuration(state: &ApiState) -> Result<(), ApiError> {
    if state.config.enable_brother_wifi_configuration {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "Brother Wi-Fi configuration is disabled; set enable_brother_wifi_configuration to true locally",
        ))
    }
}

fn require_brother_wifi_configuration_pairing(state: &ApiState) -> Result<(), ApiError> {
    if state.config.enable_brother_wifi_configuration_pairing {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "Brother Wi-Fi administrator pairing is disabled; set enable_brother_wifi_configuration_pairing to true locally",
        ))
    }
}

async fn brother_system_report(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(connection): AxumPath<String>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers).await?;
    let (_, selector) =
        brother_usb_connection_async(&state, &connection, BrotherReadOperation::Report).await?;
    #[cfg(test)]
    if let Some(report) = state
        .injected_system_reports
        .read()
        .await
        .get(&connection)
        .cloned()
    {
        return Ok(no_store_json(report.redacted()));
    }
    #[cfg(feature = "usb")]
    {
        let report = tokio::task::spawn_blocking(move || {
            crate::printer_ops::usb_system_report(Some(&selector), true)
        })
        .await
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "system report task crashed"))?
        .map_err(|_| ApiError(StatusCode::BAD_GATEWAY, "system report failed"))?;
        Ok(no_store_json(report))
    }
    #[cfg(not(feature = "usb"))]
    {
        let _ = selector;
        Err(ApiError(
            StatusCode::NOT_IMPLEMENTED,
            "USB support is unavailable in this build",
        ))
    }
}

async fn brother_usb_connection_async(
    state: &ApiState,
    connection: &str,
    operation: BrotherReadOperation,
) -> Result<(Connection, String), ApiError> {
    let connections = state.connections.read().await;
    let connection = connections.get(connection).cloned().ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "printer connection not found",
    ))?;
    drop(connections);
    let supported = match operation {
        BrotherReadOperation::Wireless => {
            matches!(connection.model.as_str(), "ql-1110nwb" | "ql-1115nwb")
        }
        BrotherReadOperation::Report => matches!(
            connection.model.as_str(),
            "ql-1100" | "ql-1110nwb" | "ql-1115nwb"
        ),
    };
    if !supported {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "operation is unsupported for this printer model",
        ));
    }
    if connection.transport["kind"] != "usb" {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Brother administration requires a USB connection",
        ));
    }
    let selector = connection.transport["device"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "stable USB device selector is required",
        ))?
        .to_owned();
    Ok((connection, selector))
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
        .route("/v1/admin/pair", post(pair_admin))
        .route("/v1/grants/me", get(current_grant))
        .route("/v1/grants/me/rotate", post(rotate_current_grant))
        .route("/v1/grants/me/revoke", post(revoke_current_grant))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/printers", get(printers))
        .route("/v1/discovery", post(discovery))
        .route("/v1/connection", post(connection))
        .route("/v1/status", get(status))
        .route(
            "/v1/printers/{connection}/brother/wifi/status",
            get(brother_wireless_status),
        )
        .route(
            "/v1/printers/{connection}/brother/wifi/scan",
            post(brother_wireless_scan),
        )
        .route(
            "/v1/printers/{connection}/brother/wifi/prepare",
            post(brother_wireless_prepare),
        )
        .route(
            "/v1/printers/{connection}/brother/wifi/configure",
            post(brother_wireless_configure),
        )
        .route(
            "/v1/printers/{connection}/brother/report",
            get(brother_system_report),
        )
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

    fn test_connection(model: &str, kind: &str) -> Connection {
        Connection {
            id: "test".into(),
            model: model.into(),
            transport: serde_json::json!({"kind": kind}),
            status: "ready".into(),
            media: None,
        }
    }

    #[test]
    fn brother_operations_are_derived_only_for_supported_usb_models() {
        let ql_1110 = connection_operations(&test_connection("ql-1110nwb", "usb"), true);
        assert_eq!(
            ql_1110,
            [
                "print",
                "status",
                "system-report",
                "wifi-configure",
                "wifi-scan",
                "wifi-status"
            ]
        );

        assert_eq!(
            connection_operations(&test_connection("ql-1115nwb", "usb"), true),
            ql_1110
        );

        assert_eq!(
            connection_operations(&test_connection("ql-1100", "usb"), true),
            ["print", "status", "system-report"]
        );
        assert_eq!(
            connection_operations(&test_connection("ql-1115nwb", "ipp"), true),
            ["print", "status"]
        );
        assert_eq!(
            connection_operations(&test_connection("ql-1115nwb", "ble"), true),
            ["print", "status"]
        );
    }

    #[test]
    fn wifi_configuration_is_hidden_without_the_local_opt_in() {
        assert_eq!(
            connection_operations(&test_connection("ql-1110nwb", "usb"), false),
            [
                "print",
                "status",
                "system-report",
                "wifi-scan",
                "wifi-status"
            ]
        );
    }

    #[tokio::test]
    async fn wifi_configuration_routes_are_forbidden_without_the_local_opt_in() {
        let mut state = test_state();
        state.config.enable_brother_wifi_configuration = false;
        let token = test_admin_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/printers/any/brother/wifi/prepare")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"ssid":"test","password":"not-returned","encryption":"aes","authentication":"wpa2-only"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn administrator_pairing_is_forbidden_without_its_separate_local_opt_in() {
        let mut state = test_state();
        state.config.enable_brother_wifi_configuration = true;
        state.config.enable_brother_wifi_configuration_pairing = false;
        let response = router(state)
            .oneshot(
                Request::post("/v1/admin/pair")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"secret":"not-used"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[test]
    fn operation_metadata_is_not_persisted_with_connection() {
        let connection = test_connection("ql-1110nwb", "usb");
        let stored = serde_json::to_value(&connection).unwrap();
        assert!(stored.get("operations").is_none());
        assert!(
            connection_json(&connection, true)
                .get("operations")
                .is_some()
        );
    }

    #[test]
    fn local_job_trace_fields_are_an_explicit_safe_allowlist() {
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            let span = local_job_execution_span(Uuid::nil(), "model", "protocol", 1, 2, 3);
            let fields = span
                .metadata()
                .unwrap()
                .fields()
                .iter()
                .map(|field| field.name())
                .collect::<Vec<_>>();
            assert_eq!(
                fields,
                [
                    "job_id",
                    "model",
                    "protocol",
                    "copies",
                    "action_count",
                    "total_bytes",
                    "bytes_written",
                    "outcome",
                    "duration_ms",
                ]
            );
        });
    }
    fn test_state() -> ApiState {
        let dir = tempfile::tempdir().unwrap().keep();
        test_state_at(&dir)
    }
    fn test_state_at(dir: &std::path::Path) -> ApiState {
        ApiState::new(
            AuthStore::load(dir.join("g.json")).unwrap(),
            Config {
                allowed_origins: vec![
                    "https://editor.example".into(),
                    "https://labels.dev1.makersbrain.net".into(),
                ],
                enable_brother_wifi_configuration: true,
                printers_path: Some(dir.join("printers.json")),
                catalogue_path: Some(dir.join("catalogues.json")),
                jobs_path: Some(dir.join("jobs.json")),
                enable_brother_wifi_configuration_pairing: true,
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

    async fn test_admin_token(state: &ApiState) -> String {
        state
            .auth
            .write()
            .await
            .issue_admin("https://editor.example", Duration::from_secs(30))
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
            document: Some(fixture["document"].clone()),
            documents: Vec::new(),
            model: "m110".into(),
            dpi: None,
            rotation: 0,
            fit: false,
            density: 6,
            copies: 1,
            payload_limit: 512,
            continuous: None,
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
    async fn authenticated_printer_state_responses_are_not_cached() {
        let state = test_state();
        let token = test_token(&state).await;
        let app = router(state);
        for (method, path) in [
            (http::Method::GET, "/v1/printers"),
            (http::Method::POST, "/v1/discovery"),
            (http::Method::GET, "/v1/status"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL),
                Some(&HeaderValue::from_static("no-store")),
                "{path}"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("application/json")),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn brother_read_routes_are_typed_redacted_authenticated_and_not_cached() {
        use http_body_util::BodyExt as _;
        use mb_printer_core::protocol::brother::wifi::{
            AccessPoint, WirelessAuthentication, WirelessEncryption,
        };

        let state = test_state();
        state.connections.write().await.insert(
            "brother-usb".into(),
            Connection {
                id: "brother-usb".into(),
                model: "ql-1110nwb".into(),
                transport: serde_json::json!({
                    "kind":"usb",
                    "device":"usb-device:04f9:209b:001:007"
                }),
                status: "ready".into(),
                media: None,
            },
        );
        let report = crate::printer_ops::parse_system_report(
            b"<<PRINTER CONFIGURATION>>\n[WLAN]\nSSID=Cafe\nChannel=6\n",
            false,
        )
        .unwrap();
        state
            .inject_brother_reads(
                "brother-usb",
                crate::printer_ops::WirelessStatus {
                    connected: Some(true),
                    ip_address: Some("192.168.1.100".into()),
                    ssid: Some("Cafe".into()),
                    encryption: Some(WirelessEncryption::TkipAes),
                    authentication: Some(WirelessAuthentication::WpaPsk),
                    infrastructure: Some(true),
                    wireless_direct: Some(false),
                },
                vec![AccessPoint {
                    ssid: "Cafe".into(),
                    channel: 6,
                    power: -42,
                    enterprise: false,
                    encrypted: true,
                }],
                report,
            )
            .await;
        let token = test_token(&state).await;
        let app = router(state);
        let preflight = Request::options("/v1/printers/brother-usb/brother/wifi/scan")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "authorization")
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .unwrap();
        let preflight = app.clone().oneshot(preflight).await.unwrap();
        assert_eq!(preflight.status(), StatusCode::OK);
        assert_eq!(
            preflight.headers()["access-control-allow-private-network"],
            "true"
        );
        for (method, path, expected_pointer) in [
            (
                http::Method::GET,
                "/v1/printers/brother-usb/brother/wifi/status",
                "/connected",
            ),
            (
                http::Method::POST,
                "/v1/printers/brother-usb/brother/wifi/scan",
                "/accessPoints/0/channel",
            ),
            (
                http::Method::GET,
                "/v1/printers/brother-usb/brother/report",
                "/sections/WLAN/SSID",
            ),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert!(body.pointer(expected_pointer).is_some(), "{path}: {body}");
            if path.ends_with("/report") {
                assert_eq!(body.pointer(expected_pointer).unwrap(), "[REDACTED]");
                assert!(!body.to_string().contains("Cafe"));
            }
        }

        let unauthenticated = Request::get("/v1/printers/brother-usb/brother/wifi/status")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(unauthenticated).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn brother_read_routes_reject_wrong_model_transport_and_missing_selector() {
        let state = test_state();
        for (id, model, transport) in [
            (
                "wrong-model",
                "m110",
                serde_json::json!({"kind":"usb","device":"serial"}),
            ),
            (
                "non-wifi-brother",
                "ql-1100",
                serde_json::json!({"kind":"usb","device":"serial"}),
            ),
            (
                "wrong-transport",
                "ql-1110nwb",
                serde_json::json!({"kind":"ipp","uri":"ipp://printer.local/ipp/print"}),
            ),
            (
                "missing-selector",
                "ql-1110nwb",
                serde_json::json!({"kind":"usb"}),
            ),
        ] {
            state.connections.write().await.insert(
                id.into(),
                Connection {
                    id: id.into(),
                    model: model.into(),
                    transport,
                    status: "ready".into(),
                    media: None,
                },
            );
        }
        let token = test_token(&state).await;
        let app = router(state);
        for id in [
            "wrong-model",
            "non-wifi-brother",
            "wrong-transport",
            "missing-selector",
        ] {
            let request = Request::get(format!("/v1/printers/{id}/brother/wifi/status"))
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn brother_wireless_prepare_requires_admin_and_never_echoes_a_password() {
        use http_body_util::BodyExt as _;

        let state = test_state();
        state.connections.write().await.insert(
            "brother-usb".into(),
            Connection {
                id: "brother-usb".into(),
                model: "ql-1110nwb".into(),
                transport: serde_json::json!({
                    "kind":"usb",
                    "device":"usb-device:04f9:209b:001:007"
                }),
                status: "ready".into(),
                media: None,
            },
        );
        let print_token = test_token(&state).await;
        let admin_token = test_admin_token(&state).await;
        let app = router(state.clone());
        let body = serde_json::json!({
            "ssid":"Office WiFi",
            "password":"test-secret-never-returned",
            "encryption":"tkip-aes",
            "authentication":"wpa-psk",
            "infrastructure":true,
            "wirelessDirect":false,
            "reboot":true
        })
        .to_string();

        let request = |token: &str| {
            Request::post("/v1/printers/brother-usb/brother/wifi/prepare")
                .header("host", "localhost:9847")
                .header("origin", "https://editor.example")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap()
        };
        let rejected = app.clone().oneshot(request(&print_token)).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(rejected.headers()[header::CACHE_CONTROL], "no-store");

        let response = app.clone().oneshot(request(&admin_token)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let text = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("Office WiFi"));
        assert!(!text.contains("test-secret-never-returned"));
        assert!(text.contains("USB or Bluetooth"));

        let approval_id = serde_json::from_str::<serde_json::Value>(&text).unwrap()["approvalId"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let settings = mb_printer_core::protocol::brother::wifi::WirelessSettings {
            ssid: "Office WiFi".into(),
            password: "test-secret-never-returned".into(),
            encryption: mb_printer_core::protocol::brother::wifi::WirelessEncryption::TkipAes,
            authentication:
                mb_printer_core::protocol::brother::wifi::WirelessAuthentication::WpaPsk,
            infrastructure: true,
            wireless_direct: false,
            reboot: true,
        };
        state
            .inject_brother_wireless_configuration("brother-usb", &settings)
            .await;
        assert!(
            state
                .auth
                .write()
                .await
                .approve_wifi_approval(approval_id)
                .unwrap()
        );
        let configure_body = serde_json::json!({
            "approvalId":approval_id,
            "ssid":"Office WiFi",
            "password":"test-secret-never-returned",
            "encryption":"tkip-aes",
            "authentication":"wpa-psk",
            "infrastructure":true,
            "wirelessDirect":false,
            "reboot":true
        })
        .to_string();
        let configured = app
            .clone()
            .oneshot(
                Request::post("/v1/printers/brother-usb/brother/wifi/configure")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(configure_body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(configured.status(), StatusCode::OK);
        assert_eq!(configured.headers()[header::CACHE_CONTROL], "no-store");
        // A consumed approval cannot replay the same password-bearing request.
        let replay = app
            .oneshot(
                Request::post("/v1/printers/brother-usb/brother/wifi/configure")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(configure_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::CONFLICT);
        assert_eq!(replay.headers()[header::CACHE_CONTROL], "no-store");
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
    async fn hosted_editor_origin_is_exact_and_grants_do_not_cross_origins() {
        let state = test_state();
        let app = router(state.clone());
        let origin = "https://labels.dev1.makersbrain.net";
        let allowed = Request::options("/v1/status")
            .header("host", "127.0.0.1:9847")
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-private-network", "true")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(allowed).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["access-control-allow-origin"], origin);
        assert_eq!(
            response.headers()["access-control-allow-private-network"],
            "true"
        );

        for rejected_origin in [
            "https://labels.dev2.makersbrain.net",
            "https://labels.dev1.makersbrain.net.evil.example",
            "https://prefix-labels.dev1.makersbrain.net",
        ] {
            let rejected = Request::options("/v1/status")
                .header("host", "127.0.0.1:9847")
                .header("origin", rejected_origin)
                .header("access-control-request-method", "GET")
                .header("access-control-request-private-network", "true")
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(rejected).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin")
            );
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-private-network")
            );
        }

        let secret = state
            .auth
            .write()
            .await
            .begin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;
        let token = state
            .auth
            .write()
            .await
            .exchange(&secret, origin, Duration::from_secs(300))
            .unwrap()
            .unwrap()
            .1;
        let same_origin = Request::get("/v1/capabilities")
            .header("host", "127.0.0.1:9847")
            .header("origin", origin)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(same_origin).await.unwrap().status(),
            StatusCode::OK
        );

        let crossed_origin = Request::get("/v1/capabilities")
            .header("host", "127.0.0.1:9847")
            .header("origin", "https://editor.example")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(crossed_origin).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let wrong_host = Request::get("/v1/capabilities")
            .header("host", "labels.dev1.makersbrain.net")
            .header("origin", origin)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(wrong_host).await.unwrap().status(),
            StatusCode::MISDIRECTED_REQUEST
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
                #[cfg(feature = "network")]
                network: None,
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
        assert!(state.config.printers_path.as_ref().unwrap().exists());
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

    #[tokio::test]
    async fn administrator_pairing_is_distinct_short_lived_and_not_cacheable() {
        let state = test_state();
        let secret = state
            .auth
            .write()
            .await
            .begin_admin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;
        let app = router(state.clone());

        // A scoped admin secret must not be accepted by the normal pairing
        // endpoint, and that failed attempt must not consume it.
        let normal_pair = Request::post("/v1/pair")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(normal_pair).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let administrator_pair = Request::post("/v1/admin/pair")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
            .unwrap();
        let response = app.clone().oneshot(administrator_pair).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        use http_body_util::BodyExt;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let pair = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        let token = pair["token"].as_str().unwrap();
        assert!(
            state
                .auth
                .read()
                .await
                .authenticate_admin(token, "https://editor.example")
                .is_some()
        );
        let grant = state.auth.read().await.grants().pop().unwrap();
        assert_eq!(grant.scope, crate::auth::GrantScope::Admin);
        assert!(grant.expires_at <= grant.created_at + 600);

        // The secret is one-time even when its intended endpoint was used.
        let replay = Request::post("/v1/admin/pair")
            .header("host", "localhost:9847")
            .header("origin", "https://editor.example")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
            .unwrap();
        assert_eq!(
            app.oneshot(replay).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn normal_pairing_created_by_a_separate_cli_process_is_visible_to_running_service() {
        let directory = tempfile::tempdir().unwrap();
        let state = test_state_at(directory.path());
        let app = router(state.clone());

        // This deliberately does not use `state.auth`: it emulates `mb-printer
        // api pair` loading and changing the durable store after the service
        // process has already constructed its ApiState.
        let mut cli_store = AuthStore::load(directory.path().join("g.json")).unwrap();
        let secret = cli_store
            .begin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;

        let response = app
            .oneshot(
                Request::post("/v1/pair")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        use http_body_util::BodyExt as _;
        let pair: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(
            state
                .auth
                .read()
                .await
                .authenticate(pair["token"].as_str().unwrap(), "https://editor.example")
                .is_some()
        );
    }

    #[tokio::test]
    async fn administrator_pairing_created_by_a_separate_cli_process_is_visible_to_running_service()
    {
        let directory = tempfile::tempdir().unwrap();
        let state = test_state_at(directory.path());
        let app = router(state.clone());
        let mut cli_store = AuthStore::load(directory.path().join("g.json")).unwrap();
        let secret = cli_store
            .begin_admin_pairing(Duration::from_secs(30))
            .unwrap()
            .value;

        let response = app
            .oneshot(
                Request::post("/v1/admin/pair")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"secret":"{secret}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        use http_body_util::BodyExt as _;
        let pair: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(
            state
                .auth
                .read()
                .await
                .authenticate_admin(pair["token"].as_str().unwrap(), "https://editor.example")
                .is_some()
        );
    }

    #[tokio::test]
    async fn wifi_approval_created_by_a_separate_cli_process_is_consumed_by_running_service() {
        let directory = tempfile::tempdir().unwrap();
        let state = test_state_at(directory.path());
        state.connections.write().await.insert(
            "brother-usb".into(),
            Connection {
                id: "brother-usb".into(),
                model: "ql-1110nwb".into(),
                transport: serde_json::json!({
                    "kind":"usb",
                    "device":"usb-device:04f9:209b:001:007"
                }),
                status: "ready".into(),
                media: None,
            },
        );
        let admin_token = test_admin_token(&state).await;
        let app = router(state.clone());
        let body = serde_json::json!({
            "ssid":"Office WiFi",
            "password":"test-secret-never-returned",
            "encryption":"tkip-aes",
            "authentication":"wpa-psk",
            "infrastructure":true,
            "wirelessDirect":false,
            "reboot":true
        })
        .to_string();
        let prepared = app
            .clone()
            .oneshot(
                Request::post("/v1/printers/brother-usb/brother/wifi/prepare")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prepared.status(), StatusCode::OK);
        use http_body_util::BodyExt as _;
        let approval: serde_json::Value =
            serde_json::from_slice(&prepared.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let approval_id: Uuid = approval["approvalId"].as_str().unwrap().parse().unwrap();

        // Emulate `mb-printer service wifi approve`: it must not share the API
        // process's in-memory AuthStore.
        let mut cli_store = AuthStore::load(directory.path().join("g.json")).unwrap();
        assert!(cli_store.approve_wifi_approval(approval_id).unwrap());

        let settings = mb_printer_core::protocol::brother::wifi::WirelessSettings {
            ssid: "Office WiFi".into(),
            password: "test-secret-never-returned".into(),
            encryption: mb_printer_core::protocol::brother::wifi::WirelessEncryption::TkipAes,
            authentication:
                mb_printer_core::protocol::brother::wifi::WirelessAuthentication::WpaPsk,
            infrastructure: true,
            wireless_direct: false,
            reboot: true,
        };
        state
            .inject_brother_wireless_configuration("brother-usb", &settings)
            .await;
        let configured = app
            .oneshot(
                Request::post("/v1/printers/brother-usb/brother/wifi/configure")
                    .header("host", "localhost:9847")
                    .header("origin", "https://editor.example")
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "approvalId":approval_id,
                            "ssid":"Office WiFi",
                            "password":"test-secret-never-returned",
                            "encryption":"tkip-aes",
                            "authentication":"wpa-psk",
                            "infrastructure":true,
                            "wirelessDirect":false,
                            "reboot":true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(configured.status(), StatusCode::OK);
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
        let error = execute_cancellable(
            &plan,
            FailFirstWrite,
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        )
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
