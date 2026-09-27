use std::path::Path;

use crate::network_paths::{self, PreparedDirectoryKind};

pub const LOG_TARGET: &str = "photo_selection_diagnostics";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticPathKind {
    Local,
    MappedDrive,
    Unc,
}

impl DiagnosticPathKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::MappedDrive => "mapped-drive",
            Self::Unc => "unc",
        }
    }
}

impl From<PreparedDirectoryKind> for DiagnosticPathKind {
    fn from(value: PreparedDirectoryKind) -> Self {
        match value {
            PreparedDirectoryKind::Local => Self::Local,
            PreparedDirectoryKind::MappedDrive => Self::MappedDrive,
            PreparedDirectoryKind::Unc => Self::Unc,
        }
    }
}

pub fn anonymized_network_target(path: &Path) -> Option<String> {
    let share_root = network_paths::unc_share_root(path)?;
    let normalized = share_root.to_string_lossy().to_ascii_lowercase();
    let digest = blake3::hash(normalized.as_bytes()).to_hex().to_string();
    Some(format!("net_{}", &digest[..16]))
}

pub fn safe_operation_event(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    windows_code: Option<u32>,
    outcome: &str,
) -> String {
    format_event(session_id, stage, path_kind, None, windows_code, outcome)
}

pub fn safe_network_operation_event(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    network_target: Option<&Path>,
    windows_code: Option<u32>,
    outcome: &str,
) -> String {
    let target_id = network_target.and_then(anonymized_network_target);
    format_event(
        session_id,
        stage,
        path_kind,
        target_id.as_deref(),
        windows_code,
        outcome,
    )
}

pub fn info_operation(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    windows_code: Option<u32>,
    outcome: &str,
) {
    log::info!(
        target: LOG_TARGET,
        "{}",
        safe_operation_event(session_id, stage, path_kind, windows_code, outcome)
    );
}

pub fn warn_operation(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    windows_code: Option<u32>,
    outcome: &str,
) {
    log::warn!(
        target: LOG_TARGET,
        "{}",
        safe_operation_event(session_id, stage, path_kind, windows_code, outcome)
    );
}

pub fn info_network_operation(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    network_target: Option<&Path>,
    windows_code: Option<u32>,
    outcome: &str,
) {
    log::info!(
        target: LOG_TARGET,
        "{}",
        safe_network_operation_event(
            session_id,
            stage,
            path_kind,
            network_target,
            windows_code,
            outcome,
        )
    );
}

pub fn warn_network_operation(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    network_target: Option<&Path>,
    windows_code: Option<u32>,
    outcome: &str,
) {
    log::warn!(
        target: LOG_TARGET,
        "{}",
        safe_network_operation_event(
            session_id,
            stage,
            path_kind,
            network_target,
            windows_code,
            outcome,
        )
    );
}

fn format_event(
    session_id: Option<&str>,
    stage: &str,
    path_kind: DiagnosticPathKind,
    target_id: Option<&str>,
    windows_code: Option<u32>,
    outcome: &str,
) -> String {
    let session_id = session_id
        .filter(|value| uuid::Uuid::parse_str(value).is_ok())
        .unwrap_or("none");
    let stage = known_stage(stage);
    let target_id = target_id.unwrap_or("none");
    let windows_code = windows_code
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".into());
    let outcome = known_outcome(outcome);
    format!(
        "event=operation session_id={session_id} stage={stage} path_kind={} target_id={target_id} windows_code={windows_code} outcome={outcome}",
        path_kind.as_str()
    )
}

fn known_stage(stage: &str) -> &'static str {
    match stage {
        "app-lifecycle" => "app-lifecycle",
        "directory-prepare" => "directory-prepare",
        "scan-prepare" => "scan-prepare",
        "scan-root-canonicalize" => "scan-root-canonicalize",
        "scan-root-open" => "scan-root-open",
        "scan-root-identity" => "scan-root-identity",
        "scan-directory-enumerate" => "scan-directory-enumerate",
        "scan-entry-read" => "scan-entry-read",
        "scan-entry-type" => "scan-entry-type",
        "scan-child-directory-open" => "scan-child-directory-open",
        "scan-file-open" => "scan-file-open",
        "scan-file-metadata" => "scan-file-metadata",
        "scan-file-hash" => "scan-file-hash",
        "scan-file-identity" => "scan-file-identity",
        "copy-prepare" => "copy-prepare",
        "copy-source-prepare" => "copy-source-prepare",
        "copy-target-prepare" => "copy-target-prepare",
        "copy-source-recheck" => "copy-source-recheck",
        "copy-target-recheck" => "copy-target-recheck",
        "copy-disconnect-pause" => "copy-disconnect-pause",
        "screenshot-save" => "screenshot-save",
        "recognition-provider" => "recognition-provider",
        _ => "unknown",
    }
}

