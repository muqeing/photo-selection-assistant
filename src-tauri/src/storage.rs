use std::{path::Path, sync::Mutex};

use crate::{
    copy_engine::PreflightReport,
    models::{
        CloudSettings, CopyHistoryItem, CopyItemStatus, PhotoNumber, ProviderProfile, ScanSnapshot,
        Session, SessionInput, SessionStatus,
    },
    numbers::canonical_number,
};
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  task_label TEXT NOT NULL CHECK(length(trim(task_label)) > 0),
  note TEXT,
  status TEXT NOT NULL,
  source_dir TEXT,
  target_dir TEXT,
  confirmation_hash TEXT,
  confirmation_expires_at INTEGER,
  workflow_revision INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS photo_numbers (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  original TEXT NOT NULL,
  canonical TEXT NOT NULL,
  confidence REAL,
  confirmed INTEGER NOT NULL,
  PRIMARY KEY(session_id, original),
  UNIQUE(session_id, canonical)
);
CREATE TABLE IF NOT EXISTS matches (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  canonical TEXT NOT NULL,
  status TEXT NOT NULL,
  selected_group_id TEXT,
  payload_json TEXT NOT NULL,
  PRIMARY KEY(session_id, canonical)
);
CREATE TABLE IF NOT EXISTS copy_items (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  canonical TEXT NOT NULL,
  source_path TEXT NOT NULL,
  target_path TEXT NOT NULL,
  planned_hash TEXT NOT NULL,
  plan_revision INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL,
  source_hash TEXT,
  skipped_reason TEXT,
  error_code TEXT,
  error_summary TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS scan_snapshots (
  session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  snapshot_json TEXT NOT NULL,
  preflight_json TEXT,
  revision INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS session_inputs (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  mime TEXT NOT NULL,
  stored_name TEXT NOT NULL,
  size INTEGER NOT NULL,
  root_identity_json TEXT NOT NULL,
  fingerprint_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("数据库错误：{0}")]
    Database(#[from] rusqlite::Error),
    #[error("设置 JSON 无效：{0}")]
    Json(#[from] serde_json::Error),
    #[error("任务标识不能为空")]
    BlankTaskLabel,
    #[error("编号尚未全部确认")]
    NumbersNotConfirmed,
    #[error("编号 canonical 无效：{0}")]
    InvalidCanonicalNumber(String),
    #[error("确认编号 canonical 重复：{0}")]
    DuplicateCanonicalNumber(String),
    #[error("云配置缺少 provider 或 secretRef")]
    InvalidCloudSettings,
    #[error("本地状态锁不可用")]
    Poisoned,
    #[error("会话不存在")]
    SessionNotFound,
    #[error("会话状态无效：{0}")]
    InvalidSessionStatus(String),
    #[error("复制清单状态无效：{0}")]
    InvalidCopyItemStatus(String),
    #[error("扫描或复制正在进行，不能修改已确认编号")]
    SessionBusy,
    #[error("会话状态已变化，操作未执行")]
    StateConflict,
    #[error("二次确认凭据无效或已过期")]
    ConfirmationRequired,
}

pub struct Storage {
    connection: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        Self::from_connection(Connection::open(path)?)
    }

    #[cfg(test)]
    fn open_in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, StorageError> {
        connection.execute_batch(SCHEMA)?;
        ensure_scan_snapshot_columns(&connection)?;
        ensure_session_columns(&connection)?;
        ensure_copy_items_schema(&mut connection)?;
        migrate_photo_number_uniqueness(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn create_session(
        &self,
        task_label: &str,
        note: Option<&str>,
    ) -> Result<Session, StorageError> {
        let task_label = task_label.trim();
        if task_label.is_empty() {
            return Err(StorageError::BlankTaskLabel);
        }
        let session = Session::new(task_label, note);
        self.connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "INSERT INTO sessions
                 (id, task_label, note, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![
                    session.id,
                    session.task_label,
                    session.note,
                    session.status.as_str(),
                    session.created_at,
                ],
            )?;
        Ok(session)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id, task_label, note, status, created_at,
                    COALESCE((SELECT MIN(confirmed) FROM photo_numbers
                              WHERE session_id = sessions.id), 0)
             FROM sessions ORDER BY created_at DESC",
        )?;
        let sessions = statement
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;
        Ok(sessions)
    }

    pub fn load_session(&self, session_id: &str) -> Result<Session, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection
            .query_row(
                "SELECT id, task_label, note, status, created_at,
                        COALESCE((SELECT MIN(confirmed) FROM photo_numbers
                                  WHERE session_id = sessions.id), 0)
                 FROM sessions WHERE id = ?1",
                [session_id],
                session_from_row,
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound)
    }

    #[cfg(test)]
    fn save_session(&self, session: &Session) -> Result<(), StorageError> {
        let changed = self
            .connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![
                    session.id,
                    session.status.as_str(),
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
        if changed == 0 {
            return Err(StorageError::SessionNotFound);
        }
        Ok(())
    }

    /// Atomically claims a workflow state. Callers must never emulate this
    /// with a separate `load_session` followed by `save_session`, because two
    /// renderer invocations may race between those operations.
    pub fn compare_and_set_status(
        &self,
        session_id: &str,
        expected: SessionStatus,
        next: SessionStatus,
    ) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE sessions
             SET status = ?3,
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?4
             WHERE id = ?1 AND status = ?2",
            params![
                session_id,
                expected.as_str(),
                next.as_str(),
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let current: Option<String> = connection
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        match current.as_deref() {
            None => Err(StorageError::SessionNotFound),
            Some("scanning" | "copying") => Err(StorageError::SessionBusy),
            Some(_) => Err(StorageError::StateConflict),
        }
    }

    pub fn begin_scan(&self, session_id: &str) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE sessions
             SET status = 'scanning',
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?2
             WHERE id = ?1
               AND status IN ('readyToScan', 'needsAttention')",
            params![session_id, chrono::Utc::now().to_rfc3339()],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let current: Option<String> = connection
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        match current.as_deref() {
            None => Err(StorageError::SessionNotFound),
            Some("scanning" | "copying") => Err(StorageError::SessionBusy),
            Some(_) => Err(StorageError::StateConflict),
        }
    }

    pub fn finish_copy_attempt(
        &self,
        session_id: &str,
        plan_revision: i64,
        next: SessionStatus,
        failure_code: Option<&str>,
        failure_summary: Option<&str>,
    ) -> Result<(), StorageError> {
        if !matches!(
            next,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            return Err(StorageError::StateConflict);
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        match next {
            SessionStatus::Cancelled => {
                transaction.execute(
                    "UPDATE copy_items
                     SET status = 'cancelled',
                         error_code = COALESCE(error_code, 'cancelled'),
                         error_summary = COALESCE(
                           error_summary, 'copy cancelled before this item completed'
                         ),
                         updated_at = ?3
                     WHERE session_id = ?1 AND plan_revision = ?2
                       AND status IN ('planned', 'copying')",
                    params![session_id, plan_revision, now],
                )?;
            }
            SessionStatus::Failed => {
                transaction.execute(
                    "UPDATE copy_items
                     SET status = 'failed',
                         error_code = COALESCE(error_code, ?3),
                         error_summary = COALESCE(error_summary, ?4),
                         updated_at = ?5
                     WHERE session_id = ?1 AND plan_revision = ?2
                       AND status IN ('planned', 'copying')",
                    params![
                        session_id,
                        plan_revision,
                        failure_code.unwrap_or("copy-aborted"),
                        failure_summary
                            .unwrap_or("copy attempt stopped before this item completed"),
                        now
                    ],
                )?;
            }
            SessionStatus::Completed => {
                let unfinished: bool = transaction.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM copy_items
                       WHERE session_id = ?1 AND plan_revision = ?2
                         AND status IN ('planned', 'copying')
                     )",
                    params![session_id, plan_revision],
                    |row| row.get(0),
                )?;
                if unfinished {
                    return Err(StorageError::StateConflict);
                }
            }
            _ => return Err(StorageError::StateConflict),
        }
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = ?3,
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?4
             WHERE id = ?1 AND status = 'copying'
               AND EXISTS(
                 SELECT 1 FROM scan_snapshots
                 WHERE session_id = ?1 AND revision = ?2
               )",
            params![session_id, plan_revision, next.as_str(), now],
        )?;
        if changed == 1 {
            transaction.commit()?;
            Ok(())
        } else {
            Err(StorageError::StateConflict)
        }
    }

    pub fn issue_confirmation(&self, session_id: &str) -> Result<String, StorageError> {
        let token = uuid::Uuid::new_v4().to_string();
        let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        let expires_at = chrono::Utc::now().timestamp().saturating_add(15 * 60);
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE sessions
             SET confirmation_hash = ?2,
                 confirmation_expires_at = ?3,
                 updated_at = ?4
             WHERE id = ?1 AND status = 'readyToCopy'",
            params![
                session_id,
                token_hash,
                expires_at,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        if changed == 1 {
            Ok(token)
        } else {
            let current: Option<String> = connection
                .query_row(
                    "SELECT status FROM sessions WHERE id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .optional()?;
            match current.as_deref() {
                None => Err(StorageError::SessionNotFound),
                Some("scanning" | "copying") => Err(StorageError::SessionBusy),
                Some(_) => Err(StorageError::StateConflict),
            }
        }
    }

    pub fn consume_confirmation_and_start_copy(
        &self,
        session_id: &str,
        expected_revision: i64,
        supplied: Option<&str>,
        confirmation_required: bool,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current: Option<(String, Option<String>, Option<i64>)> = transaction
            .query_row(
                "SELECT status, confirmation_hash, confirmation_expires_at
                 FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (status, expected_hash, expires_at) = current.ok_or(StorageError::SessionNotFound)?;
        if status == "scanning" || status == "copying" {
            return Err(StorageError::SessionBusy);
        }
        if status != "readyToCopy" {
            return Err(StorageError::StateConflict);
        }
        let revision_matches: bool = transaction.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM scan_snapshots s
               JOIN copy_items c
                 ON c.session_id = s.session_id
                AND c.plan_revision = s.revision
               WHERE s.session_id = ?1 AND s.revision = ?2
             )",
            params![session_id, expected_revision],
            |row| row.get(0),
        )?;
        if !revision_matches {
            return Err(StorageError::StateConflict);
        }
        if confirmation_required {
            let supplied_hash = supplied
                .map(|token| blake3::hash(token.as_bytes()).to_hex().to_string())
                .ok_or(StorageError::ConfirmationRequired)?;
            if expected_hash.as_deref() != Some(supplied_hash.as_str())
                || expires_at.map_or(true, |value| value < chrono::Utc::now().timestamp())
            {
                return Err(StorageError::ConfirmationRequired);
            }
        }
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = 'copying',
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?2
             WHERE id = ?1 AND status = 'readyToCopy'",
            params![session_id, chrono::Utc::now().to_rfc3339()],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn save_worker_preflight(
        &self,
        session_id: &str,
        plan_revision: i64,
        preflight: &PreflightReport,
        next: Option<SessionStatus>,
    ) -> Result<(), StorageError> {
        if next
            .as_ref()
            .is_some_and(|status| *status != SessionStatus::NeedsAttention)
        {
            return Err(StorageError::StateConflict);
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE scan_snapshots
             SET preflight_json = ?3, updated_at = ?4
             WHERE session_id = ?1 AND revision = ?2
               AND EXISTS(
                 SELECT 1 FROM sessions
                 WHERE id = ?1 AND status = 'copying'
               )",
            params![
                session_id,
                plan_revision,
                serde_json::to_string(preflight)?,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        if let Some(next) = next {
            let changed = transaction.execute(
                "UPDATE sessions
                 SET status = ?3,
                     confirmation_hash = NULL,
                     confirmation_expires_at = NULL,
                     updated_at = ?4
                 WHERE id = ?1 AND status = 'copying'
                   AND EXISTS(
                     SELECT 1 FROM scan_snapshots
                     WHERE session_id = ?1 AND revision = ?2
                   )",
                params![
                    session_id,
                    plan_revision,
                    next.as_str(),
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            if changed != 1 {
                return Err(StorageError::StateConflict);
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn pause_copy_for_disconnect(
        &self,
        session_id: &str,
        plan_revision: i64,
        preflight: &PreflightReport,
    ) -> Result<(), StorageError> {
        if !preflight.source_disconnected && !preflight.target_disconnected {
            return Err(StorageError::StateConflict);
        }
        let preflight_json = serde_json::to_string(preflight)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        let changed = transaction.execute(
            "UPDATE scan_snapshots
             SET preflight_json = ?3, updated_at = ?4
             WHERE session_id = ?1 AND revision = ?2
               AND EXISTS(
                 SELECT 1 FROM sessions
                 WHERE id = ?1 AND status = 'copying'
               )",
            params![session_id, plan_revision, preflight_json, now],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        transaction.execute(
            "UPDATE copy_items
             SET status = 'planned',
                 error_code = NULL,
                 error_summary = NULL,
                 updated_at = ?3
             WHERE session_id = ?1
               AND plan_revision = ?2
               AND status = 'copying'",
            params![session_id, plan_revision, now],
        )?;
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = 'needsAttention',
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?3
             WHERE id = ?1
               AND status = 'copying'
               AND EXISTS(
                 SELECT 1 FROM scan_snapshots
                 WHERE session_id = ?1 AND revision = ?2
               )",
            params![session_id, plan_revision, now],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn save_attention_preflight(
        &self,
        session_id: &str,
        plan_revision: i64,
        preflight: &PreflightReport,
    ) -> Result<(), StorageError> {
        let changed = self
            .connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "UPDATE scan_snapshots
                 SET preflight_json = ?3, updated_at = ?4
                 WHERE session_id = ?1 AND revision = ?2
                   AND EXISTS(
                     SELECT 1 FROM sessions
                     WHERE id = ?1 AND status = 'needsAttention'
                   )",
                params![
                    session_id,
                    plan_revision,
                    serde_json::to_string(preflight)?,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StorageError::StateConflict)
        }
    }

    pub fn recover_interrupted_sessions(&self) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        transaction.execute(
            "UPDATE copy_items
             SET status = 'failed',
                 error_code = COALESCE(error_code, 'interrupted'),
                 error_summary = COALESCE(error_summary, 'application exited during copy'),
                 updated_at = ?1
             WHERE status IN ('planned', 'copying')
               AND session_id IN (
                 SELECT id FROM sessions WHERE status = 'copying'
               )",
            [&now],
        )?;
        transaction.execute(
            "UPDATE sessions
             SET status = 'readyToScan', updated_at = ?1
             WHERE status IN ('scanning', 'copying')",
            [&now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn validate_session_input_state(&self, session_id: &str) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let current: Option<String> = connection
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        match current.as_deref() {
            None => Err(StorageError::SessionNotFound),
            Some("scanning" | "copying") => Err(StorageError::SessionBusy),
            Some("draft" | "awaitingNumberConfirmation") => Ok(()),
            Some(_) => Err(StorageError::StateConflict),
        }
    }

    pub fn prepare_session_input(&self, session_id: &str) -> Result<(), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let current: Option<String> = connection
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        match current.as_deref() {
            None => Err(StorageError::SessionNotFound),
            Some("scanning" | "copying") => Err(StorageError::SessionBusy),
            Some("draft") => {
                let changed = connection.execute(
                    "UPDATE sessions
                     SET status = 'awaitingNumberConfirmation', updated_at = ?2
                     WHERE id = ?1 AND status = 'draft'",
                    params![session_id, chrono::Utc::now().to_rfc3339()],
                )?;
                if changed == 1 {
                    Ok(())
                } else {
                    Err(StorageError::StateConflict)
                }
            }
            Some("awaitingNumberConfirmation") => Ok(()),
            Some(_) => Err(StorageError::StateConflict),
        }
    }

    pub fn save_session_input(&self, input: &SessionInput) -> Result<(), StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "INSERT INTO session_inputs(
                   id, session_id, name, kind, mime, stored_name, size,
                   root_identity_json, fingerprint_json, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    input.id,
                    input.session_id,
                    input.name,
                    input.kind,
                    input.mime,
                    input.stored_name,
                    input.size,
                    serde_json::to_string(&input.root_identity)?,
                    serde_json::to_string(&input.fingerprint)?,
                    input.created_at,
                ],
            )?;
        Ok(())
    }

    pub fn list_session_inputs(&self, session_id: &str) -> Result<Vec<SessionInput>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id, session_id, name, kind, mime, stored_name, size,
                    root_identity_json, fingerprint_json, created_at
             FROM session_inputs WHERE session_id = ?1 ORDER BY created_at, id",
        )?;
        let inputs = statement
            .query_map([session_id], |row| {
                let root_identity: String = row.get(7)?;
                let fingerprint: String = row.get(8)?;
                Ok(SessionInput {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    name: row.get(2)?,
                    kind: row.get(3)?,
                    mime: row.get(4)?,
                    stored_name: row.get(5)?,
                    size: row.get(6)?,
                    root_identity: serde_json::from_str(&root_identity).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    fingerprint: serde_json::from_str(&fingerprint).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            8,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    created_at: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)?;
        Ok(inputs)
    }

    pub fn load_session_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<SessionInput, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection
            .query_row(
                "SELECT id, session_id, name, kind, mime, stored_name, size,
                        root_identity_json, fingerprint_json, created_at
                 FROM session_inputs WHERE session_id = ?1 AND id = ?2",
                params![session_id, input_id],
                |row| {
                    let root_identity: String = row.get(7)?;
                    let fingerprint: String = row.get(8)?;
                    Ok(SessionInput {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        name: row.get(2)?,
                        kind: row.get(3)?,
                        mime: row.get(4)?,
                        stored_name: row.get(5)?,
                        size: row.get(6)?,
                        root_identity: serde_json::from_str(&root_identity).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        fingerprint: serde_json::from_str(&fingerprint).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        created_at: row.get(9)?,
                    })
                },
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound)
    }

    pub fn delete_session_inputs(&self, session_id: &str) -> Result<(), StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "DELETE FROM session_inputs WHERE session_id = ?1",
                [session_id],
            )?;
        Ok(())
    }

    pub fn bind_paths(
        &self,
        session_id: &str,
        source: Option<&Path>,
        target: Option<&Path>,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current_status: Option<String> = transaction
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let current_status = current_status.ok_or(StorageError::SessionNotFound)?;
        if current_status == "scanning" || current_status == "copying" {
            return Err(StorageError::SessionBusy);
        }
        let changed = transaction.execute(
            "UPDATE sessions
             SET source_dir = COALESCE(?2, source_dir),
                 target_dir = COALESCE(?3, target_dir),
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 workflow_revision = workflow_revision + 1,
                 status = CASE
                   WHEN EXISTS(SELECT 1 FROM photo_numbers WHERE session_id = ?1)
                   THEN 'readyToScan' ELSE status END,
                 updated_at = ?4
             WHERE id = ?1",
            params![
                session_id,
                source.map(|value| value.to_string_lossy().into_owned()),
                target.map(|value| value.to_string_lossy().into_owned()),
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        if changed == 0 {
            return Err(StorageError::SessionNotFound);
        }
        supersede_unfinished_plan(&transaction, session_id, &chrono::Utc::now().to_rfc3339())?;
        transaction.execute(
            "DELETE FROM scan_snapshots WHERE session_id = ?1",
            [session_id],
        )?;
        transaction.execute("DELETE FROM matches WHERE session_id = ?1", [session_id])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_bound_paths(
        &self,
        session_id: &str,
    ) -> Result<(Option<std::path::PathBuf>, Option<std::path::PathBuf>), StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection
            .query_row(
                "SELECT source_dir, target_dir FROM sessions WHERE id = ?1",
                [session_id],
                |row| {
                    let source: Option<String> = row.get(0)?;
                    let target: Option<String> = row.get(1)?;
                    Ok((source.map(Into::into), target.map(Into::into)))
                },
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound)
    }

    #[cfg(test)]
    fn save_scan_snapshot(
        &self,
        session_id: &str,
        snapshot: &ScanSnapshot,
        preflight: Option<&PreflightReport>,
    ) -> Result<(), StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "INSERT INTO scan_snapshots(session_id, snapshot_json, preflight_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
               snapshot_json = excluded.snapshot_json,
               preflight_json = excluded.preflight_json,
               revision = scan_snapshots.revision + 1,
               updated_at = excluded.updated_at",
                params![
                    session_id,
                    serde_json::to_string(snapshot)?,
                    preflight.map(serde_json::to_string).transpose()?,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
        Ok(())
    }

    pub fn save_scan_result_if_status(
        &self,
        session_id: &str,
        snapshot: &ScanSnapshot,
        preflight: Option<&PreflightReport>,
        expected: SessionStatus,
        next: Option<SessionStatus>,
        copy_plan: Option<&[CopyHistoryItem]>,
    ) -> Result<i64, StorageError> {
        if expected != SessionStatus::Scanning
            || !matches!(
                next,
                Some(SessionStatus::NeedsAttention | SessionStatus::ReadyToCopy)
            )
            || copy_plan.is_some() != (next == Some(SessionStatus::ReadyToCopy))
        {
            return Err(StorageError::StateConflict);
        }
        let result = self.save_scan_result_if_status_inner(
            session_id,
            snapshot,
            preflight,
            expected.clone(),
            next,
            copy_plan,
        );
        if result.is_err() && expected == SessionStatus::Scanning {
            // A snapshot write is part of completing a claimed scan. SQLite
            // rolls the failed transaction back to `scanning`; converge that
            // claim with a separate CAS so restart is never required.
            let _ = self.recover_scan_failure(session_id);
        }
        result
    }

    fn save_scan_result_if_status_inner(
        &self,
        session_id: &str,
        snapshot: &ScanSnapshot,
        preflight: Option<&PreflightReport>,
        expected: SessionStatus,
        next: Option<SessionStatus>,
        copy_plan: Option<&[CopyHistoryItem]>,
    ) -> Result<i64, StorageError> {
        let snapshot_json = serde_json::to_string(snapshot)?;
        let preflight_json = preflight.map(serde_json::to_string).transpose()?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current: Option<(String, i64)> = transaction
            .query_row(
                "SELECT status, workflow_revision FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (current, workflow_revision) = current.ok_or(StorageError::SessionNotFound)?;
        if current != expected.as_str() {
            return if current == "scanning" || current == "copying" {
                Err(StorageError::SessionBusy)
            } else {
                Err(StorageError::StateConflict)
            };
        }
        let revision = workflow_revision.saturating_add(1);
        let now = chrono::Utc::now().to_rfc3339();
        supersede_unfinished_plan(&transaction, session_id, &now)?;
        transaction.execute(
            "INSERT INTO scan_snapshots(
               session_id, snapshot_json, preflight_json, revision, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
               snapshot_json = excluded.snapshot_json,
               preflight_json = excluded.preflight_json,
               revision = excluded.revision,
               updated_at = excluded.updated_at",
            params![session_id, snapshot_json, preflight_json, revision, now],
        )?;
        if let Some(items) = copy_plan {
            insert_copy_plan(&transaction, session_id, revision, items)?;
        }
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = COALESCE(?4, status),
                 workflow_revision = ?3,
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?5
             WHERE id = ?1 AND status = ?2 AND workflow_revision = ?6",
            params![
                session_id,
                expected.as_str(),
                revision,
                next.as_ref().map(SessionStatus::as_str),
                now,
                workflow_revision
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        transaction.commit()?;
        Ok(revision)
    }

    pub fn recover_scan_failure(&self, session_id: &str) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().to_rfc3339();
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = 'readyToScan',
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 workflow_revision = workflow_revision + 1,
                 updated_at = ?2
             WHERE id = ?1 AND status IN ('scanning', 'readyToCopy')",
            params![session_id, now],
        )?;
        if changed == 1 {
            supersede_unfinished_plan(&transaction, session_id, &now)?;
            transaction.commit()?;
            return Ok(());
        }
        let current: Option<String> = transaction
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let result = match current.as_deref() {
            None => Err(StorageError::SessionNotFound),
            Some("needsAttention" | "readyToScan") => Ok(()),
            Some(_) => Err(StorageError::StateConflict),
        };
        if result.is_ok() {
            transaction.commit()?;
        }
        result
    }

    pub fn load_scan_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<ScanSnapshot>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let value: Option<String> = connection
            .query_row(
                "SELECT snapshot_json FROM scan_snapshots WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .transpose()
    }

    pub fn load_scan_snapshot_with_revision(
        &self,
        session_id: &str,
    ) -> Result<Option<(ScanSnapshot, i64)>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let value: Option<(String, i64)> = connection
            .query_row(
                "SELECT snapshot_json, revision
                 FROM scan_snapshots WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        value
            .map(|(json, revision)| {
                serde_json::from_str(&json)
                    .map(|snapshot| (snapshot, revision))
                    .map_err(StorageError::from)
            })
            .transpose()
    }

    pub fn save_resolution_snapshot(
        &self,
        session_id: &str,
        snapshot: &ScanSnapshot,
        preflight: &PreflightReport,
        expected_revision: i64,
        ready_to_copy: bool,
        copy_plan: Option<&[CopyHistoryItem]>,
    ) -> Result<(), StorageError> {
        if copy_plan.is_some() != ready_to_copy {
            return Err(StorageError::StateConflict);
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current_status: Option<(String, i64)> = transaction
            .query_row(
                "SELECT status, workflow_revision FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (current_status, workflow_revision) =
            current_status.ok_or(StorageError::SessionNotFound)?;
        match current_status.as_str() {
            "scanning" | "copying" => return Err(StorageError::SessionBusy),
            "needsAttention" => {}
            _ => return Err(StorageError::StateConflict),
        }
        if workflow_revision != expected_revision {
            return Err(StorageError::StateConflict);
        }
        let revision = expected_revision.saturating_add(1);
        let now = chrono::Utc::now().to_rfc3339();
        let changed = transaction.execute(
            "UPDATE scan_snapshots
             SET snapshot_json = ?3,
                 preflight_json = ?4,
                 revision = ?5,
                 updated_at = ?6
             WHERE session_id = ?1 AND revision = ?2",
            params![
                session_id,
                expected_revision,
                serde_json::to_string(snapshot)?,
                serde_json::to_string(preflight)?,
                revision,
                now
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        supersede_unfinished_plan(&transaction, session_id, &now)?;
        if let Some(items) = copy_plan {
            insert_copy_plan(&transaction, session_id, revision, items)?;
        }
        let changed = transaction.execute(
            "UPDATE sessions
             SET status = CASE WHEN ?3 THEN 'readyToCopy' ELSE status END,
                 workflow_revision = ?2,
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 updated_at = ?4
             WHERE id = ?1 AND status = 'needsAttention'
               AND workflow_revision = ?5",
            params![session_id, revision, ready_to_copy, now, expected_revision],
        )?;
        if changed != 1 {
            return Err(StorageError::StateConflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_preflight(
        &self,
        session_id: &str,
    ) -> Result<Option<PreflightReport>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let value: Option<Option<String>> = connection
            .query_row(
                "SELECT preflight_json FROM scan_snapshots WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        value
            .flatten()
            .map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .transpose()
    }

    pub fn save_setting<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), StorageError> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)?.execute(
            "INSERT INTO settings(key, value_json) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub fn load_setting<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .transpose()
    }

    pub fn list_providers(&self) -> Result<Vec<ProviderProfile>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection
            .prepare("SELECT value_json FROM settings WHERE key LIKE 'provider:%' ORDER BY key")?;
        let values = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .collect()
    }

    pub fn save_provider_and_settings<T: serde::Serialize>(
        &self,
        profile: &ProviderProfile,
        settings: &T,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        if profile.is_default {
            let mut statement = transaction
                .prepare("SELECT key, value_json FROM settings WHERE key LIKE 'provider:%'")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for (key, json) in rows {
                let mut other: ProviderProfile = serde_json::from_str(&json)?;
                if other.id != profile.id && other.is_default {
                    other.is_default = false;
                    transaction.execute(
                        "UPDATE settings SET value_json = ?2 WHERE key = ?1",
                        params![key, serde_json::to_string(&other)?],
                    )?;
                }
            }
        }
        transaction.execute(
            "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            params![
                format!("provider:{}", profile.id),
                serde_json::to_string(profile)?
            ],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value_json) VALUES ('app-settings', ?1)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            [serde_json::to_string(settings)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_provider_and_settings<T: serde::Serialize>(
        &self,
        profile_id: &str,
        settings: &T,
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM settings WHERE key = ?1",
            [format!("provider:{profile_id}")],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value_json) VALUES ('app-settings', ?1)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            [serde_json::to_string(settings)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_confirmed_numbers(
        &self,
        session_id: &str,
        numbers: &[PhotoNumber],
    ) -> Result<(), StorageError> {
        let mut canonicals = std::collections::HashSet::new();
        for number in numbers {
            if canonical_number(&number.canonical).as_deref() != Some(number.canonical.as_str()) {
                return Err(StorageError::InvalidCanonicalNumber(
                    number.canonical.clone(),
                ));
            }
            if !canonicals.insert(number.canonical.as_str()) {
                return Err(StorageError::DuplicateCanonicalNumber(
                    number.canonical.clone(),
                ));
            }
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current_status: Option<String> = transaction
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let current_status = current_status.ok_or(StorageError::SessionNotFound)?;
        if current_status == "scanning" || current_status == "copying" {
            return Err(StorageError::SessionBusy);
        }
        transaction.execute(
            "DELETE FROM photo_numbers WHERE session_id = ?1",
            [session_id],
        )?;
        for number in numbers {
            transaction.execute(
                "INSERT INTO photo_numbers
                 (session_id, original, canonical, confidence, confirmed)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    number.original,
                    number.canonical,
                    number.confidence,
                    number.confirmed,
                ],
            )?;
        }
        supersede_unfinished_plan(&transaction, session_id, &chrono::Utc::now().to_rfc3339())?;
        transaction.execute(
            "DELETE FROM scan_snapshots WHERE session_id = ?1",
            [session_id],
        )?;
        transaction.execute("DELETE FROM matches WHERE session_id = ?1", [session_id])?;
        let next_status = if !numbers.is_empty() && numbers.iter().all(|number| number.confirmed) {
            "readyToScan"
        } else {
            "awaitingNumberConfirmation"
        };
        transaction.execute(
            "UPDATE sessions
             SET status = ?3,
                 confirmation_hash = NULL,
                 confirmation_expires_at = NULL,
                 workflow_revision = workflow_revision + 1,
                 updated_at = ?2
             WHERE id = ?1",
            params![session_id, chrono::Utc::now().to_rfc3339(), next_status],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_numbers(&self, session_id: &str) -> Result<Vec<PhotoNumber>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT original, canonical, confidence, confirmed
             FROM photo_numbers WHERE session_id = ?1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(PhotoNumber {
                original: row.get(0)?,
                canonical: row.get(1)?,
                confidence: row.get(2)?,
                confirmed: row.get::<_, i64>(3)? == 1,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    #[cfg(test)]
    fn save_copy_plan(
        &self,
        session_id: &str,
        items: &[CopyHistoryItem],
    ) -> Result<(), StorageError> {
        if items.iter().any(|item| item.session_id != session_id) {
            return Err(StorageError::StateConflict);
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction()?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [session_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StorageError::SessionNotFound);
        }
        let now = chrono::Utc::now().to_rfc3339();
        supersede_unfinished_plan(&transaction, session_id, &now)?;
        let revision: i64 = transaction.query_row(
            "SELECT workflow_revision FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        insert_copy_plan(&transaction, session_id, revision, items)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn update_copy_item(
        &self,
        item_id: &str,
        plan_revision: i64,
        status: CopyItemStatus,
        source_hash: Option<&str>,
        skipped_reason: Option<&str>,
        error_code: Option<&str>,
        error_summary: Option<&str>,
    ) -> Result<(), StorageError> {
        let changed = self
            .connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "UPDATE copy_items
                 SET status = ?2,
                     source_hash = COALESCE(?3, source_hash),
                     skipped_reason = ?4,
                     error_code = ?5,
                     error_summary = ?6,
                     updated_at = ?7
                 WHERE id = ?1
                   AND plan_revision = ?8
                   AND status IN ('planned', 'copying')",
                params![
                    item_id,
                    status.as_str(),
                    source_hash,
                    skipped_reason,
                    error_code,
                    error_summary,
                    chrono::Utc::now().to_rfc3339(),
                    plan_revision
                ],
            )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StorageError::StateConflict)
        }
    }

    pub fn load_copy_items(&self, session_id: &str) -> Result<Vec<CopyHistoryItem>, StorageError> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT id, session_id, canonical, source_path, target_path,
                    planned_hash, plan_revision, status, source_hash, skipped_reason,
                    error_code, error_summary, created_at, updated_at
             FROM copy_items WHERE session_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map([session_id], |row| {
            let status: String = row.get(7)?;
            let status = copy_item_status_from_str(&status).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(CopyHistoryItem {
                id: row.get(0)?,
                session_id: row.get(1)?,
                canonical_number: row.get(2)?,
                source: std::path::PathBuf::from(row.get::<_, String>(3)?),
                target: std::path::PathBuf::from(row.get::<_, String>(4)?),
                planned_hash: row.get(5)?,
                plan_revision: row.get(6)?,
                status,
                source_hash: row.get(8)?,
                skipped_reason: row.get(9)?,
                error_code: row.get(10)?,
                error_summary: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Persists only non-secret cloud configuration. Credentials must be stored
    /// with `crate::secrets` and represented here by `secret_ref`.
    pub fn save_cloud_settings(
        &self,
        profile_id: &str,
        settings: &CloudSettings,
    ) -> Result<(), StorageError> {
        if settings.provider.trim().is_empty() || settings.secret_ref.trim().is_empty() {
            return Err(StorageError::InvalidCloudSettings);
        }
        let key = cloud_settings_key(profile_id);
        self.connection
            .lock()
            .map_err(|_| StorageError::Poisoned)?
            .execute(
                "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                params![key, serde_json::to_string(settings)?],
            )?;
        Ok(())
    }

    pub fn load_cloud_settings(
        &self,
        profile_id: &str,
    ) -> Result<Option<CloudSettings>, StorageError> {
        let key = cloud_settings_key(profile_id);
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|json| serde_json::from_str(&json).map_err(StorageError::from))
            .transpose()
    }
}

fn supersede_unfinished_plan(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    now: &str,
) -> Result<(), StorageError> {
    transaction.execute(
        "UPDATE copy_items
         SET status = 'planSuperseded',
             error_code = COALESCE(error_code, 'plan-superseded'),
             error_summary = COALESCE(error_summary, 'copy plan superseded by newer inputs'),
             updated_at = ?2
         WHERE session_id = ?1 AND status IN ('planned', 'copying')",
        params![session_id, now],
    )?;
    Ok(())
}

fn insert_copy_plan(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    revision: i64,
    items: &[CopyHistoryItem],
) -> Result<(), StorageError> {
    if items.iter().any(|item| item.session_id != session_id) {
        return Err(StorageError::StateConflict);
    }
    for item in items {
        transaction.execute(
            "INSERT INTO copy_items(
               id, session_id, canonical, source_path, target_path,
               planned_hash, plan_revision, status, source_hash, skipped_reason,
               error_code, error_summary, created_at, updated_at
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
             )",
            params![
                item.id,
                item.session_id,
                item.canonical_number,
                item.source.to_string_lossy().into_owned(),
                item.target.to_string_lossy().into_owned(),
                item.planned_hash,
                revision,
                item.status.as_str(),
                item.source_hash,
                item.skipped_reason,
                item.error_code,
                item.error_summary,
                item.created_at,
                item.updated_at,
            ],
        )?;
    }
    Ok(())
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let status: String = row.get(3)?;
    let status = match status.as_str() {
        "draft" => SessionStatus::Draft,
        "awaitingNumberConfirmation" => SessionStatus::AwaitingNumberConfirmation,
        "readyToScan" => SessionStatus::ReadyToScan,
        "scanning" => SessionStatus::Scanning,
        "needsAttention" => SessionStatus::NeedsAttention,
        "readyToCopy" => SessionStatus::ReadyToCopy,
        "copying" => SessionStatus::Copying,
        "completed" => SessionStatus::Completed,
        "failed" => SessionStatus::Failed,
        "cancelled" => SessionStatus::Cancelled,
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(StorageError::InvalidSessionStatus(status)),
            ))
        }
    };
    Ok(Session {
        id: row.get(0)?,
        task_label: row.get(1)?,
        note: row.get(2)?,
        status,
        created_at: row.get(4)?,
        numbers_confirmed: row.get::<_, i64>(5)? == 1,
    })
}

fn cloud_settings_key(profile_id: &str) -> String {
    format!("cloud-settings:{profile_id}")
}

fn ensure_session_columns(connection: &Connection) -> Result<(), StorageError> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        columns
    };
    if !columns.contains("confirmation_hash") {
        connection.execute("ALTER TABLE sessions ADD COLUMN confirmation_hash TEXT", [])?;
    }
    if !columns.contains("confirmation_expires_at") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN confirmation_expires_at INTEGER",
            [],
        )?;
    }
    if !columns.contains("workflow_revision") {
        connection.execute(
            "ALTER TABLE sessions
             ADD COLUMN workflow_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        connection.execute(
            "UPDATE sessions
             SET workflow_revision = COALESCE(
               (SELECT revision FROM scan_snapshots
                WHERE session_id = sessions.id),
               0
             )",
            [],
        )?;
    }
    Ok(())
}

fn ensure_scan_snapshot_columns(connection: &Connection) -> Result<(), StorageError> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(scan_snapshots)")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        columns
    };
    if !columns.contains("revision") {
        connection.execute(
            "ALTER TABLE scan_snapshots
             ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn copy_item_status_from_str(value: &str) -> Result<CopyItemStatus, StorageError> {
    match value {
        "planned" => Ok(CopyItemStatus::Planned),
        "copying" => Ok(CopyItemStatus::Copying),
        "copied" => Ok(CopyItemStatus::Copied),
        "skipped" => Ok(CopyItemStatus::Skipped),
        "failed" => Ok(CopyItemStatus::Failed),
        "cancelled" => Ok(CopyItemStatus::Cancelled),
        "planSuperseded" => Ok(CopyItemStatus::PlanSuperseded),
        _ => Err(StorageError::InvalidCopyItemStatus(value.to_owned())),
    }
}

fn ensure_copy_items_schema(connection: &mut Connection) -> Result<(), StorageError> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(copy_items)")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        columns
    };
    if columns.contains("id")
        && columns.contains("canonical")
        && columns.contains("planned_hash")
        && columns.contains("plan_revision")
        && columns.contains("error_summary")
    {
        connection.execute(
            "CREATE INDEX IF NOT EXISTS copy_items_session_created
             ON copy_items(session_id, created_at)",
            [],
        )?;
        return Ok(());
    }

    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "ALTER TABLE copy_items RENAME TO copy_items_legacy;
         CREATE TABLE copy_items (
           id TEXT PRIMARY KEY,
           session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
           canonical TEXT NOT NULL,
           source_path TEXT NOT NULL,
           target_path TEXT NOT NULL,
           planned_hash TEXT NOT NULL,
           plan_revision INTEGER NOT NULL DEFAULT 0,
           status TEXT NOT NULL,
           source_hash TEXT,
           skipped_reason TEXT,
           error_code TEXT,
           error_summary TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );",
    )?;
    let now = chrono::Utc::now().to_rfc3339();
    transaction.execute(
        "INSERT INTO copy_items(
           id, session_id, canonical, source_path, target_path, planned_hash,
           plan_revision, status, source_hash, skipped_reason, error_code, error_summary,
           created_at, updated_at
         )
         SELECT lower(hex(randomblob(16))), session_id, '', source_path, target_path,
                COALESCE(source_hash, ''), 0, status, source_hash, NULL, error_code,
                CASE WHEN error_code IS NULL THEN NULL ELSE error_code END, ?1, ?1
         FROM copy_items_legacy",
        [&now],
    )?;
    transaction.execute_batch(
        "DROP TABLE copy_items_legacy;
         CREATE INDEX IF NOT EXISTS copy_items_session_created
           ON copy_items(session_id, created_at);",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Upgrades databases created before canonical numbers were normalized and
/// uniquely constrained. The transaction retains the earliest row for each
/// `(session_id, canonical_number(canonical))` pair before enforcing that
/// invariant for legacy table definitions.
fn migrate_photo_number_uniqueness(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction()?;
    let legacy_rows = {
        let mut statement = transaction.prepare(
            "SELECT rowid, session_id, canonical
             FROM photo_numbers
             ORDER BY rowid",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };

    let mut retained_rows = std::collections::BTreeMap::new();
    for (rowid, session_id, canonical) in &legacy_rows {
        let normalized = canonical_number(canonical)
            .ok_or_else(|| StorageError::InvalidCanonicalNumber(canonical.clone()))?;
        retained_rows
            .entry((session_id.clone(), normalized.clone()))
            .or_insert(*rowid);
    }

    let retained_rowids = retained_rows
        .values()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    for (rowid, _, _) in legacy_rows {
        if !retained_rowids.contains(&rowid) {
            transaction.execute("DELETE FROM photo_numbers WHERE rowid = ?1", [rowid])?;
        }
    }

    for ((_, canonical), rowid) in retained_rows {
        transaction.execute(
            "UPDATE photo_numbers SET canonical = ?1 WHERE rowid = ?2",
            params![canonical, rowid],
        )?;
    }
    transaction.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS photo_numbers_session_canonical_unique
         ON photo_numbers(session_id, canonical);",
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{PhotoNumber, ScanSnapshot};
    use std::{
        path::PathBuf,
        sync::{Arc, Barrier},
        thread,
    };

    fn confirmed_number(value: &str) -> PhotoNumber {
        PhotoNumber {
            original: value.into(),
            canonical: value.into(),
            confidence: Some(1.0),
            confirmed: true,
        }
    }

    fn copying_session_with_copying_item(db: &Storage) -> (String, String, i64) {
        let session = db.create_session("paused transaction", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
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
            skipped_numbers: vec![],
        };
        let item = CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "planned-hash",
        );
        let revision = db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                Some(&PreflightReport::default()),
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(std::slice::from_ref(&item)),
            )
            .unwrap();
        db.consume_confirmation_and_start_copy(&session.id, revision, None, false)
            .unwrap();
        db.update_copy_item(
            &item.id,
            revision,
            CopyItemStatus::Copying,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        (session.id, item.id, revision)
    }

    #[test]
    fn disconnect_pause_atomically_resets_copying_rows_and_saves_attention() {
        let db = Storage::open_in_memory().unwrap();
        let (session_id, item_id, revision) = copying_session_with_copying_item(&db);
        let preflight = PreflightReport {
            source_disconnected: true,
            ..PreflightReport::default()
        };

        db.pause_copy_for_disconnect(&session_id, revision, &preflight)
            .unwrap();

        assert_eq!(
            db.load_session(&session_id).unwrap().status,
            SessionStatus::NeedsAttention
        );
        assert!(
            db.load_preflight(&session_id)
                .unwrap()
                .unwrap()
                .source_disconnected
        );
        assert_eq!(
            db.load_copy_items(&session_id)
                .unwrap()
                .into_iter()
                .find(|item| item.id == item_id)
                .unwrap()
                .status,
            CopyItemStatus::Planned
        );
    }

    #[test]
    fn a_fresh_scan_can_claim_a_needs_attention_session() {
        let db = Storage::open_in_memory().unwrap();
        let (session_id, _, revision) = copying_session_with_copying_item(&db);
        db.pause_copy_for_disconnect(
            &session_id,
            revision,
            &PreflightReport {
                target_disconnected: true,
                ..PreflightReport::default()
            },
        )
        .unwrap();

        db.begin_scan(&session_id).unwrap();

        assert_eq!(
            db.load_session(&session_id).unwrap().status,
            SessionStatus::Scanning
        );
    }

    #[test]
    fn disconnect_pause_rolls_back_row_and_preflight_when_status_update_fails() {
        let db = Storage::open_in_memory().unwrap();
        let (session_id, item_id, revision) = copying_session_with_copying_item(&db);
        db.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_disconnect_pause
                 BEFORE UPDATE OF status ON sessions
                 WHEN NEW.status = 'needsAttention'
                 BEGIN
                   SELECT RAISE(ABORT, 'injected pause failure');
                 END;",
            )
            .unwrap();
        let preflight = PreflightReport {
            target_disconnected: true,
            ..PreflightReport::default()
        };

        assert!(db
            .pause_copy_for_disconnect(&session_id, revision, &preflight)
            .is_err());

        assert_eq!(
            db.load_session(&session_id).unwrap().status,
            SessionStatus::Copying
        );
        assert!(
            !db.load_preflight(&session_id)
                .unwrap()
                .unwrap()
                .target_disconnected
        );
        assert_eq!(
            db.load_copy_items(&session_id)
                .unwrap()
                .into_iter()
                .find(|item| item.id == item_id)
                .unwrap()
                .status,
            CopyItemStatus::Copying
        );
    }

    #[test]
    fn target_disconnect_pause_atomically_persists_pending_cleanup_and_resets_copying_row() {
        let db = Storage::open_in_memory().unwrap();
        let (session_id, item_id, revision) = copying_session_with_copying_item(&db);
        let pending = crate::copy_engine::PendingTargetPart {
            target: PathBuf::from(r"\\nas\share\target\IMG_1234.CR3"),
            part_name: ".photo-selector.part-exact".into(),
            identity: Default::default(),
        };
        let preflight = PreflightReport {
            target_disconnected: true,
            pending_target_parts: vec![pending.clone()],
            ..PreflightReport::default()
        };

        db.pause_copy_for_disconnect(&session_id, revision, &preflight)
            .unwrap();

        assert_eq!(
            db.load_session(&session_id).unwrap().status,
            SessionStatus::NeedsAttention
        );
        let saved = db.load_preflight(&session_id).unwrap().unwrap();
        assert!(saved.target_disconnected);
        assert_eq!(saved.pending_target_parts, vec![pending]);
        assert_eq!(
            db.load_copy_items(&session_id)
                .unwrap()
                .into_iter()
                .find(|item| item.id == item_id)
                .unwrap()
                .status,
            CopyItemStatus::Planned
        );
    }

    #[test]
    fn concurrent_scan_compare_and_set_has_exactly_one_winner() {
        let db = Arc::new(Storage::open_in_memory().unwrap());
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let attempts = (0..2)
            .map(|_| {
                let db = Arc::clone(&db);
                let barrier = Arc::clone(&barrier);
                let session_id = session.id.clone();
                thread::spawn(move || {
                    barrier.wait();
                    db.compare_and_set_status(
                        &session_id,
                        SessionStatus::ReadyToScan,
                        SessionStatus::Scanning,
                    )
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::Scanning
        );
    }

    #[test]
    fn confirmation_token_survives_restart_as_a_hash_and_remains_one_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let db = Storage::open(&path).unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
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
            skipped_numbers: vec![],
        };
        let planned = CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "planned",
        );
        let revision = db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[planned]),
            )
            .unwrap();
        let token = db.issue_confirmation(&session.id).unwrap();
        drop(db);

        let reopened = Storage::open(&path).unwrap();
        reopened
            .consume_confirmation_and_start_copy(&session.id, revision, Some(&token), true)
            .unwrap();
        assert_eq!(
            reopened.load_session(&session.id).unwrap().status,
            SessionStatus::Copying
        );
        assert!(matches!(
            reopened.consume_confirmation_and_start_copy(&session.id, revision, Some(&token), true),
            Err(StorageError::SessionBusy)
        ));

        let persisted = std::fs::read(&path).unwrap();
        assert!(!persisted
            .windows(token.len())
            .any(|window| window == token.as_bytes()));
    }

    #[test]
    fn editing_numbers_revokes_the_old_confirmation_token_and_plan_revision() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
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
            skipped_numbers: vec![],
        };
        let planned = CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "planned",
        );
        let revision = db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[planned]),
            )
            .unwrap();
        let token = db.issue_confirmation(&session.id).unwrap();
        let mut edited = confirmed_number("5678");
        edited.confirmed = false;

        db.save_confirmed_numbers(&session.id, &[edited]).unwrap();

        assert!(matches!(
            db.consume_confirmation_and_start_copy(&session.id, revision, Some(&token), true),
            Err(StorageError::StateConflict)
        ));
        assert!(db.load_scan_snapshot(&session.id).unwrap().is_none());
    }

    #[test]
    fn failed_scan_snapshot_save_never_leaves_session_scanning() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
            &session.id,
            SessionStatus::ReadyToScan,
            SessionStatus::Scanning,
        )
        .unwrap();
        db.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_scan_snapshot
                 BEFORE INSERT ON scan_snapshots
                 BEGIN SELECT RAISE(ABORT, 'injected scan save failure'); END;",
            )
            .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };

        assert!(db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[]),
            )
            .is_err());
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
    }

    #[test]
    fn failed_scan_status_commit_never_leaves_session_scanning() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
            &session.id,
            SessionStatus::ReadyToScan,
            SessionStatus::Scanning,
        )
        .unwrap();
        db.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_scan_status
                 BEFORE UPDATE OF status ON sessions
                 WHEN NEW.status = 'readyToCopy'
                 BEGIN SELECT RAISE(ABORT, 'injected scan status failure'); END;",
            )
            .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };

        assert!(db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[]),
            )
            .is_err());
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
        assert!(db.load_scan_snapshot(&session.id).unwrap().is_none());
    }

    #[test]
    fn ready_to_copy_is_never_committed_without_its_copy_plan() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
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
            skipped_numbers: vec![],
        };
        db.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_copy_plan
                 BEFORE INSERT ON copy_items
                 BEGIN SELECT RAISE(ABORT, 'injected plan failure'); END;",
            )
            .unwrap();

        let planned = crate::models::CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "blake3:planned",
        );
        assert!(db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::ReadyToCopy),
                Some(&[planned]),
            )
            .is_err());
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
    }

    #[test]
    fn invalidating_a_snapshot_marks_unfinished_plan_rows_superseded() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        let planned = crate::models::CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "blake3:planned",
        );
        db.save_copy_plan(&session.id, &[planned]).unwrap();

        db.save_confirmed_numbers(&session.id, &[confirmed_number("5678")])
            .unwrap();

        let status: String = db
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM copy_items WHERE session_id = ?1",
                [&session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "planSuperseded");
    }

    #[test]
    fn failed_path_invalidation_rolls_back_bindings_snapshot_and_plan_status() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.bind_paths(
            &session.id,
            Some(Path::new("/source")),
            Some(Path::new("/target")),
        )
        .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };
        db.save_scan_snapshot(&session.id, &snapshot, None).unwrap();
        let planned = CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "planned",
        );
        db.save_copy_plan(&session.id, &[planned.clone()]).unwrap();
        db.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_snapshot_invalidation
                 BEFORE DELETE ON scan_snapshots
                 BEGIN SELECT RAISE(ABORT, 'injected invalidation failure'); END;",
            )
            .unwrap();

        assert!(db
            .bind_paths(&session.id, Some(Path::new("/other")), None)
            .is_err());

        assert_eq!(
            db.load_bound_paths(&session.id).unwrap(),
            (Some("/source".into()), Some("/target".into()))
        );
        assert_eq!(db.load_scan_snapshot(&session.id).unwrap(), Some(snapshot));
        let history = db.load_copy_items(&session.id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, CopyItemStatus::Planned);
    }

    #[test]
    fn stale_worker_cannot_overwrite_a_superseded_plan_row() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        let old = crate::models::CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/old.CR3"),
            Path::new("/target/old.CR3"),
            "old",
        );
        db.save_copy_plan(&session.id, &[old.clone()]).unwrap();
        let new = crate::models::CopyHistoryItem::planned(
            &session.id,
            "5678",
            Path::new("/source/new.CR3"),
            Path::new("/target/new.CR3"),
            "new",
        );
        db.save_copy_plan(&session.id, &[new]).unwrap();

        assert!(matches!(
            db.update_copy_item(
                &old.id,
                old.plan_revision,
                CopyItemStatus::Copied,
                Some("old"),
                None,
                None,
                None,
            ),
            Err(StorageError::StateConflict)
        ));
        let status: String = db
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM copy_items WHERE id = ?1",
                [&old.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "planSuperseded");
    }

    #[test]
    fn concurrent_resolution_revision_cas_publishes_exactly_one_plan() {
        let db = Arc::new(Storage::open_in_memory().unwrap());
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.compare_and_set_status(
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
            skipped_numbers: vec![],
        };
        let revision = db
            .save_scan_result_if_status(
                &session.id,
                &snapshot,
                None,
                SessionStatus::Scanning,
                Some(SessionStatus::NeedsAttention),
                None,
            )
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let attempts = (0..2)
            .map(|attempt| {
                let db = Arc::clone(&db);
                let barrier = Arc::clone(&barrier);
                let session_id = session.id.clone();
                let snapshot = snapshot.clone();
                thread::spawn(move || {
                    let planned = CopyHistoryItem::planned(
                        &session_id,
                        "1234",
                        Path::new("/source/IMG_1234.CR3"),
                        Path::new(&format!("/target/winner-{attempt}.CR3")),
                        "planned",
                    );
                    barrier.wait();
                    db.save_resolution_snapshot(
                        &session_id,
                        &snapshot,
                        &PreflightReport::default(),
                        revision,
                        true,
                        Some(&[planned]),
                    )
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToCopy
        );
        let (_, current_revision) = db
            .load_scan_snapshot_with_revision(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(current_revision, revision + 1);
        let history = db.load_copy_items(&session.id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].plan_revision, current_revision);
        assert_eq!(history[0].status, CopyItemStatus::Planned);
    }

    #[test]
    fn copy_items_persist_full_terminal_history_across_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let db = Storage::open(&path).unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        let planned = crate::models::CopyHistoryItem::planned(
            &session.id,
            "1234",
            Path::new("/source/IMG_1234.CR3"),
            Path::new("/target/IMG_1234.CR3"),
            "blake3:planned",
        );
        db.save_copy_plan(&session.id, &[planned.clone()]).unwrap();
        db.update_copy_item(
            &planned.id,
            planned.plan_revision,
            crate::models::CopyItemStatus::Failed,
            Some("blake3:actual"),
            None,
            Some("source-changed"),
            Some("source bytes no longer match the scan snapshot"),
        )
        .unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("5678")])
            .unwrap();
        drop(db);

        let reopened = Storage::open(&path).unwrap();
        let history = reopened.load_copy_items(&session.id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].canonical_number, "1234");
        assert_eq!(history[0].planned_hash, "blake3:planned");
        assert_eq!(history[0].source_hash.as_deref(), Some("blake3:actual"));
        assert_eq!(history[0].status, crate::models::CopyItemStatus::Failed);
        assert_eq!(history[0].error_code.as_deref(), Some("source-changed"));
        assert!(history[0].error_summary.is_some());
    }

    #[test]
    fn legacy_copy_items_are_migrated_without_losing_terminal_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                   id TEXT PRIMARY KEY,
                   task_label TEXT NOT NULL,
                   note TEXT,
                   status TEXT NOT NULL,
                   source_dir TEXT,
                   target_dir TEXT,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE copy_items (
                   session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                   source_path TEXT NOT NULL,
                   target_path TEXT NOT NULL,
                   status TEXT NOT NULL,
                   source_hash TEXT,
                   error_code TEXT,
                   PRIMARY KEY(session_id, source_path, target_path)
                 );
                 INSERT INTO sessions(
                   id, task_label, status, created_at, updated_at
                 ) VALUES ('legacy', 'old task', 'failed', 'now', 'now');
                 INSERT INTO copy_items(
                   session_id, source_path, target_path, status, source_hash, error_code
                 ) VALUES (
                   'legacy', '/source/old.jpg', '/target/old.jpg',
                   'failed', 'old-hash', 'old-error'
                 );",
            )
            .unwrap();

        let db = Storage::from_connection(connection).unwrap();
        let history = db.load_copy_items("legacy").unwrap();

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, CopyItemStatus::Failed);
        assert_eq!(history[0].source_hash.as_deref(), Some("old-hash"));
        assert_eq!(history[0].error_code.as_deref(), Some("old-error"));
    }

    #[test]
    fn saving_confirmed_numbers_invalidates_every_scan_artifact() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };
        db.save_scan_snapshot(&session.id, &snapshot, None).unwrap();

        db.save_confirmed_numbers(&session.id, &[confirmed_number("5678")])
            .unwrap();

        assert!(db.load_scan_snapshot(&session.id).unwrap().is_none());
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
    }

    #[test]
    fn saving_an_unconfirmed_number_edit_persists_it_and_invalidates_copy_authority() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };
        db.save_scan_snapshot(&session.id, &snapshot, None).unwrap();
        let mut edited = confirmed_number("5678");
        edited.confirmed = false;

        db.save_confirmed_numbers(&session.id, &[edited]).unwrap();

        assert_eq!(db.load_numbers(&session.id).unwrap()[0].canonical, "5678");
        assert!(!db.load_numbers(&session.id).unwrap()[0].confirmed);
        assert!(db.load_scan_snapshot(&session.id).unwrap().is_none());
        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::AwaitingNumberConfirmation
        );
    }

    #[test]
    fn session_input_metadata_round_trips_with_safety_identities() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        let input = SessionInput {
            id: "input-1".into(),
            session_id: session.id.clone(),
            name: "客户截图.png".into(),
            kind: "image".into(),
            mime: "image/png".into(),
            stored_name: "opaque-file-name".into(),
            size: 8,
            root_identity: crate::models::FileIdentity {
                device: 11,
                file_index: 12,
            },
            fingerprint: crate::models::FileFingerprint {
                identity: crate::models::FileIdentity {
                    device: 21,
                    file_index: 22,
                },
                size: 8,
                modified_ms: 23,
                content_hash: "hash".into(),
            },
            created_at: "now".into(),
        };

        db.save_session_input(&input).unwrap();
        let loaded = db.load_session_input(&session.id, "input-1").unwrap();

        assert_eq!(loaded, input);
        assert_eq!(db.list_session_inputs(&session.id).unwrap(), vec![input]);
    }

    #[test]
    fn scan_snapshot_and_bound_paths_survive_storage_reopen_semantics() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        db.bind_paths(
            &session.id,
            Some(Path::new("/source")),
            Some(Path::new("/target")),
        )
        .unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };
        db.save_scan_snapshot(&session.id, &snapshot, None).unwrap();

        assert_eq!(
            db.load_bound_paths(&session.id).unwrap(),
            (Some("/source".into()), Some("/target".into()))
        );
        assert_eq!(db.load_scan_snapshot(&session.id).unwrap(), Some(snapshot));
    }

    #[test]
    fn interrupted_work_is_reopened_at_a_rescannable_state_without_losing_snapshot() {
        let db = Storage::open_in_memory().unwrap();
        let mut session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        session = db.load_session(&session.id).unwrap();
        session.status = SessionStatus::Copying;
        db.save_session(&session).unwrap();
        let snapshot = ScanSnapshot {
            source: "/source".into(),
            source_root_identity: Default::default(),
            target: "/target".into(),
            items: vec![],
            skipped_numbers: vec![],
        };
        db.save_scan_snapshot(&session.id, &snapshot, None).unwrap();

        db.recover_interrupted_sessions().unwrap();

        assert_eq!(
            db.load_session(&session.id).unwrap().status,
            SessionStatus::ReadyToScan
        );
        assert_eq!(db.load_scan_snapshot(&session.id).unwrap(), Some(snapshot));
    }

    #[test]
    fn confirmed_numbers_cannot_change_while_copy_uses_the_snapshot() {
        let db = Storage::open_in_memory().unwrap();
        let mut session = db.create_session("BD-240718", None).unwrap();
        db.save_confirmed_numbers(&session.id, &[confirmed_number("1234")])
            .unwrap();
        session = db.load_session(&session.id).unwrap();
        session.status = SessionStatus::Copying;
        db.save_session(&session).unwrap();

        assert!(matches!(
            db.save_confirmed_numbers(&session.id, &[confirmed_number("5678")]),
            Err(StorageError::SessionBusy)
        ));
        assert!(matches!(
            db.bind_paths(&session.id, Some(Path::new("/other")), None),
            Err(StorageError::SessionBusy)
        ));
        assert_eq!(db.load_numbers(&session.id).unwrap()[0].canonical, "1234");
    }

    #[test]
    fn session_requires_nonblank_label_and_round_trips() {
        let db = Storage::open_in_memory().unwrap();
        assert!(db.create_session("   ", None).is_err());
        let created = db.create_session("BD-240718", Some("亲子写真")).unwrap();
        db.save_confirmed_numbers(
            &created.id,
            &[PhotoNumber {
                original: "01234".into(),
                canonical: "1234".into(),
                confidence: Some(0.91),
                confirmed: true,
            }],
        )
        .unwrap();
        let loaded = db.load_numbers(&created.id).unwrap();
        assert_eq!(created.task_label, "BD-240718");
        assert_eq!(loaded[0].canonical, "1234");
    }

    #[test]
    fn rejects_duplicate_or_invalid_canonical_numbers() {
        let db = Storage::open_in_memory().unwrap();
        let session = db.create_session("BD-240718", None).unwrap();
        let duplicate = [
            PhotoNumber {
                original: "01234".into(),
                canonical: "1234".into(),
                confidence: None,
                confirmed: true,
            },
            PhotoNumber {
                original: "1234".into(),
                canonical: "1234".into(),
                confidence: None,
                confirmed: true,
            },
        ];

        assert!(matches!(
            db.save_confirmed_numbers(&session.id, &duplicate),
            Err(StorageError::DuplicateCanonicalNumber(value)) if value == "1234"
        ));
        assert!(matches!(
            db.save_confirmed_numbers(
                &session.id,
                &[PhotoNumber {
                    original: "bad".into(),
                    canonical: "not-a-number".into(),
                    confidence: None,
                    confirmed: true,
                }],
            ),
            Err(StorageError::InvalidCanonicalNumber(_))
        ));
    }

    #[test]
    fn cloud_settings_reject_secret_fields_and_only_persists_a_reference() {
        for forbidden_key in ["apiKey", "secret", "token", "password"] {
            let value = serde_json::json!({
                "provider": "example",
                "secretRef": "profile-1",
                forbidden_key: "must-not-be-persisted"
            });
            assert!(serde_json::from_value::<CloudSettings>(value).is_err());
        }

        let db = Storage::open_in_memory().unwrap();
        let settings = CloudSettings {
            provider: "example".into(),
            endpoint: Some("https://api.example.invalid".into()),
            model: Some("vision".into()),
            secret_ref: "profile-1".into(),
        };
        db.save_cloud_settings("profile-1", &settings).unwrap();

        assert_eq!(db.load_cloud_settings("profile-1").unwrap(), Some(settings));
    }

    #[test]
    fn provider_and_default_settings_are_saved_in_one_transaction() {
        use crate::models::{AddressMode, ApiFormat};

        let db = Storage::open_in_memory().unwrap();
        let profile = |id: &str| ProviderProfile {
            id: id.into(),
            name: id.into(),
            template: "custom".into(),
            address: "https://api.example.com/v1".into(),
            address_mode: AddressMode::BaseUrl,
            api_format: ApiFormat::Responses,
            model: "vision".into(),
            fallback_model: None,
            timeout_seconds: 30,
            enabled: true,
            is_default: true,
            secret_ref: id.into(),
        };
        db.save_provider_and_settings(
            &profile("first"),
            &serde_json::json!({"defaultProviderId":"first"}),
        )
        .unwrap();
        db.save_provider_and_settings(
            &profile("second"),
            &serde_json::json!({"defaultProviderId":"second"}),
        )
        .unwrap();

        let providers = db.list_providers().unwrap();
        assert_eq!(providers.iter().filter(|value| value.is_default).count(), 1);
        assert!(providers
            .iter()
            .any(|value| value.id == "second" && value.is_default));
        assert_eq!(
            db.load_setting::<serde_json::Value>("app-settings")
                .unwrap()
                .unwrap()["defaultProviderId"],
            "second"
        );
    }

    #[test]
    fn migration_normalizes_and_deduplicates_leading_zero_legacy_fixture() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    task_label TEXT NOT NULL,
                    note TEXT,
                    status TEXT NOT NULL,
                    source_dir TEXT,
                    target_dir TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE photo_numbers (
                    session_id TEXT NOT NULL,
                    original TEXT NOT NULL,
                    canonical TEXT NOT NULL,
                    confidence REAL,
                    confirmed INTEGER NOT NULL,
                    PRIMARY KEY(session_id, original)
                );
                INSERT INTO sessions (id, task_label, status, created_at, updated_at)
                VALUES
                    ('legacy-session', 'legacy', 'draft', 'now', 'now'),
                    ('other-session', 'other', 'draft', 'now', 'now');
                INSERT INTO photo_numbers (session_id, original, canonical, confirmed)
                VALUES
                    ('legacy-session', '01234', '01234', 1),
                    ('legacy-session', '1234', '1234', 1),
                    ('other-session', '001234', '001234', 1);",
            )
            .unwrap();

        let db = Storage::from_connection(connection).unwrap();
        let numbers = db.load_numbers("legacy-session").unwrap();

        assert_eq!(numbers.len(), 1);
        assert_eq!(numbers[0].original, "01234");
        assert_eq!(numbers[0].canonical, "1234");
        assert_eq!(
            db.load_numbers("other-session").unwrap()[0].canonical,
            "1234"
        );
        let duplicate_insert = db.connection.lock().unwrap().execute(
            "INSERT INTO photo_numbers (session_id, original, canonical, confirmed)
                 VALUES (?1, ?2, ?3, 1)",
            params!["legacy-session", "another-original", "1234"],
        );
        assert!(duplicate_insert.is_err());
    }
}
