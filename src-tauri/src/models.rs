use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Draft,
    AwaitingNumberConfirmation,
    ReadyToScan,
    Scanning,
    NeedsAttention,
    ReadyToCopy,
    Copying,
    Completed,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::AwaitingNumberConfirmation => "awaitingNumberConfirmation",
            Self::ReadyToScan => "readyToScan",
            Self::Scanning => "scanning",
            Self::NeedsAttention => "needsAttention",
            Self::ReadyToCopy => "readyToCopy",
            Self::Copying => "copying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    SubmitInputs,
    ConfirmNumbers,
    StartScan,
    RequireAttention,
    ResolveAttention,
    StartCopy,
    FinishCopy,
    Fail,
    Cancel,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("编号尚未确认")]
    NumbersNotConfirmed,
    #[error("无效的会话状态迁移")]
    InvalidTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub task_label: String,
    pub note: Option<String>,
    pub status: SessionStatus,
    pub created_at: String,
    #[serde(default)]
    pub numbers_confirmed: bool,
}

impl Session {
    pub fn new(task_label: &str, note: Option<&str>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            task_label: task_label.to_string(),
            note: note.map(str::to_string),
            status: SessionStatus::Draft,
            created_at: Utc::now().to_rfc3339(),
            numbers_confirmed: false,
        }
    }

    pub fn transition(&mut self, action: SessionAction) -> Result<SessionStatus, SessionError> {
        let next = match (&self.status, action) {
            (SessionStatus::Draft, SessionAction::SubmitInputs) => {
                SessionStatus::AwaitingNumberConfirmation
            }
            (SessionStatus::AwaitingNumberConfirmation, SessionAction::ConfirmNumbers) => {
                self.numbers_confirmed = true;
                SessionStatus::ReadyToScan
            }
            (_, SessionAction::StartScan) if !self.numbers_confirmed => {
                return Err(SessionError::NumbersNotConfirmed);
            }
            (SessionStatus::ReadyToScan, SessionAction::StartScan) => SessionStatus::Scanning,
            (SessionStatus::Scanning, SessionAction::RequireAttention) => {
                SessionStatus::NeedsAttention
            }
            (SessionStatus::Copying, SessionAction::RequireAttention) => {
                SessionStatus::NeedsAttention
            }
            (SessionStatus::Scanning, SessionAction::ResolveAttention)
            | (SessionStatus::NeedsAttention, SessionAction::ResolveAttention) => {
                SessionStatus::ReadyToCopy
            }
            (SessionStatus::ReadyToCopy, SessionAction::StartCopy) => SessionStatus::Copying,
            (SessionStatus::Copying, SessionAction::FinishCopy) => SessionStatus::Completed,
            (_, SessionAction::Fail) => SessionStatus::Failed,
            (_, SessionAction::Cancel) => SessionStatus::Cancelled,
            _ => return Err(SessionError::InvalidTransition),
        };
        self.status = next.clone();
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoNumber {
    pub original: String,
    pub canonical: String,
    pub confidence: Option<f32>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInput {
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub kind: String,
    pub mime: String,
    pub stored_name: String,
    pub size: u64,
    pub root_identity: FileIdentity,
    pub fingerprint: FileFingerprint,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AddressMode {
    BaseUrl,
    FullEndpoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiFormat {
    Responses,
    ChatCompletions,
}

/// Non-secret settings for an OpenAI-compatible cloud model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    #[serde(default = "default_provider_template")]
    pub template: String,
    pub address: String,
    pub address_mode: AddressMode,
    pub api_format: ApiFormat,
    pub model: String,
    pub fallback_model: Option<String>,
    pub timeout_seconds: u64,
    pub enabled: bool,
    pub is_default: bool,
    /// Keyring entry name only; never an API key.
    pub secret_ref: String,
}

fn default_provider_template() -> String {
    "custom".into()
}

/// Non-secret cloud-provider configuration persisted in SQLite.
///
/// The provider credential itself is held by the system keyring. `secret_ref` is
/// only the stable profile identifier used to look it up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloudSettings {
    pub provider: String,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub secret_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentity {
    pub device: u64,
    pub file_index: u64,
}

impl Default for FileIdentity {
    fn default() -> Self {
        Self {
            device: 0,
            file_index: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileFingerprint {
    pub identity: FileIdentity,
    pub size: u64,
    pub modified_ms: u64,
    /// Optional BLAKE3 of the complete file contents. Filename-only scans keep
    /// this empty; copying computes a source digest while transferring and
    /// verifies the completed target against it.
    #[serde(default)]
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedFile {
    pub path: PathBuf,
    pub stem: String,
    pub extension: String,
    pub canonical_number: String,
    pub size: u64,
    pub modified_ms: u64,
    pub family_key: PathBuf,
    #[serde(default)]
    pub fingerprint: FileFingerprint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MatchStatus {
    Complete,
    Partial,
    Ambiguous,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateGroup {
    pub id: String,
    pub files: Vec<IndexedFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberMatch {
    pub canonical_number: String,
    pub status: MatchStatus,
    pub groups: Vec<CandidateGroup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSnapshot {
    pub source: PathBuf,
    #[serde(default)]
    pub source_root_identity: FileIdentity,
    pub target: PathBuf,
    pub items: Vec<NumberMatch>,
    #[serde(default)]
    pub skipped_numbers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedFile {
    #[serde(default)]
    pub canonical_number: String,
    pub source: PathBuf,
    pub target: PathBuf,
    #[serde(default)]
    pub fingerprint: FileFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CopyItemStatus {
    Planned,
    Copying,
    Copied,
    Skipped,
    Failed,
    Cancelled,
    PlanSuperseded,
}

impl CopyItemStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Copying => "copying",
            Self::Copied => "copied",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::PlanSuperseded => "planSuperseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyHistoryItem {
    pub id: String,
    pub session_id: String,
    pub canonical_number: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub planned_hash: String,
    #[serde(default)]
    pub plan_revision: i64,
    pub status: CopyItemStatus,
    pub source_hash: Option<String>,
    pub skipped_reason: Option<String>,
    pub error_code: Option<String>,
    pub error_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl CopyHistoryItem {
    pub fn planned(
        session_id: &str,
        canonical_number: &str,
        source: &Path,
        target: &Path,
        planned_hash: &str,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_owned(),
            canonical_number: canonical_number.to_owned(),
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            planned_hash: planned_hash.to_owned(),
            plan_revision: 0,
            status: CopyItemStatus::Planned,
            source_hash: None,
            skipped_reason: None,
            error_code: None,
            error_summary: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceIndex {
    pub root: PathBuf,
    pub root_identity: FileIdentity,
    pub files: Vec<IndexedFile>,
}

#[cfg(test)]
mod session_state {
    use super::*;

    #[test]
    fn cannot_scan_before_numbers_are_confirmed() {
        let mut session = Session::new("订单 A", None);
        assert_eq!(
            session.transition(SessionAction::StartScan),
            Err(SessionError::NumbersNotConfirmed)
        );
        session.transition(SessionAction::SubmitInputs).unwrap();
        session.transition(SessionAction::ConfirmNumbers).unwrap();
        assert_eq!(
            session.transition(SessionAction::StartScan),
            Ok(SessionStatus::Scanning)
        );
    }
}
