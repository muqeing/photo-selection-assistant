use std::{collections::HashSet, path::PathBuf, process::ExitCode};

use rusqlite::{Connection, OpenFlags};

fn main() -> ExitCode {
    let Some(app_data) = std::env::var_os("APPDATA") else {
        print_failure("probe-app-data", None);
        return ExitCode::from(2);
    };
    let database = PathBuf::from(app_data)
        .join("top.muliai.photo-selection-assistant")
        .join("state.sqlite3");
    let Ok(connection) = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        print_failure("probe-database-open", None);
        return ExitCode::from(2);
    };
    let session = connection.query_row(
        "SELECT id, source_dir
         FROM sessions
         WHERE source_dir IS NOT NULL
           AND status IN ('readyToScan', 'scanning', 'needsAttention')
         ORDER BY updated_at DESC
         LIMIT 1",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    );
    let Ok((session_id, source)) = session else {
        print_failure("probe-session-read", None);
        return ExitCode::from(2);
    };
    let Ok(mut statement) =
        connection.prepare("SELECT canonical FROM photo_numbers WHERE session_id = ?1")
    else {
        print_failure("probe-number-read", None);
        return ExitCode::from(2);
    };
    let Ok(rows) = statement.query_map([session_id], |row| row.get::<_, String>(0)) else {
        print_failure("probe-number-read", None);
        return ExitCode::from(2);
    };
    let Ok(wanted_numbers) = rows.collect::<Result<HashSet<_>, _>>() else {
        print_failure("probe-number-read", None);
        return ExitCode::from(2);
    };
    if wanted_numbers.is_empty() {
        print_failure("probe-number-read", None);
        return ExitCode::from(2);
    }

    let report =
        app_lib::path_free_scan_probe_for_numbers(PathBuf::from(source).as_path(), &wanted_numbers);
    println!(
        "{}",
        serde_json::json!({
            "outcome": report.outcome,
            "file_count": report.file_count,
            "stage": report.stage,
            "windows_code": report.windows_code,
        })
    );
    if report.outcome == "succeeded" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_failure(stage: &str, windows_code: Option<i32>) {
    println!(
        "{}",
        serde_json::json!({
            "outcome": "failed",
            "file_count": 0,
            "stage": stage,
            "windows_code": windows_code,
        })
    );
}
