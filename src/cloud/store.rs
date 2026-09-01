// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::jobs::{Job, JobState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs, io,
    path::Path,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudPrintRequest {
    #[serde(default)]
    pub document: Option<serde_json::Value>,
    #[serde(default)]
    pub documents: Vec<serde_json::Value>,
    pub model: String,
    #[serde(default)]
    pub dpi: Option<u16>,
    #[serde(default)]
    pub rotation: u16,
    #[serde(default)]
    pub fit: bool,
    #[serde(default = "default_density")]
    pub density: u8,
    #[serde(default = "default_copies")]
    pub copies: u16,
    #[serde(default = "default_payload_limit")]
    pub payload_limit: usize,
    #[serde(default)]
    pub continuous: Option<ContinuousPrintOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuousPrintOptions {
    pub cut_mode: ContinuousCutMode,
    pub extra_feed_before_mm: f64,
    pub extra_feed_after_mm: f64,
    pub chain_copies: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ContinuousCutMode {
    AfterEach,
    AfterJob,
    None,
}

const fn default_density() -> u8 {
    6
}
const fn default_copies() -> u16 {
    1
}
const fn default_payload_limit() -> usize {
    512
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudJob {
    pub id: Uuid,
    pub printer_id: Uuid,
    pub request_sha256: String,
    pub request: CloudPrintRequest,
    pub local_job: Job,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudJobStore {
    jobs: BTreeMap<Uuid, CloudJob>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReceiveError {
    #[error("cloud job ID is invalid")]
    InvalidJobId,
    #[error("cloud printer ID is invalid")]
    InvalidPrinterId,
    #[error("cloud job payload exceeds the configured limit")]
    TooLarge,
    #[error("cloud job payload digest does not match")]
    DigestMismatch,
    #[error("cloud job ID was reused with a different payload")]
    Conflict,
    #[error("cloud job request is invalid")]
    InvalidRequest,
}

#[derive(Debug)]
pub enum ReceiveOutcome<'a> {
    New(&'a CloudJob),
    Existing(&'a CloudJob),
}

impl CloudJobStore {
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(self).map_err(io::Error::other)?,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(temporary, path)
    }

    pub fn receive(
        &mut self,
        job_id: &str,
        printer_id: &str,
        bytes: &[u8],
        expected_sha256: &str,
        maximum_bytes: usize,
    ) -> Result<ReceiveOutcome<'_>, ReceiveError> {
        let id = Uuid::parse_str(job_id).map_err(|_| ReceiveError::InvalidJobId)?;
        let printer_id = Uuid::parse_str(printer_id).map_err(|_| ReceiveError::InvalidPrinterId)?;
        if bytes.len() > maximum_bytes {
            return Err(ReceiveError::TooLarge);
        }
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != expected_sha256 {
            return Err(ReceiveError::DigestMismatch);
        }
        let request: CloudPrintRequest =
            serde_json::from_slice(bytes).map_err(|_| ReceiveError::InvalidRequest)?;
        if request.document.is_some() == !request.documents.is_empty()
            || request.model.is_empty()
            || request.copies == 0
            || !(1..=8).contains(&request.density)
            || request.payload_limit == 0
            || !matches!(request.rotation, 0 | 90 | 180 | 270)
        {
            return Err(ReceiveError::InvalidRequest);
        }
        let mut local_job = Job::new();
        local_job.id = id;
        local_job.request_hash = Some(actual.clone());
        local_job.resumable = serde_json::to_value(&request).ok();
        match self.jobs.entry(id) {
            Entry::Occupied(entry) => {
                let existing = entry.into_mut();
                if existing.request_sha256 == actual && existing.printer_id == printer_id {
                    Ok(ReceiveOutcome::Existing(existing))
                } else {
                    Err(ReceiveError::Conflict)
                }
            }
            Entry::Vacant(entry) => Ok(ReceiveOutcome::New(entry.insert(CloudJob {
                id,
                printer_id,
                request_sha256: actual,
                request,
                local_job,
            }))),
        }
    }

    pub fn get(&self, id: Uuid) -> Option<&CloudJob> {
        self.jobs.get(&id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut CloudJob> {
        self.jobs.get_mut(&id)
    }

    pub fn non_terminal(&self) -> impl Iterator<Item = &CloudJob> {
        self.jobs.values().filter(|job| !job.local_job.terminal())
    }

    pub fn reconcile_interrupted(&mut self) {
        for job in self.jobs.values_mut() {
            if job.local_job.state == JobState::Running {
                job.local_job.state = if job.local_job.potentially_accepted_write {
                    JobState::OutcomeUnknown
                } else {
                    JobState::Queued
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Vec<u8> {
        serde_json::to_vec(&CloudPrintRequest {
            document: Some(serde_json::json!({"version":4})),
            documents: Vec::new(),
            model: "m110".into(),
            dpi: None,
            rotation: 0,
            fit: false,
            density: 6,
            copies: 1,
            continuous: None,
            payload_limit: 512,
        })
        .unwrap()
    }

    #[test]
    fn duplicate_delivery_returns_the_same_durable_job() {
        let job_id = Uuid::new_v4().to_string();
        let printer_id = Uuid::new_v4().to_string();
        let bytes = request();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let mut store = CloudJobStore::default();
        assert!(matches!(
            store.receive(&job_id, &printer_id, &bytes, &digest, 4096),
            Ok(ReceiveOutcome::New(_))
        ));
        assert!(matches!(
            store.receive(&job_id, &printer_id, &bytes, &digest, 4096),
            Ok(ReceiveOutcome::Existing(_))
        ));
        assert_eq!(store.jobs.len(), 1);
    }

    #[test]
    fn conflicting_duplicate_is_rejected() {
        let job_id = Uuid::new_v4().to_string();
        let printer_id = Uuid::new_v4().to_string();
        let bytes = request();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let mut store = CloudJobStore::default();
        store
            .receive(&job_id, &printer_id, &bytes, &digest, 4096)
            .unwrap();
        let other_printer = Uuid::new_v4().to_string();
        assert_eq!(
            store
                .receive(&job_id, &other_printer, &bytes, &digest, 4096)
                .unwrap_err(),
            ReceiveError::Conflict
        );
    }

    #[test]
    fn restart_never_requeues_a_job_after_a_possible_write() {
        let job_id = Uuid::new_v4().to_string();
        let printer_id = Uuid::new_v4().to_string();
        let bytes = request();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let mut store = CloudJobStore::default();
        store
            .receive(&job_id, &printer_id, &bytes, &digest, 4096)
            .unwrap();
        let job = store.get_mut(Uuid::parse_str(&job_id).unwrap()).unwrap();
        job.local_job.state = JobState::Running;
        job.local_job.potentially_accepted_write = true;
        store.reconcile_interrupted();
        assert_eq!(
            store
                .get(Uuid::parse_str(&job_id).unwrap())
                .unwrap()
                .local_job
                .state,
            JobState::OutcomeUnknown
        );
    }
}
