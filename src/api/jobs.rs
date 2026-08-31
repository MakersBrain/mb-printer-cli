// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local print-job submission, execution, persistence, and HTTP lifecycle.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Sse, sse::Event},
};
use mb_printer_core::{
    Document, capabilities,
    protocol::{self, Options},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use uuid::Uuid;

use super::{ApiError, ApiState, authorize, canonical_document, origin};
use crate::{
    config::Config,
    jobs::{Job, JobState},
    raster,
    transport::{CaptureTransport, PhysicalEvent, SerialTransport, TcpTransport, WriteTransport},
};

pub(super) fn load_jobs(config: &Config) -> HashMap<Uuid, Job> {
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
    restored_jobs
        .into_iter()
        .map(|mut job| {
            if !job.terminal() {
                job.state = JobState::OutcomeUnknown;
                job.error = Some("service restarted before terminal outcome".into());
            }
            (job.id, job)
        })
        .collect()
}

pub(super) fn save_jobs(state: &ApiState, jobs: &HashMap<Uuid, Job>) -> Result<(), ApiError> {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JobView {
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
pub(super) fn api_render_for_printer(
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
pub(super) struct Cancellable<T> {
    pub(super) inner: T,
    pub(super) cancel: Arc<AtomicBool>,
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
pub(super) fn error_progress(
    error: &mb_printer_native::ExecuteError,
) -> Option<&mb_printer_native::Progress> {
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
pub(super) fn execute_cancellable<T: mb_printer_native::Transport>(
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
pub(super) async fn submit_job(
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

pub(super) struct SubmitOutcome {
    pub(super) status: StatusCode,
    pub(super) job: Job,
}

/// One execution boundary shared by loopback HTTP submissions and cloud jobs.
///
/// Callers select an already configured connection in the request. Only this
/// boundary resolves that connection into a native transport, validates and
/// plans the document, persists the job, and starts physical execution.
#[derive(Clone)]
pub(super) struct JobExecutor {
    state: ApiState,
}

impl JobExecutor {
    pub(super) fn new(state: ApiState) -> Self {
        Self { state }
    }

    #[tracing::instrument(
        name = "local_api.job.submit",
        skip_all,
        fields(
            job_id = tracing::field::Empty,
            model = tracing::field::Empty,
            protocol = tracing::field::Empty,
            copies = tracing::field::Empty,
            action_count = tracing::field::Empty,
            total_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
        )
    )]
    pub(super) async fn submit(
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
            tracing::Span::current().record("job_id", tracing::field::display(existing.id));
            tracing::Span::current().record(
                "protocol",
                existing.protocol.as_deref().unwrap_or("unknown"),
            );
            tracing::Span::current().record("action_count", existing.action_count as u64);
            tracing::Span::current().record("total_bytes", existing.total_bytes);
            tracing::Span::current().record("outcome", "replayed");
            tracing::info!("local API print job replayed");
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
        #[cfg(not(all(feature = "bluetooth-linux", target_os = "linux")))]
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
        tracing::Span::current().record("model", model.as_str());
        tracing::Span::current().record("copies", u64::from(request.copies));
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
        tracing::Span::current().record("job_id", tracing::field::display(job.id));
        tracing::Span::current().record("protocol", job.protocol.as_deref().unwrap_or("unknown"));
        tracing::Span::current().record("action_count", plan.actions.len() as u64);
        tracing::Span::current().record("total_bytes", job.total_bytes);
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
        let execution_span = local_job_execution_span(
            id,
            &printer.id,
            job.protocol.as_deref().unwrap_or("unknown"),
            request.copies,
            job.action_count,
            job.total_bytes,
        );
        #[cfg(feature = "bluetooth")]
        let worker_runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let _entered = execution_span.enter();
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
                #[cfg(all(feature = "bluetooth-linux", target_os = "linux"))]
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
                #[cfg(not(all(feature = "bluetooth-linux", target_os = "linux")))]
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
            execution_span.record("bytes_written", running.bytes_written);
            execution_span.record("outcome", job_state_name(running.state));
            execution_span.record(
                "duration_ms",
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            tracing::info!("local API print job finished");
            let _ = events.send(running.clone());
            let mut jobs = worker_state.jobs.blocking_write();
            jobs.insert(id, running);
            let _ = save_jobs(&worker_state, &jobs);
        });
        tracing::Span::current().record("outcome", "accepted");
        tracing::info!("local API print job accepted");
        Ok(SubmitOutcome {
            status: StatusCode::ACCEPTED,
            job: accepted,
        })
    }
}

fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::CancelRequested => "cancel-requested",
        JobState::CancelledBeforeSend => "cancelled-before-send",
        JobState::CancelledPartial => "cancelled-partial",
        JobState::OutcomeUnknown => "outcome-unknown",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
    }
}

pub(super) fn local_job_execution_span(
    job_id: Uuid,
    model: &str,
    protocol: &str,
    copies: u16,
    action_count: usize,
    total_bytes: u64,
) -> tracing::Span {
    tracing::info_span!(
        "local_api.job.execute",
        job_id = %job_id,
        model,
        protocol,
        copies,
        action_count,
        total_bytes,
        bytes_written = tracing::field::Empty,
        outcome = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    )
}
pub(super) async fn get_job(
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
pub(super) async fn cancel_job(
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
    tracing::info!(job_id = %id, "local API print job cancellation requested");
    Ok(Json(JobView::from(&job)))
}
pub(super) async fn job_events(
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
