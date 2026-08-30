// SPDX-License-Identifier: AGPL-3.0-or-later
use super::{PROTOCOL_VERSION, store::CloudJobStore, wire};
use crate::{
    VERSION,
    api::ApiState,
    config::{CloudConfig, CloudPrinter},
    jobs::{Job, JobState},
};
use futures_util::StreamExt;
use rand::Rng as _;
use std::{io, path::Path, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, metadata::MetadataValue, transport::Endpoint};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("cloud agent configuration is invalid: {0}")]
    Configuration(&'static str),
    #[error("cloud agent credentials cannot be read")]
    Credentials(#[source] io::Error),
    #[error("cloud agent transport failed: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("cloud agent stream failed: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("cloud agent authorization metadata is invalid")]
    Authorization,
    #[error("cloud job persistence failed")]
    Persistence(#[source] io::Error),
}

fn status(job: &Job) -> wire::JobStatus {
    wire::JobStatus {
        job_id: job.id.to_string(),
        state: match job.state {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::CancelRequested => "cancel-requested",
            JobState::CancelledBeforeSend => "cancelled-before-send",
            JobState::CancelledPartial => "cancelled-partial",
            JobState::OutcomeUnknown => "outcome-unknown",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
        }
        .into(),
        terminal: job.terminal(),
        last_completed_action: job.last_completed_action.map_or(-1, i64::from),
        bytes_sent: job.bytes_written,
        total_bytes: job.total_bytes,
        potentially_accepted_write: job.potentially_accepted_write,
        error_code: job
            .error
            .as_deref()
            .map(safe_error_code)
            .unwrap_or_default(),
    }
}

fn safe_error_code(error: &str) -> String {
    let normalized = error
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    normalized
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-")
}

fn published(printer: &CloudPrinter) -> wire::PublishedPrinter {
    wire::PublishedPrinter {
        printer_id: printer.id.to_string(),
        name: printer.name.clone(),
        model: printer.model.clone(),
        enabled: printer.enabled,
    }
}

fn owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cloud token is group/world readable",
            ));
        }
    }
    Ok(())
}

