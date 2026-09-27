mod commands;
mod copy_engine;
mod diagnostics;
mod matching;
mod models;
pub mod network_paths;
mod numbers;
mod providers;
pub mod secrets;
pub mod storage;

use tauri::Manager;
use tauri_plugin_log::{RotationStrategy, Target, TargetKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFreeScanProbeReport {
    pub outcome: &'static str,
    pub file_count: usize,
    pub stage: Option<&'static str>,
    pub windows_code: Option<i32>,
}

pub fn path_free_scan_probe(source: &std::path::Path) -> PathFreeScanProbeReport {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    path_free_report(matching::scan_source_index_cancellable(
        source,
        None,
        &matching::default_extensions(),
        &cancelled,
    ))
}

pub fn path_free_scan_probe_for_numbers(
    source: &std::path::Path,
    wanted_numbers: &std::collections::HashSet<String>,
) -> PathFreeScanProbeReport {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    path_free_report(matching::scan_source_index_for_numbers_cancellable(
        source,
        None,
        &matching::default_extensions(),
        wanted_numbers,
        &cancelled,
    ))
}

fn path_free_report(
    result: Result<models::SourceIndex, matching::MatchError>,
) -> PathFreeScanProbeReport {
    match result {
        Ok(index) => PathFreeScanProbeReport {
            outcome: "succeeded",
            file_count: index.files.len(),
            stage: None,
            windows_code: None,
        },
        Err(error) => PathFreeScanProbeReport {
            outcome: "failed",
            file_count: 0,
            stage: error.diagnostic_stage().or(Some("scan-unclassified")),
            windows_code: error.raw_os_error(),
        },
    }
}

#[cfg(test)]
mod scan_probe_tests {
    #[test]
    fn path_free_probe_reports_a_fixed_stage_without_returning_the_source_path() {
        let missing = tempfile::tempdir()
            .unwrap()
            .path()
            .join("SECRET_CUSTOMER_SOURCE");
        let report = super::path_free_scan_probe(&missing);

        assert_eq!(report.outcome, "failed");
        assert_eq!(report.stage, Some("scan-root-canonicalize"));
        assert_eq!(report.file_count, 0);
        assert!(!format!("{report:?}").contains("SECRET_CUSTOMER_SOURCE"));
    }

    #[test]
    fn path_free_probe_can_scan_only_confirmed_numbers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("IMG_0007.JPG"), b"wanted").unwrap();
        std::fs::write(root.path().join("IMG_0099.JPG"), b"unrelated").unwrap();

        let report =
            super::path_free_scan_probe_for_numbers(root.path(), &["7".to_string()].into());

        assert_eq!(report.outcome, "succeeded");
        assert_eq!(report.file_count, 1);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut log_targets = vec![Target::new(TargetKind::LogDir {
        file_name: Some("photo-selection-assistant".into()),
    })
    .filter(|metadata| metadata.target() == diagnostics::LOG_TARGET)];
    if cfg!(debug_assertions) {
        log_targets.push(
            Target::new(TargetKind::Stdout)
                .filter(|metadata| metadata.target() == diagnostics::LOG_TARGET),
        );
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .targets(log_targets)
                .max_file_size(1_000_000)
                .rotation_strategy(RotationStrategy::KeepSome(5))
                .build(),
        )
        .setup(|app| {
            app.manage(commands::AppState::open(app.handle()).expect("无法初始化本地状态"));
            diagnostics::info_operation(
                None,
                "app-lifecycle",
                diagnostics::DiagnosticPathKind::Local,
                None,
                "started",
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_session,
            commands::list_sessions,
            commands::load_settings,
            commands::list_providers,
            commands::provider_templates,
            commands::open_session,
            commands::save_session_input,
            commands::list_session_inputs,
            commands::read_session_input,
            commands::save_confirmed_numbers,
            commands::choose_directory,
            commands::bind_manual_directory,
            commands::default_target_for_source,
            commands::target_under_selected_base,
            commands::scan_and_match,
            commands::resolve_match,
            commands::resolve_ambiguous_match,
            commands::read_candidate_preview,
            commands::start_copy,
            commands::cancel_copy,
            commands::recheck_copy,
            commands::cancel_session,
            commands::save_settings,
            commands::save_provider,
            commands::delete_provider,
            commands::test_provider,
            commands::test_provider_draft,
            commands::list_provider_models,
            commands::recognize_cloud,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
