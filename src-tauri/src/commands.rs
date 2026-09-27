use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

use crate::{
    copy_engine::{
        capture_directory_identity, capture_file_fingerprint, cleanup_pending_target_parts,
        execute_copy_with_updates, preflight_copy_cancellable, read_bound_source_file, CopyError,
        CopyItemOutcome, CopyPlan, CopyReport, CopyStopReason, PreflightReport,
        TargetDisconnectInfo,
    },
    diagnostics::{self, DiagnosticPathKind},
    matching::{
        default_extensions, match_numbers, scan_source_index_for_numbers_with_progress_cancellable,
        ScanProgressUpdate,
    },
    models::{
        CopyHistoryItem, CopyItemStatus, MatchStatus, NumberMatch, PhotoNumber, ProviderProfile,
        ScanSnapshot, SelectedFile, Session, SessionError, SessionInput, SessionStatus,
    },
    network_paths::{self, ConnectMode},
    providers,
    storage::{Storage, StorageError},
};

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("会话标识无效")]
    InvalidSessionId,
    #[error("目录无效")]
    InvalidDirectory,
    #[error("照片目录无法读取或已断开，请重新选择源照片目录后再扫描")]
    SourceDirectoryUnavailable,
    #[error("目标目录不可用或不安全，请重新选择目标目录后再扫描")]
    TargetDirectoryUnavailable,
    #[error("目录未由本会话的原生选择器授权")]
    PathNotAuthorized,
    #[error("二次确认已失效，请返回扫描结果重新确认")]
    ConfirmationRequired,
    #[error("用户取消了目录选择")]
    DialogCancelled,
    #[error("已取消 Windows 网络登录，当前会话和已确认编号已保留")]
    SmbLoginCancelled,
    #[error("Windows 网络凭据冲突：同一服务器已使用其他账号连接，请先断开冲突连接后重试")]
    SmbCredentialConflict,
    #[error("SMB 网络目录不可用，请检查 NAS、局域网和共享地址后重试")]
    SmbUnavailable,
    #[error("OFFLINE_FALLBACK_REQUIRED:{0}")]
    OfflineFallbackRequired(String),
    #[error("会话状态无效：{0}")]
    Session(#[from] SessionError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("操作失败：{0}")]
    Other(String),
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub recognition_mode: String,
    pub default_provider_id: Option<String>,
    pub cloud_fallback_offline: bool,
    pub second_confirmation_enabled: bool,
    pub extensions: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            recognition_mode: "offline".into(),
            default_provider_id: None,
            cloud_fallback_offline: true,
            second_confirmation_enabled: false,
            extensions: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchReport {
    pub items: Vec<NumberMatch>,
    pub skipped_numbers: Vec<String>,
    pub auto_copy_started: bool,
    pub requires_second_confirmation: bool,
    pub confirmation_token: Option<String>,
    pub copy_job: Option<CopyLaunch>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecognitionDraft {
    pub detected_order_id: Option<String>,
    pub numbers: Vec<PhotoNumber>,
    pub raw_text: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundPaths {
    pub source: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWorkflow {
    pub session: Session,
    pub numbers: Vec<PhotoNumber>,
    pub inputs: Vec<SessionInputSummary>,
    pub bindings: BoundPaths,
    pub snapshot: Option<ScanSnapshot>,
    pub preflight: Option<PreflightReport>,
    pub copy_items: Vec<CopyHistoryItem>,
    pub requires_second_confirmation: bool,
    pub confirmation_token: Option<String>,
    pub source_available: bool,
    pub target_available: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInputSummary {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub mime: String,
    pub size: u64,
}

impl From<&SessionInput> for SessionInputSummary {
    fn from(input: &SessionInput) -> Self {
        Self {
            id: input.id.clone(),
            name: input.name.clone(),
            kind: input.kind.clone(),
            mime: input.mime.clone(),
            size: input.size,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectoryPurpose {
    Source,
    TargetBase,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MatchResolution {
    SelectGroup {
        #[serde(rename = "groupId")]
        group_id: String,
    },
    AcceptPartial,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionResult {
    pub ready_to_copy: bool,
    pub confirmation_token: Option<String>,
    pub copy_job: Option<CopyLaunch>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyLaunch {
    pub job_id: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettingsResult {
    pub settings: AppSettings,
    pub providers: Vec<ProviderProfile>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockingIssue {
    Ambiguous,
    PartialFormat,
    Missing,
    TargetConflict,
    PermissionDenied,
    InsufficientSpace,
    SourceChanged,
    SourceDisconnected,
    TargetDisconnected,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModalAction {
    Recheck,
    Cancel,
    Manual,
    Close,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyPausedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub issue: BlockingIssue,
    pub title: String,
    pub message: String,
    pub affected: Vec<String>,
    pub actions: Vec<ModalAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_disconnect: Option<TargetDisconnectInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyCompletionReport {
    pub job_id: String,
    pub status: CompletionStatus,
    pub copied_count: usize,
    pub skipped_identical_count: usize,
    pub skipped_user_count: usize,
    pub failed_count: usize,
    pub copied_bytes: u64,
    pub source: String,
    pub target: String,
    pub started_at: Option<String>,
    pub finished_at: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyProgressPayload {
    pub job_id: String,
    pub current_file: String,
    pub completed_files: usize,
    pub total_files: usize,
    pub copied_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanPhase {
    Scanning,
    Complete,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgressPayload {
    pub phase: ScanPhase,
    pub checked_files: usize,
    pub matched_files: usize,
    pub scanned_directories: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent<'a, T> {
    pub session_id: &'a str,
    pub payload: &'a T,
}

impl<'a, T> Clone for SessionEvent<'a, T> {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id,
            payload: self.payload,
        }
    }
}

pub struct AppState {
    pub storage: Storage,
    pub app_data_dir: PathBuf,
    cancelled: Mutex<HashMap<String, Arc<AtomicBool>>>,
    provider_credentials: Mutex<()>,
}

impl AppState {
    pub fn open(app: &AppHandle) -> Result<Self, CommandError> {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| CommandError::Other(error.to_string()))?;
        fs::create_dir_all(&app_data_dir)?;
        let storage = Storage::open(&app_data_dir.join("state.sqlite3"))?;
        storage.recover_interrupted_sessions()?;
        Ok(Self {
            storage,
            app_data_dir,
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        })
    }
}

fn validate_session_id(value: &str) -> Result<String, CommandError> {
    Ok(Uuid::parse_str(value)
        .map_err(|_| CommandError::InvalidSessionId)?
        .to_string())
}

fn cleanup_session_inputs(app_data_dir: &Path, session_id: &str) -> Result<(), std::io::Error> {
    let input_dir = app_data_dir
        .join("sessions")
        .join(session_id)
        .join("inputs");
    match fs::remove_dir_all(input_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn finalize_copy_session(
    state: &AppState,
    session_id: &str,
    plan_revision: i64,
    status: SessionStatus,
) -> Result<Option<String>, CommandError> {
    debug_assert!(matches!(
        status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ));
    state.storage.finish_copy_attempt(
        session_id,
        plan_revision,
        status,
        Some("copy-aborted"),
        Some("copy attempt stopped before this item completed"),
    )?;
    if let Err(error) = cleanup_session_inputs(&state.app_data_dir, session_id) {
        return Ok(Some(error.to_string()));
    }
    Ok(state
        .storage
        .delete_session_inputs(session_id)
        .err()
        .map(|error| error.to_string()))
}

fn load_session(state: &AppState, id: &str) -> Result<Session, CommandError> {
    let id = validate_session_id(id)?;
    let mut session = state.storage.load_session(&id)?;
    let numbers = state.storage.load_numbers(&id)?;
    session.numbers_confirmed =
        !numbers.is_empty() && numbers.iter().all(|number| number.confirmed);
    Ok(session)
}

fn settings(state: &AppState) -> Result<AppSettings, CommandError> {
    Ok(state
        .storage
        .load_setting("app-settings")?
        .unwrap_or_default())
}

fn default_target(source: &Path) -> Result<PathBuf, CommandError> {
    let parent = source.parent().ok_or(CommandError::InvalidDirectory)?;
    Ok(parent.join("照片成片").join("待精修的原片"))
}

fn map_network_path_error(error: crate::network_paths::NetworkPathError) -> CommandError {
    match error {
        crate::network_paths::NetworkPathError::Cancelled => CommandError::SmbLoginCancelled,
        crate::network_paths::NetworkPathError::CredentialConflict => {
            CommandError::SmbCredentialConflict
        }
        crate::network_paths::NetworkPathError::AccessDenied(_)
        | crate::network_paths::NetworkPathError::Unavailable(_) => CommandError::SmbUnavailable,
        crate::network_paths::NetworkPathError::InvalidPath => CommandError::InvalidDirectory,
    }
}

#[cfg(windows)]
fn owner_hwnd(app: &AppHandle) -> Result<isize, CommandError> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| CommandError::Other("主窗口不可用".into()))?;
    window
        .hwnd()
        .map(|handle| handle.0 as isize)
        .map_err(|error| CommandError::Other(format!("主窗口句柄不可用：{error}")))
}

#[cfg(windows)]
fn interactive_connect_mode(app: &AppHandle) -> Result<ConnectMode, CommandError> {
    Ok(ConnectMode::AllowCredentialPrompt {
        owner: owner_hwnd(app)?,
    })
}

#[cfg(not(windows))]
fn interactive_connect_mode(_app: &AppHandle) -> Result<ConnectMode, CommandError> {
    Ok(ConnectMode::SilentOnly)
}

fn prepare_directory_for_user(
    app: &AppHandle,
    session_id: &str,
    stage: &str,
    path: &Path,
) -> Result<PathBuf, CommandError> {
    let mode = match interactive_connect_mode(app) {
        Ok(mode) => mode,
        Err(error) => {
            let (path_kind, network_target) =
                if diagnostics::anonymized_network_target(path).is_some() {
                    (DiagnosticPathKind::Unc, Some(path))
                } else {
                    (DiagnosticPathKind::Local, None)
                };
            diagnostics::warn_network_operation(
                Some(session_id),
                stage,
                path_kind,
                network_target,
                None,
                "failed",
            );
            return Err(error);
        }
    };
    match network_paths::prepare_directory_with_diagnostics(path, mode) {
        Ok(prepared) => {
            diagnostics::info_network_operation(
                Some(session_id),
                stage,
                prepared.kind.into(),
                prepared.network_target.as_deref(),
                None,
                "succeeded",
            );
            Ok(prepared.path)
        }
        Err(error) => {
            let windows_code = error.error.windows_code();
            diagnostics::warn_network_operation(
                Some(session_id),
                stage,
                error.kind.into(),
                error.network_target.as_deref(),
                windows_code,
                "failed",
            );
            Err(map_network_path_error(error.error))
        }
    }
}

fn prepare_target_directory_for_user(
    app: &AppHandle,
    session_id: &str,
    stage: &str,
    path: &Path,
) -> Result<PathBuf, CommandError> {
    let mode = interactive_connect_mode(app)?;
    match network_paths::prepare_target_directory_with_diagnostics(path, mode) {
        Ok(prepared) => {
            diagnostics::info_network_operation(
                Some(session_id),
                stage,
                prepared.kind.into(),
                prepared.network_target.as_deref(),
                None,
                "succeeded",
            );
            Ok(prepared.path)
        }
        Err(error) => {
            diagnostics::warn_network_operation(
                Some(session_id),
                stage,
                error.kind.into(),
                error.network_target.as_deref(),
                error.error.windows_code(),
                "failed",
            );
            Err(map_network_path_error(error.error))
        }
    }
}

fn map_source_scan_prepare_error(error: CommandError) -> CommandError {
    match error {
        CommandError::InvalidDirectory | CommandError::SmbUnavailable => {
            CommandError::SourceDirectoryUnavailable
        }
        error => error,
    }
}

fn map_target_scan_prepare_error(error: CommandError) -> CommandError {
    match error {
        CommandError::InvalidDirectory | CommandError::SmbUnavailable => {
            CommandError::TargetDirectoryUnavailable
        }
        error => error,
    }
}

fn map_scan_error(error: crate::matching::MatchError) -> CommandError {
    if matches!(
        error.io_kind(),
        Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
    ) {
        CommandError::SourceDirectoryUnavailable
    } else {
        CommandError::Other(error.to_string())
    }
}

fn safe_scan_failure_event(
    session_id: &str,
    source: &Path,
    error: &crate::matching::MatchError,
) -> Option<String> {
    let stage = error.diagnostic_stage()?;
    let network_target = diagnostics::anonymized_network_target(source)
        .is_some()
        .then_some(source);
    let path_kind = if network_target.is_some() {
        DiagnosticPathKind::Unc
    } else {
        DiagnosticPathKind::Local
    };
    let windows_code = error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok());
    Some(diagnostics::safe_network_operation_event(
        Some(session_id),
        stage,
        path_kind,
        network_target,
        windows_code,
        "failed",
    ))
}

fn warn_scan_failure(session_id: &str, source: &Path, error: &crate::matching::MatchError) {
    if let Some(event) = safe_scan_failure_event(session_id, source, error) {
        log::warn!(target: diagnostics::LOG_TARGET, "{event}");
    }
}

fn bound_paths(state: &AppState, id: &str) -> Result<BoundPaths, CommandError> {
    let (source, target) = state.storage.load_bound_paths(id)?;
    Ok(BoundPaths {
        source: source.map(|path| path.to_string_lossy().into_owned()),
        target: target.map(|path| path.to_string_lossy().into_owned()),
    })
}

fn bind_selected_directory_with(
    state: &AppState,
    id: &str,
    purpose: DirectoryPurpose,
    selected: &Path,
    prepare: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
) -> Result<BoundPaths, CommandError> {
    let selected = prepare(selected)?;
    match purpose {
        DirectoryPurpose::Source => {
            let target = default_target(&selected)?;
            state
                .storage
                .bind_paths(id, Some(&selected), Some(&target))?;
        }
        DirectoryPurpose::TargetBase => {
            let target = selected.join("照片成片").join("待精修的原片");
            state.storage.bind_paths(id, None, Some(&target))?;
        }
    }
    bound_paths(state, id)
}

fn parse_manual_directory(value: &str) -> Result<PathBuf, CommandError> {
    let value = value.trim().trim_matches('"').trim();
    if value.is_empty() || value.contains('\0') {
        return Err(CommandError::InvalidDirectory);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(CommandError::InvalidDirectory);
    }
    Ok(path)
}

fn issue_confirmation(state: &AppState, id: &str) -> Result<String, CommandError> {
    Ok(state.storage.issue_confirmation(id)?)
}

fn emit_session<T: Serialize>(
    app: &AppHandle,
    event: &str,
    session_id: &str,
    payload: &T,
) -> Result<(), CommandError> {
    app.emit_to(
        "main",
        event,
        SessionEvent {
            session_id,
            payload,
        },
    )
    .map_err(|error| CommandError::Other(error.to_string()))
}

fn redacted_path(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "所选目录".into())
}

fn pause_payload(
    issue: BlockingIssue,
    title: &str,
    message: &str,
    affected: Vec<String>,
) -> CopyPausedPayload {
    CopyPausedPayload {
        job_id: None,
        issue,
        title: title.into(),
        message: message.into(),
        affected,
        actions: vec![ModalAction::Recheck, ModalAction::Cancel],
        target_disconnect: None,
    }
}

fn completion_payload(
    job_id: &str,
    status: CompletionStatus,
    snapshot: &ScanSnapshot,
    copied_count: usize,
    skipped_count: usize,
    failed_count: usize,
    copied_bytes: u64,
    started_at: &str,
    message: &str,
) -> CopyCompletionReport {
    CopyCompletionReport {
        job_id: job_id.into(),
        status,
        copied_count,
        skipped_identical_count: skipped_count,
        skipped_user_count: snapshot.skipped_numbers.len(),
        failed_count,
        copied_bytes,
        source: redacted_path(&snapshot.source),
        target: redacted_path(&snapshot.target),
        started_at: Some(started_at.into()),
        finished_at: chrono::Utc::now().to_rfc3339(),
        message: message.into(),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CompletionStats {
    copied_count: usize,
    skipped_identical_count: usize,
    failed_count: usize,
    copied_bytes: u64,
}

fn completion_stats(
    snapshot: &ScanSnapshot,
    history: &[CopyHistoryItem],
    plan_revision: i64,
) -> CompletionStats {
    let sizes = plan(snapshot)
        .files
        .into_iter()
        .map(|file| ((file.source, file.target), file.fingerprint.size))
        .collect::<HashMap<_, _>>();
    let mut stats = CompletionStats::default();
    for item in history
        .iter()
        .filter(|item| item.plan_revision == plan_revision)
    {
        match item.status {
            CopyItemStatus::Copied => {
                stats.copied_count += 1;
                stats.copied_bytes = stats.copied_bytes.saturating_add(
                    sizes
                        .get(&(item.source.clone(), item.target.clone()))
                        .copied()
                        .unwrap_or(0),
                );
            }
            CopyItemStatus::Skipped
                if item.skipped_reason.as_deref() == Some("identical-target") =>
            {
                stats.skipped_identical_count += 1;
            }
            CopyItemStatus::Failed => stats.failed_count += 1,
            _ => {}
        }
    }
    stats
}

#[tauri::command]
pub fn create_session(
    state: State<'_, AppState>,
    task_label: String,
    note: Option<String>,
) -> Result<Session, CommandError> {
    Ok(state.storage.create_session(&task_label, note.as_deref())?)
}

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> Result<Vec<Session>, CommandError> {
    Ok(state.storage.list_sessions()?)
}

#[tauri::command]
pub fn load_settings(state: State<'_, AppState>) -> Result<AppSettings, CommandError> {
    settings(&state)
}

#[tauri::command]
pub fn list_providers(state: State<'_, AppState>) -> Result<Vec<ProviderProfile>, CommandError> {
    Ok(state.storage.list_providers()?)
}

#[tauri::command]
pub fn provider_templates() -> Vec<providers::ProviderTemplate> {
    providers::provider_templates()
}

#[tauri::command]
pub fn open_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<SessionWorkflow, CommandError> {
    let id = validate_session_id(&session_id)?;
    let session = load_session(&state, &id)?;
    let numbers = state.storage.load_numbers(&id)?;
    let inputs = state
        .storage
        .list_session_inputs(&id)?
        .iter()
        .map(SessionInputSummary::from)
        .collect();
    let bindings = bound_paths(&state, &id)?;
    let snapshot = state.storage.load_scan_snapshot(&id)?;
    let preflight = state.storage.load_preflight(&id)?;
    let copy_items = state.storage.load_copy_items(&id)?;
    let requires_second_confirmation = settings(&state)?.second_confirmation_enabled
        && session.status == SessionStatus::ReadyToCopy;
    let confirmation_token = if requires_second_confirmation {
        Some(issue_confirmation(&state, &id)?)
    } else {
        None
    };
    let source_available = bindings.source.as_ref().is_some_and(|value| {
        let path = Path::new(value);
        path.canonicalize()
            .ok()
            .as_deref()
            .is_some_and(|canonical| canonical == path && canonical.is_dir())
    });
    let target_available = bindings.target.as_ref().is_some_and(|value| {
        let path = Path::new(value);
        path.is_dir() || path.parent().is_some_and(Path::is_dir)
    });
    Ok(SessionWorkflow {
        session,
        numbers,
        inputs,
        bindings,
        snapshot,
        preflight,
        copy_items,
        requires_second_confirmation,
        confirmation_token,
        source_available,
        target_available,
    })
}

const INPUT_METADATA_HEADER: &str = "x-photo-input-metadata";
const MAX_INPUT_METADATA_BYTES: usize = 8 * 1024;
const MAX_INPUT_METADATA_HEADER_BYTES: usize = MAX_INPUT_METADATA_BYTES.div_ceil(3) * 4;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionInputMetadata {
    session_id: String,
    name: String,
    kind: String,
}

fn parse_session_input_request<'a>(
    metadata_header: Option<&str>,
    body: &'a tauri::ipc::InvokeBody,
) -> Result<(SessionInputMetadata, &'a [u8]), CommandError> {
    use base64::Engine;

    let bytes = match body {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.as_slice(),
        tauri::ipc::InvokeBody::Json(_) => {
            return Err(CommandError::Other("输入必须使用原始二进制传输".into()))
        }
    };
    // This absolute bound is checked while the request body is still borrowed.
    // Do not clone the raw body until every metadata and kind-specific check passes.
    if bytes.len() > MAX_IMAGE_INPUT_BYTES {
        return Err(CommandError::Other("输入文件过大".into()));
    }
    let encoded = metadata_header
        .filter(|value| !value.is_empty() && value.len() <= MAX_INPUT_METADATA_HEADER_BYTES)
        .ok_or_else(|| CommandError::Other("输入元数据缺失或过长".into()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| CommandError::Other("输入元数据编码无效".into()))?;
    if decoded.len() > MAX_INPUT_METADATA_BYTES {
        return Err(CommandError::Other("输入元数据过长".into()));
    }
    let mut metadata: SessionInputMetadata = serde_json::from_slice(&decoded)
        .map_err(|_| CommandError::Other("输入元数据格式无效".into()))?;
    metadata.session_id = validate_session_id(&metadata.session_id)?;
    let kind_limit = match metadata.kind.as_str() {
        "image" => MAX_IMAGE_INPUT_BYTES,
        "text" => MAX_TEXT_INPUT_BYTES,
        _ => return Err(CommandError::Other("输入类型无效".into())),
    };
    if bytes.len() > kind_limit {
        return Err(CommandError::Other("输入文件过大".into()));
    }
    Ok((metadata, bytes))
}

#[tauri::command]
pub fn save_session_input(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<SessionInputSummary, CommandError> {
    let metadata_header = request
        .headers()
        .get(INPUT_METADATA_HEADER)
        .and_then(|value| value.to_str().ok());
    let (metadata, borrowed_bytes) = parse_session_input_request(metadata_header, request.body())?;
    let is_screenshot = metadata.kind == "image";
    let diagnostic_session_id = metadata.session_id.clone();
    let result = save_borrowed_session_input_with(
        &state,
        metadata,
        borrowed_bytes,
        validate_session_input,
        <[u8]>::to_vec,
    );
    if is_screenshot && result.is_err() {
        diagnostics::warn_operation(
            Some(&diagnostic_session_id),
            "screenshot-save",
            DiagnosticPathKind::Local,
            None,
            "failed",
        );
    }
    result
}

fn save_borrowed_session_input_with<V, C>(
    state: &AppState,
    metadata: SessionInputMetadata,
    borrowed_bytes: &[u8],
    validate_content: V,
    clone_content: C,
) -> Result<SessionInputSummary, CommandError>
where
    V: FnOnce(&str, &[u8]) -> Result<&'static str, CommandError>,
    C: FnOnce(&[u8]) -> Vec<u8>,
{
    // Authorize the session with a read-only query before image decoding or
    // cloning the potentially 24 MiB IPC body.
    state
        .storage
        .validate_session_input_state(&metadata.session_id)?;
    let mime = validate_content(&metadata.kind, borrowed_bytes)?;
    let bytes = clone_content(borrowed_bytes);
    save_session_input_prevalidated(
        state,
        &metadata.session_id,
        metadata.name,
        metadata.kind,
        bytes,
        mime,
    )
}

#[cfg(test)]
fn save_session_input_inner(
    state: &AppState,
    id: &str,
    name: String,
    kind: String,
    bytes: Vec<u8>,
) -> Result<SessionInputSummary, CommandError> {
    state.storage.validate_session_input_state(id)?;
    // Validate every user-controlled byte before prepare_session_input advances
    // the workflow or any session directory/file is created.
    let mime = validate_session_input(&kind, &bytes)?;
    save_session_input_prevalidated(state, id, name, kind, bytes, mime)
}

fn save_session_input_prevalidated(
    state: &AppState,
    id: &str,
    name: String,
    kind: String,
    bytes: Vec<u8>,
    mime: &'static str,
) -> Result<SessionInputSummary, CommandError> {
    state.storage.prepare_session_input(id)?;
    let safe_name = Path::new(&name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("input")
        .to_string();
    let input_dir = state.app_data_dir.join("sessions").join(id).join("inputs");
    fs::create_dir_all(&input_dir)?;
    let input_id = Uuid::new_v4().to_string();
    let stored_name = input_id.clone();
    let path = input_dir.join(&stored_name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&path)?;
    use std::io::{Seek, SeekFrom, Write};
    file.write_all(&bytes)?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    let fingerprint = capture_file_fingerprint(&mut file)
        .map_err(|error| CommandError::Other(error.to_string()))?;
    let input = SessionInput {
        id: input_id,
        session_id: id.into(),
        name: safe_name,
        kind,
        mime: mime.into(),
        stored_name,
        size: bytes.len() as u64,
        root_identity: capture_directory_identity(&input_dir)
            .map_err(|error| CommandError::Other(error.to_string()))?,
        fingerprint,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(error) = state.storage.save_session_input(&input) {
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(SessionInputSummary::from(&input))
}

const MAX_IMAGE_INPUT_BYTES: usize = providers::MAX_RECOGNITION_IMAGE_BYTES;
const MAX_TEXT_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 50_000_000;
const MAX_IMAGE_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
// image-webp 0.2.0 does not honor image::Limits for all internal buffers.
// 20M pixels leaves roughly 32+ MiB below our 256 MiB target when budgeting
// a conservative 10 bytes/pixel plus the maximum 24 MiB compressed input.
const MAX_STATIC_WEBP_PIXELS: u64 = 20_000_000;

fn validate_session_input(kind: &str, bytes: &[u8]) -> Result<&'static str, CommandError> {
    match kind {
        "text" => {
            if bytes.is_empty() || bytes.len() > MAX_TEXT_INPUT_BYTES {
                return Err(CommandError::Other("文字输入为空或过大".into()));
            }
            std::str::from_utf8(bytes)
                .map_err(|_| CommandError::Other("文字输入编码无效".into()))?;
            Ok("text/plain")
        }
        "image" => validate_image_input(bytes),
        _ => Err(CommandError::Other("输入类型无效".into())),
    }
}

fn image_decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_ALLOC);
    limits
}

fn image_dimension_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_DECODE_ALLOC);
    limits
}

fn validate_image_input(bytes: &[u8]) -> Result<&'static str, CommandError> {
    use std::io::Cursor;

    if bytes.is_empty() || bytes.len() > MAX_IMAGE_INPUT_BYTES {
        return Err(CommandError::Other("导入图片为空或超过 24 MiB".into()));
    }
    let (format, mime) = if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        if !jpeg_has_exact_eoi(bytes) {
            return Err(CommandError::Other("导入图片内容损坏或不完整".into()));
        }
        (image::ImageFormat::Jpeg, "image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if !png_has_valid_structure(bytes) {
            return Err(CommandError::Other("导入图片内容损坏或不完整".into()));
        }
        (image::ImageFormat::Png, "image/png")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let declared = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed-size slice"));
        if usize::try_from(declared)
            .ok()
            .and_then(|size| size.checked_add(8))
            != Some(bytes.len())
        {
            return Err(CommandError::Other("导入图片内容损坏或不完整".into()));
        }
        validate_static_webp_structure(bytes)?;
        (image::ImageFormat::WebP, "image/webp")
    } else {
        return Err(CommandError::Other(
            "导入图片格式无效，仅支持 JPEG、PNG、WebP".into(),
        ));
    };

    let mut dimensions_reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    dimensions_reader.limits(image_dimension_limits());
    let (width, height) = dimensions_reader
        .into_dimensions()
        .map_err(|_| CommandError::Other("导入图片内容损坏或不完整".into()))?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| CommandError::Other("导入图片尺寸过大".into()))?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_IMAGE_PIXELS
    {
        return Err(CommandError::Other(
            "导入图片尺寸过大（最大边长 20000，最多 5000 万像素）".into(),
        ));
    }

    let mut decode_reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    decode_reader.limits(image_decode_limits());
    decode_reader
        .decode()
        .map_err(|_| CommandError::Other("导入图片内容损坏或无法安全解码".into()))?;
    Ok(mime)
}

fn jpeg_has_exact_eoi(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) {
        return false;
    }
    let mut offset = 2_usize;
    let mut in_scan = false;
    loop {
        if in_scan {
            while offset < bytes.len() && bytes[offset] != 0xff {
                offset += 1;
            }
            if offset >= bytes.len() {
                return false;
            }
        } else if bytes.get(offset) != Some(&0xff) {
            return false;
        }

        offset += 1;
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let Some(&marker) = bytes.get(offset) else {
            return false;
        };
        offset += 1;

        let marker_from_scan = in_scan;
        if marker_from_scan {
            match marker {
                0x00 | 0x01 | 0xd0..=0xd7 => continue,
                0xd9 => return offset == bytes.len(),
                _ => in_scan = false,
            }
        }

        match marker {
            0xd9 => return offset == bytes.len(),
            0xda => {
                if !advance_jpeg_segment(bytes, &mut offset) {
                    return false;
                }
                in_scan = true;
            }
            // DNL can appear inside entropy data and the scan resumes after
            // its length-prefixed payload.
            0xdc if marker_from_scan => {
                if !advance_jpeg_segment(bytes, &mut offset) {
                    return false;
                }
                in_scan = true;
            }
            0x01 => {}
            0x00 | 0xd0..=0xd8 => return false,
            _ => {
                if !advance_jpeg_segment(bytes, &mut offset) {
                    return false;
                }
            }
        }
    }
}

fn advance_jpeg_segment(bytes: &[u8], offset: &mut usize) -> bool {
    let Some(length_bytes) = bytes.get(*offset..offset.saturating_add(2)) else {
        return false;
    };
    let length = u16::from_be_bytes([length_bytes[0], length_bytes[1]]) as usize;
    if length < 2 {
        return false;
    }
    let Some(end) = offset.checked_add(length) else {
        return false;
    };
    if end > bytes.len() {
        return false;
    }
    *offset = end;
    true
}

fn png_has_valid_structure(bytes: &[u8]) -> bool {
    let mut offset = 8_usize;
    let mut chunk_index = 0_usize;
    let mut seen_ihdr = false;
    let mut seen_plte = false;
    let mut seen_idat = false;
    let mut idat_closed = false;
    let mut color_type = None;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("fixed-size slice"),
        ) as usize;
        let Some(data_start) = offset.checked_add(8) else {
            return false;
        };
        let Some(data_end) = data_start.checked_add(length) else {
            return false;
        };
        let Some(chunk_end) = data_end.checked_add(4) else {
            return false;
        };
        if chunk_end > bytes.len() {
            return false;
        }
        let kind = &bytes[offset + 4..offset + 8];
        if !kind.iter().all(u8::is_ascii_alphabetic) || kind[2].is_ascii_lowercase() {
            return false;
        }
        let stored_crc =
            u32::from_be_bytes(bytes[data_end..chunk_end].try_into().expect("CRC slice"));
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(kind);
        hasher.update(&bytes[data_start..data_end]);
        if hasher.finalize() != stored_crc {
            return false;
        }

        match kind {
            b"IHDR" => {
                if chunk_index != 0 || seen_ihdr || length != 13 {
                    return false;
                }
                seen_ihdr = true;
                color_type = Some(bytes[data_start + 9]);
            }
            b"PLTE" => {
                if !seen_ihdr
                    || seen_plte
                    || seen_idat
                    || length == 0
                    || length > 256 * 3
                    || length % 3 != 0
                    || matches!(color_type, Some(0 | 4))
                {
                    return false;
                }
                seen_plte = true;
            }
            b"IDAT" => {
                if !seen_ihdr || idat_closed || matches!(color_type, Some(3)) && !seen_plte {
                    return false;
                }
                seen_idat = true;
            }
            b"IEND" => {
                return seen_ihdr
                    && seen_idat
                    && (!matches!(color_type, Some(3)) || seen_plte)
                    && length == 0
                    && chunk_end == bytes.len();
            }
            _ => {
                // PNG defines any unrecognized chunk with an uppercase first
                // letter as critical; it is unsafe to decode while ignoring it.
                if !seen_ihdr || kind[0].is_ascii_uppercase() {
                    return false;
                }
                if seen_idat {
                    idat_closed = true;
                }
            }
        }
        chunk_index += 1;
        offset = chunk_end;
    }
    false
}

fn validate_static_webp_structure(bytes: &[u8]) -> Result<(), CommandError> {
    let mut offset = 12_usize;
    let mut chunk_index = 0_usize;
    let mut seen_vp8x = false;
    let mut image_chunks = 0_u8;

    while offset < bytes.len() {
        let Some(header_end) = offset.checked_add(8) else {
            return Err(invalid_image_content());
        };
        if header_end > bytes.len() {
            return Err(invalid_image_content());
        }
        let kind: [u8; 4] = bytes[offset..offset + 4]
            .try_into()
            .expect("WebP FourCC slice");
        let length =
            u32::from_le_bytes(bytes[offset + 4..header_end].try_into().expect("WebP size"))
                as usize;
        let data_start = header_end;
        let Some(data_end) = data_start.checked_add(length) else {
            return Err(invalid_image_content());
        };
        let Some(chunk_end) = data_end.checked_add(length % 2) else {
            return Err(invalid_image_content());
        };
        if chunk_end > bytes.len() {
            return Err(invalid_image_content());
        }
        let payload = &bytes[data_start..data_end];

        match &kind {
            b"ANIM" | b"ANMF" => return Err(animated_webp_error()),
            b"VP8X" => {
                if chunk_index != 0 || seen_vp8x || payload.len() != 10 {
                    return Err(invalid_image_content());
                }
                if payload[0] & 0x02 != 0 {
                    return Err(animated_webp_error());
                }
                seen_vp8x = true;
                validate_webp_dimensions(
                    read_u24_le(&payload[4..7]) + 1,
                    read_u24_le(&payload[7..10]) + 1,
                )?;
            }
            b"VP8 " => {
                if payload.len() < 10 || payload[0] & 1 != 0 || payload[3..6] != [0x9d, 0x01, 0x2a]
                {
                    return Err(invalid_image_content());
                }
                image_chunks = image_chunks.saturating_add(1);
                validate_webp_dimensions(
                    u32::from(u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff),
                    u32::from(u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff),
                )?;
            }
            b"VP8L" => {
                if payload.len() < 5 || payload[0] != 0x2f {
                    return Err(invalid_image_content());
                }
                let bits = u32::from_le_bytes(payload[1..5].try_into().expect("VP8L bits"));
                if bits >> 29 != 0 {
                    return Err(invalid_image_content());
                }
                image_chunks = image_chunks.saturating_add(1);
                validate_webp_dimensions((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)?;
            }
            _ => {}
        }
        if image_chunks > 1 {
            return Err(invalid_image_content());
        }
        offset = chunk_end;
        chunk_index += 1;
    }

    if offset != bytes.len() || image_chunks != 1 {
        return Err(invalid_image_content());
    }
    Ok(())
}

fn read_u24_le(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn validate_webp_dimensions(width: u32, height: u32) -> Result<(), CommandError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(webp_too_large_error)?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels > MAX_STATIC_WEBP_PIXELS
    {
        return Err(webp_too_large_error());
    }
    Ok(())
}

fn invalid_image_content() -> CommandError {
    CommandError::Other("导入图片内容损坏或不完整".into())
}

fn animated_webp_error() -> CommandError {
    CommandError::Other("不支持动画 WebP，请先转换为静态图片".into())
}

fn webp_too_large_error() -> CommandError {
    CommandError::Other("WebP 图片像素过大（最多 2000 万像素）".into())
}

#[tauri::command]
pub fn list_session_inputs(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<SessionInputSummary>, CommandError> {
    let id = validate_session_id(&session_id)?;
    load_session(&state, &id)?;
    Ok(state
        .storage
        .list_session_inputs(&id)?
        .iter()
        .map(SessionInputSummary::from)
        .collect())
}

#[tauri::command]
pub fn read_session_input(
    state: State<'_, AppState>,
    session_id: String,
    input_id: String,
) -> Result<tauri::ipc::Response, CommandError> {
    let id = validate_session_id(&session_id)?;
    Ok(tauri::ipc::Response::new(read_session_input_inner(
        &state, &id, &input_id,
    )?))
}

fn read_session_input_inner(
    state: &AppState,
    id: &str,
    input_id: &str,
) -> Result<Vec<u8>, CommandError> {
    let input = state.storage.load_session_input(id, input_id)?;
    let input_dir = state.app_data_dir.join("sessions").join(id).join("inputs");
    let selected = SelectedFile {
        canonical_number: String::new(),
        source: input_dir.join(&input.stored_name),
        target: PathBuf::new(),
        fingerprint: input.fingerprint,
    };
    let bytes = read_bound_source_file(
        &input_dir,
        &input.root_identity,
        &selected,
        if input.kind == "image" {
            MAX_IMAGE_INPUT_BYTES as u64
        } else {
            MAX_TEXT_INPUT_BYTES as u64
        },
    )
    .map_err(|error| CommandError::Other(error.to_string()))?;
    Ok(bytes)
}

#[tauri::command]
pub fn save_confirmed_numbers(
    state: State<'_, AppState>,
    session_id: String,
    numbers: Vec<PhotoNumber>,
) -> Result<(), CommandError> {
    let id = validate_session_id(&session_id)?;
    state.storage.save_confirmed_numbers(&id, &numbers)?;
    Ok(())
}

#[tauri::command]
pub async fn choose_directory(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    purpose: DirectoryPurpose,
) -> Result<BoundPaths, CommandError> {
    let id = validate_session_id(&session_id)?;
    load_session(&state, &id)?;
    let selected = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .ok_or(CommandError::DialogCancelled)?
        .into_path()
        .map_err(|_| CommandError::InvalidDirectory)?;
    bind_selected_directory_with(&state, &id, purpose, &selected, |path| {
        prepare_directory_for_user(&app, &id, "directory-prepare", path)
    })
}

#[tauri::command]
pub fn bind_manual_directory(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    purpose: DirectoryPurpose,
    path: String,
) -> Result<BoundPaths, CommandError> {
    let id = validate_session_id(&session_id)?;
    load_session(&state, &id)?;
    let selected = parse_manual_directory(&path)?;
    bind_selected_directory_with(&state, &id, purpose, &selected, |path| {
        prepare_directory_for_user(&app, &id, "manual-directory-prepare", path)
    })
}

#[tauri::command]
pub fn default_target_for_source(
    state: State<'_, AppState>,
    session_id: String,
    source: String,
) -> Result<String, CommandError> {
    let id = validate_session_id(&session_id)?;
    let source = PathBuf::from(source)
        .canonicalize()
        .map_err(|_| CommandError::InvalidDirectory)?;
    let (bound_source, _) = state.storage.load_bound_paths(&id)?;
    if bound_source.as_deref() != Some(source.as_path()) {
        return Err(CommandError::PathNotAuthorized);
    }
    let target = default_target(&source)?;
    state.storage.bind_paths(&id, None, Some(&target))?;
    Ok(target.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn target_under_selected_base(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<String, CommandError> {
    let paths = choose_directory(app, state, session_id, DirectoryPurpose::TargetBase).await?;
    paths.target.ok_or(CommandError::InvalidDirectory)
}

fn blocking_report(snapshot: &ScanSnapshot) -> PreflightReport {
    let skipped: HashSet<&str> = snapshot
        .skipped_numbers
        .iter()
        .map(String::as_str)
        .collect();
    let mut report = PreflightReport::default();
    for item in &snapshot.items {
        if skipped.contains(item.canonical_number.as_str()) {
            continue;
        }
        match item.status {
            MatchStatus::Ambiguous => report.ambiguous.push(item.canonical_number.clone()),
            MatchStatus::Partial => report.partial.push(item.canonical_number.clone()),
            MatchStatus::Missing => report.missing.push(item.canonical_number.clone()),
            MatchStatus::Complete => {}
        }
    }
    report
}

fn plan(snapshot: &ScanSnapshot) -> CopyPlan {
    let skipped: HashSet<&str> = snapshot
        .skipped_numbers
        .iter()
        .map(String::as_str)
        .collect();
    CopyPlan {
        source_root: snapshot.source.clone(),
        source_root_identity: snapshot.source_root_identity.clone(),
        target_root: snapshot.target.clone(),
        files: snapshot
            .items
            .iter()
            .filter(|item| !skipped.contains(item.canonical_number.as_str()))
            .flat_map(|item| item.groups.iter().flat_map(|group| group.files.iter()))
            .map(|file| SelectedFile {
                canonical_number: file.canonical_number.clone(),
                source: file.path.clone(),
                target: snapshot
                    .target
                    .join(file.path.file_name().unwrap_or_default()),
                fingerprint: file.fingerprint.clone(),
            })
            .collect(),
    }
}

fn copy_plan_items(session_id: &str, snapshot: &ScanSnapshot) -> Vec<CopyHistoryItem> {
    let mut items = plan(snapshot)
        .files
        .iter()
        .map(|file| {
            CopyHistoryItem::planned(
                session_id,
                &file.canonical_number,
                &file.source,
                &file.target,
                &file.fingerprint.content_hash,
            )
        })
        .collect::<Vec<_>>();
    for canonical in &snapshot.skipped_numbers {
        let mut skipped = CopyHistoryItem::planned(
            session_id,
            canonical,
            &snapshot.source,
            &snapshot.target,
            "",
        );
        skipped.status = CopyItemStatus::Skipped;
        skipped.skipped_reason = Some("user-skipped".into());
        items.push(skipped);
    }
    items
}

fn validate_snapshot_binding(
    state: &AppState,
    id: &str,
    snapshot: &ScanSnapshot,
) -> Result<(), CommandError> {
    let (source, target) = state.storage.load_bound_paths(id)?;
    if source.as_deref() != Some(snapshot.source.as_path())
        || target.as_deref() != Some(snapshot.target.as_path())
        || snapshot.items.iter().any(|item| {
            item.groups.iter().any(|group| {
                group
                    .files
                    .iter()
                    .any(|file| !file.path.starts_with(&snapshot.source))
            })
        })
    {
        return Err(CommandError::PathNotAuthorized);
    }
    Ok(())
}

fn claim_copy_start_with(
    state: &AppState,
    id: &str,
    confirmation_token: Option<&str>,
    prepare_source: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    prepare_target: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
) -> Result<(ScanSnapshot, i64), CommandError> {
    let (snapshot, plan_revision) = state
        .storage
        .load_scan_snapshot_with_revision(id)?
        .ok_or_else(|| CommandError::Other("请先扫描并处理候选".into()))?;
    let prepared_source = prepare_source(&snapshot.source)?;
    if prepared_source != snapshot.source {
        return Err(CommandError::PathNotAuthorized);
    }
    let prepared_target = prepare_target(&snapshot.target)?;
    if prepared_target != snapshot.target {
        return Err(CommandError::PathNotAuthorized);
    }
    validate_snapshot_binding(state, id, &snapshot)?;
    if blocking_report(&snapshot).has_blocking_issue() {
        return Err(CommandError::Other("仍有匹配项需要人工处理".into()));
    }
    let confirmation_required = settings(state)?.second_confirmation_enabled;
    match state.storage.consume_confirmation_and_start_copy(
        id,
        plan_revision,
        confirmation_token,
        confirmation_required,
    ) {
        Ok(()) => Ok((snapshot, plan_revision)),
        Err(StorageError::ConfirmationRequired) => Err(CommandError::ConfirmationRequired),
        Err(error) => Err(error.into()),
    }
}

fn validate_recheck_with(
    state: &AppState,
    id: &str,
    prepare_source: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    prepare_target: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    cleanup_pending: impl FnOnce(
        &CopyPlan,
        &[crate::copy_engine::PendingTargetPart],
    ) -> Result<(), CopyError>,
) -> Result<i64, CommandError> {
    let (snapshot, plan_revision) = state
        .storage
        .load_scan_snapshot_with_revision(id)?
        .ok_or_else(|| CommandError::Other("请先重新扫描".into()))?;
    validate_snapshot_binding(state, id, &snapshot)?;
    if blocking_report(&snapshot).has_blocking_issue() {
        return Err(CommandError::Other("仍有匹配项需要逐项处理".into()));
    }
    let prepared_source = prepare_source(&snapshot.source)?;
    if prepared_source != snapshot.source {
        return Err(CommandError::PathNotAuthorized);
    }
    let prepared_target = prepare_target(&snapshot.target)?;
    if prepared_target != snapshot.target {
        return Err(CommandError::PathNotAuthorized);
    }
    let copy_plan = plan(&snapshot);
    let previous_preflight = state.storage.load_preflight(id)?.unwrap_or_default();
    cleanup_pending(&copy_plan, &previous_preflight.pending_target_parts).map_err(|error| {
        if matches!(error, CopyError::TargetDisconnected { .. }) {
            CommandError::TargetDirectoryUnavailable
        } else {
            CommandError::Other(format!("待清理临时文件校验失败：{error}"))
        }
    })?;
    let mut report = preflight_copy_cancellable(&copy_plan, &AtomicBool::new(false))
        .map_err(|error| CommandError::Other(error.to_string()))?;
    let history = state.storage.load_copy_items(id)?;
    report.terminal_target_changed =
        terminal_copy_targets_changed(&report, &history, plan_revision);
    if report.terminal_target_changed {
        state
            .storage
            .save_attention_preflight(id, plan_revision, &report)?;
        return Err(CommandError::Other("已复制目标发生变化，请重新扫描".into()));
    }
    if report.source_changed {
        return Err(CommandError::Other(
            "源目录或文件已变化，请重新扫描后再复制".into(),
        ));
    }
    if report.source_disconnected {
        return Err(CommandError::SourceDirectoryUnavailable);
    }
    if report.target_disconnected {
        state
            .storage
            .save_attention_preflight(id, plan_revision, &report)?;
        return Err(CommandError::TargetDirectoryUnavailable);
    }
    if report.has_blocking_issue() {
        return Err(CommandError::Other(
            "完整预检仍有阻塞项，请处理后重新检查".into(),
        ));
    }
    Ok(plan_revision)
}

fn terminal_copy_targets_changed(
    preflight: &PreflightReport,
    history: &[CopyHistoryItem],
    plan_revision: i64,
) -> bool {
    if !preflight.is_complete_for_terminal_validation() {
        return false;
    }
    let identical = preflight.identical.iter().collect::<HashSet<_>>();
    history
        .iter()
        .filter(|item| {
            item.plan_revision == plan_revision
                && (item.status == CopyItemStatus::Copied
                    || (item.status == CopyItemStatus::Skipped
                        && item.skipped_reason.as_deref() == Some("identical-target")))
        })
        .any(|item| {
            item.planned_hash.is_empty()
                || item.source_hash.as_deref() != Some(item.planned_hash.as_str())
                || !identical.contains(&item.target.to_string_lossy().into_owned())
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanTargetProbe {
    Missing,
    Prepare,
}

fn probe_scan_target_with(
    target: &Path,
    metadata: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
) -> Result<ScanTargetProbe, CommandError> {
    match metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(CommandError::TargetDirectoryUnavailable)
        }
        Ok(_) => Ok(ScanTargetProbe::Prepare),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || matches!(error.raw_os_error(), Some(2 | 3)) =>
        {
            Ok(ScanTargetProbe::Missing)
        }
        Err(_) => Ok(ScanTargetProbe::Prepare),
    }
}

fn probe_scan_target(target: &Path) -> Result<ScanTargetProbe, CommandError> {
    probe_scan_target_with(target, |path| fs::symlink_metadata(path))
}

fn scan_path_matches_bound(caller: &Path, bound: &Path) -> bool {
    caller.as_os_str() == bound.as_os_str()
}

fn prepare_authorized_scan_paths_with_boundaries(
    source: &Path,
    target: &Path,
    bound_source: Option<&Path>,
    bound_target: Option<&Path>,
    prepare_source: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    prepare_target: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    probe_target: impl FnOnce(&Path) -> Result<ScanTargetProbe, CommandError>,
) -> Result<(PathBuf, PathBuf), CommandError> {
    let (Some(bound_source), Some(bound_target)) = (bound_source, bound_target) else {
        return Err(CommandError::PathNotAuthorized);
    };
    if !scan_path_matches_bound(source, bound_source)
        || !scan_path_matches_bound(target, bound_target)
    {
        return Err(CommandError::PathNotAuthorized);
    }

    let source = prepare_source(source)?;
    if !scan_path_matches_bound(&source, bound_source) {
        return Err(CommandError::PathNotAuthorized);
    }
    let target = match probe_target(target)? {
        ScanTargetProbe::Missing => target.to_path_buf(),
        ScanTargetProbe::Prepare => prepare_target(target)?,
    };
    if !scan_path_matches_bound(&target, bound_target) {
        return Err(CommandError::PathNotAuthorized);
    }
    Ok((source, target))
}

fn prepare_scan_and_begin_with(
    state: &AppState,
    id: &str,
    source: &Path,
    target: &Path,
    prepare_source: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    prepare_target: impl FnOnce(&Path) -> Result<PathBuf, CommandError>,
    probe_target: impl FnOnce(&Path) -> Result<ScanTargetProbe, CommandError>,
) -> Result<(PathBuf, PathBuf), CommandError> {
    let (bound_source, bound_target) = state.storage.load_bound_paths(id)?;
    let paths = prepare_authorized_scan_paths_with_boundaries(
        source,
        target,
        bound_source.as_deref(),
        bound_target.as_deref(),
        prepare_source,
        prepare_target,
        probe_target,
    )?;
    state.storage.begin_scan(id)?;
    Ok(paths)
}

#[tauri::command]
pub async fn scan_and_match(
    app: AppHandle,
    session_id: String,
    source: String,
    target: String,
) -> Result<MatchReport, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        scan_and_match_blocking(app, session_id, source, target)
    })
    .await
    .map_err(|_| CommandError::Other("扫描后台任务异常".into()))?
}

fn scan_and_match_blocking(
    app: AppHandle,
    session_id: String,
    source: String,
    target: String,
) -> Result<MatchReport, CommandError> {
    let state = app.state::<AppState>();
    let id = validate_session_id(&session_id)?;
    let source = PathBuf::from(source);
    let target = PathBuf::from(target);
    let (source, target) = prepare_scan_and_begin_with(
        &state,
        &id,
        &source,
        &target,
        |path| {
            prepare_directory_for_user(&app, &id, "scan-prepare", path)
                .map_err(map_source_scan_prepare_error)
        },
        |path| {
            prepare_directory_for_user(&app, &id, "scan-prepare", path)
                .map_err(map_target_scan_prepare_error)
        },
        probe_scan_target,
    )?;
    let scan_cancelled = Arc::new(AtomicBool::new(false));
    match state.cancelled.lock() {
        Ok(mut tasks) => {
            tasks.insert(id.clone(), scan_cancelled.clone());
        }
        Err(_) => {
            let _ = state.storage.recover_scan_failure(&id);
            return Err(CommandError::Other("扫描状态不可用".into()));
        }
    }
    let scan_started = Instant::now();
    let outcome = (|| -> Result<MatchReport, CommandError> {
        let _ = emit_session(
            &app,
            "scan-progress",
            &id,
            &ScanProgressPayload {
                phase: ScanPhase::Scanning,
                checked_files: 0,
                matched_files: 0,
                scanned_directories: 0,
                elapsed_ms: 0,
            },
        );

        let result =
            (|| -> Result<(crate::models::SourceIndex, Vec<NumberMatch>), CommandError> {
                let configured = settings(&state)?.extensions;
                let extensions: HashSet<String> = if configured.is_empty() {
                    default_extensions()
                } else {
                    configured
                        .into_iter()
                        .map(|value| value.to_ascii_lowercase())
                        .collect()
                };
                let numbers = state
                    .storage
                    .load_numbers(&id)?
                    .into_iter()
                    .map(|value| value.canonical)
                    .collect::<Vec<_>>();
                let wanted_numbers = numbers.iter().cloned().collect::<HashSet<_>>();
                let mut last_progress = ScanProgressUpdate {
                    checked_files: 0,
                    matched_files: 0,
                    scanned_directories: 0,
                };
                let mut last_emitted_files = 0;
                let indexed = scan_source_index_for_numbers_with_progress_cancellable(
                    &source,
                    Some(&target),
                    &extensions,
                    &wanted_numbers,
                    &scan_cancelled,
                    |progress| {
                        last_progress = progress;
                        if progress.checked_files.saturating_sub(last_emitted_files) < 64 {
                            return;
                        }
                        last_emitted_files = progress.checked_files;
                        let _ = emit_session(
                            &app,
                            "scan-progress",
                            &id,
                            &ScanProgressPayload {
                                phase: ScanPhase::Scanning,
                                checked_files: progress.checked_files,
                                matched_files: progress.matched_files,
                                scanned_directories: progress.scanned_directories,
                                elapsed_ms: scan_started
                                    .elapsed()
                                    .as_millis()
                                    .min(u128::from(u64::MAX))
                                    as u64,
                            },
                        );
                    },
                )
                .map_err(|error| {
                    warn_scan_failure(&id, &source, &error);
                    map_scan_error(error)
                })?;
                let matched = match_numbers(&numbers, &indexed.files);
                let _ = emit_session(
                    &app,
                    "scan-progress",
                    &id,
                    &ScanProgressPayload {
                        phase: ScanPhase::Complete,
                        checked_files: last_progress.checked_files,
                        matched_files: last_progress.matched_files,
                        scanned_directories: last_progress.scanned_directories,
                        elapsed_ms: scan_started.elapsed().as_millis().min(u128::from(u64::MAX))
                            as u64,
                    },
                );
                Ok((indexed, matched))
            })();
        let (indexed, items) = match result {
            Ok(result) => result,
            Err(error) => return Err(error),
        };

        let snapshot = ScanSnapshot {
            source: indexed.root,
            source_root_identity: indexed.root_identity,
            target,
            items: items.clone(),
            skipped_numbers: vec![],
        };
        let blocking = blocking_report(&snapshot);
        if blocking.has_blocking_issue() {
            state.storage.save_scan_result_if_status(
                &id,
                &snapshot,
                Some(&blocking),
                SessionStatus::Scanning,
                Some(SessionStatus::NeedsAttention),
                None,
            )?;
            let (issue, title, affected) = if !blocking.ambiguous.is_empty() {
                (
                    BlockingIssue::Ambiguous,
                    "同编号存在多组候选",
                    blocking.ambiguous.clone(),
                )
            } else if !blocking.partial.is_empty() {
                (
                    BlockingIssue::PartialFormat,
                    "只找到部分格式",
                    blocking.partial.clone(),
                )
            } else {
                (
                    BlockingIssue::Missing,
                    "没有找到编号",
                    blocking.missing.clone(),
                )
            };
            emit_session(
                &app,
                "copy-paused",
                &id,
                &CopyPausedPayload {
                    job_id: None,
                    issue,
                    title: title.into(),
                    message: "请关闭弹窗，在匹配清单中逐项选择、接受或跳过。".into(),
                    affected,
                    actions: vec![ModalAction::Close, ModalAction::Manual, ModalAction::Cancel],
                    target_disconnect: None,
                },
            )?;
            return Ok(MatchReport {
                items,
                skipped_numbers: vec![],
                auto_copy_started: false,
                requires_second_confirmation: false,
                confirmation_token: None,
                copy_job: None,
            });
        }

        let copy_items = copy_plan_items(&id, &snapshot);
        state.storage.save_scan_result_if_status(
            &id,
            &snapshot,
            Some(&blocking),
            SessionStatus::Scanning,
            Some(SessionStatus::ReadyToCopy),
            Some(&copy_items),
        )?;
        if let Ok(mut tasks) = state.cancelled.lock() {
            if tasks
                .get(&id)
                .is_some_and(|flag| Arc::ptr_eq(flag, &scan_cancelled))
            {
                tasks.remove(&id);
            }
        }
        let second_confirmation = settings(&state)?.second_confirmation_enabled;
        let confirmation_token = if second_confirmation {
            Some(issue_confirmation(&state, &id)?)
        } else {
            None
        };
        let copy_job = if second_confirmation {
            None
        } else {
            Some(launch_copy(app.clone(), &state, &id, None)?)
        };
        let auto_copy_started = copy_job.is_some();
        Ok(MatchReport {
            items,
            skipped_numbers: vec![],
            auto_copy_started,
            requires_second_confirmation: second_confirmation,
            confirmation_token,
            copy_job,
        })
    })();
    if let Ok(mut tasks) = state.cancelled.lock() {
        if tasks
            .get(&id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &scan_cancelled))
        {
            tasks.remove(&id);
        }
    }
    if outcome.is_err() {
        let _ = state.storage.recover_scan_failure(&id);
    }
    outcome
}

fn resolve_match_inner(
    state: &AppState,
    id: &str,
    number: &str,
    resolution: MatchResolution,
) -> Result<(ScanSnapshot, i64), CommandError> {
    let (mut snapshot, revision) = state
        .storage
        .load_scan_snapshot_with_revision(id)?
        .ok_or_else(|| CommandError::Other("请重新扫描后再处理候选".into()))?;
    validate_snapshot_binding(state, id, &snapshot)?;
    let item = snapshot
        .items
        .iter_mut()
        .find(|item| item.canonical_number == number)
        .ok_or_else(|| CommandError::Other("未找到待处理编号".into()))?;
    match resolution {
        MatchResolution::SelectGroup { group_id } if item.status == MatchStatus::Ambiguous => {
            let group = item
                .groups
                .iter()
                .find(|group| group.id == group_id)
                .cloned()
                .ok_or_else(|| CommandError::Other("候选组无效".into()))?;
            item.groups = vec![group];
            item.status = MatchStatus::Complete;
        }
        MatchResolution::AcceptPartial if item.status == MatchStatus::Partial => {
            item.status = MatchStatus::Complete;
        }
        MatchResolution::Skip
            if item.status == MatchStatus::Partial || item.status == MatchStatus::Missing =>
        {
            if !snapshot.skipped_numbers.iter().any(|value| value == number) {
                snapshot.skipped_numbers.push(number.to_string());
            }
        }
        _ => return Err(CommandError::Other("该决策不适用于当前匹配状态".into())),
    }
    Ok((snapshot, revision))
}

fn finish_resolution(
    state: &AppState,
    id: &str,
    snapshot: &ScanSnapshot,
    revision: i64,
) -> Result<ResolutionResult, CommandError> {
    let report = blocking_report(snapshot);
    let ready = !report.has_blocking_issue();
    let copy_items = ready.then(|| copy_plan_items(id, snapshot));
    state.storage.save_resolution_snapshot(
        id,
        snapshot,
        &report,
        revision,
        ready,
        copy_items.as_deref(),
    )?;
    let post_save = (|| -> Result<Option<String>, CommandError> {
        if ready && settings(state)?.second_confirmation_enabled {
            Ok(Some(issue_confirmation(state, id)?))
        } else {
            Ok(None)
        }
    })();
    let token = match post_save {
        Ok(token) => token,
        Err(error) => {
            if ready {
                let _ = state.storage.recover_scan_failure(id);
            }
            return Err(error);
        }
    };
    Ok(ResolutionResult {
        ready_to_copy: ready,
        confirmation_token: token,
        copy_job: None,
    })
}

#[tauri::command]
pub fn resolve_match(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    number: String,
    resolution: MatchResolution,
) -> Result<ResolutionResult, CommandError> {
    let id = validate_session_id(&session_id)?;
    let (snapshot, revision) = resolve_match_inner(&state, &id, &number, resolution)?;
    let mut result = finish_resolution(&state, &id, &snapshot, revision)?;
    if result.ready_to_copy && !settings(&state)?.second_confirmation_enabled {
        result.copy_job = Some(launch_copy(app, &state, &id, None)?);
    }
    Ok(result)
}

#[tauri::command]
pub fn resolve_ambiguous_match(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    number: String,
    group_id: String,
) -> Result<ResolutionResult, CommandError> {
    let id = validate_session_id(&session_id)?;
    let (snapshot, revision) = resolve_match_inner(
        &state,
        &id,
        &number,
        MatchResolution::SelectGroup { group_id },
    )?;
    let mut result = finish_resolution(&state, &id, &snapshot, revision)?;
    if result.ready_to_copy && !settings(&state)?.second_confirmation_enabled {
        result.copy_job = Some(launch_copy(app, &state, &id, None)?);
    }
    Ok(result)
}

fn candidate_preview_envelope(extension: Option<&str>, mut bytes: Vec<u8>) -> Vec<u8> {
    let tag = match extension.map(str::to_ascii_lowercase).as_deref() {
        None => return vec![0],
        Some("png") => 2,
        Some("webp") => 3,
        Some("jpg" | "jpeg") => 1,
        Some(_) => unreachable!("candidate preview extension was filtered"),
    };
    bytes.insert(0, tag);
    bytes
}

#[tauri::command]
pub fn read_candidate_preview(
    state: State<'_, AppState>,
    session_id: String,
    number: String,
    group_id: String,
) -> Result<tauri::ipc::Response, CommandError> {
    let id = validate_session_id(&session_id)?;
    let snapshot = state
        .storage
        .load_scan_snapshot(&id)?
        .ok_or_else(|| CommandError::Other("扫描快照不存在，请重新扫描".into()))?;
    validate_snapshot_binding(&state, &id, &snapshot)?;
    let group = snapshot
        .items
        .iter()
        .find(|item| item.canonical_number == number)
        .and_then(|item| item.groups.iter().find(|group| group.id == group_id))
        .ok_or_else(|| CommandError::Other("候选组不存在".into()))?;
    let preview = group.files.iter().find(|file| {
        matches!(
            file.extension.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "webp"
        )
    });
    let Some(preview) = preview else {
        return Ok(tauri::ipc::Response::new(candidate_preview_envelope(
            None,
            vec![],
        )));
    };
    if preview.size > 24 * 1024 * 1024 {
        return Err(CommandError::Other("候选预览文件过大".into()));
    }
    let bytes = read_bound_source_file(
        &snapshot.source,
        &snapshot.source_root_identity,
        &SelectedFile {
            canonical_number: preview.canonical_number.clone(),
            source: preview.path.clone(),
            target: PathBuf::new(),
            fingerprint: preview.fingerprint.clone(),
        },
        24 * 1024 * 1024,
    )
    .map_err(|error| CommandError::Other(error.to_string()))?;
    Ok(tauri::ipc::Response::new(candidate_preview_envelope(
        Some(&preview.extension),
        bytes,
    )))
}

fn mark_worker_attention(
    app: &AppHandle,
    state: &AppState,
    id: &str,
    plan_revision: i64,
    job_id: &str,
    report: &PreflightReport,
) -> Result<(), CommandError> {
    state.storage.save_worker_preflight(
        id,
        plan_revision,
        report,
        Some(SessionStatus::NeedsAttention),
    )?;
    emit_worker_attention(app, id, job_id, report)
}

fn emit_worker_attention(
    app: &AppHandle,
    id: &str,
    job_id: &str,
    report: &PreflightReport,
) -> Result<(), CommandError> {
    let redacted_conflicts = report
        .conflicts
        .iter()
        .filter_map(|value| Path::new(value).file_name())
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let (issue, title, message, affected) = if report.terminal_target_changed {
        (
            BlockingIssue::SourceChanged,
            "已复制目标发生变化，请重新扫描",
            "目标文件已被删除或替换，请重新扫描后再复制。",
            vec![],
        )
    } else if !redacted_conflicts.is_empty() {
        (
            BlockingIssue::TargetConflict,
            "目标目录存在不同内容的同名文件",
            "请处理目标文件后重新检查。",
            redacted_conflicts,
        )
    } else if report.permission_denied || report.atomic_commit_unsupported {
        (
            BlockingIssue::PermissionDenied,
            "目标目录不支持安全写入",
            "请检查权限或更换支持原子提交的目标目录。",
            vec![],
        )
    } else if report.insufficient_space {
        (
            BlockingIssue::InsufficientSpace,
            "目标磁盘空间不足",
            "释放空间后重新检查。",
            vec![],
        )
    } else if report.target_disconnected {
        (
            BlockingIssue::TargetDisconnected,
            "目标目录或 NAS 已断开",
            "恢复目标目录连接后重新检查。",
            vec![],
        )
    } else if report.source_disconnected {
        (
            BlockingIssue::SourceDisconnected,
            "源目录或 NAS 已断开",
            "恢复连接后重新检查。",
            vec![],
        )
    } else if report.source_changed {
        (
            BlockingIssue::SourceChanged,
            "复制期间源文件发生变化",
            "请重新扫描确认文件。",
            vec![],
        )
    } else if !report.ambiguous.is_empty() {
        (
            BlockingIssue::Ambiguous,
            "同编号存在多组候选",
            "请重新选择候选组。",
            report.ambiguous.clone(),
        )
    } else if !report.partial.is_empty() {
        (
            BlockingIssue::PartialFormat,
            "只找到部分格式",
            "请确认是否接受。",
            report.partial.clone(),
        )
    } else {
        (
            BlockingIssue::Missing,
            "没有找到编号",
            "请确认是否跳过。",
            report.missing.clone(),
        )
    };
    let mut payload = pause_payload(issue, title, message, affected);
    payload.job_id = Some(job_id.into());
    payload.target_disconnect = report.target_disconnect;
    emit_session(app, "copy-paused", id, &payload)
}

fn pause_disconnected_copy_with(
    state: &AppState,
    id: &str,
    plan_revision: i64,
    report: &CopyReport,
    emit: impl FnOnce(&PreflightReport) -> Result<(), CommandError>,
) -> Result<bool, WorkerFailure> {
    let preflight = match report.stop_reason {
        Some(CopyStopReason::SourceDisconnected) => PreflightReport {
            source_disconnected: true,
            ..PreflightReport::default()
        },
        Some(CopyStopReason::TargetDisconnected) => PreflightReport {
            target_disconnected: true,
            target_disconnect: report.target_disconnect,
            pending_target_parts: report.pending_target_parts.clone(),
            ..PreflightReport::default()
        },
        _ => return Ok(false),
    };
    state
        .storage
        .pause_copy_for_disconnect(id, plan_revision, &preflight)
        .map_err(|error| WorkerFailure::RecoverablePersistence(error.into()))?;
    let (diagnostic_stage, diagnostic_code, diagnostic_outcome) =
        disconnect_pause_diagnostic(&preflight);
    if let Ok(Some(snapshot)) = state.storage.load_scan_snapshot(id) {
        let disconnected_path = if preflight.target_disconnected {
            &snapshot.target
        } else {
            &snapshot.source
        };
        let (path_kind, network_target) =
            if diagnostics::anonymized_network_target(disconnected_path).is_some() {
                (DiagnosticPathKind::Unc, Some(disconnected_path.as_path()))
            } else {
                (DiagnosticPathKind::Local, None)
            };
        diagnostics::info_network_operation(
            Some(id),
            diagnostic_stage,
            path_kind,
            network_target,
            diagnostic_code,
            diagnostic_outcome,
        );
    } else {
        diagnostics::info_operation(
            Some(id),
            diagnostic_stage,
            DiagnosticPathKind::Local,
            diagnostic_code,
            diagnostic_outcome,
        );
    }
    emit(&preflight)?;
    Ok(true)
}

fn disconnect_pause_diagnostic(
    preflight: &PreflightReport,
) -> (&'static str, Option<u32>, &'static str) {
    (
        "copy-disconnect-pause",
        preflight
            .target_disconnect
            .and_then(|info| info.windows_code),
        "paused",
    )
}

#[derive(Debug)]
enum WorkerFailure {
    Terminal(CommandError),
    RecoverablePersistence(CommandError),
}

impl From<CommandError> for WorkerFailure {
    fn from(error: CommandError) -> Self {
        Self::Terminal(error)
    }
}

impl From<StorageError> for WorkerFailure {
    fn from(error: StorageError) -> Self {
        Self::Terminal(error.into())
    }
}

fn remove_copy_task(state: &AppState, id: &str) -> Result<(), CommandError> {
    state
        .cancelled
        .lock()
        .map_err(|_| CommandError::Other("复制状态不可用".into()))?
        .remove(id);
    Ok(())
}

fn settle_worker_failure_with(
    state: &AppState,
    id: &str,
    plan_revision: i64,
    job_id: &str,
    started_at: &str,
    failure: WorkerFailure,
    emit: impl FnOnce(&CopyCompletionReport) -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    remove_copy_task(state, id)?;
    let (error, mut preserve_recoverable_state) = match failure {
        WorkerFailure::Terminal(error) => (error, false),
        WorkerFailure::RecoverablePersistence(error) => (error, true),
    };
    let mut terminal_persistence_error = None;
    let cleanup_error = if preserve_recoverable_state {
        None
    } else {
        match finalize_copy_session(state, id, plan_revision, SessionStatus::Failed) {
            Ok(cleanup_error) => cleanup_error,
            Err(finalize_error) => {
                preserve_recoverable_state = true;
                terminal_persistence_error = Some(finalize_error.to_string());
                None
            }
        }
    };
    let snapshot = state
        .storage
        .load_scan_snapshot(id)?
        .ok_or_else(|| CommandError::Other("扫描快照不存在，请重新扫描".into()))?;
    let stats = completion_stats(
        &snapshot,
        &state.storage.load_copy_items(id)?,
        plan_revision,
    );
    let mut message = if preserve_recoverable_state {
        format!("复制状态持久化失败，已保留会话、输入与复制历史供恢复：{error}")
    } else {
        error.to_string()
    };
    if let Some(finalize_error) = terminal_persistence_error {
        message.push_str(&format!("；终态持久化失败：{finalize_error}"));
    }
    if let Some(cleanup_error) = cleanup_error {
        message.push_str(&format!("；临时输入清理失败：{cleanup_error}"));
    }
    emit(&completion_payload(
        job_id,
        CompletionStatus::Failed,
        &snapshot,
        stats.copied_count,
        stats.skipped_identical_count,
        stats.failed_count.max(1),
        stats.copied_bytes,
        started_at,
        &message,
    ))
}

fn persist_copy_outcome_with<E>(
    cancelled: &AtomicBool,
    outcome: CopyItemOutcome,
    update: impl FnOnce(
        CopyItemStatus,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        Option<&str>,
    ) -> Result<(), E>,
) -> Result<(), E> {
    let result = match &outcome {
        CopyItemOutcome::Started => update(CopyItemStatus::Copying, None, None, None, None),
        CopyItemOutcome::Copied { source_hash } => {
            update(CopyItemStatus::Copied, Some(source_hash), None, None, None)
        }
        CopyItemOutcome::SkippedIdentical { source_hash } => update(
            CopyItemStatus::Skipped,
            Some(source_hash),
            Some("identical-target"),
            None,
            None,
        ),
        CopyItemOutcome::Interrupted => return Ok(()),
        CopyItemOutcome::Failed { code, summary } => update(
            CopyItemStatus::Failed,
            None,
            None,
            Some(code),
            Some(summary),
        ),
        CopyItemOutcome::Cancelled => update(
            CopyItemStatus::Cancelled,
            None,
            None,
            Some("cancelled"),
            Some("copy cancelled before this item completed"),
        ),
    };
    if result.is_err() {
        cancelled.store(true, Ordering::Relaxed);
    }
    result
}

fn run_copy_worker(
    app: AppHandle,
    id: String,
    job_id: String,
    plan_revision: i64,
    cancelled: Arc<AtomicBool>,
) {
    let state = app.state::<AppState>();
    let started_at = chrono::Utc::now().to_rfc3339();
    let outcome = (|| -> Result<(), WorkerFailure> {
        let (snapshot, current_revision) = state
            .storage
            .load_scan_snapshot_with_revision(&id)?
            .ok_or_else(|| CommandError::Other("扫描快照不存在，请重新扫描".into()))?;
        if current_revision != plan_revision {
            return Err(StorageError::StateConflict.into());
        }
        validate_snapshot_binding(&state, &id, &snapshot)?;
        let copy_plan = plan(&snapshot);
        let mut preflight = match preflight_copy_cancellable(&copy_plan, &cancelled) {
            Ok(report) => report,
            Err(CopyError::Cancelled) => {
                let cleanup_error =
                    finalize_copy_session(&state, &id, plan_revision, SessionStatus::Cancelled)?;
                let message = cleanup_error
                    .map(|error| format!("复制已取消；临时输入清理失败：{error}"))
                    .unwrap_or_else(|| "复制已取消。".into());
                emit_session(
                    &app,
                    "copy-complete",
                    &id,
                    &completion_payload(
                        &job_id,
                        CompletionStatus::Cancelled,
                        &snapshot,
                        0,
                        0,
                        0,
                        0,
                        &started_at,
                        &message,
                    ),
                )?;
                return Ok(());
            }
            Err(error) => return Err(CommandError::Other(error.to_string()).into()),
        };
        let current_items = state.storage.load_copy_items(&id)?;
        preflight.terminal_target_changed =
            terminal_copy_targets_changed(&preflight, &current_items, plan_revision);
        if preflight.has_blocking_issue() {
            mark_worker_attention(&app, &state, &id, plan_revision, &job_id, &preflight)?;
            return Ok(());
        }
        state
            .storage
            .save_worker_preflight(&id, plan_revision, &preflight, None)?;

        let current_items = current_items
            .into_iter()
            .filter(|item| {
                item.plan_revision == plan_revision && item.status == CopyItemStatus::Planned
            })
            .collect::<Vec<_>>();
        let planned_items = current_items
            .iter()
            .cloned()
            .map(|item| ((item.source, item.target), item.id))
            .collect::<HashMap<_, _>>();
        let execution_plan = CopyPlan {
            source_root: copy_plan.source_root.clone(),
            source_root_identity: copy_plan.source_root_identity.clone(),
            target_root: copy_plan.target_root.clone(),
            files: copy_plan
                .files
                .into_iter()
                .filter(|file| {
                    planned_items.contains_key(&(file.source.clone(), file.target.clone()))
                })
                .collect(),
        };
        let total_files = execution_plan.files.len();
        let total_bytes = execution_plan
            .files
            .iter()
            .map(|file| file.fingerprint.size)
            .sum::<u64>();
        let mut copied_bytes = 0_u64;
        let mut seen_files = HashSet::new();
        let mut history_error = None;
        let copy_report = execute_copy_with_updates(
            &execution_plan,
            &cancelled,
            |path, bytes| {
                copied_bytes = copied_bytes.saturating_add(bytes);
                let current_file = path
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "文件".into());
                seen_files.insert(current_file.clone());
                let _ = emit_session(
                    &app,
                    "copy-progress",
                    &id,
                    &CopyProgressPayload {
                        job_id: job_id.clone(),
                        current_file,
                        completed_files: seen_files.len().saturating_sub(1),
                        total_files,
                        copied_bytes,
                        total_bytes,
                    },
                );
            },
            |file, outcome| {
                if history_error.is_some() {
                    cancelled.store(true, Ordering::Relaxed);
                    return;
                }
                let Some(item_id) = planned_items.get(&(file.source.clone(), file.target.clone()))
                else {
                    cancelled.store(true, Ordering::Relaxed);
                    history_error =
                        Some(CommandError::Other("持久复制清单与执行计划不一致".into()));
                    return;
                };
                let update = persist_copy_outcome_with(
                    &cancelled,
                    outcome,
                    |status, source_hash, skipped_reason, error_code, error_summary| {
                        state.storage.update_copy_item(
                            item_id,
                            plan_revision,
                            status,
                            source_hash,
                            skipped_reason,
                            error_code,
                            error_summary,
                        )
                    },
                );
                if let Err(error) = update {
                    history_error = Some(error.into());
                }
            },
        );
        if let Some(error) = history_error {
            return Err(WorkerFailure::RecoverablePersistence(error));
        }
        if pause_disconnected_copy_with(&state, &id, plan_revision, &copy_report, |preflight| {
            emit_worker_attention(&app, &id, &job_id, preflight)
        })? {
            return Ok(());
        }
        let persisted_stats = completion_stats(
            &snapshot,
            &state.storage.load_copy_items(&id)?,
            plan_revision,
        );
        if copy_report.cancelled {
            let cleanup_error =
                finalize_copy_session(&state, &id, plan_revision, SessionStatus::Cancelled)?;
            let stats = completion_stats(
                &snapshot,
                &state.storage.load_copy_items(&id)?,
                plan_revision,
            );
            let message = cleanup_error
                .map(|error| format!("复制已取消；临时输入清理失败：{error}"))
                .unwrap_or_else(|| "复制已取消。".into());
            emit_session(
                &app,
                "copy-complete",
                &id,
                &completion_payload(
                    &job_id,
                    CompletionStatus::Cancelled,
                    &snapshot,
                    stats.copied_count,
                    stats.skipped_identical_count,
                    stats.failed_count,
                    stats.copied_bytes,
                    &started_at,
                    &message,
                ),
            )?;
        } else if !copy_report.failed.is_empty() || persisted_stats.failed_count > 0 {
            let cleanup_error =
                finalize_copy_session(&state, &id, plan_revision, SessionStatus::Failed)?;
            let stats = completion_stats(
                &snapshot,
                &state.storage.load_copy_items(&id)?,
                plan_revision,
            );
            let failed_files = copy_report
                .failed
                .iter()
                .filter_map(|(path, _)| Path::new(path).file_name())
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut message = if failed_files.is_empty() {
                format!("{} 个文件复制失败", stats.failed_count)
            } else {
                format!(
                    "{} 个文件复制失败：{}",
                    stats.failed_count,
                    failed_files.join("、")
                )
            };
            if let Some(error) = cleanup_error {
                message.push_str(&format!("；临时输入清理失败：{error}"));
            }
            emit_session(
                &app,
                "copy-complete",
                &id,
                &completion_payload(
                    &job_id,
                    CompletionStatus::Failed,
                    &snapshot,
                    stats.copied_count,
                    stats.skipped_identical_count,
                    stats.failed_count,
                    stats.copied_bytes,
                    &started_at,
                    &message,
                ),
            )?;
        } else {
            let cleanup_error =
                finalize_copy_session(&state, &id, plan_revision, SessionStatus::Completed)?;
            let stats = completion_stats(
                &snapshot,
                &state.storage.load_copy_items(&id)?,
                plan_revision,
            );
            let message = cleanup_error
                .map(|error| format!("复制完成；临时输入清理失败：{error}"))
                .unwrap_or_else(|| "复制完成。".into());
            emit_session(
                &app,
                "copy-complete",
                &id,
                &completion_payload(
                    &job_id,
                    CompletionStatus::Completed,
                    &snapshot,
                    stats.copied_count,
                    stats.skipped_identical_count,
                    stats.failed_count,
                    stats.copied_bytes,
                    &started_at,
                    &message,
                ),
            )?;
        }
        Ok(())
    })();

    let settled = match outcome {
        Ok(()) => remove_copy_task(&state, &id),
        Err(failure) => settle_worker_failure_with(
            &state,
            &id,
            plan_revision,
            &job_id,
            &started_at,
            failure,
            |report| emit_session(&app, "copy-complete", &id, report),
        ),
    };
    if let Err(error) = settled {
        eprintln!("failed to settle copy worker: {error}");
    }
}

fn launch_copy(
    app: AppHandle,
    state: &AppState,
    id: &str,
    confirmation_token: Option<&str>,
) -> Result<CopyLaunch, CommandError> {
    let mut tasks = state
        .cancelled
        .lock()
        .map_err(|_| CommandError::Other("复制状态不可用".into()))?;
    if tasks.contains_key(id) {
        return Err(CommandError::Other("复制任务已在运行".into()));
    }
    let (_, plan_revision) = claim_copy_start_with(
        state,
        id,
        confirmation_token,
        |source| {
            prepare_directory_for_user(&app, id, "copy-source-prepare", source)
                .map_err(map_source_scan_prepare_error)
        },
        |target| {
            prepare_target_directory_for_user(&app, id, "copy-target-prepare", target)
                .map_err(map_target_scan_prepare_error)
        },
    )?;
    let cancelled = Arc::new(AtomicBool::new(false));
    tasks.insert(id.to_string(), cancelled.clone());
    drop(tasks);
    let worker_id = id.to_string();
    let job_id = Uuid::new_v4().to_string();
    let worker_job_id = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_copy_worker(app, worker_id, worker_job_id, plan_revision, cancelled)
    });
    Ok(CopyLaunch {
        job_id,
        status: "copying",
    })
}

#[tauri::command]
pub fn start_copy(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    confirmation_token: Option<String>,
) -> Result<CopyLaunch, CommandError> {
    let id = validate_session_id(&session_id)?;
    launch_copy(app, &state, &id, confirmation_token.as_deref())
}

#[tauri::command]
pub fn cancel_copy(state: State<'_, AppState>, session_id: String) -> Result<(), CommandError> {
    let id = validate_session_id(&session_id)?;
    if let Some(cancelled) = state
        .cancelled
        .lock()
        .map_err(|_| CommandError::Other("复制状态不可用".into()))?
        .get(&id)
    {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
pub fn recheck_copy(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> Result<CopyLaunch, CommandError> {
    let id = validate_session_id(&session_id)?;
    validate_recheck_with(
        &state,
        &id,
        |source| {
            prepare_directory_for_user(&app, &id, "copy-source-recheck", source)
                .map_err(map_source_scan_prepare_error)
        },
        |target| {
            prepare_target_directory_for_user(&app, &id, "copy-target-recheck", target)
                .map_err(map_target_scan_prepare_error)
        },
        cleanup_pending_target_parts,
    )?;
    state.storage.compare_and_set_status(
        &id,
        SessionStatus::NeedsAttention,
        SessionStatus::ReadyToCopy,
    )?;
    let launch = (|| {
        let token = settings(&state)?
            .second_confirmation_enabled
            .then(|| issue_confirmation(&state, &id))
            .transpose()?;
        launch_copy(app, &state, &id, token.as_deref())
    })();
    if launch.is_err() {
        let _ = state.storage.compare_and_set_status(
            &id,
            SessionStatus::ReadyToCopy,
            SessionStatus::NeedsAttention,
        );
    }
    launch
}

#[tauri::command]
pub fn cancel_session(state: State<'_, AppState>, session_id: String) -> Result<(), CommandError> {
    let id = validate_session_id(&session_id)?;
    let session = load_session(&state, &id)?;
    if session.status == SessionStatus::Copying || session.status == SessionStatus::Scanning {
        return cancel_copy(state, id);
    }
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ) {
        return Ok(());
    }
    state
        .storage
        .compare_and_set_status(&id, session.status, SessionStatus::Cancelled)?;
    cleanup_session_inputs(&state.app_data_dir, &id)?;
    state.storage.delete_session_inputs(&id)?;
    Ok(())
}

#[tauri::command]
pub fn save_settings(
    state: State<'_, AppState>,
    settings: AppSettings,
) -> Result<(), CommandError> {
    if settings.recognition_mode == "cloud" {
        let id = settings
            .default_provider_id
            .as_deref()
            .ok_or_else(|| CommandError::Other("云端识别必须先设置默认云模型".into()))?;
        let profile = provider(&state, id)?;
        if !profile.enabled {
            return Err(CommandError::Other("默认云模型当前未启用".into()));
        }
    } else if settings.recognition_mode != "offline" {
        return Err(CommandError::Other("识别模式无效".into()));
    }
    Ok(state.storage.save_setting("app-settings", &settings)?)
}

#[tauri::command]
pub fn save_provider(
    state: State<'_, AppState>,
    mut profile: ProviderProfile,
    api_key: String,
) -> Result<ProviderSettingsResult, CommandError> {
    if profile.id.trim().is_empty() || profile.name.trim().is_empty() {
        return Err(CommandError::Other("云服务配置名称无效".into()));
    }
    if profile.is_default && !profile.enabled {
        return Err(CommandError::Other("默认云模型必须启用".into()));
    }
    providers::resolve_endpoint(&profile)
        .map_err(|error| CommandError::Other(error.to_string()))?;
    with_provider_credential_guard(&state.provider_credentials, || {
        let existing = optional_provider(&state, &profile.id)?;
        profile.secret_ref = secret_ref_for_provider_save(&profile, existing.as_ref(), &api_key)?;
        let previous_secret_ref = existing
            .as_ref()
            .map(|value| value.secret_ref.clone())
            .filter(|value| value != &profile.secret_ref);
        let mut app_settings = settings(&state)?;
        if profile.is_default {
            app_settings.recognition_mode = "cloud".into();
            app_settings.default_provider_id = Some(profile.id.clone());
        } else if app_settings.default_provider_id.as_deref() == Some(&profile.id) {
            app_settings.recognition_mode = "offline".into();
            app_settings.default_provider_id = None;
        }
        if api_key.is_empty() {
            state
                .storage
                .save_provider_and_settings(&profile, &app_settings)?;
        } else {
            rotate_provider_secret(
                || {
                    crate::secrets::save_api_key(&profile.secret_ref, &api_key)
                        .map_err(|error| CommandError::Other(error.to_string()))
                },
                || {
                    state
                        .storage
                        .save_provider_and_settings(&profile, &app_settings)
                        .map_err(CommandError::from)
                },
                || {
                    crate::secrets::delete_api_key(&profile.secret_ref)
                        .map_err(|error| CommandError::Other(error.to_string()))
                },
                || {
                    if let Some(previous_secret_ref) = previous_secret_ref.as_deref() {
                        crate::secrets::delete_api_key(previous_secret_ref)
                            .map_err(|error| CommandError::Other(error.to_string()))?;
                    }
                    Ok(())
                },
            )?;
        }
        Ok(ProviderSettingsResult {
            settings: app_settings,
            providers: state.storage.list_providers()?,
        })
    })
}

#[tauri::command]
pub fn delete_provider(
    state: State<'_, AppState>,
    profile_id: String,
) -> Result<ProviderSettingsResult, CommandError> {
    with_provider_credential_guard(&state.provider_credentials, || {
        let profile = provider(&state, &profile_id)?;
        let mut app_settings = settings(&state)?;
        if app_settings.default_provider_id.as_deref() == Some(&profile_id) {
            app_settings.default_provider_id = None;
            app_settings.recognition_mode = "offline".into();
        }
        delete_provider_and_retire_secret(
            || {
                state
                    .storage
                    .delete_provider_and_settings(&profile_id, &app_settings)
                    .map_err(CommandError::from)
            },
            || match crate::secrets::delete_api_key(&profile.secret_ref) {
                Ok(()) | Err(crate::secrets::SecretError::Keyring(keyring::Error::NoEntry)) => {
                    Ok(())
                }
                Err(error) => Err(CommandError::Other(error.to_string())),
            },
        )?;
        Ok(ProviderSettingsResult {
            settings: app_settings,
            providers: state.storage.list_providers()?,
        })
    })
}

fn provider(state: &AppState, id: &str) -> Result<ProviderProfile, CommandError> {
    optional_provider(state, id)?.ok_or_else(|| CommandError::Other("云服务配置不存在".into()))
}

fn optional_provider(state: &AppState, id: &str) -> Result<Option<ProviderProfile>, CommandError> {
    Ok(state.storage.load_setting(&format!("provider:{id}"))?)
}

fn with_provider_credential_guard<T>(
    guard: &Mutex<()>,
    operation: impl FnOnce() -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    let _guard = guard
        .lock()
        .map_err(|_| CommandError::Other("云服务凭据状态锁已损坏".into()))?;
    operation()
}

fn provider_with_key(
    state: &AppState,
    id: &str,
) -> Result<(ProviderProfile, String), CommandError> {
    with_provider_credential_guard(&state.provider_credentials, || {
        let profile = provider(state, id)?;
        let key = crate::secrets::load_api_key(&profile.secret_ref)
            .map_err(|error| CommandError::Other(error.to_string()))?;
        Ok((profile, key))
    })
}

fn existing_secret_ref_for_draft(
    draft: &ProviderProfile,
    existing: &ProviderProfile,
) -> Result<String, CommandError> {
    if draft.id != existing.id {
        return Err(CommandError::Other("云服务配置标识不匹配".into()));
    }
    let draft_origin = url::Url::parse(&draft.address)
        .map_err(|_| CommandError::Other("API 地址无效".into()))?
        .origin();
    let existing_origin = url::Url::parse(&existing.address)
        .map_err(|_| CommandError::Other("已保存的 API 地址无效".into()))?
        .origin();
    if draft_origin != existing_origin {
        return Err(CommandError::Other(
            "修改 API 服务域名时必须重新填写 API Key".into(),
        ));
    }
    Ok(existing.secret_ref.clone())
}

fn secret_ref_for_provider_save(
    draft: &ProviderProfile,
    existing: Option<&ProviderProfile>,
    api_key: &str,
) -> Result<String, CommandError> {
    match existing {
        Some(existing) if api_key.is_empty() => existing_secret_ref_for_draft(draft, existing),
        Some(_) => Ok(fresh_provider_secret_ref(&draft.id)),
        None if api_key.is_empty() => Err(CommandError::Other("新配置必须填写 API Key".into())),
        None => Ok(fresh_provider_secret_ref(&draft.id)),
    }
}

fn fresh_provider_secret_ref(profile_id: &str) -> String {
    format!("{profile_id}:{}", Uuid::new_v4())
}

fn rotate_provider_secret(
    save_new: impl FnOnce() -> Result<(), CommandError>,
    commit_profile: impl FnOnce() -> Result<(), CommandError>,
    rollback_new: impl FnOnce() -> Result<(), CommandError>,
    retire_old: impl FnOnce() -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    // A fresh reference keeps the old profile/key pair usable until the
    // profile transaction commits. Compensation can therefore leak only an
    // unreferenced key; it can never strand an active profile without a key.
    save_new()?;
    if let Err(commit_error) = commit_profile() {
        return match rollback_new() {
            Ok(()) => Err(commit_error),
            Err(rollback_error) => Err(CommandError::Other(format!(
                "云服务配置保存失败：{commit_error}；新凭据回滚也失败：{rollback_error}"
            ))),
        };
    }
    if let Err(retire_error) = retire_old() {
        return Err(CommandError::Other(format!(
            "云服务配置与新凭据已保存，但旧凭据清理失败：{retire_error}"
        )));
    }
    Ok(())
}

fn delete_provider_and_retire_secret(
    delete_profile: impl FnOnce() -> Result<(), CommandError>,
    retire_secret: impl FnOnce() -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    // Commit the database deletion first. If keychain cleanup fails, the
    // remaining key is orphaned but no active profile points at a missing key.
    delete_profile()?;
    retire_secret().map_err(|error| {
        CommandError::Other(format!("云服务配置已删除，但钥匙串凭据清理失败：{error}"))
    })
}

#[tauri::command]
pub async fn test_provider(
    state: State<'_, AppState>,
    profile_id: String,
) -> Result<providers::ProviderTestResult, CommandError> {
    let (profile, key) = provider_with_key(&state, &profile_id)?;
    Ok(providers::test_provider(&profile, &key).await)
}

#[tauri::command]
pub async fn test_provider_draft(
    state: State<'_, AppState>,
    mut profile: ProviderProfile,
    api_key: String,
) -> Result<providers::ProviderTestResult, CommandError> {
    let key = if api_key.is_empty() {
        with_provider_credential_guard(&state.provider_credentials, || {
            let existing = provider(&state, &profile.id)?;
            let secret_ref = existing_secret_ref_for_draft(&profile, &existing)?;
            crate::secrets::load_api_key(&secret_ref)
                .map_err(|error| CommandError::Other(error.to_string()))
        })?
    } else {
        api_key
    };
    profile.secret_ref.clear();
    Ok(providers::test_provider(&profile, &key).await)
}

#[tauri::command]
pub async fn list_provider_models(
    state: State<'_, AppState>,
    profile_id: String,
) -> Result<Vec<String>, CommandError> {
    let (profile, key) = provider_with_key(&state, &profile_id)?;
    providers::list_models(&profile, &key)
        .await
        .map_err(|error| CommandError::Other(error.to_string()))
}

#[tauri::command]
pub async fn recognize_cloud(
    state: State<'_, AppState>,
    profile_id: String,
    session_id: String,
    input_ids: Vec<String>,
) -> Result<RecognitionDraft, CommandError> {
    let id = validate_session_id(&session_id)?;
    recognize_cloud_inner(&state, &profile_id, &id, &input_ids).await
}

fn invalid_recognition_batch() -> CommandError {
    CommandError::Other("识别图片批次无效或过大（最多 12 张、合计 48 MiB）".into())
}

fn load_persisted_recognition_images(
    state: &AppState,
    session_id: &str,
    input_ids: &[String],
) -> Result<Vec<Vec<u8>>, CommandError> {
    load_session(state, session_id)?;
    if input_ids.is_empty() || input_ids.len() > providers::MAX_RECOGNITION_IMAGES {
        return Err(invalid_recognition_batch());
    }

    let mut unique = HashSet::with_capacity(input_ids.len());
    for input_id in input_ids {
        if Uuid::parse_str(input_id).is_err() || !unique.insert(input_id.as_str()) {
            return Err(invalid_recognition_batch());
        }
    }

    let mut selected = Vec::with_capacity(input_ids.len());
    let mut total = 0_usize;
    for input_id in input_ids {
        let input = state
            .storage
            .load_session_input(session_id, input_id)
            .map_err(|_| invalid_recognition_batch())?;
        let size = usize::try_from(input.size).map_err(|_| invalid_recognition_batch())?;
        if input.kind != "image"
            || size == 0
            || size > providers::MAX_RECOGNITION_IMAGE_BYTES
            || input.fingerprint.size != input.size
        {
            return Err(invalid_recognition_batch());
        }
        total = total
            .checked_add(size)
            .filter(|total| *total <= providers::MAX_RECOGNITION_BATCH_BYTES)
            .ok_or_else(invalid_recognition_batch)?;
        selected.push(input);
    }

    // All IDs, ownership, kinds, and sizes have passed before the first file
    // read. Files are then opened sequentially through the existing bound-root,
    // identity, no-follow reader.
    let mut images = Vec::with_capacity(selected.len());
    for input in selected {
        let bytes = read_session_input_inner(state, session_id, &input.id)
            .map_err(|_| CommandError::Other("持久化识别图片无法安全读取".into()))?;
        let validated_mime = validate_image_input(&bytes)
            .map_err(|_| CommandError::Other("持久化识别图片无法安全读取".into()))?;
        if bytes.len() as u64 != input.size || validated_mime != input.mime {
            return Err(CommandError::Other("持久化识别图片无法安全读取".into()));
        }
        images.push(bytes);
    }
    Ok(images)
}

async fn recognize_cloud_inner(
    state: &AppState,
    profile_id: &str,
    session_id: &str,
    input_ids: &[String],
) -> Result<RecognitionDraft, CommandError> {
    // Persisted-input validation and bounded reads must complete before the
    // keyring is touched or any provider request can be created.
    let images = load_persisted_recognition_images(state, session_id, input_ids)?;
    let (profile, key) = provider_with_key(&state, &profile_id)?;
    match providers::recognize_cloud(&profile, &key, &images).await {
        Ok(result) => Ok(RecognitionDraft {
            detected_order_id: result.detected_order_id,
            numbers: result.numbers,
            raw_text: String::new(),
            method: result.method,
        }),
        Err(error) => {
            diagnostics::warn_operation(
                Some(session_id),
                "recognition-provider",
                DiagnosticPathKind::Local,
                None,
                "failed",
            );
            if settings(&state)?.cloud_fallback_offline {
                Err(CommandError::OfflineFallbackRequired(error.to_string()))
            } else {
                Err(CommandError::Other(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_input_metadata_header(session_id: &str, name: &str, kind: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "sessionId": session_id,
                "name": name,
                "kind": kind,
            })
            .to_string(),
        )
    }

    fn safe_tempdir() -> tempfile::TempDir {
        #[cfg(target_os = "macos")]
        {
            return tempfile::tempdir_in("/private/tmp").unwrap();
        }
        #[cfg(not(target_os = "macos"))]
        tempfile::tempdir().unwrap()
    }

    fn test_state(directory: &Path) -> AppState {
        AppState {
            storage: Storage::open(&directory.join("state.sqlite3")).unwrap(),
            app_data_dir: directory.to_path_buf(),
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        }
    }

    #[test]
    fn fresh_install_does_not_require_second_confirmation() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());

        let effective = settings(&state).unwrap();

        assert!(!effective.second_confirmation_enabled);
    }

    #[test]
    fn raw_input_request_accepts_utf8_metadata_and_rejects_json_or_oversized_bodies() {
        let session_id = Uuid::new_v4().to_string();
        let header = raw_input_metadata_header(&session_id, "客户手写编号.png", "image");
        let raw = tauri::ipc::InvokeBody::Raw(vec![1, 2, 3]);
        let (metadata, bytes) = parse_session_input_request(Some(&header), &raw).unwrap();
        assert_eq!(metadata.session_id, session_id);
        assert_eq!(metadata.name, "客户手写编号.png");
        assert_eq!(metadata.kind, "image");
        assert_eq!(bytes, &[1, 2, 3]);

        assert!(parse_session_input_request(
            Some(&header),
            &tauri::ipc::InvokeBody::Json(serde_json::json!([1, 2, 3])),
        )
        .is_err());
        assert!(parse_session_input_request(
            Some(&header),
            &tauri::ipc::InvokeBody::Raw(vec![0; MAX_IMAGE_INPUT_BYTES + 1]),
        )
        .is_err());
    }

    #[test]
    fn raw_input_request_rejects_missing_or_malformed_metadata_before_save() {
        let session_id = Uuid::new_v4().to_string();
        let raw = tauri::ipc::InvokeBody::Raw(vec![1]);
        let oversized_header = "a".repeat(MAX_INPUT_METADATA_HEADER_BYTES + 1);
        let cases = [
            None,
            Some("not-base64url"),
            Some("e30"),
            Some(oversized_header.as_str()),
        ];
        for header in cases {
            assert!(parse_session_input_request(header, &raw).is_err());
        }
        for header in [
            raw_input_metadata_header("not-a-session", "one.png", "image"),
            raw_input_metadata_header(&session_id, "one.png", "binary"),
        ] {
            assert!(parse_session_input_request(Some(&header), &raw).is_err());
        }
    }

    #[test]
    fn candidate_preview_envelope_is_raw_tagged_and_keeps_the_24_mib_payload() {
        assert_eq!(candidate_preview_envelope(None, vec![]), vec![0]);
        assert_eq!(
            candidate_preview_envelope(Some("JPG"), vec![7, 8]),
            vec![1, 7, 8]
        );
        assert_eq!(
            candidate_preview_envelope(Some("png"), vec![7, 8]),
            vec![2, 7, 8]
        );
        assert_eq!(
            candidate_preview_envelope(Some("webp"), vec![7, 8]),
            vec![3, 7, 8]
        );
        let boundary = candidate_preview_envelope(Some("jpeg"), vec![0; MAX_IMAGE_INPUT_BYTES]);
        assert_eq!(boundary.len(), MAX_IMAGE_INPUT_BYTES + 1);
        assert_eq!(boundary[0], 1);
    }

    fn decode_fixture(value: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .unwrap()
    }

    fn valid_jpeg_bytes() -> Vec<u8> {
        decode_fixture(
            "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAJqADAAQAAAABAAAANgAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8AAEQgANgAmAwEiAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAABAgMEBQYHCAkKC//EALUQAAIBAwMCBAMFBQQEAAABfQECAwAEEQUSITFBBhNRYQcicRQygZGhCCNCscEVUtHwJDNicoIJChYXGBkaJSYnKCkqNDU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6g4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2drh4uPk5ebn6Onq8fLz9PX29/j5+v/EAB8BAAMBAQEBAQEBAQEAAAAAAAABAgMEBQYHCAkKC//EALURAAIBAgQEAwQHBQQEAAECdwABAgMRBAUhMQYSQVEHYXETIjKBCBRCkaGxwQkjM1LwFWJy0QoWJDThJfEXGBkaJicoKSo1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoKDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uLj5OXm5+jp6vLz9PX29/j5+v/bAEMAAgICAgICAwICAwQDAwMEBQQEBAQFBwUFBQUFBwgHBwcHBwcICAgICAgICAoKCgoKCgsLCwsLDQ0NDQ0NDQ0NDf/bAEMBAgICAwMDBgMDBg0JBwkNDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDQ0NDf/dAAQAA//aAAwDAQACEQMRAD8A/fyiiigAooooAKKKKAP/0P38ooooAKKKKACiiigD/9H9/KKKKACiiigAooooA//S/fyiiigAooooAKKKKAP/2Q==",
        )
    }

    fn valid_progressive_jpeg_bytes() -> Vec<u8> {
        decode_fixture(
            "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAEKADAAQAAAABAAAADAAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8IAEQgADAAQAwEiAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAMCBAEFAAYHCAkKC//EAMMQAAEDAwIEAwQGBAcGBAgGcwECAAMRBBIhBTETIhAGQVEyFGFxIweBIJFCFaFSM7EkYjAWwXLRQ5I0ggjhU0AlYxc18JNzolBEsoPxJlQ2ZJR0wmDShKMYcOInRTdls1V1pJXDhfLTRnaA40dWZrQJChkaKCkqODk6SElKV1hZWmdoaWp3eHl6hoeIiYqQlpeYmZqgpaanqKmqsLW2t7i5usDExcbHyMnK0NTV1tfY2drg5OXm5+jp6vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAQIAAwQFBgcICQoL/8QAwxEAAgIBAwMDAgMFAgUCBASHAQACEQMQEiEEIDFBEwUwIjJRFEAGMyNhQhVxUjSBUCSRoUOxFgdiNVPw0SVgwUThcvEXgmM2cCZFVJInotIICQoYGRooKSo3ODk6RkdISUpVVldYWVpkZWZnaGlqc3R1dnd4eXqAg4SFhoeIiYqQk5SVlpeYmZqgo6SlpqeoqaqwsrO0tba3uLm6wMLDxMXGx8jJytDT1NXW19jZ2uDi4+Tl5ufo6ery8/T19vf4+fr/2wBDAAICAgICAgMCAgMFAwMDBQYFBQUFBggGBgYGBggKCAgICAgICgoKCgoKCgoMDAwMDAwODg4ODg8PDw8PDw8PDw//2wBDAQICAgQEBAcEBAcQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/2gAMAwEAAhEDEQAAAfqNVWct/9oACAEBAAEFAlrKHGsrKupo6X//2gAIAQMRAT8Bt//aAAgBAhEBPwF//9oACAEBAAY/AtHRT1ej/8QAMxABAAMAAgICAgIDAQEAAAILAREAITFBUWFxgZGhscHw0RDh8SAwQFBgcICQoLDA0OD/2gAIAQEAAT8hfCOlbQcVctpe7N//2gAMAwEAAhEDEQAAEKf/xAAzEQEBAQADAAECBQUBAQABAQkBABEhMRBBUWEgcfCRgaGx0cHh8TBAUGBwgJCgsMDQ4P/aAAgBAxEBPxDGdX//2gAIAQIRAT8Q+1//2gAIAQEAAT8QmTQMJ7i8GhOHuqw+ILPFUIppCnMTf//Z",
        )
    }

    fn valid_png_bytes() -> Vec<u8> {
        include_bytes!("../icons/32x32.png").to_vec()
    }

    fn valid_webp_bytes() -> Vec<u8> {
        decode_fixture(
            "UklGRvgAAABXRUJQVlA4TOsAAAAvR8AREJeAoG3bePvP4/xJhsb8G0jbJmPG7l/UZyCSzKRPYCVQiUD9Bx7CsqksaCQMy5AZOkDy6w9sI0ly8gIt7hbyjxZe/3pgRPSfgdu2cZyk3dnbzh/Mx2mbiqfAAuBkwITXCOCD2PrDCTjubRDi+mN/Qx1H68DF0vr6K2ndn9g4WhcAp2VoXcPVImh9Q+gEra8IFn6obL2lYyZg8xz6QsBmr+HyN2Zzzfpjt4Vs7uHi+rPZHpETsHkL3VcJb+N+xxrhbd4vgfBeGnqp8HbpNxbe/s4gvO58QJIjb7DkSLNGOZLBRWfzdzEDAA==",
        )
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn png_with_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = valid_png_bytes();
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let checksum = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
        bytes
    }

    fn padded_png_bytes(target_size: usize) -> Vec<u8> {
        let source = valid_png_bytes();
        let mut iend_offset = 8;
        while iend_offset + 12 <= source.len() {
            let length =
                u32::from_be_bytes(source[iend_offset..iend_offset + 4].try_into().unwrap())
                    as usize;
            if &source[iend_offset + 4..iend_offset + 8] == b"IEND" {
                break;
            }
            iend_offset += length + 12;
        }
        assert!(target_size >= source.len() + 12);
        let payload_length = target_size - source.len() - 12;
        let mut result = Vec::with_capacity(target_size);
        result.extend_from_slice(&source[..iend_offset]);
        result.extend_from_slice(&(payload_length as u32).to_be_bytes());
        result.extend_from_slice(b"stEg");
        result.resize(result.len() + payload_length, 0);
        let chunk_start = iend_offset + 4;
        result.extend_from_slice(&crc32(&result[chunk_start..]).to_be_bytes());
        result.extend_from_slice(&source[iend_offset..]);
        assert_eq!(result.len(), target_size);
        result
    }

    fn png_with_chunk_before_iend(kind: [u8; 4], payload: &[u8], valid_crc: bool) -> Vec<u8> {
        let source = valid_png_bytes();
        let iend_offset = source.len() - 12;
        assert_eq!(&source[iend_offset + 4..iend_offset + 8], b"IEND");
        let mut result = Vec::with_capacity(source.len() + payload.len() + 12);
        result.extend_from_slice(&source[..iend_offset]);
        result.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        result.extend_from_slice(&kind);
        result.extend_from_slice(payload);
        let checksum = if valid_crc {
            crc32(&result[iend_offset + 4..])
        } else {
            0
        };
        result.extend_from_slice(&checksum.to_be_bytes());
        result.extend_from_slice(&source[iend_offset..]);
        result
    }

    fn png_with_chunk_before_first_idat(source: &[u8], kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut offset = 8_usize;
        while &source[offset + 4..offset + 8] != b"IDAT" {
            let length =
                u32::from_be_bytes(source[offset..offset + 4].try_into().unwrap()) as usize;
            offset += length + 12;
        }
        let mut result = Vec::with_capacity(source.len() + payload.len() + 12);
        result.extend_from_slice(&source[..offset]);
        result.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        result.extend_from_slice(&kind);
        result.extend_from_slice(payload);
        let checksum_start = offset + 4;
        result.extend_from_slice(&crc32(&result[checksum_start..]).to_be_bytes());
        result.extend_from_slice(&source[offset..]);
        result
    }

    fn png_with_color_type(color_type: u8) -> Vec<u8> {
        let mut bytes = valid_png_bytes();
        bytes[25] = color_type;
        let checksum = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
        bytes
    }

    fn synthetic_webp(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (kind, payload) in chunks {
            body.extend_from_slice(kind);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
            if payload.len() % 2 != 0 {
                body.push(0);
            }
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    fn vp8x_payload(flags: u8, width: u32, height: u32) -> Vec<u8> {
        let mut payload = vec![flags, 0, 0, 0];
        let width = width - 1;
        let height = height - 1;
        payload.extend_from_slice(&width.to_le_bytes()[..3]);
        payload.extend_from_slice(&height.to_le_bytes()[..3]);
        payload
    }

    #[test]
    fn save_session_input_accepts_fully_decodable_supported_images() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state.storage.create_session("image formats", None).unwrap();
        let cases = [
            ("customer.jpg", valid_jpeg_bytes(), "image/jpeg"),
            ("customer.png", valid_png_bytes(), "image/png"),
            ("renamed.txt", valid_webp_bytes(), "image/webp"),
        ];

        for (name, bytes, expected_mime) in cases {
            let saved =
                save_session_input_inner(&state, &session.id, name.into(), "image".into(), bytes)
                    .unwrap();
            assert_eq!(saved.mime, expected_mime);
        }
    }

    #[test]
    fn invalid_inputs_are_rejected_before_session_or_filesystem_mutation() {
        let cases = [
            ("bad-kind", "binary", vec![1]),
            ("invalid-text", "text", vec![0xff]),
            ("empty-image", "image", vec![]),
            ("header-only-jpeg", "image", vec![0xff, 0xd8, 0xff]),
            ("header-only-png", "image", b"\x89PNG\r\n\x1a\n".to_vec()),
            ("header-only-webp", "image", b"RIFF\0\0\0\0WEBP".to_vec()),
        ];
        for (label, kind, bytes) in cases {
            let directory = safe_tempdir();
            let state = test_state(directory.path());
            let session = state.storage.create_session(label, None).unwrap();
            save_session_input_inner(
                &state,
                &session.id,
                format!("{label}.bin"),
                kind.into(),
                bytes,
            )
            .unwrap_err();
            assert_eq!(
                state.storage.load_session(&session.id).unwrap().status,
                SessionStatus::Draft,
                "{label} advanced the session"
            );
            assert!(state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .is_empty());
            assert!(!directory
                .path()
                .join("sessions")
                .join(&session.id)
                .join("inputs")
                .exists());
        }
    }

    #[test]
    fn nonexistent_session_is_rejected_before_content_validation_or_body_clone() {
        use std::cell::Cell;

        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session_id = Uuid::new_v4().to_string();
        let metadata = SessionInputMetadata {
            session_id: session_id.clone(),
            name: "malformed.png".into(),
            kind: "image".into(),
        };
        let validation_calls = Cell::new(0);
        let clone_calls = Cell::new(0);

        let error = save_borrowed_session_input_with(
            &state,
            metadata,
            b"not-an-image",
            |_, _| {
                validation_calls.set(validation_calls.get() + 1);
                Err(CommandError::Other("content validator must not run".into()))
            },
            |bytes| {
                clone_calls.set(clone_calls.get() + 1);
                bytes.to_vec()
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CommandError::Storage(StorageError::SessionNotFound)
        ));
        assert_eq!(validation_calls.get(), 0);
        assert_eq!(clone_calls.get(), 0);
        assert!(!directory.path().join("sessions").join(session_id).exists());
    }

    #[test]
    fn busy_session_rejects_a_24_mib_body_before_validation_or_clone_without_mutation() {
        use std::cell::Cell;

        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state.storage.create_session("busy input", None).unwrap();
        state
            .storage
            .compare_and_set_status(&session.id, SessionStatus::Draft, SessionStatus::Scanning)
            .unwrap();
        let metadata = SessionInputMetadata {
            session_id: session.id.clone(),
            name: "boundary.png".into(),
            kind: "image".into(),
        };
        let body = vec![0; MAX_IMAGE_INPUT_BYTES];
        let validation_calls = Cell::new(0);
        let clone_calls = Cell::new(0);

        let error = save_borrowed_session_input_with(
            &state,
            metadata,
            &body,
            |_, _| {
                validation_calls.set(validation_calls.get() + 1);
                Ok("image/png")
            },
            |bytes| {
                clone_calls.set(clone_calls.get() + 1);
                bytes.to_vec()
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CommandError::Storage(StorageError::SessionBusy)
        ));
        assert_eq!(validation_calls.get(), 0);
        assert_eq!(clone_calls.get(), 0);
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::Scanning
        );
        assert!(state
            .storage
            .list_session_inputs(&session.id)
            .unwrap()
            .is_empty());
        assert!(!directory.path().join("sessions").join(&session.id).exists());
    }

    #[test]
    fn authorized_input_is_fully_validated_then_cloned_exactly_once() {
        use std::cell::{Cell, RefCell};

        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state.storage.create_session("ordered input", None).unwrap();
        let metadata = SessionInputMetadata {
            session_id: session.id.clone(),
            name: "customer.txt".into(),
            kind: "text".into(),
        };
        let validation_calls = Cell::new(0);
        let clone_calls = Cell::new(0);
        let order = RefCell::new(Vec::new());

        let saved = save_borrowed_session_input_with(
            &state,
            metadata,
            b"0012",
            |kind, bytes| {
                validation_calls.set(validation_calls.get() + 1);
                order.borrow_mut().push("validate");
                assert_eq!(kind, "text");
                assert_eq!(bytes, b"0012");
                Ok("text/plain")
            },
            |bytes| {
                clone_calls.set(clone_calls.get() + 1);
                order.borrow_mut().push("clone");
                bytes.to_vec()
            },
        )
        .unwrap();

        assert_eq!(validation_calls.get(), 1);
        assert_eq!(clone_calls.get(), 1);
        assert_eq!(*order.borrow(), ["validate", "clone"]);
        assert_eq!(saved.size, 4);
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::AwaitingNumberConfirmation
        );
        assert_eq!(
            state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn malformed_and_resource_exhausting_images_are_rejected_without_mutation() {
        let mut truncated_jpeg = valid_jpeg_bytes();
        truncated_jpeg.truncate(truncated_jpeg.len() - 2);
        let mut jpeg_with_fake_final_eoi = valid_jpeg_bytes();
        jpeg_with_fake_final_eoi.extend_from_slice(b"untrusted-tail\xff\xd9");
        let mut mismatched_png = b"\x89PNG\r\n\x1a\n".to_vec();
        mismatched_png.extend_from_slice(&valid_jpeg_bytes());
        let mut trailing_polyglot = valid_webp_bytes();
        trailing_polyglot.extend_from_slice(b"<script>alert(1)</script>");
        let mut invalid_iend_crc = valid_png_bytes();
        let last = invalid_iend_crc.len() - 1;
        invalid_iend_crc[last] ^= 1;
        let invalid_ancillary_crc = png_with_chunk_before_iend(*b"stEg", b"metadata", false);
        let cases = [
            ("truncated", truncated_jpeg, "内容损坏"),
            ("jpeg-fake-final-eoi", jpeg_with_fake_final_eoi, "内容损坏"),
            ("format-mismatch", mismatched_png, "内容损坏"),
            ("trailing-polyglot", trailing_polyglot, "内容损坏"),
            ("invalid-iend-crc", invalid_iend_crc, "内容损坏"),
            ("invalid-ancillary-crc", invalid_ancillary_crc, "内容损坏"),
            ("too-wide", png_with_dimensions(20_001, 1), "尺寸过大"),
            (
                "too-many-pixels",
                png_with_dimensions(10_000, 5_001),
                "尺寸过大",
            ),
        ];
        for (label, bytes, expected_message) in cases {
            let directory = safe_tempdir();
            let state = test_state(directory.path());
            let session = state.storage.create_session(label, None).unwrap();
            let error = save_session_input_inner(
                &state,
                &session.id,
                format!("{label}.png"),
                "image".into(),
                bytes,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(expected_message),
                "unexpected {label} error: {error}"
            );
            assert_eq!(
                state.storage.load_session(&session.id).unwrap().status,
                SessionStatus::Draft
            );
            assert!(!directory
                .path()
                .join("sessions")
                .join(&session.id)
                .join("inputs")
                .exists());
        }
    }

    #[test]
    fn valid_exif_jpeg_and_ancillary_png_remain_accepted() {
        assert_eq!(
            validate_image_input(&valid_jpeg_bytes()).unwrap(),
            "image/jpeg"
        );
        let progressive = valid_progressive_jpeg_bytes();
        assert!(progressive.windows(2).any(|bytes| bytes == [0xff, 0xc2]));
        assert!(
            progressive
                .windows(2)
                .filter(|bytes| *bytes == [0xff, 0xda])
                .count()
                > 1
        );
        assert_eq!(validate_image_input(&progressive).unwrap(), "image/jpeg");
        assert_eq!(
            validate_image_input(&png_with_chunk_before_iend(*b"stEg", b"metadata", true)).unwrap(),
            "image/png"
        );
    }

    #[test]
    fn jpeg_boundary_parser_handles_entropy_escapes_restarts_tem_and_marker_fill() {
        let synthetic = [
            0xff, 0xd8, // SOI
            0xff, 0xda, 0x00, 0x02, // minimal SOS segment for boundary parsing
            0x11, 0xff, 0x00, 0x22, // entropy byte and FF00 escape
            0xff, 0xd0, 0x33, // restart marker and more entropy
            0xff, 0x01, 0x44, // TEM marker and more entropy
            0xff, 0xff, 0xd9, // marker fill followed by the real EOI
        ];
        assert!(jpeg_has_exact_eoi(&synthetic));
    }

    #[test]
    fn png_structure_rejects_unknown_critical_and_invalid_plte_placement() {
        let unknown_critical = png_with_chunk_before_iend(*b"ABCD", b"critical", true);
        let plte_after_idat = png_with_chunk_before_iend(*b"PLTE", &[0, 0, 0], true);
        let with_plte = png_with_chunk_before_first_idat(&valid_png_bytes(), *b"PLTE", &[0, 0, 0]);
        let duplicate_plte = png_with_chunk_before_first_idat(&with_plte, *b"PLTE", &[1, 1, 1]);

        assert!(!png_has_valid_structure(&unknown_critical));
        assert!(!png_has_valid_structure(&plte_after_idat));
        assert!(!png_has_valid_structure(&duplicate_plte));
        assert!(png_has_valid_structure(&png_with_chunk_before_iend(
            *b"stEg",
            b"post-idat metadata",
            true,
        )));
    }

    #[test]
    fn png_structure_enforces_color_type_plte_rules() {
        let indexed_without_plte = png_with_color_type(3);
        let grayscale_with_plte =
            png_with_chunk_before_first_idat(&png_with_color_type(0), *b"PLTE", &[0, 0, 0]);
        assert!(!png_has_valid_structure(&indexed_without_plte));
        assert!(!png_has_valid_structure(&grayscale_with_plte));
    }

    #[test]
    fn animated_and_oversized_webp_headers_are_rejected_before_decode() {
        let animated =
            synthetic_webp(&[(*b"VP8X", vp8x_payload(0x02, 1, 1)), (*b"ANIM", vec![0; 6])]);
        let oversized_vp8x = synthetic_webp(&[(*b"VP8X", vp8x_payload(0, 5_000, 5_000))]);

        let mut vp8 = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
        vp8.extend_from_slice(&16_383_u16.to_le_bytes());
        vp8.extend_from_slice(&16_383_u16.to_le_bytes());
        let oversized_vp8 = synthetic_webp(&[(*b"VP8 ", vp8)]);

        let dimensions = (16_383_u32 - 1) | ((16_383_u32 - 1) << 14);
        let mut vp8l = vec![0x2f];
        vp8l.extend_from_slice(&dimensions.to_le_bytes());
        let oversized_vp8l = synthetic_webp(&[(*b"VP8L", vp8l)]);

        assert!(validate_image_input(&animated)
            .unwrap_err()
            .to_string()
            .contains("不支持动画"));
        for bytes in [oversized_vp8x, oversized_vp8, oversized_vp8l] {
            let error = validate_image_input(&bytes).unwrap_err().to_string();
            assert!(
                error.contains("WebP") && error.contains("像素"),
                "unexpected WebP limit error: {error}"
            );
        }
        assert_eq!(
            validate_image_input(&valid_webp_bytes()).unwrap(),
            "image/webp"
        );
    }

    #[test]
    fn image_byte_limit_accepts_valid_boundary_and_rejects_larger_before_mutation() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let accepted = state.storage.create_session("at limit", None).unwrap();
        let bytes = padded_png_bytes(24 * 1024 * 1024);
        let saved = save_session_input_inner(
            &state,
            &accepted.id,
            "at-limit.png".into(),
            "image".into(),
            bytes,
        )
        .unwrap();
        assert_eq!(saved.size, 24 * 1024 * 1024);

        let rejected = state.storage.create_session("over limit", None).unwrap();
        let oversized = padded_png_bytes(24 * 1024 * 1024 + 1);
        save_session_input_inner(
            &state,
            &rejected.id,
            "too-large.png".into(),
            "image".into(),
            oversized,
        )
        .unwrap_err();
        assert_eq!(
            state.storage.load_session(&rejected.id).unwrap().status,
            SessionStatus::Draft
        );
        assert!(!directory
            .path()
            .join("sessions")
            .join(&rejected.id)
            .join("inputs")
            .exists());
    }

    fn save_png_input(state: &AppState, session_id: &str, name: &str, size: usize) -> String {
        let bytes = if size <= valid_png_bytes().len() {
            valid_png_bytes()
        } else {
            padded_png_bytes(size)
        };
        save_session_input_inner(state, session_id, name.into(), "image".into(), bytes)
            .unwrap()
            .id
    }

    fn persist_legacy_image_input(
        state: &AppState,
        session_id: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> String {
        use std::io::{Seek, SeekFrom, Write};

        let input_id = Uuid::new_v4().to_string();
        let input_root = state
            .app_data_dir
            .join("sessions")
            .join(session_id)
            .join("inputs");
        fs::create_dir_all(&input_root).unwrap();
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(input_root.join(&input_id))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let fingerprint = capture_file_fingerprint(&mut file).unwrap();
        state
            .storage
            .save_session_input(&SessionInput {
                id: input_id.clone(),
                session_id: session_id.into(),
                name: name.into(),
                kind: "image".into(),
                mime: mime.into(),
                stored_name: input_id.clone(),
                size: bytes.len() as u64,
                root_identity: capture_directory_identity(&input_root).unwrap(),
                fingerprint,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();
        input_id
    }

    #[test]
    fn persisted_recognition_revalidates_legacy_content_and_exact_mime_before_provider_access() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let cases = [
            (
                "legacy header",
                "legacy.jpg",
                "image/jpeg",
                vec![0xff, 0xd8, 0xff],
            ),
            (
                "mime mismatch",
                "mismatch.png",
                "image/jpeg",
                valid_png_bytes(),
            ),
        ];

        for (label, name, mime, bytes) in cases {
            let session = state.storage.create_session(label, None).unwrap();
            let input_id = persist_legacy_image_input(&state, &session.id, name, mime, &bytes);
            let error = tauri::async_runtime::block_on(recognize_cloud_inner(
                &state,
                "provider-must-not-be-reached",
                &session.id,
                &[input_id],
            ))
            .unwrap_err();
            assert_eq!(error.to_string(), "操作失败：持久化识别图片无法安全读取");
        }
    }

    #[test]
    fn persisted_recognition_ids_require_unique_owned_image_inputs() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let current = state.storage.create_session("current", None).unwrap();
        let other = state.storage.create_session("other", None).unwrap();
        let image_id = save_png_input(&state, &current.id, "image.png", 8);
        let other_id = save_png_input(&state, &other.id, "other.png", 8);
        let text_id = save_session_input_inner(
            &state,
            &current.id,
            "numbers.txt".into(),
            "text".into(),
            b"1234".to_vec(),
        )
        .unwrap()
        .id;

        for ids in [
            vec![image_id.clone(), image_id.clone()],
            vec![other_id],
            vec![text_id],
            vec![Uuid::new_v4().to_string()],
            vec!["not-an-input-id".into()],
        ] {
            let error = tauri::async_runtime::block_on(recognize_cloud_inner(
                &state,
                "provider-that-does-not-exist",
                &current.id,
                &ids,
            ))
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "操作失败：识别图片批次无效或过大（最多 12 张、合计 48 MiB）"
            );
        }
    }

    #[test]
    fn persisted_recognition_rejects_oversized_image_metadata_before_file_read() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state
            .storage
            .create_session("oversized metadata", None)
            .unwrap();
        let input_id = Uuid::new_v4().to_string();
        let input_root = directory
            .path()
            .join("sessions")
            .join(&session.id)
            .join("inputs");
        fs::create_dir_all(&input_root).unwrap();
        let oversized = (providers::MAX_RECOGNITION_IMAGE_BYTES + 1) as u64;
        state
            .storage
            .save_session_input(&SessionInput {
                id: input_id.clone(),
                session_id: session.id.clone(),
                name: "bounded-error.png".into(),
                kind: "image".into(),
                mime: "image/png".into(),
                stored_name: input_id.clone(),
                size: oversized,
                root_identity: capture_directory_identity(&input_root).unwrap(),
                fingerprint: crate::models::FileFingerprint {
                    size: oversized,
                    ..Default::default()
                },
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .unwrap();

        let error =
            load_persisted_recognition_images(&state, &session.id, &[input_id]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "操作失败：识别图片批次无效或过大（最多 12 张、合计 48 MiB）"
        );
    }

    #[test]
    fn persisted_recognition_accepts_exact_count_and_aggregate_boundaries() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let count_session = state.storage.create_session("count limit", None).unwrap();
        let count_ids = (0..providers::MAX_RECOGNITION_IMAGES)
            .map(|index| {
                save_png_input(&state, &count_session.id, &format!("count-{index}.png"), 8)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            load_persisted_recognition_images(&state, &count_session.id, &count_ids)
                .unwrap()
                .len(),
            providers::MAX_RECOGNITION_IMAGES
        );

        let byte_session = state.storage.create_session("byte limit", None).unwrap();
        let byte_ids = [
            save_png_input(&state, &byte_session.id, "first.png", 24 * 1024 * 1024),
            save_png_input(&state, &byte_session.id, "second.png", 24 * 1024 * 1024),
        ];
        let images =
            load_persisted_recognition_images(&state, &byte_session.id, &byte_ids).unwrap();
        assert_eq!(
            images.iter().map(Vec::len).sum::<usize>(),
            providers::MAX_RECOGNITION_BATCH_BYTES
        );
    }

    #[test]
    fn persisted_recognition_rejects_count_and_aggregate_before_credentials() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let count_session = state.storage.create_session("too many", None).unwrap();
        let count_ids = (0..=providers::MAX_RECOGNITION_IMAGES)
            .map(|index| {
                save_png_input(&state, &count_session.id, &format!("count-{index}.png"), 8)
            })
            .collect::<Vec<_>>();
        let count_error = tauri::async_runtime::block_on(recognize_cloud_inner(
            &state,
            "provider-that-does-not-exist",
            &count_session.id,
            &count_ids,
        ))
        .unwrap_err();
        assert_eq!(
            count_error.to_string(),
            "操作失败：识别图片批次无效或过大（最多 12 张、合计 48 MiB）"
        );

        let byte_session = state.storage.create_session("too large", None).unwrap();
        let byte_ids = [
            save_png_input(&state, &byte_session.id, "one.png", 16 * 1024 * 1024),
            save_png_input(&state, &byte_session.id, "two.png", 16 * 1024 * 1024),
            save_png_input(&state, &byte_session.id, "three.png", 16 * 1024 * 1024 + 1),
        ];
        let byte_error = tauri::async_runtime::block_on(recognize_cloud_inner(
            &state,
            "provider-that-does-not-exist",
            &byte_session.id,
            &byte_ids,
        ))
        .unwrap_err();
        assert_eq!(byte_error.to_string(), count_error.to_string());
    }

    fn paused_copied_session(
        state: &AppState,
        directory: &Path,
        label: &str,
    ) -> (String, PathBuf, PathBuf, i64) {
        let session = state.storage.create_session(label, None).unwrap();
        let source = directory.join(format!("{label}-source"));
        let target = directory.join(format!("{label}-target"));
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let source_file = source.join("IMG_0007.JPG");
        let target_file = target.join("IMG_0007.JPG");
        fs::write(&source_file, b"trusted").unwrap();
        fs::write(&target_file, b"trusted").unwrap();
        state
            .storage
            .save_confirmed_numbers(
                &session.id,
                &[PhotoNumber {
                    original: "7".into(),
                    canonical: "7".into(),
                    confidence: None,
                    confirmed: true,
                }],
            )
            .unwrap();
        let fingerprint =
            capture_file_fingerprint(&mut fs::File::open(&source_file).unwrap()).unwrap();
        let snapshot = ScanSnapshot {
            source: source.clone(),
            source_root_identity: capture_directory_identity(&source).unwrap(),
            target: target.clone(),
            items: vec![NumberMatch {
                canonical_number: "7".into(),
                status: MatchStatus::Complete,
                groups: vec![crate::models::CandidateGroup {
                    id: "group-7".into(),
                    files: vec![crate::models::IndexedFile {
                        path: source_file.clone(),
                        stem: "IMG_0007".into(),
                        extension: "JPG".into(),
                        canonical_number: "7".into(),
                        size: fingerprint.size,
                        modified_ms: fingerprint.modified_ms,
                        family_key: "IMG_0007".into(),
                        fingerprint: fingerprint.clone(),
                    }],
                }],
            }],
            skipped_numbers: vec![],
        };
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        state
            .storage
            .compare_and_set_status(
                &session.id,
                SessionStatus::ReadyToScan,
                SessionStatus::Scanning,
            )
            .unwrap();
        let plan_items = copy_plan_items(&session.id, &snapshot);
        let item_id = plan_items[0].id.clone();
        let revision = state
            .storage
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&PreflightReport::default()),
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&plan_items),
            )
            .unwrap();
        state
            .storage
            .consume_confirmation_and_start_copy(&session.id, revision, None, false)
            .unwrap();
        state
            .storage
            .update_copy_item(
                &item_id,
                revision,
                CopyItemStatus::Copied,
                Some(&fingerprint.content_hash),
                None,
                None,
                None,
            )
            .unwrap();
        state
            .storage
            .save_worker_preflight(
                &session.id,
                revision,
                &PreflightReport {
                    source_disconnected: true,
                    ..PreflightReport::default()
                },
                Some(SessionStatus::NeedsAttention),
            )
            .unwrap();
        (session.id, source_file, target_file, revision)
    }

    fn two_file_copy_plan(directory: &Path) -> CopyPlan {
        let source_root = directory.join("copy-source");
        let target_root = directory.join("copy-target");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let files = ["first.jpg", "second.jpg"]
            .into_iter()
            .map(|name| {
                let source = source_root.join(name);
                fs::write(&source, name.as_bytes()).unwrap();
                SelectedFile {
                    canonical_number: name.into(),
                    target: target_root.join(name),
                    fingerprint: capture_file_fingerprint(&mut fs::File::open(&source).unwrap())
                        .unwrap(),
                    source,
                }
            })
            .collect();
        CopyPlan {
            source_root: source_root.clone(),
            source_root_identity: capture_directory_identity(&source_root).unwrap(),
            target_root,
            files,
        }
    }

    #[test]
    fn started_history_failure_stops_before_any_physical_copy() {
        let directory = safe_tempdir();
        let plan = two_file_copy_plan(directory.path());
        let cancelled = AtomicBool::new(false);
        let mut persistence_failed = false;

        let report = execute_copy_with_updates(
            &plan,
            &cancelled,
            |_, _| {},
            |_, outcome| {
                if persist_copy_outcome_with(
                    &cancelled,
                    outcome,
                    |status, _, _, _, _| -> Result<(), StorageError> {
                        if status == CopyItemStatus::Copying {
                            Err(StorageError::StateConflict)
                        } else {
                            Ok(())
                        }
                    },
                )
                .is_err()
                {
                    persistence_failed = true;
                }
            },
        );

        assert!(persistence_failed);
        assert!(report.cancelled);
        assert!(!plan.files[0].target.exists());
        assert!(!plan.files[1].target.exists());
    }

    #[test]
    fn copied_history_failure_prevents_the_next_physical_copy() {
        let directory = safe_tempdir();
        let plan = two_file_copy_plan(directory.path());
        let cancelled = AtomicBool::new(false);
        let mut persistence_failed = false;

        let report = execute_copy_with_updates(
            &plan,
            &cancelled,
            |_, _| {},
            |_, outcome| {
                if persist_copy_outcome_with(
                    &cancelled,
                    outcome,
                    |status, _, _, _, _| -> Result<(), StorageError> {
                        if status == CopyItemStatus::Copied {
                            Err(StorageError::StateConflict)
                        } else {
                            Ok(())
                        }
                    },
                )
                .is_err()
                {
                    persistence_failed = true;
                }
            },
        );

        assert!(persistence_failed);
        assert!(report.cancelled);
        assert!(plan.files[0].target.exists());
        assert!(!plan.files[1].target.exists());
    }

    #[test]
    fn resumed_completion_stats_include_the_entire_same_revision_history() {
        let directory = safe_tempdir();
        let plan = two_file_copy_plan(directory.path());
        let snapshot = ScanSnapshot {
            source: plan.source_root.clone(),
            source_root_identity: plan.source_root_identity.clone(),
            target: plan.target_root.clone(),
            items: plan
                .files
                .iter()
                .map(|file| NumberMatch {
                    canonical_number: file.canonical_number.clone(),
                    status: MatchStatus::Complete,
                    groups: vec![crate::models::CandidateGroup {
                        id: file.canonical_number.clone(),
                        files: vec![crate::models::IndexedFile {
                            path: file.source.clone(),
                            stem: file.canonical_number.clone(),
                            extension: "jpg".into(),
                            canonical_number: file.canonical_number.clone(),
                            size: file.fingerprint.size,
                            modified_ms: file.fingerprint.modified_ms,
                            family_key: PathBuf::from(&file.canonical_number),
                            fingerprint: file.fingerprint.clone(),
                        }],
                    }],
                })
                .collect(),
            skipped_numbers: vec![],
        };
        let mut history = copy_plan_items("session", &snapshot);
        for item in &mut history {
            item.plan_revision = 7;
            item.status = CopyItemStatus::Copied;
            item.source_hash = Some(item.planned_hash.clone());
        }
        let mut identical = CopyHistoryItem::planned(
            "session",
            "identical",
            Path::new("/source/identical.jpg"),
            Path::new("/target/identical.jpg"),
            "same",
        );
        identical.plan_revision = 7;
        identical.status = CopyItemStatus::Skipped;
        identical.source_hash = Some("same".into());
        identical.skipped_reason = Some("identical-target".into());
        history.push(identical);
        let mut failed = CopyHistoryItem::planned(
            "session",
            "failed",
            Path::new("/source/failed.jpg"),
            Path::new("/target/failed.jpg"),
            "failed",
        );
        failed.plan_revision = 7;
        failed.status = CopyItemStatus::Failed;
        history.push(failed);
        let mut old_revision = history[0].clone();
        old_revision.id = "old-revision".into();
        old_revision.plan_revision = 6;
        history.push(old_revision);

        let stats = completion_stats(&snapshot, &history, 7);

        assert_eq!(stats.copied_count, 2);
        assert_eq!(stats.skipped_identical_count, 1);
        assert_eq!(stats.failed_count, 1);
        assert_eq!(
            stats.copied_bytes,
            plan.files
                .iter()
                .map(|file| file.fingerprint.size)
                .sum::<u64>()
        );
    }

    #[test]
    fn incomplete_preflight_never_reclassifies_terminal_history_as_target_changed() {
        let mut copied = CopyHistoryItem::planned(
            "session",
            "7",
            Path::new("/source/IMG_0007.JPG"),
            Path::new("/target/IMG_0007.JPG"),
            "trusted",
        );
        copied.plan_revision = 3;
        copied.status = CopyItemStatus::Copied;
        copied.source_hash = Some("trusted".into());

        for report in [
            PreflightReport {
                source_disconnected: true,
                ..PreflightReport::default()
            },
            PreflightReport {
                source_changed: true,
                ..PreflightReport::default()
            },
            PreflightReport {
                permission_denied: true,
                ..PreflightReport::default()
            },
            PreflightReport {
                atomic_commit_unsupported: true,
                ..PreflightReport::default()
            },
        ] {
            assert!(
                !terminal_copy_targets_changed(&report, std::slice::from_ref(&copied), 3),
                "{report:?}"
            );
        }
    }

    #[test]
    fn ordinary_worker_failure_finalizes_failed_and_removes_the_task_entry() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let (session_id, _, _, revision) =
            paused_copied_session(&state, directory.path(), "ordinary-worker-error");
        state
            .storage
            .compare_and_set_status(
                &session_id,
                SessionStatus::NeedsAttention,
                SessionStatus::ReadyToCopy,
            )
            .unwrap();
        state
            .storage
            .consume_confirmation_and_start_copy(&session_id, revision, None, false)
            .unwrap();
        state
            .cancelled
            .lock()
            .unwrap()
            .insert(session_id.clone(), Arc::new(AtomicBool::new(false)));
        let mut emitted_failed = false;

        settle_worker_failure_with(
            &state,
            &session_id,
            revision,
            "job",
            "started",
            WorkerFailure::Terminal(CommandError::Other("ordinary worker error".into())),
            |report| {
                emitted_failed = matches!(report.status, CompletionStatus::Failed);
                Ok(())
            },
        )
        .unwrap();

        assert!(emitted_failed);
        assert_eq!(
            state.storage.load_session(&session_id).unwrap().status,
            SessionStatus::Failed
        );
        assert!(!state.cancelled.lock().unwrap().contains_key(&session_id));
    }

    #[test]
    fn explicit_persistence_failure_preserves_recovery_state_but_removes_task_entry() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let (session_id, _, _, revision) =
            paused_copied_session(&state, directory.path(), "persistence-worker-error");
        state
            .storage
            .compare_and_set_status(
                &session_id,
                SessionStatus::NeedsAttention,
                SessionStatus::ReadyToCopy,
            )
            .unwrap();
        state
            .storage
            .consume_confirmation_and_start_copy(&session_id, revision, None, false)
            .unwrap();
        state
            .cancelled
            .lock()
            .unwrap()
            .insert(session_id.clone(), Arc::new(AtomicBool::new(false)));
        let mut diagnostic = String::new();

        settle_worker_failure_with(
            &state,
            &session_id,
            revision,
            "job",
            "started",
            WorkerFailure::RecoverablePersistence(CommandError::Other(
                "history write failed".into(),
            )),
            |report| {
                diagnostic = report.message.clone();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            state.storage.load_session(&session_id).unwrap().status,
            SessionStatus::Copying
        );
        assert!(diagnostic.contains("已保留会话、输入与复制历史供恢复"));
        assert!(!state.cancelled.lock().unwrap().contains_key(&session_id));
    }

    #[test]
    fn recheck_blocks_when_a_terminal_copied_target_was_deleted_or_replaced() {
        for (label, replacement) in [
            ("deleted", None),
            ("replaced", Some(b"tampered".as_slice())),
        ] {
            let directory = safe_tempdir();
            let state = test_state(directory.path());
            let (session_id, _, target, _) = paused_copied_session(&state, directory.path(), label);
            fs::remove_file(&target).unwrap();
            if let Some(bytes) = replacement {
                fs::write(&target, bytes).unwrap();
            }

            let error = validate_recheck_with(
                &state,
                &session_id,
                |source| Ok(source.to_path_buf()),
                |target| Ok(target.to_path_buf()),
                cleanup_pending_target_parts,
            )
            .unwrap_err();

            assert!(
                error.to_string().contains("已复制目标发生变化，请重新扫描"),
                "{label}: {error}"
            );
            assert_eq!(
                state.storage.load_session(&session_id).unwrap().status,
                SessionStatus::NeedsAttention
            );
            assert!(
                state
                    .storage
                    .load_preflight(&session_id)
                    .unwrap()
                    .unwrap()
                    .terminal_target_changed,
                "{label}"
            );
        }
    }

    #[test]
    fn recheck_prepares_both_directories_before_touching_a_pending_partial() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let (session_id, source, target, _) =
            paused_copied_session(&state, directory.path(), "target-reconnect-first");
        let order = std::cell::RefCell::new(Vec::new());

        let error = validate_recheck_with(
            &state,
            &session_id,
            |path| {
                order.borrow_mut().push("source");
                assert_eq!(path, source.parent().unwrap());
                Ok(path.to_path_buf())
            },
            |path| {
                order.borrow_mut().push("target");
                assert_eq!(path, target.parent().unwrap());
                Err(CommandError::TargetDirectoryUnavailable)
            },
            |_, _| {
                order.borrow_mut().push("cleanup");
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(error, CommandError::TargetDirectoryUnavailable));
        assert_eq!(*order.borrow(), vec!["source", "target"]);
        assert_eq!(
            state.storage.load_session(&session_id).unwrap().status,
            SessionStatus::NeedsAttention
        );
    }

    #[test]
    fn recheck_share_root_raw3_without_parent_proof_keeps_the_stored_pending_record() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let (session_id, source_file, target_file, revision) =
            paused_copied_session(&state, directory.path(), "raw3-recheck");
        let pending = crate::copy_engine::PendingTargetPart {
            target: target_file,
            part_name: ".photo-selector.part-exact".into(),
            identity: Default::default(),
        };
        let disconnect = TargetDisconnectInfo {
            windows_code: Some(3),
            operation: crate::copy_engine::TargetOperation::OpenRoot,
        };
        state
            .storage
            .save_attention_preflight(
                &session_id,
                revision,
                &PreflightReport {
                    target_disconnected: true,
                    target_disconnect: Some(disconnect),
                    pending_target_parts: vec![pending.clone()],
                    ..PreflightReport::default()
                },
            )
            .unwrap();

        let error = validate_recheck_with(
            &state,
            &session_id,
            |source| {
                assert_eq!(source, source_file.parent().unwrap());
                Ok(source.to_path_buf())
            },
            |target| Ok(target.to_path_buf()),
            |_, _| {
                Err(CopyError::TargetDisconnected {
                    info: disconnect,
                    pending_part: None,
                })
            },
        )
        .unwrap_err();

        assert!(matches!(error, CommandError::TargetDirectoryUnavailable));
        let saved = state.storage.load_preflight(&session_id).unwrap().unwrap();
        assert_eq!(saved.pending_target_parts, vec![pending]);
        assert_eq!(saved.target_disconnect, Some(disconnect));
    }

    #[test]
    fn source_selection_persists_the_prepared_resolved_directory() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state
            .storage
            .create_session("resolved source", None)
            .unwrap();
        let selected = directory.path().join("mapped-drive/source");
        let resolved = directory.path().join("resolved-unc/source");
        fs::create_dir_all(&selected).unwrap();
        fs::create_dir_all(&resolved).unwrap();

        let paths = bind_selected_directory_with(
            &state,
            &session.id,
            DirectoryPurpose::Source,
            &selected,
            |_| Ok(resolved.clone()),
        )
        .unwrap();

        assert_eq!(paths.source.as_deref(), Some(resolved.to_str().unwrap()));
        assert_eq!(
            state.storage.load_bound_paths(&session.id).unwrap(),
            (
                Some(resolved.clone()),
                Some(default_target(&resolved).unwrap())
            )
        );
    }

    #[test]
    fn manual_directory_input_requires_a_non_empty_absolute_path() {
        let directory = safe_tempdir();
        assert_eq!(
            parse_manual_directory(directory.path().to_string_lossy().as_ref()).unwrap(),
            directory.path()
        );
        assert!(matches!(
            parse_manual_directory("   "),
            Err(CommandError::InvalidDirectory)
        ));
        assert!(matches!(
            parse_manual_directory("relative/photos"),
            Err(CommandError::InvalidDirectory)
        ));
    }

    #[test]
    fn cancelled_network_login_preserves_session_numbers_and_existing_bindings() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state
            .storage
            .create_session("cancelled login", None)
            .unwrap();
        let numbers = vec![PhotoNumber {
            original: "1234".into(),
            canonical: "1234".into(),
            confidence: None,
            confirmed: true,
        }];
        save_session_input_inner(
            &state,
            &session.id,
            "selection.txt".into(),
            "text".into(),
            b"1234".to_vec(),
        )
        .unwrap();
        state
            .storage
            .save_confirmed_numbers(&session.id, &numbers)
            .unwrap();
        let old_source = directory.path().join("old-source");
        let old_target = directory.path().join("old-target");
        fs::create_dir_all(&old_source).unwrap();
        fs::create_dir_all(&old_target).unwrap();
        state
            .storage
            .bind_paths(&session.id, Some(&old_source), Some(&old_target))
            .unwrap();

        let error = bind_selected_directory_with(
            &state,
            &session.id,
            DirectoryPurpose::Source,
            &directory.path().join("mapped-drive/source"),
            |_| Err(CommandError::SmbLoginCancelled),
        )
        .unwrap_err();

        assert!(matches!(error, CommandError::SmbLoginCancelled));
        assert_eq!(load_session(&state, &session.id).unwrap().id, session.id);
        assert_eq!(state.storage.load_numbers(&session.id).unwrap(), numbers);
        assert_eq!(
            state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            state.storage.load_bound_paths(&session.id).unwrap(),
            (Some(old_source), Some(old_target))
        );
    }

    #[test]
    fn cancelled_scan_preparation_preserves_persistent_state_before_status_cas() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state
            .storage
            .create_session("cancelled scan reconnect", None)
            .unwrap();
        let numbers = vec![PhotoNumber {
            original: "5678".into(),
            canonical: "5678".into(),
            confidence: None,
            confirmed: true,
        }];
        save_session_input_inner(
            &state,
            &session.id,
            "selection.txt".into(),
            "text".into(),
            b"5678".to_vec(),
        )
        .unwrap();
        state
            .storage
            .save_confirmed_numbers(&session.id, &numbers)
            .unwrap();
        let source = directory.path().join("bound-source");
        let target = directory.path().join("bound-target");
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        assert_eq!(
            load_session(&state, &session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );

        let result = prepare_scan_and_begin_with(
            &state,
            &session.id,
            &source,
            &target,
            |_| Err(CommandError::SmbLoginCancelled),
            |_| panic!("target preparation must not run after source cancellation"),
            |_| panic!("target probing must not run after source cancellation"),
        );

        assert!(matches!(result, Err(CommandError::SmbLoginCancelled)));
        assert_eq!(
            load_session(&state, &session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
        assert_eq!(state.storage.load_numbers(&session.id).unwrap(), numbers);
        assert_eq!(
            state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            state.storage.load_bound_paths(&session.id).unwrap(),
            (Some(source), Some(target))
        );
    }

    #[test]
    fn copy_launch_reconnects_source_and_target_before_confirmation_consumption() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state
            .storage
            .create_session("copy reconnect", None)
            .unwrap();
        state
            .storage
            .save_confirmed_numbers(
                &session.id,
                &[PhotoNumber {
                    original: "7".into(),
                    canonical: "7".into(),
                    confidence: None,
                    confirmed: true,
                }],
            )
            .unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        state
            .storage
            .compare_and_set_status(
                &session.id,
                SessionStatus::ReadyToScan,
                SessionStatus::Scanning,
            )
            .unwrap();
        let snapshot = ScanSnapshot {
            source: source.clone(),
            source_root_identity: capture_directory_identity(&source).unwrap(),
            target: target.clone(),
            items: vec![],
            skipped_numbers: vec![],
        };
        let mut skipped = CopyHistoryItem::planned(&session.id, "7", &source, &target, "");
        skipped.status = CopyItemStatus::Skipped;
        skipped.skipped_reason = Some("user-skipped".into());
        state
            .storage
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&PreflightReport::default()),
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[skipped]),
            )
            .unwrap();
        let confirmation = issue_confirmation(&state, &session.id).unwrap();
        let prepared = std::cell::RefCell::new(Vec::new());

        let error = claim_copy_start_with(
            &state,
            &session.id,
            Some(&confirmation),
            |prepared_source| {
                prepared.borrow_mut().push("source");
                assert_eq!(prepared_source, source);
                Ok(prepared_source.to_path_buf())
            },
            |prepared_target| {
                prepared.borrow_mut().push("target");
                assert_eq!(prepared_target, target);
                Err(CommandError::TargetDirectoryUnavailable)
            },
        )
        .unwrap_err();

        assert_eq!(*prepared.borrow(), vec!["source", "target"]);
        assert!(matches!(error, CommandError::TargetDirectoryUnavailable));
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToCopy
        );
        claim_copy_start_with(
            &state,
            &session.id,
            Some(&confirmation),
            |prepared_source| Ok(prepared_source.to_path_buf()),
            |prepared_target| Ok(prepared_target.to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::Copying
        );
    }

    #[test]
    fn source_disconnect_pause_preserves_terminal_history_and_session_inputs() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state.storage.create_session("paused copy", None).unwrap();
        save_session_input_inner(
            &state,
            &session.id,
            "selection.txt".into(),
            "text".into(),
            b"7".to_vec(),
        )
        .unwrap();
        state
            .storage
            .save_confirmed_numbers(
                &session.id,
                &[PhotoNumber {
                    original: "7".into(),
                    canonical: "7".into(),
                    confidence: None,
                    confirmed: true,
                }],
            )
            .unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        state
            .storage
            .compare_and_set_status(
                &session.id,
                SessionStatus::ReadyToScan,
                SessionStatus::Scanning,
            )
            .unwrap();
        let snapshot = ScanSnapshot {
            source: source.clone(),
            source_root_identity: capture_directory_identity(&source).unwrap(),
            target: target.clone(),
            items: vec![],
            skipped_numbers: vec![],
        };
        let copied = CopyHistoryItem::planned(
            &session.id,
            "7",
            &source.join("first.jpg"),
            &target.join("first.jpg"),
            "first",
        );
        let pending = CopyHistoryItem::planned(
            &session.id,
            "8",
            &source.join("second.jpg"),
            &target.join("second.jpg"),
            "second",
        );
        let revision = state
            .storage
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&PreflightReport::default()),
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[copied.clone(), pending.clone()]),
            )
            .unwrap();
        state
            .storage
            .consume_confirmation_and_start_copy(&session.id, revision, None, false)
            .unwrap();
        state
            .storage
            .update_copy_item(
                &copied.id,
                revision,
                CopyItemStatus::Copied,
                Some("first"),
                None,
                None,
                None,
            )
            .unwrap();
        let mut emitted = false;
        let paused = pause_disconnected_copy_with(
            &state,
            &session.id,
            revision,
            &CopyReport {
                stop_reason: Some(CopyStopReason::SourceDisconnected),
                ..CopyReport::default()
            },
            |preflight| {
                emitted = preflight.source_disconnected;
                Ok(())
            },
        )
        .unwrap();

        assert!(paused);
        assert!(emitted);
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::NeedsAttention
        );
        assert!(
            state
                .storage
                .load_preflight(&session.id)
                .unwrap()
                .unwrap()
                .source_disconnected
        );
        let history = state.storage.load_copy_items(&session.id).unwrap();
        assert_eq!(history[0].status, CopyItemStatus::Copied);
        assert_eq!(history[1].status, CopyItemStatus::Planned);
        assert_eq!(
            state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn target_disconnect_pause_persists_exact_pending_cleanup_and_emits_target_issue() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let (session_id, _, target, revision) =
            paused_copied_session(&state, directory.path(), "target-disconnect");
        state
            .storage
            .compare_and_set_status(
                &session_id,
                SessionStatus::NeedsAttention,
                SessionStatus::ReadyToCopy,
            )
            .unwrap();
        state
            .storage
            .consume_confirmation_and_start_copy(&session_id, revision, None, false)
            .unwrap();
        let pending = crate::copy_engine::PendingTargetPart {
            target,
            part_name: ".photo-selector.part-exact".into(),
            identity: Default::default(),
        };
        let disconnect = TargetDisconnectInfo {
            windows_code: Some(64),
            operation: crate::copy_engine::TargetOperation::Write,
        };
        let mut emitted = None;

        let paused = pause_disconnected_copy_with(
            &state,
            &session_id,
            revision,
            &CopyReport {
                stop_reason: Some(CopyStopReason::TargetDisconnected),
                target_disconnect: Some(disconnect),
                pending_target_parts: vec![pending.clone()],
                ..CopyReport::default()
            },
            |preflight| {
                emitted = Some(preflight.clone());
                Ok(())
            },
        )
        .unwrap();

        assert!(paused);
        let emitted = emitted.unwrap();
        assert!(emitted.target_disconnected);
        assert!(!emitted.source_disconnected);
        assert_eq!(emitted.target_disconnect, Some(disconnect));
        assert_eq!(
            disconnect_pause_diagnostic(&emitted),
            ("copy-disconnect-pause", Some(64), "paused")
        );
        assert_eq!(emitted.pending_target_parts, vec![pending.clone()]);
        let saved = state.storage.load_preflight(&session_id).unwrap().unwrap();
        assert!(saved.target_disconnected);
        assert_eq!(saved.target_disconnect, Some(disconnect));
        assert_eq!(saved.pending_target_parts, vec![pending]);
        assert_eq!(
            state.storage.load_session(&session_id).unwrap().status,
            SessionStatus::NeedsAttention
        );
    }

    #[test]
    fn recheck_reconnects_then_runs_full_preflight_and_keeps_source_change_paused() {
        let directory = safe_tempdir();
        let state = test_state(directory.path());
        let session = state.storage.create_session("recheck copy", None).unwrap();
        state
            .storage
            .save_confirmed_numbers(
                &session.id,
                &[PhotoNumber {
                    original: "7".into(),
                    canonical: "7".into(),
                    confidence: None,
                    confirmed: true,
                }],
            )
            .unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let source_file = source.join("IMG_0007.JPG");
        fs::write(&source_file, b"trusted").unwrap();
        let mut opened = fs::File::open(&source_file).unwrap();
        let fingerprint = capture_file_fingerprint(&mut opened).unwrap();
        let snapshot = ScanSnapshot {
            source: source.clone(),
            source_root_identity: capture_directory_identity(&source).unwrap(),
            target: target.clone(),
            items: vec![NumberMatch {
                canonical_number: "7".into(),
                status: MatchStatus::Complete,
                groups: vec![crate::models::CandidateGroup {
                    id: "group-7".into(),
                    files: vec![crate::models::IndexedFile {
                        path: source_file.clone(),
                        stem: "IMG_0007".into(),
                        extension: "JPG".into(),
                        canonical_number: "7".into(),
                        size: fingerprint.size,
                        modified_ms: fingerprint.modified_ms,
                        family_key: "IMG_0007".into(),
                        fingerprint,
                    }],
                }],
            }],
            skipped_numbers: vec![],
        };
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        state
            .storage
            .compare_and_set_status(
                &session.id,
                SessionStatus::ReadyToScan,
                SessionStatus::Scanning,
            )
            .unwrap();
        let revision = state
            .storage
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&PreflightReport::default()),
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&copy_plan_items(&session.id, &snapshot)),
            )
            .unwrap();
        let part_name = ".photo-selector.part-recheck-exact";
        let part_path = target.join(part_name);
        fs::write(&part_path, b"interrupted partial").unwrap();
        let part_identity = capture_file_fingerprint(&mut fs::File::open(&part_path).unwrap())
            .unwrap()
            .identity;
        state
            .storage
            .consume_confirmation_and_start_copy(&session.id, revision, None, false)
            .unwrap();
        state
            .storage
            .save_worker_preflight(
                &session.id,
                revision,
                &PreflightReport {
                    target_disconnected: true,
                    pending_target_parts: vec![crate::copy_engine::PendingTargetPart {
                        target: target.join("IMG_0007.JPG"),
                        part_name: part_name.into(),
                        identity: part_identity,
                    }],
                    ..PreflightReport::default()
                },
                Some(SessionStatus::NeedsAttention),
            )
            .unwrap();
        fs::write(&source_file, b"changed").unwrap();
        let order = std::cell::RefCell::new(Vec::new());

        let error = validate_recheck_with(
            &state,
            &session.id,
            |prepared_source| {
                order.borrow_mut().push("source");
                assert_eq!(prepared_source, source);
                Ok(prepared_source.to_path_buf())
            },
            |prepared_target| {
                order.borrow_mut().push("target");
                assert_eq!(prepared_target, target);
                Ok(prepared_target.to_path_buf())
            },
            |plan, pending| {
                order.borrow_mut().push("cleanup");
                cleanup_pending_target_parts(plan, pending)
            },
        )
        .unwrap_err();

        assert_eq!(*order.borrow(), vec!["source", "target", "cleanup"]);
        assert!(!part_path.exists());
        assert!(error.to_string().contains("重新扫描"));
        assert_eq!(
            state.storage.load_session(&session.id).unwrap().status,
            SessionStatus::NeedsAttention
        );
    }

    #[test]
    fn network_prepare_errors_map_to_clear_command_messages_without_system_codes() {
        let cancelled = map_network_path_error(crate::network_paths::NetworkPathError::Cancelled);
        let conflict =
            map_network_path_error(crate::network_paths::NetworkPathError::CredentialConflict);
        let unavailable =
            map_network_path_error(crate::network_paths::NetworkPathError::Unavailable(1219));

        assert!(matches!(cancelled, CommandError::SmbLoginCancelled));
        assert!(matches!(conflict, CommandError::SmbCredentialConflict));
        assert!(matches!(unavailable, CommandError::SmbUnavailable));
        assert!(!unavailable.to_string().contains("1219"));
    }

    #[test]
    fn scan_maps_source_and_target_preparation_failures_separately() {
        assert!(matches!(
            map_source_scan_prepare_error(CommandError::InvalidDirectory),
            CommandError::SourceDirectoryUnavailable
        ));
        assert!(matches!(
            map_source_scan_prepare_error(CommandError::SmbUnavailable),
            CommandError::SourceDirectoryUnavailable
        ));
        assert!(matches!(
            map_target_scan_prepare_error(CommandError::InvalidDirectory),
            CommandError::TargetDirectoryUnavailable
        ));
        assert!(matches!(
            map_target_scan_prepare_error(CommandError::SmbUnavailable),
            CommandError::TargetDirectoryUnavailable
        ));
    }

    #[test]
    fn scan_accepts_the_resolved_unc_paths_returned_by_directory_selection() {
        let source = PathBuf::from(r"\\nas\photos\resolved-source");
        let target = PathBuf::from(r"\\nas\photos\照片成片\待精修的原片");
        let prepared = std::cell::RefCell::new(Vec::new());

        let paths = prepare_authorized_scan_paths_with_boundaries(
            &source,
            &target,
            Some(source.as_path()),
            Some(target.as_path()),
            |path| {
                prepared.borrow_mut().push(path.to_path_buf());
                Ok(path.to_path_buf())
            },
            |_| panic!("a missing target must not be prepared"),
            |_| Ok(ScanTargetProbe::Missing),
        )
        .unwrap();

        assert_eq!(paths, (source.clone(), target.clone()));
        assert_eq!(prepared.into_inner(), [source]);
    }

    #[test]
    fn scan_treats_windows_path_not_found_code_as_a_missing_generated_target() {
        let target = Path::new(r"\\nas\photos\订单\照片成片\待精修的原片");

        let probe =
            probe_scan_target_with(target, |_| Err(std::io::Error::from_raw_os_error(3))).unwrap();

        assert_eq!(probe, ScanTargetProbe::Missing);
    }

    #[test]
    fn scan_rejects_an_arbitrary_unc_source_before_any_external_side_effect() {
        let caller_source = PathBuf::from(r"\\attacker\share\source");
        let bound_source = PathBuf::from(r"\\nas\photos\source");
        let target = PathBuf::from(r"\\nas\photos\target");
        let source_prepares = std::cell::Cell::new(0);
        let target_prepares = std::cell::Cell::new(0);
        let target_probes = std::cell::Cell::new(0);

        let result = prepare_authorized_scan_paths_with_boundaries(
            &caller_source,
            &target,
            Some(bound_source.as_path()),
            Some(target.as_path()),
            |path| {
                source_prepares.set(source_prepares.get() + 1);
                Ok(path.to_path_buf())
            },
            |path| {
                target_prepares.set(target_prepares.get() + 1);
                Ok(path.to_path_buf())
            },
            |_| {
                target_probes.set(target_probes.get() + 1);
                Ok(ScanTargetProbe::Prepare)
            },
        );

        assert!(matches!(result, Err(CommandError::PathNotAuthorized)));
        assert_eq!(source_prepares.get(), 0, "must not cross the WNet boundary");
        assert_eq!(target_prepares.get(), 0, "must not cross the WNet boundary");
        assert_eq!(
            target_probes.get(),
            0,
            "must not probe an unauthorized target"
        );
    }

    #[test]
    fn scan_rejects_an_arbitrary_unc_target_before_source_or_target_side_effects() {
        let source = PathBuf::from(r"\\nas\photos\source");
        let caller_target = PathBuf::from(r"\\attacker\share\target");
        let bound_target = PathBuf::from(r"\\nas\photos\target");
        let source_prepares = std::cell::Cell::new(0);
        let target_prepares = std::cell::Cell::new(0);
        let target_probes = std::cell::Cell::new(0);

        let result = prepare_authorized_scan_paths_with_boundaries(
            &source,
            &caller_target,
            Some(source.as_path()),
            Some(bound_target.as_path()),
            |path| {
                source_prepares.set(source_prepares.get() + 1);
                Ok(path.to_path_buf())
            },
            |path| {
                target_prepares.set(target_prepares.get() + 1);
                Ok(path.to_path_buf())
            },
            |_| {
                target_probes.set(target_probes.get() + 1);
                Ok(ScanTargetProbe::Prepare)
            },
        );

        assert!(matches!(result, Err(CommandError::PathNotAuthorized)));
        assert_eq!(source_prepares.get(), 0, "precheck both paths first");
        assert_eq!(target_prepares.get(), 0, "must not cross the WNet boundary");
        assert_eq!(
            target_probes.get(),
            0,
            "must not probe an unauthorized target"
        );
    }

    #[test]
    fn path_aliases_never_match_the_bound_path_exactly() {
        let pairs = [
            (
                r"\\nas\share\source.",
                r"\\?\UNC\nas\share\source.",
                "trailing dot",
            ),
            (
                r"\\nas\share\source ",
                r"\\?\UNC\nas\share\source ",
                "trailing space",
            ),
            (
                r"\\nas\share\CON",
                r"\\?\UNC\nas\share\CON",
                "reserved component",
            ),
            (
                r"\\nas\share\photo.jpg:stream",
                r"\\?\UNC\nas\share\photo.jpg:stream",
                "alternate data stream",
            ),
            (
                r"\\nas\share\source",
                r"\\?\UNC\nas/share/source",
                "verbatim slash variant",
            ),
            (r"\\nas\share\source", r"\\NAS\SHARE\SOURCE", "case variant"),
            (
                r"\\nas\share\source",
                r"//nas/share/source",
                "separator variant",
            ),
        ];

        for (bound, caller, case) in pairs {
            assert!(
                !scan_path_matches_bound(Path::new(bound), Path::new(caller)),
                "{case} must not widen authorization"
            );
        }
    }

    #[test]
    fn scan_rejects_every_path_alias_before_prepare_or_probe() {
        let pairs = [
            (r"\\nas\share\source.", r"\\?\UNC\nas\share\source."),
            (r"\\nas\share\source ", r"\\?\UNC\nas\share\source "),
            (r"\\nas\share\CON", r"\\?\UNC\nas\share\CON"),
            (
                r"\\nas\share\photo.jpg:stream",
                r"\\?\UNC\nas\share\photo.jpg:stream",
            ),
            (r"\\nas\share\source", r"\\?\UNC\nas/share/source"),
            (r"\\nas\share\source", r"\\NAS\SHARE\SOURCE"),
            (r"\\nas\share\source", r"//nas/share/source"),
        ];
        let trusted_target = Path::new(r"\\nas\share\target");

        for (bound, caller) in pairs {
            let source_prepares = std::cell::Cell::new(0);
            let target_prepares = std::cell::Cell::new(0);
            let target_probes = std::cell::Cell::new(0);
            let result = prepare_authorized_scan_paths_with_boundaries(
                Path::new(caller),
                trusted_target,
                Some(Path::new(bound)),
                Some(trusted_target),
                |path| {
                    source_prepares.set(source_prepares.get() + 1);
                    Ok(path.to_path_buf())
                },
                |path| {
                    target_prepares.set(target_prepares.get() + 1);
                    Ok(path.to_path_buf())
                },
                |_| {
                    target_probes.set(target_probes.get() + 1);
                    Ok(ScanTargetProbe::Prepare)
                },
            );

            assert!(matches!(result, Err(CommandError::PathNotAuthorized)));
            assert_eq!(source_prepares.get(), 0, "source alias: {caller}");
            assert_eq!(target_prepares.get(), 0, "source alias: {caller}");
            assert_eq!(target_probes.get(), 0, "source alias: {caller}");

            let trusted_source = Path::new(r"\\nas\share\trusted-source");
            let source_prepares = std::cell::Cell::new(0);
            let target_prepares = std::cell::Cell::new(0);
            let target_probes = std::cell::Cell::new(0);
            let result = prepare_authorized_scan_paths_with_boundaries(
                trusted_source,
                Path::new(caller),
                Some(trusted_source),
                Some(Path::new(bound)),
                |path| {
                    source_prepares.set(source_prepares.get() + 1);
                    Ok(path.to_path_buf())
                },
                |path| {
                    target_prepares.set(target_prepares.get() + 1);
                    Ok(path.to_path_buf())
                },
                |_| {
                    target_probes.set(target_probes.get() + 1);
                    Ok(ScanTargetProbe::Prepare)
                },
            );

            assert!(matches!(result, Err(CommandError::PathNotAuthorized)));
            assert_eq!(source_prepares.get(), 0, "target alias: {caller}");
            assert_eq!(target_prepares.get(), 0, "target alias: {caller}");
            assert_eq!(target_probes.get(), 0, "target alias: {caller}");
        }
    }

    #[test]
    fn scan_rejects_a_prepared_source_that_differs_from_the_bound_directory() {
        let directory = safe_tempdir();
        let bound = directory.path().join("resolved-unc/source");
        let different = directory.path().join("resolved-unc/different-source");
        let target = directory.path().join("not-yet-created-target");
        let prepared = std::cell::Cell::new(false);

        let result = prepare_authorized_scan_paths_with_boundaries(
            &bound,
            &target,
            Some(bound.as_path()),
            Some(target.as_path()),
            |_| {
                prepared.set(true);
                Ok(different.clone())
            },
            |_| panic!("an unauthorized source must not prepare the target"),
            |_| panic!("an unauthorized source must not probe the target"),
        );

        assert!(matches!(result, Err(CommandError::PathNotAuthorized)));
        assert!(prepared.get(), "the post-prepare identity check must run");
    }

    #[test]
    fn scan_prepares_an_existing_target_only_after_lexical_authorization() {
        let directory = safe_tempdir();
        let source = directory.path().join("source");
        let target = directory.path().join("existing-target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let prepared = std::cell::RefCell::new(Vec::new());

        let paths = prepare_authorized_scan_paths_with_boundaries(
            &source,
            &target,
            Some(source.as_path()),
            Some(target.as_path()),
            |path| {
                prepared.borrow_mut().push(path.to_path_buf());
                Ok(path.to_path_buf())
            },
            |path| {
                prepared.borrow_mut().push(path.to_path_buf());
                Ok(path.to_path_buf())
            },
            probe_scan_target,
        )
        .unwrap();

        assert_eq!(paths, (source.clone(), target.clone()));
        assert_eq!(prepared.into_inner(), [source, target]);
    }

    #[test]
    fn target_probe_errors_still_enter_preparation_for_network_recovery() {
        let directory = safe_tempdir();
        let source = directory.path().join("source");
        let target = directory.path().join("temporarily-unreachable-target");
        let mut target_prepared = false;

        let paths = prepare_authorized_scan_paths_with_boundaries(
            &source,
            &target,
            Some(source.as_path()),
            Some(target.as_path()),
            |path| Ok(path.to_path_buf()),
            |path| {
                target_prepared = true;
                Ok(path.to_path_buf())
            },
            |path| {
                probe_scan_target_with(path, |_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "network provider unavailable",
                    ))
                })
            },
        )
        .unwrap();

        assert_eq!(paths, (source, target));
        assert!(target_prepared);
    }

    #[test]
    fn scan_rejects_non_directory_and_symlink_targets_without_preparing_them() {
        let directory = safe_tempdir();
        let file = directory.path().join("target-file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(matches!(
            probe_scan_target_with(&file, |path| fs::symlink_metadata(path)),
            Err(CommandError::TargetDirectoryUnavailable)
        ));

        #[cfg(unix)]
        {
            let real = directory.path().join("real-target");
            let link = directory.path().join("target-link");
            fs::create_dir(&real).unwrap();
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(matches!(
                probe_scan_target_with(&link, |path| fs::symlink_metadata(path)),
                Err(CommandError::TargetDirectoryUnavailable)
            ));
        }
    }

    #[test]
    fn unavailable_scan_directory_is_reported_without_a_raw_os_error() {
        let missing = safe_tempdir().path().join("removed-source");
        let prepared = network_paths::prepare_directory(&missing, ConnectMode::SilentOnly)
            .map_err(map_network_path_error)
            .map_err(map_source_scan_prepare_error);
        assert!(matches!(
            prepared,
            Err(CommandError::SourceDirectoryUnavailable)
        ));
        let mapped = map_scan_error(crate::matching::MatchError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "The system cannot find the path specified",
        )));
        assert!(matches!(mapped, CommandError::SourceDirectoryUnavailable));
        assert!(!mapped.to_string().contains("os error"));
    }

    #[test]
    fn scan_failure_diagnostic_event_is_staged_and_path_free() {
        let source = Path::new(r"\\SERVER_SECRET\SHARE_SECRET\CUSTOMER_SECRET\PHOTO_SECRET_00123");
        let error = crate::matching::MatchError::StagedIo {
            stage: crate::matching::ScanFailureStage::ChildDirectoryOpen,
            source: std::io::Error::from_raw_os_error(3),
        };

        let event = safe_scan_failure_event("2d8f1c36-0c5d-4ed1-8a07-8df63af0b720", source, &error)
            .expect("staged scan errors produce diagnostics");

        assert!(event.contains("stage=scan-child-directory-open"));
        assert!(event.contains("path_kind=unc"));
        assert!(event.contains("target_id=net_"));
        assert!(event.contains("windows_code=3"));
        for secret in [
            "SERVER_SECRET",
            "SHARE_SECRET",
            "CUSTOMER_SECRET",
            "PHOTO_SECRET",
            "00123",
        ] {
            assert!(!event.contains(secret), "{event}");
        }
    }

    #[test]
    fn session_event_payloads_serialize_to_the_typescript_contract() {
        let pause = pause_payload(
            BlockingIssue::TargetConflict,
            "冲突",
            "处理后重试",
            vec!["IMG_0007.JPG".into()],
        );
        assert_eq!(
            serde_json::to_value(pause).unwrap(),
            serde_json::json!({
                "issue": "target-conflict",
                "title": "冲突",
                "message": "处理后重试",
                "affected": ["IMG_0007.JPG"],
                "actions": ["recheck", "cancel"],
            })
        );
        let mut target_pause = pause_payload(
            BlockingIssue::TargetDisconnected,
            "断开",
            "重新检查",
            vec![],
        );
        target_pause.target_disconnect = Some(TargetDisconnectInfo {
            windows_code: Some(53),
            operation: crate::copy_engine::TargetOperation::OpenTarget,
        });
        let target_value = serde_json::to_value(target_pause).unwrap();
        assert_eq!(target_value["targetDisconnect"]["windowsCode"], 53);
        assert_eq!(target_value["targetDisconnect"]["operation"], "open-target");
        assert_eq!(
            serde_json::to_value(CopyProgressPayload {
                job_id: "job-1".into(),
                current_file: "IMG_0007.JPG".into(),
                completed_files: 1,
                total_files: 2,
                copied_bytes: 3,
                total_bytes: 4,
            })
            .unwrap()["currentFile"],
            "IMG_0007.JPG"
        );
        let progress = CopyProgressPayload {
            job_id: "job-1".into(),
            current_file: "IMG_0007.JPG".into(),
            completed_files: 1,
            total_files: 2,
            copied_bytes: 3,
            total_bytes: 4,
        };
        assert_eq!(
            serde_json::to_value(SessionEvent {
                session_id: "session-1",
                payload: &progress,
            })
            .unwrap()["sessionId"],
            "session-1"
        );
        assert_eq!(
            serde_json::to_value(CopyLaunch {
                job_id: "job-1".into(),
                status: "copying",
            })
            .unwrap(),
            serde_json::json!({"jobId": "job-1", "status": "copying"})
        );
    }

    #[test]
    fn provider_draft_uses_only_the_saved_secret_reference() {
        let saved = ProviderProfile {
            id: "provider-1".into(),
            name: "saved".into(),
            template: "custom".into(),
            address: "https://example.com/v1".into(),
            address_mode: crate::models::AddressMode::BaseUrl,
            api_format: crate::models::ApiFormat::Responses,
            model: "saved-model".into(),
            fallback_model: None,
            timeout_seconds: 30,
            enabled: true,
            is_default: false,
            secret_ref: "server-controlled-reference".into(),
        };
        let mut draft = saved.clone();
        draft.name = "unsaved edit".into();
        draft.model = "unsaved-model".into();
        draft.secret_ref = "client-controlled-reference".into();

        assert_eq!(
            existing_secret_ref_for_draft(&draft, &saved).unwrap(),
            "server-controlled-reference"
        );
        draft.address = "https://example.com/other/path".into();
        assert_eq!(
            existing_secret_ref_for_draft(&draft, &saved).unwrap(),
            "server-controlled-reference"
        );
        draft.address = "https://example.com:443/explicit-default-port".into();
        assert_eq!(
            existing_secret_ref_for_draft(&draft, &saved).unwrap(),
            "server-controlled-reference"
        );
        draft.address = "https://example.com:444/v1".into();
        assert!(existing_secret_ref_for_draft(&draft, &saved).is_err());
        draft.address = "http://example.com/v1".into();
        assert!(existing_secret_ref_for_draft(&draft, &saved).is_err());
        draft.address = "https://attacker.example/v1".into();
        assert!(existing_secret_ref_for_draft(&draft, &saved).is_err());
    }

    #[test]
    fn provider_save_requires_a_new_key_when_origin_changes() {
        let saved = ProviderProfile {
            id: "provider-1".into(),
            name: "saved".into(),
            template: "custom".into(),
            address: "https://example.com/v1".into(),
            address_mode: crate::models::AddressMode::BaseUrl,
            api_format: crate::models::ApiFormat::Responses,
            model: "saved-model".into(),
            fallback_model: None,
            timeout_seconds: 30,
            enabled: true,
            is_default: false,
            secret_ref: "server-controlled-reference".into(),
        };
        let mut draft = saved.clone();
        draft.secret_ref = "client-controlled-reference".into();
        draft.address = "https://other.example/v1".into();

        assert!(secret_ref_for_provider_save(&draft, Some(&saved), "").is_err());
        let rotated = secret_ref_for_provider_save(&draft, Some(&saved), "new-key").unwrap();
        assert_ne!(rotated, "server-controlled-reference");
        assert!(rotated.starts_with("provider-1:"));
        draft.address = "https://example.com/new/path".into();
        assert_eq!(
            secret_ref_for_provider_save(&draft, Some(&saved), "").unwrap(),
            "server-controlled-reference"
        );
    }

    #[test]
    fn generated_provider_secret_references_are_fresh_and_server_scoped() {
        let first = fresh_provider_secret_ref("provider-1");
        let second = fresh_provider_secret_ref("provider-1");

        assert_ne!(first, second);
        assert!(first.starts_with("provider-1:"));
        assert!(!first.contains("client-controlled-reference"));
    }

    #[test]
    fn secret_rotation_commits_before_retiring_the_old_key() {
        let events = std::cell::RefCell::new(Vec::new());

        rotate_provider_secret(
            || {
                events.borrow_mut().push("save-new");
                Ok(())
            },
            || {
                events.borrow_mut().push("commit-profile");
                Ok(())
            },
            || {
                events.borrow_mut().push("rollback-new");
                Ok(())
            },
            || {
                events.borrow_mut().push("retire-old");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            events.into_inner(),
            ["save-new", "commit-profile", "retire-old"]
        );
    }

    #[test]
    fn failed_profile_commit_removes_the_unreferenced_new_key() {
        let events = std::cell::RefCell::new(Vec::new());

        let result = rotate_provider_secret(
            || {
                events.borrow_mut().push("save-new");
                Ok(())
            },
            || {
                events.borrow_mut().push("commit-profile");
                Err(CommandError::Other("database failed".into()))
            },
            || {
                events.borrow_mut().push("rollback-new");
                Ok(())
            },
            || {
                events.borrow_mut().push("retire-old");
                Ok(())
            },
        );

        assert!(result.unwrap_err().to_string().contains("database failed"));
        assert_eq!(
            events.into_inner(),
            ["save-new", "commit-profile", "rollback-new"]
        );
    }

    #[test]
    fn failed_new_key_rollback_is_returned_with_the_profile_commit_error() {
        let result = rotate_provider_secret(
            || Ok(()),
            || Err(CommandError::Other("database failed".into())),
            || Err(CommandError::Other("new key cleanup failed".into())),
            || Ok(()),
        );

        let message = result.unwrap_err().to_string();
        assert!(message.contains("database failed"));
        assert!(message.contains("new key cleanup failed"));
    }

    #[test]
    fn failed_old_key_retirement_is_returned_after_the_new_profile_is_safe() {
        let result = rotate_provider_secret(
            || Ok(()),
            || Ok(()),
            || Ok(()),
            || Err(CommandError::Other("old key cleanup failed".into())),
        );

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("old key cleanup failed"));
    }

    #[test]
    fn provider_delete_commits_the_profile_removal_before_retiring_the_key() {
        let events = std::cell::RefCell::new(Vec::new());

        delete_provider_and_retire_secret(
            || {
                events.borrow_mut().push("delete-profile");
                Ok(())
            },
            || {
                events.borrow_mut().push("retire-key");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(events.into_inner(), ["delete-profile", "retire-key"]);
    }

    #[test]
    fn provider_delete_surfaces_key_retirement_failure_without_restoring_a_profile() {
        let events = std::cell::RefCell::new(Vec::new());

        let result = delete_provider_and_retire_secret(
            || {
                events.borrow_mut().push("delete-profile");
                Ok(())
            },
            || {
                events.borrow_mut().push("retire-key");
                Err(CommandError::Other("keychain unavailable".into()))
            },
        );

        assert_eq!(events.into_inner(), ["delete-profile", "retire-key"]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("配置已删除"));
        assert!(message.contains("keychain unavailable"));
    }

    #[test]
    fn failed_provider_delete_keeps_the_key_for_the_still_active_profile() {
        let events = std::cell::RefCell::new(Vec::new());

        let result = delete_provider_and_retire_secret(
            || {
                events.borrow_mut().push("delete-profile");
                Err(CommandError::Other("database unavailable".into()))
            },
            || {
                events.borrow_mut().push("retire-key");
                Ok(())
            },
        );

        assert_eq!(events.into_inner(), ["delete-profile"]);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("database unavailable"));
    }

    #[test]
    fn concurrent_provider_reads_observe_a_matching_profile_and_secret() {
        use std::sync::mpsc;
        use std::thread;

        let credential_guard = Arc::new(Mutex::new(()));
        let values = Arc::new(Mutex::new((
            "old-ref".to_string(),
            HashMap::from([
                ("old-ref".to_string(), "old-key".to_string()),
                ("new-ref".to_string(), "new-key".to_string()),
            ]),
        )));
        let (profile_loaded_tx, profile_loaded_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let reader_guard = Arc::clone(&credential_guard);
        let reader_values = Arc::clone(&values);
        let reader = thread::spawn(move || {
            with_provider_credential_guard(&reader_guard, || {
                let secret_ref = reader_values.lock().unwrap().0.clone();
                profile_loaded_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                let key = reader_values
                    .lock()
                    .unwrap()
                    .1
                    .get(&secret_ref)
                    .unwrap()
                    .clone();
                Ok((secret_ref, key))
            })
            .unwrap()
        });
        profile_loaded_rx.recv().unwrap();
        let writer_guard = Arc::clone(&credential_guard);
        let writer_values = Arc::clone(&values);
        let writer = thread::spawn(move || {
            with_provider_credential_guard(&writer_guard, || {
                let mut values = writer_values.lock().unwrap();
                values.0 = "new-ref".into();
                values.1.remove("old-ref");
                Ok(())
            })
            .unwrap();
        });
        continue_tx.send(()).unwrap();

        assert_eq!(
            reader.join().unwrap(),
            ("old-ref".to_string(), "old-key".to_string())
        );
        writer.join().unwrap();
        let current = with_provider_credential_guard(&credential_guard, || {
            let values = values.lock().unwrap();
            let secret_ref = values.0.clone();
            Ok((
                secret_ref.clone(),
                values.1.get(&secret_ref).unwrap().clone(),
            ))
        })
        .unwrap();
        assert_eq!(current, ("new-ref".to_string(), "new-key".to_string()));
    }

    #[test]
    fn session_inputs_list_read_and_reload_from_persistent_metadata() {
        let directory = safe_tempdir();
        let database = directory.path().join("state.sqlite3");
        let state = AppState {
            storage: Storage::open(&database).unwrap(),
            app_data_dir: directory.path().to_path_buf(),
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        };
        let session = state.storage.create_session("inputs", None).unwrap();
        let bytes = valid_png_bytes();
        let saved = save_session_input_inner(
            &state,
            &session.id,
            "customer.png".into(),
            "image".into(),
            bytes.clone(),
        )
        .unwrap();
        assert_eq!(
            state
                .storage
                .list_session_inputs(&session.id)
                .unwrap()
                .len(),
            1
        );
        let persisted = state
            .storage
            .load_session_input(&session.id, &saved.id)
            .unwrap();
        let input_root = directory
            .path()
            .join("sessions")
            .join(&session.id)
            .join("inputs");
        assert_eq!(
            capture_directory_identity(&input_root).unwrap(),
            persisted.root_identity
        );
        let mut persisted_file = fs::File::open(input_root.join(&persisted.stored_name)).unwrap();
        assert_eq!(
            capture_file_fingerprint(&mut persisted_file).unwrap(),
            persisted.fingerprint
        );
        assert_eq!(
            read_session_input_inner(&state, &session.id, &saved.id).unwrap(),
            bytes
        );
        drop(state);

        let reopened = AppState {
            storage: Storage::open(&database).unwrap(),
            app_data_dir: directory.path().to_path_buf(),
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        };
        assert_eq!(
            reopened
                .storage
                .load_session_input(&session.id, &saved.id)
                .unwrap()
                .mime,
            "image/png"
        );
        assert_eq!(
            read_session_input_inner(&reopened, &session.id, &saved.id).unwrap(),
            bytes
        );
    }

    #[test]
    fn acceptance_fixture_runs_text_confirmation_scan_resolution_copy_and_history() {
        use crate::matching::scan_source_index_cancellable;

        let directory = safe_tempdir();
        let order = directory.path().join("验收订单");
        let source = order.join("原始照片");
        let target = order.join("照片成片/待精修的原片");
        fs::create_dir_all(source.join("RAW")).unwrap();
        fs::create_dir_all(source.join("JPG")).unwrap();
        fs::create_dir_all(source.join("重号目录")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("RAW/IMG_01234.CR3"), b"raw-1234").unwrap();
        fs::write(source.join("JPG/IMG_01234.JPG"), b"jpg-1234").unwrap();
        fs::write(source.join("RAW/IMG_0781.CR3"), b"raw-781").unwrap();
        fs::write(source.join("JPG/IMG_0781.JPG"), b"jpg-781").unwrap();
        fs::write(source.join("重号目录/IMG_001234.JPG"), b"duplicate").unwrap();
        fs::copy(source.join("JPG/IMG_0781.JPG"), target.join("IMG_0781.JPG")).unwrap();

        let state = AppState {
            storage: Storage::open(&directory.path().join("state.sqlite3")).unwrap(),
            app_data_dir: directory.path().to_path_buf(),
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        };
        state
            .storage
            .save_setting(
                "app-settings",
                &AppSettings {
                    second_confirmation_enabled: true,
                    ..AppSettings::default()
                },
            )
            .unwrap();
        let session = state.storage.create_session("匿名验收订单", None).unwrap();
        save_session_input_inner(
            &state,
            &session.id,
            "选片文字.txt".into(),
            "text".into(),
            b"1234\n781".to_vec(),
        )
        .unwrap();
        state
            .storage
            .save_confirmed_numbers(
                &session.id,
                &[
                    PhotoNumber {
                        original: "1234".into(),
                        canonical: "1234".into(),
                        confidence: None,
                        confirmed: true,
                    },
                    PhotoNumber {
                        original: "781".into(),
                        canonical: "781".into(),
                        confidence: None,
                        confirmed: true,
                    },
                ],
            )
            .unwrap();
        let source = source.canonicalize().unwrap();
        state
            .storage
            .bind_paths(&session.id, Some(&source), Some(&target))
            .unwrap();
        state
            .storage
            .compare_and_set_status(
                &session.id,
                SessionStatus::ReadyToScan,
                SessionStatus::Scanning,
            )
            .unwrap();
        let indexed = scan_source_index_cancellable(
            &source,
            Some(&target),
            &default_extensions(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let items = match_numbers(&["1234".into(), "781".into()], &indexed.files);
        assert_eq!(items[0].status, MatchStatus::Ambiguous);
        assert_eq!(items[1].status, MatchStatus::Complete);
        let mut snapshot = ScanSnapshot {
            source: indexed.root,
            source_root_identity: indexed.root_identity,
            target: target.clone(),
            items,
            skipped_numbers: vec![],
        };
        let blocking = blocking_report(&snapshot);
        state
            .storage
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&blocking),
                SessionStatus::Scanning,
                Some(SessionStatus::NeedsAttention),
                None,
            )
            .unwrap();
        let selected_group = snapshot.items[0]
            .groups
            .iter()
            .find(|group| group.files.len() == 2)
            .unwrap()
            .id
            .clone();
        let (resolved, revision) = resolve_match_inner(
            &state,
            &session.id,
            "1234",
            MatchResolution::SelectGroup {
                group_id: selected_group,
            },
        )
        .unwrap();
        snapshot = resolved;
        let confirmation = finish_resolution(&state, &session.id, &snapshot, revision)
            .unwrap()
            .confirmation_token
            .unwrap();
        let (persisted, plan_revision) = state
            .storage
            .load_scan_snapshot_with_revision(&session.id)
            .unwrap()
            .unwrap();
        let plan = plan(&persisted);
        let preflight = preflight_copy_cancellable(&plan, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            preflight.identical,
            vec![target.join("IMG_0781.JPG").display().to_string()]
        );
        state
            .storage
            .consume_confirmation_and_start_copy(
                &session.id,
                plan_revision,
                Some(&confirmation),
                true,
            )
            .unwrap();
        let item_ids = state
            .storage
            .load_copy_items(&session.id)
            .unwrap()
            .into_iter()
            .map(|item| ((item.source, item.target), item.id))
            .collect::<HashMap<_, _>>();
        let report = execute_copy_with_updates(
            &plan,
            &AtomicBool::new(false),
            |_, _| {},
            |file, outcome| {
                let id = item_ids
                    .get(&(file.source.clone(), file.target.clone()))
                    .unwrap();
                match outcome {
                    CopyItemOutcome::Started => state
                        .storage
                        .update_copy_item(
                            id,
                            plan_revision,
                            CopyItemStatus::Copying,
                            None,
                            None,
                            None,
                            None,
                        )
                        .unwrap(),
                    CopyItemOutcome::Copied { source_hash } => state
                        .storage
                        .update_copy_item(
                            id,
                            plan_revision,
                            CopyItemStatus::Copied,
                            Some(&source_hash),
                            None,
                            None,
                            None,
                        )
                        .unwrap(),
                    CopyItemOutcome::SkippedIdentical { source_hash } => state
                        .storage
                        .update_copy_item(
                            id,
                            plan_revision,
                            CopyItemStatus::Skipped,
                            Some(&source_hash),
                            Some("identical-target"),
                            None,
                            None,
                        )
                        .unwrap(),
                    other => panic!("unexpected fixture outcome: {other:?}"),
                }
            },
        );
        assert_eq!(report.copied.len(), 3);
        assert_eq!(report.skipped_identical.len(), 1);
        state
            .storage
            .finish_copy_attempt(
                &session.id,
                plan_revision,
                SessionStatus::Completed,
                None,
                None,
            )
            .unwrap();
        let history = state.storage.load_copy_items(&session.id).unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(
            history
                .iter()
                .filter(|item| item.status == CopyItemStatus::Copied)
                .count(),
            3
        );
        assert!(history
            .iter()
            .any(|item| item.skipped_reason.as_deref() == Some("identical-target")));
        assert_eq!(
            fs::read(source.join("RAW/IMG_01234.CR3")).unwrap(),
            b"raw-1234"
        );
        assert_eq!(fs::read(target.join("IMG_01234.CR3")).unwrap(), b"raw-1234");

        let conflict_root = order.join("冲突目标");
        fs::create_dir_all(&conflict_root).unwrap();
        fs::write(conflict_root.join("IMG_01234.JPG"), b"different-target").unwrap();
        let conflict_plan = CopyPlan {
            source_root: plan.source_root.clone(),
            source_root_identity: plan.source_root_identity.clone(),
            target_root: conflict_root.clone(),
            files: plan
                .files
                .iter()
                .cloned()
                .map(|mut file| {
                    file.target = conflict_root.join(file.source.file_name().unwrap());
                    file
                })
                .collect(),
        };
        assert!(
            preflight_copy_cancellable(&conflict_plan, &AtomicBool::new(false))
                .unwrap()
                .conflicts
                .iter()
                .any(|path| path.ends_with("IMG_01234.JPG"))
        );

        fs::rename(&source, order.join("断开后的原始照片")).unwrap();
        match preflight_copy_cancellable(&plan, &AtomicBool::new(false)) {
            Ok(report) => assert!(report.source_disconnected || report.source_changed),
            Err(CopyError::SourceDisconnected | CopyError::InvalidSource) => {}
            other => panic!("unexpected disconnected-source result: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn session_input_preview_rejects_a_swapped_persistent_root() {
        let directory = safe_tempdir();
        let state = AppState {
            storage: Storage::open(&directory.path().join("state.sqlite3")).unwrap(),
            app_data_dir: directory.path().to_path_buf(),
            cancelled: Mutex::new(HashMap::new()),
            provider_credentials: Mutex::new(()),
        };
        let session = state.storage.create_session("inputs", None).unwrap();
        let saved = save_session_input_inner(
            &state,
            &session.id,
            "customer.png".into(),
            "image".into(),
            valid_png_bytes(),
        )
        .unwrap();
        let input_root = directory
            .path()
            .join("sessions")
            .join(&session.id)
            .join("inputs");
        fs::rename(&input_root, input_root.with_extension("moved")).unwrap();
        fs::create_dir(&input_root).unwrap();
        fs::write(input_root.join(&saved.id), b"\x89PNG\r\n\x1a\nreplacement").unwrap();

        assert!(read_session_input_inner(&state, &session.id, &saved.id).is_err());
    }

    #[test]
    fn completion_report_separates_user_and_identical_skips() {
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec!["12".into(), "34".into()],
        };
        let report = completion_payload(
            "job-1",
            CompletionStatus::Completed,
            &snapshot,
            1,
            3,
            0,
            10,
            "start",
            "done",
        );

        assert_eq!(report.skipped_identical_count, 3);
        assert_eq!(report.skipped_user_count, 2);
    }

    #[test]
    fn blocking_report_requires_explicit_partial_or_missing_decision() {
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![NumberMatch {
                canonical_number: "1234".into(),
                status: MatchStatus::Missing,
                groups: vec![],
            }],
            skipped_numbers: vec![],
        };
        assert!(blocking_report(&snapshot).has_blocking_issue());
        let snapshot = ScanSnapshot {
            skipped_numbers: vec!["1234".into()],
            ..snapshot
        };
        assert!(!blocking_report(&snapshot).has_blocking_issue());
    }

    #[test]
    fn terminal_cleanup_removes_only_the_sessions_input_directory() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = Uuid::new_v4().to_string();
        let inputs = directory
            .path()
            .join("sessions")
            .join(&session_id)
            .join("inputs");
        let metadata = inputs.parent().unwrap().join("metadata.json");
        fs::create_dir_all(&inputs).unwrap();
        fs::write(inputs.join("recognition.png"), b"private-input").unwrap();
        fs::write(&metadata, b"keep").unwrap();

        cleanup_session_inputs(directory.path(), &session_id).unwrap();

        assert!(!inputs.exists());
        assert_eq!(fs::read(metadata).unwrap(), b"keep");
    }

    #[test]
    fn terminal_cleanup_failure_is_returned_to_the_event_builder() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = Uuid::new_v4().to_string();
        let inputs = directory
            .path()
            .join("sessions")
            .join(&session_id)
            .join("inputs");
        fs::create_dir_all(inputs.parent().unwrap()).unwrap();
        fs::write(&inputs, b"not-a-directory").unwrap();

        let error = cleanup_session_inputs(directory.path(), &session_id).unwrap_err();

        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(inputs.is_file());
    }

    #[test]
    fn completed_cancelled_and_failed_copy_sessions_all_cleanup_inputs() {
        for terminal in [
            SessionStatus::Completed,
            SessionStatus::Cancelled,
            SessionStatus::Failed,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let storage = Storage::open(&directory.path().join("state.sqlite3")).unwrap();
            let session = storage.create_session("cleanup", None).unwrap();
            storage
                .save_confirmed_numbers(
                    &session.id,
                    &[PhotoNumber {
                        original: "7".into(),
                        canonical: "7".into(),
                        confidence: None,
                        confirmed: true,
                    }],
                )
                .unwrap();
            storage
                .compare_and_set_status(
                    &session.id,
                    SessionStatus::ReadyToScan,
                    SessionStatus::Scanning,
                )
                .unwrap();
            let snapshot = ScanSnapshot {
                source: "/source".into(),
                source_root_identity: Default::default(),
                target: "/target".into(),
                items: vec![],
                skipped_numbers: vec!["7".into()],
            };
            let mut skipped = CopyHistoryItem::planned(
                &session.id,
                "7",
                Path::new("/source"),
                Path::new("/target"),
                "",
            );
            skipped.status = CopyItemStatus::Skipped;
            skipped.skipped_reason = Some("user-skipped".into());
            let plan_revision = storage
                .save_scan_result_if_status(
                    &session.id,
                    &snapshot,
                    None,
                    SessionStatus::Scanning,
                    Some(SessionStatus::ReadyToCopy),
                    Some(&[skipped]),
                )
                .unwrap();
            storage
                .consume_confirmation_and_start_copy(&session.id, plan_revision, None, false)
                .unwrap();
            let inputs = directory
                .path()
                .join("sessions")
                .join(&session.id)
                .join("inputs");
            fs::create_dir_all(&inputs).unwrap();
            fs::write(inputs.join("recognition.png"), b"private").unwrap();
            let state = AppState {
                storage,
                app_data_dir: directory.path().to_path_buf(),
                cancelled: Mutex::new(HashMap::new()),
                provider_credentials: Mutex::new(()),
            };

            assert_eq!(
                finalize_copy_session(&state, &session.id, plan_revision, terminal.clone())
                    .unwrap(),
                None
            );
            assert_eq!(
                state.storage.load_session(&session.id).unwrap().status,
                terminal
            );
            assert!(!inputs.exists());
        }
    }
}
