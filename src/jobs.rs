// SPDX-License-Identifier: AGPL-3.0-or-later
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
    Queued,
    Running,
    CancelRequested,
    CancelledBeforeSend,
    CancelledPartial,
    OutcomeUnknown,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub state: JobState,
    pub last_completed_action: Option<u32>,
    pub bytes_written: u64,
    pub total_bytes: u64,
    pub potentially_accepted_write: bool,
    pub protocol: Option<String>,
    pub action_count: usize,
    pub error: Option<String>,
    #[serde(default)]
    pub resumable: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at_ms: u128,
    #[serde(default)]
    pub updated_at_ms: u128,
}

impl Job {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            id: Uuid::new_v4(),
            state: JobState::Queued,
            last_completed_action: None,
            bytes_written: 0,
            total_bytes: 0,
            potentially_accepted_write: false,
            protocol: None,
            action_count: 0,
            error: None,
            resumable: None,
            created_at_ms: now,
            updated_at_ms: now,
        }
    }
    pub fn request_cancel(&mut self) {
        if !self.terminal() {
            self.state = JobState::CancelRequested;
            self.updated_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
        }
    }
    pub fn finish_cancellation(&mut self, ambiguous_disconnect: bool) {
        self.state = if !self.potentially_accepted_write {
            JobState::CancelledBeforeSend
        } else if ambiguous_disconnect {
            JobState::OutcomeUnknown
        } else {
            JobState::CancelledPartial
        };
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self.state,
            JobState::CancelledBeforeSend
                | JobState::CancelledPartial
                | JobState::OutcomeUnknown
                | JobState::Completed
                | JobState::Failed
        )
    }
}

impl Default for Job {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_before_write_is_precise() {
        let mut j = Job::new();
        j.request_cancel();
        j.finish_cancellation(false);
        assert_eq!(j.state, JobState::CancelledBeforeSend);
    }
    #[test]
    fn cancellation_after_write_is_partial() {
        let mut j = Job::new();
        j.potentially_accepted_write = true;
        j.request_cancel();
        j.finish_cancellation(false);
        assert_eq!(j.state, JobState::CancelledPartial);
    }
    #[test]
    fn ambiguous_disconnect_is_unknown() {
        let mut j = Job::new();
        j.potentially_accepted_write = true;
        j.request_cancel();
        j.finish_cancellation(true);
        assert_eq!(j.state, JobState::OutcomeUnknown);
    }
}