fn known_outcome(outcome: &str) -> &'static str {
    match outcome {
        "started" => "started",
        "succeeded" => "succeeded",
        "failed" => "failed",
        "paused" => "paused",
        "cancelled" => "cancelled",
        "fallback" => "fallback",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        anonymized_network_target, safe_network_operation_event, safe_operation_event,
        DiagnosticPathKind,
    };

    const INTERNAL_SESSION_ID: &str = "2d8f1c36-0c5d-4ed1-8a07-8df63af0b720";

    #[test]
    fn network_target_is_a_short_stable_digest_of_only_the_unc_share_root() {
        let first = anonymized_network_target(Path::new(
            r"\\SERVER_SECRET\SHARE_SECRET\USER_SECRET\PHOTO_SECRET_00123.jpg",
        ))
        .expect("UNC paths have a target identifier");
        let second = anonymized_network_target(Path::new(
            r"\\server_secret\share_secret\OTHER_SUBDIR\OTHER_FILE.raw",
        ))
        .expect("UNC paths have a target identifier");

        assert_eq!(first, second);
        assert!(first.starts_with("net_"));
        assert_eq!(first.len(), "net_".len() + 16);
        assert!(first["net_".len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
    }

    #[test]
    fn operation_event_never_contains_path_or_customer_secret_markers() {
        let path = Path::new(
            r"\\SERVER_SECRET\SHARE_SECRET\USER_SECRET\SUBDIR_SECRET\FILE_SECRET_PHOTO_00123.jpg",
        );
        let event = safe_network_operation_event(
            Some(INTERNAL_SESSION_ID),
            "scan-prepare",
            DiagnosticPathKind::Unc,
            Some(path),
            Some(53),
            "failed",
        );

        for marker in [
            "SERVER_SECRET",
            "SHARE_SECRET",
            "USER_SECRET",
            "SUBDIR_SECRET",
            "FILE_SECRET",
            "PHOTO",
            "00123",
        ] {
            assert!(
                !event.contains(marker),
                "diagnostic event leaked marker {marker}: {event}"
            );
        }
        assert!(event.contains("session_id=2d8f1c36-0c5d-4ed1-8a07-8df63af0b720"));
        assert!(event.contains("stage=scan-prepare"));
        assert!(event.contains("path_kind=unc"));
        assert!(event.contains("target_id=net_"));
        assert!(event.contains("windows_code=53"));
        assert!(event.contains("outcome=failed"));
    }

    #[test]
    fn runtime_kind_keeps_a_local_drive_local_and_a_resolved_mapping_mapped() {
        let local = safe_network_operation_event(
            Some(INTERNAL_SESSION_ID),
            "directory-prepare",
            DiagnosticPathKind::Local,
            None,
            None,
            "succeeded",
        );
        let mapped = safe_network_operation_event(
            Some(INTERNAL_SESSION_ID),
            "directory-prepare",
            DiagnosticPathKind::MappedDrive,
            Some(Path::new(r"\\SERVER_SECRET\SHARE_SECRET\job")),
            None,
            "succeeded",
        );

        assert!(local.contains("path_kind=local"));
        assert!(local.contains("target_id=none"));
        assert!(!local.contains("mapped-drive"));
        assert!(mapped.contains("path_kind=mapped-drive"));
        assert!(mapped.contains("target_id=net_"));
        assert!(!mapped.contains("SERVER_SECRET"));
        assert!(!mapped.contains("SHARE_SECRET"));
    }

    #[test]
    fn only_internal_uuid_session_ids_and_known_labels_are_emitted() {
        let event = safe_operation_event(
            Some("ORDER_SECRET_20260725"),
            "stage-with-SECRET",
            DiagnosticPathKind::MappedDrive,
            None,
            "outcome-with-SECRET",
        );

        assert!(!event.contains("ORDER_SECRET"));
        assert!(!event.contains("stage-with-SECRET"));
        assert!(!event.contains("outcome-with-SECRET"));
        assert!(event.contains("session_id=none"));
        assert!(event.contains("stage=unknown"));
        assert!(event.contains("path_kind=mapped-drive"));
        assert!(event.contains("target_id=none"));
        assert!(event.contains("windows_code=none"));
        assert!(event.contains("outcome=unknown"));
    }

    #[test]
    fn copy_reconnect_stages_are_fixed_and_arbitrary_stages_remain_unknown() {
        for stage in [
            "copy-source-prepare",
            "copy-target-prepare",
            "copy-source-recheck",
            "copy-target-recheck",
            "copy-disconnect-pause",
        ] {
            let event = safe_network_operation_event(
                Some(INTERNAL_SESSION_ID),
                stage,
                DiagnosticPathKind::Unc,
                Some(Path::new(r"\\SERVER_SECRET\SHARE_SECRET\job")),
                Some(64),
                "paused",
            );
            assert!(event.contains(&format!("stage={stage}")), "{event}");
            assert!(event.contains("windows_code=64"), "{event}");
            assert!(!event.contains("stage=unknown"), "{event}");
        }

        let arbitrary = safe_operation_event(
            Some(INTERNAL_SESSION_ID),
            "copy-target-user-controlled",
            DiagnosticPathKind::Unc,
            Some(64),
            "paused",
        );
        assert!(arbitrary.contains("stage=unknown"));
    }

    #[test]
    fn scan_failure_stages_remain_machine_readable_without_path_details() {
        for stage in [
            "scan-root-canonicalize",
            "scan-root-open",
            "scan-root-identity",
            "scan-directory-enumerate",
            "scan-entry-read",
            "scan-entry-type",
            "scan-child-directory-open",
            "scan-file-open",
            "scan-file-metadata",
            "scan-file-hash",
            "scan-file-identity",
        ] {
            let event =
                safe_operation_event(None, stage, DiagnosticPathKind::Unc, Some(3), "failed");
            assert!(
                event.contains(&format!("stage={stage} ")),
                "scan stage was discarded: {event}"
            );
            assert!(event.contains("windows_code=3"));
            assert!(!event.contains('\\'));
            assert!(!event.contains('/'));
        }
    }

    #[test]
    fn local_and_drive_paths_do_not_create_network_target_identifiers() {
        assert_eq!(
            anonymized_network_target(Path::new("/Users/USER_SECRET/PHOTO_SECRET.jpg")),
            None
        );
        assert_eq!(
            anonymized_network_target(Path::new(r"Z:\USER_SECRET\PHOTO_SECRET.jpg")),
            None
        );
    }
}