pub async fn run(
    config: CloudConfig,
    api: ApiState,
    maximum_bytes: usize,
) -> Result<(), AgentError> {
    if config.server.trim().is_empty() {
        return Err(AgentError::Configuration("server is empty"));
    }
    owner_only(&config.token_path).map_err(AgentError::Credentials)?;
    let token = std::fs::read_to_string(&config.token_path)
        .map_err(AgentError::Credentials)?
        .trim()
        .to_owned();
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        return Err(AgentError::Configuration("token is malformed"));
    }
    let mut store = CloudJobStore::load(&config.jobs_path).map_err(AgentError::Persistence)?;
    store.reconcile_interrupted();
    store
        .save(&config.jobs_path)
        .map_err(AgentError::Persistence)?;
    let store = Arc::new(Mutex::new(store));
    let mut delay = Duration::from_secs(1);
    loop {
        match run_session(&config, &token, &api, store.clone(), maximum_bytes).await {
            Ok(()) => delay = Duration::from_secs(1),
            Err(error) => {
                eprintln!("mb-printer cloud: {error}");
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
        let jitter_ms = rand::rng().random_range(0..=250);
        tokio::time::sleep(delay + Duration::from_millis(jitter_ms)).await;
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Enrollment {
    pub agent_id: Uuid,
    pub token: String,
    pub agent_url: String,
}

pub async fn enroll(server: &str, code: &str) -> Result<Enrollment, Box<dyn std::error::Error>> {
    let mut url = reqwest::Url::parse(server)?;
    if url.scheme() != "https"
        && !(url.scheme() == "http"
            && url
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1")))
    {
        return Err("cloud enrollment requires HTTPS (HTTP is allowed only on loopback)".into());
    }
    url.set_path("/v1/printer-enrollments/exchange");
    url.set_query(None);
    url.set_fragment(None);
    let response = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({"code":code}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("cloud enrollment was rejected ({})", response.status()).into());
    }
    let enrollment = response.json::<Enrollment>().await?;
    if enrollment.token.len() < 32 || enrollment.token.chars().any(char::is_whitespace) {
        return Err("cloud enrollment returned a malformed token".into());
    }
    let agent_url = reqwest::Url::parse(&enrollment.agent_url)?;
    if agent_url.scheme() != "https"
        && !(agent_url.scheme() == "http"
            && agent_url
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1")))
    {
        return Err("cloud enrollment returned an insecure agent URL".into());
    }
    Ok(enrollment)
}

pub fn save_token(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(temporary, path)
}

async fn run_session(
    config: &CloudConfig,
    token: &str,
    api: &ApiState,
    store: Arc<Mutex<CloudJobStore>>,
    maximum_bytes: usize,
) -> Result<(), AgentError> {
    let channel = Endpoint::from_shared(config.server.clone())?
        .connect()
        .await?;
    let mut client = wire::printer_agent_service_client::PrinterAgentServiceClient::new(channel);
    let (sender, receiver) = mpsc::channel::<wire::AgentMessage>(16);
    let initial_jobs = store
        .lock()
        .await
        .non_terminal()
        .map(|job| status(&job.local_job))
        .collect();
    sender
        .send(wire::AgentMessage {
            payload: Some(wire::agent_message::Payload::Hello(wire::AgentHello {
                agent_id: config.agent_id.to_string(),
                protocol_version: PROTOCOL_VERSION,
                software_version: VERSION.into(),
                printers: config.printers.iter().map(published).collect(),
                jobs: initial_jobs,
            })),
        })
        .await
        .map_err(|_| AgentError::Configuration("agent stream closed"))?;
    let mut request = Request::new(ReceiverStream::new(receiver));
    request.metadata_mut().insert(
        "authorization",
        MetadataValue::try_from(format!("Bearer {token}"))
            .map_err(|_| AgentError::Authorization)?,
    );
    let mut inbound = client.session(request).await?.into_inner();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let jobs = store.lock().await.non_terminal().map(|job|status(&job.local_job)).collect();
                if sender.send(wire::AgentMessage { payload: Some(wire::agent_message::Payload::Heartbeat(wire::Heartbeat { jobs })) }).await.is_err() {
                    return Ok(());
                }
            }
            message = inbound.next() => {
                let Some(message) = message else { return Ok(()) };
                let message = message?;
                match message.payload {
                    Some(wire::broker_message::Payload::Hello(hello)) => {
                        if hello.protocol_version != PROTOCOL_VERSION {
                            return Err(AgentError::Configuration("unsupported broker protocol"));
                        }
                    }
                    Some(wire::broker_message::Payload::PrintJob(job)) => {
                        receive_job(config, api.clone(), store.clone(), sender.clone(), job, maximum_bytes).await;
                    }
                    Some(wire::broker_message::Payload::CancelJob(cancel)) => {
                        if let Ok(id) = Uuid::parse_str(&cancel.job_id) {
                            let _ = api.cancel_cloud_job(id).await;
                        }
                    }
                    None => {}
                }
            }
        }
    }
}

async fn receive_job(
    config: &CloudConfig,
    api: ApiState,
    store: Arc<Mutex<CloudJobStore>>,
    sender: mpsc::Sender<wire::AgentMessage>,
    message: wire::PrintJob,
    maximum_bytes: usize,
) {
    let Ok(job_id) = Uuid::parse_str(&message.job_id) else {
        return;
    };
    let Some(printer) = config
        .printers
        .iter()
        .find(|printer| printer.enabled && printer.id.to_string() == message.printer_id)
        .cloned()
    else {
        return;
    };
    let (request, existing_terminal) = {
        let mut jobs = store.lock().await;
        let received = match jobs.receive(
            &message.job_id,
            &message.printer_id,
            &message.request_json,
            &message.sha256,
            maximum_bytes,
        ) {
            Ok(received) => received,
            Err(_) => return,
        };
        let request = match &received {
            super::store::ReceiveOutcome::New(job)
            | super::store::ReceiveOutcome::Existing(job) => job.request.clone(),
        };
        let existing_terminal = match &received {
            super::store::ReceiveOutcome::New(_) => None,
            super::store::ReceiveOutcome::Existing(job) if job.local_job.terminal() => {
                Some(job.local_job.clone())
            }
            super::store::ReceiveOutcome::Existing(_) => None,
        };
        if jobs.save(&config.jobs_path).is_err() {
            return;
        }
        (request, existing_terminal)
    };
    let _ = sender
        .send(wire::AgentMessage {
            payload: Some(wire::agent_message::Payload::JobReceived(
                wire::JobReceived {
                    job_id: message.job_id.clone(),
                    sha256: message.sha256.clone(),
                },
            )),
        })
        .await;
    if let Some(job) = existing_terminal {
        let _ = sender
            .send(wire::AgentMessage {
                payload: Some(wire::agent_message::Payload::JobResult(wire::JobResult {
                    job: Some(status(&job)),
                })),
            })
            .await;
        return;
    }
    let job = match api
        .submit_cloud_job(job_id, &printer.connection_id, &request, &message.sha256)
        .await
    {
        Ok(job) => job,
        Err(error) => {
            let failed = {
                let mut jobs = store.lock().await;
                let Some(stored) = jobs.get_mut(job_id) else {
                    return;
                };
                stored.local_job.state = JobState::Failed;
                stored.local_job.error = Some(error.1.to_owned());
                let failed = stored.local_job.clone();
                if jobs.save(&config.jobs_path).is_err() {
                    return;
                }
                failed
            };
            let _ = sender
                .send(wire::AgentMessage {
                    payload: Some(wire::agent_message::Payload::JobResult(wire::JobResult {
                        job: Some(status(&failed)),
                    })),
                })
                .await;
            return;
        }
    };
    {
        let mut jobs = store.lock().await;
        if let Some(stored) = jobs.get_mut(job_id) {
            stored.local_job = job;
        }
        let _ = jobs.save(&config.jobs_path);
    }
    tokio::spawn(report_job(
        api,
        store,
        config.jobs_path.clone(),
        sender,
        job_id,
    ));
}

async fn report_job(
    api: ApiState,
    store: Arc<Mutex<CloudJobStore>>,
    path: std::path::PathBuf,
    sender: mpsc::Sender<wire::AgentMessage>,
    id: Uuid,
) {
    let mut previous_updated = 0;
    loop {
        let Some(job) = api.cloud_job(id).await else {
            return;
        };
        if job.updated_at_ms != previous_updated {
            previous_updated = job.updated_at_ms;
            {
                let mut jobs = store.lock().await;
                if let Some(stored) = jobs.get_mut(id) {
                    stored.local_job = job.clone();
                }
                if jobs.save(&path).is_err() {
                    return;
                }
            }
            let payload = if job.terminal() {
                wire::agent_message::Payload::JobResult(wire::JobResult {
                    job: Some(status(&job)),
                })
            } else {
                wire::agent_message::Payload::JobProgress(wire::JobProgress {
                    job: Some(status(&job)),
                })
            };
            if sender
                .send(wire::AgentMessage {
                    payload: Some(payload),
                })
                .await
                .is_err()
            {
                return;
            }
        }
        if job.terminal() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
