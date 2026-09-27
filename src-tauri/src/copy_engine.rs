#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs, sync::atomic::AtomicBool};
    #[cfg(not(target_os = "macos"))]
    use tempfile::tempdir;
    use tempfile::TempDir;

    fn test_tempdir() -> TempDir {
        #[cfg(target_os = "macos")]
        {
            return tempfile::tempdir_in("/private/tmp").unwrap();
        }
        #[cfg(not(target_os = "macos"))]
        tempdir().unwrap()
    }

    fn selected(source: PathBuf, target: PathBuf) -> SelectedFile {
        let mut fingerprint = source_fingerprint(&open_source(&source).unwrap()).unwrap();
        fingerprint.content_hash = hash_file(&source).unwrap().to_hex().to_string();
        SelectedFile {
            canonical_number: String::new(),
            source,
            target,
            fingerprint,
        }
    }

    fn copy_plan(source_root: &Path, target_root: PathBuf, files: Vec<SelectedFile>) -> CopyPlan {
        let root = Dir::open_ambient_dir(source_root, ambient_authority()).unwrap();
        let source_root_identity = source_fingerprint(&root.into_std_file()).unwrap().identity;
        CopyPlan {
            source_root: source_root.to_path_buf(),
            source_root_identity,
            target_root,
            files,
        }
    }

    #[test]
    fn identical_target_is_skipped_and_different_target_blocks() {
        let dir = test_tempdir();
        let source = dir.path().join("source.CR3");
        let target = dir.path().join("target.CR3");
        fs::write(&source, b"same").unwrap();
        fs::write(&target, b"same").unwrap();
        assert_eq!(
            compare_existing(&source, &target).unwrap(),
            TargetConflict::Identical
        );

        fs::write(&target, b"different").unwrap();
        assert_eq!(
            compare_existing(&source, &target).unwrap(),
            TargetConflict::Different
        );
    }

    #[test]
    fn successful_copy_has_same_hash_and_no_part_file() {
        let dir = test_tempdir();
        let source = dir.path().join("IMG_1.JPG");
        let target = dir.path().join("out/IMG_1.JPG");
        fs::write(&source, b"photo-bytes").unwrap();
        copy_one(&source, &target, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(hash_file(&source).unwrap(), hash_file(&target).unwrap());
        assert!(fs::read_dir(target.parent().unwrap()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".part-")));
    }

    #[test]
    fn copying_does_not_require_a_one_megabyte_thread_stack() {
        let directory = test_tempdir();
        let source = directory.path().join("source.ARW");
        let target = directory.path().join("output/target.ARW");
        fs::write(&source, vec![7_u8; 2 * 1024 * 1024]).unwrap();

        let copied = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || copy_one(&source, &target, &AtomicBool::new(false), |_| {}))
            .unwrap()
            .join()
            .expect("copying must not overflow a small Windows-sized stack");

        copied.unwrap();
    }

    #[test]
    fn filename_only_preflight_does_not_hash_source_contents() {
        use crate::matching::{default_extensions, scan_source_index_for_numbers_cancellable};

        let root = test_tempdir();
        let source = root.path().join("IMG_0007.JPG");
        let target_root = root.path().join("output");
        fs::write(&source, b"photo bytes").unwrap();
        fs::create_dir(&target_root).unwrap();
        let index = scan_source_index_for_numbers_cancellable(
            root.path(),
            Some(&target_root),
            &default_extensions(),
            &["7".to_string()].into(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let indexed = index.files[0].clone();
        assert!(indexed.fingerprint.content_hash.is_empty());
        let plan = CopyPlan {
            source_root: index.root,
            source_root_identity: index.root_identity,
            target_root: target_root.clone(),
            files: vec![SelectedFile {
                canonical_number: "7".into(),
                source: indexed.path,
                target: target_root.join("IMG_0007.JPG"),
                fingerprint: indexed.fingerprint,
            }],
        };
        let hash_calls = Cell::new(0);

        let report = preflight_copy_cancellable_using(&plan, &AtomicBool::new(false), |_, _, _| {
            hash_calls.set(hash_calls.get() + 1);
            Ok(blake3::hash(b"must not be called"))
        })
        .unwrap();

        assert_eq!(hash_calls.get(), 0);
        assert!(!report.source_changed);
    }

    #[test]
    fn filename_only_plan_is_verified_after_copy() {
        use crate::matching::{default_extensions, scan_source_index_for_numbers_cancellable};

        let root = test_tempdir();
        let source = root.path().join("IMG_0007.JPG");
        let target_root = root.path().join("output");
        let target = target_root.join("IMG_0007.JPG");
        fs::write(&source, b"photo bytes").unwrap();
        let index = scan_source_index_for_numbers_cancellable(
            root.path(),
            Some(&target_root),
            &default_extensions(),
            &["7".to_string()].into(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let indexed = index.files[0].clone();
        let plan = CopyPlan {
            source_root: index.root,
            source_root_identity: index.root_identity,
            target_root,
            files: vec![SelectedFile {
                canonical_number: "7".into(),
                source: indexed.path,
                target: target.clone(),
                fingerprint: indexed.fingerprint,
            }],
        };

        let report =
            execute_copy_with_updates(&plan, &AtomicBool::new(false), |_, _| {}, |_, _| {});

        assert_eq!(report.copied.len(), 1);
        assert_eq!(hash_file(&source).unwrap(), hash_file(&target).unwrap());
    }

    #[test]
    fn cancellation_is_checked_before_post_copy_hash_reads() {
        let dir = test_tempdir();
        let source = dir.path().join("IMG_1.JPG");
        let target = dir.path().join("out/IMG_1.JPG");
        let bytes = vec![7_u8; 2 * 1024 * 1024];
        fs::write(&source, &bytes).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut copied = 0_u64;

        let result = copy_one(&source, &target, &cancelled, |count| {
            copied += count;
            if copied == bytes.len() as u64 {
                cancelled.store(true, Ordering::Relaxed);
            }
        });

        assert!(matches!(result, Err(CopyError::Cancelled)));
        assert!(!target.exists());
        assert_no_part_files(target.parent().unwrap());
    }

    #[test]
    fn windows_disconnect_codes_are_machine_readable_only_for_unc_source_operations() {
        for code in [3, 53, 64, 67, 121, 1231, 2250] {
            assert!(
                matches!(
                    map_source_open_error(std::io::Error::from_raw_os_error(code), true),
                    CopyError::SourceDisconnected
                ),
                "Windows error {code} was not classified as a source disconnect"
            );
            assert!(
                !matches!(
                    map_source_open_error(std::io::Error::from_raw_os_error(code), false),
                    CopyError::SourceDisconnected
                ),
                "local source error {code} was reclassified as a disconnect"
            );
        }
        assert!(matches!(
            map_source_open_error(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                true,
            ),
            CopyError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn windows_disconnect_codes_are_machine_readable_only_for_unc_target_operations() {
        for code in [3, 53, 64, 67, 121, 1231, 2250] {
            assert!(
                matches!(
                    map_target_io_error(
                        std::io::Error::from_raw_os_error(code),
                        true,
                        TargetOperation::OpenTarget,
                    ),
                    CopyError::TargetDisconnected {
                        info: TargetDisconnectInfo {
                            windows_code: Some(actual),
                            operation: TargetOperation::OpenTarget,
                        },
                        pending_part: None,
                    } if actual == code as u32
                ),
                "Windows error {code} was not classified as a target disconnect"
            );
            assert!(
                !matches!(
                    map_target_io_error(
                        std::io::Error::from_raw_os_error(code),
                        false,
                        TargetOperation::OpenTarget,
                    ),
                    CopyError::TargetDisconnected { .. }
                ),
                "local target error {code} was reclassified as a disconnect"
            );
        }
        assert!(matches!(
            map_target_io_error(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
                true,
                TargetOperation::OpenTarget,
            ),
            CopyError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn source_disconnect_cleans_current_part_stops_later_items_and_keeps_commits() {
        struct DisconnectingReader {
            emitted: bool,
        }

        impl Read for DisconnectingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.emitted {
                    self.emitted = true;
                    buffer[..7].copy_from_slice(b"partial");
                    return Ok(7);
                }
                Err(std::io::Error::from_raw_os_error(53))
            }
        }

        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        let mut files = Vec::new();
        for (canonical, contents) in [
            ("committed", b"first".as_slice()),
            ("disconnect", b"second".as_slice()),
            ("later", b"third".as_slice()),
        ] {
            let source = source_root.join(format!("{canonical}.jpg"));
            fs::write(&source, contents).unwrap();
            files.push(SelectedFile {
                canonical_number: canonical.into(),
                target: target_root.join(format!("{canonical}.jpg")),
                ..selected(source, PathBuf::new())
            });
        }
        let plan = copy_plan(&source_root, target_root.clone(), files);
        let mut later_attempted = false;
        let mut failed_outcomes = Vec::new();

        let report = execute_copy_with_updates_using(
            &plan,
            &AtomicBool::new(false),
            |_, _| {},
            |file, outcome| {
                if let CopyItemOutcome::Failed { code, .. } = outcome {
                    failed_outcomes.push((file.canonical_number.clone(), code));
                }
            },
            |source_file, item, target, source_is_unc, cancelled, progress| match item
                .canonical_number
                .as_str()
            {
                "committed" => copy_one_at(
                    source_file,
                    &item.fingerprint,
                    target,
                    source_is_unc,
                    cancelled,
                    progress,
                ),
                "disconnect" => {
                    let mut part = PendingPart::create(&target.parent)?;
                    let mut source = DisconnectingReader { emitted: false };
                    let result = copy_stream_to(
                        &mut source,
                        part.file_mut(),
                        true,
                        false,
                        cancelled,
                        progress,
                    );
                    part.finish(result.map(|_| ())).map(|_| String::new())
                }
                "later" => {
                    later_attempted = true;
                    Ok(String::new())
                }
                _ => unreachable!(),
            },
        );

        assert_eq!(report.stop_reason, Some(CopyStopReason::SourceDisconnected));
        assert_eq!(report.copied.len(), 1);
        assert!(report.failed.is_empty());
        assert!(failed_outcomes.is_empty());
        assert!(target_root.join("committed.jpg").exists());
        assert!(!target_root.join("disconnect.jpg").exists());
        assert!(!later_attempted);
        assert_no_part_files(&target_root);
    }

    #[test]
    fn target_disconnect_stops_later_items_keeps_commits_and_reports_exact_pending_part() {
        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        let mut files = Vec::new();
        for (canonical, contents) in [
            ("committed", b"first".as_slice()),
            ("disconnect", b"second".as_slice()),
            ("later", b"third".as_slice()),
        ] {
            let source = source_root.join(format!("{canonical}.jpg"));
            fs::write(&source, contents).unwrap();
            files.push(SelectedFile {
                canonical_number: canonical.into(),
                target: target_root.join(format!("{canonical}.jpg")),
                ..selected(source, PathBuf::new())
            });
        }
        let plan = copy_plan(&source_root, target_root.clone(), files);
        let pending = PendingTargetPart {
            target: target_root.join("disconnect.jpg"),
            part_name: ".photo-selector.part-safe-test".into(),
            identity: FileIdentity::default(),
        };
        let mut later_attempted = false;

        let report = execute_copy_with_updates_using(
            &plan,
            &AtomicBool::new(false),
            |_, _| {},
            |_, _| {},
            |source_file, item, target, source_is_unc, cancelled, progress| match item
                .canonical_number
                .as_str()
            {
                "committed" => copy_one_at(
                    source_file,
                    &item.fingerprint,
                    target,
                    source_is_unc,
                    cancelled,
                    progress,
                ),
                "disconnect" => Err(CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(64),
                        operation: TargetOperation::Write,
                    },
                    pending_part: Some(pending.clone()),
                }),
                "later" => {
                    later_attempted = true;
                    Ok(String::new())
                }
                _ => unreachable!(),
            },
        );

        assert_eq!(report.stop_reason, Some(CopyStopReason::TargetDisconnected));
        assert_eq!(report.copied.len(), 1);
        assert_eq!(
            report.target_disconnect,
            Some(TargetDisconnectInfo {
                windows_code: Some(64),
                operation: TargetOperation::Write,
            })
        );
        assert_eq!(report.pending_target_parts, vec![pending]);
        assert!(report.failed.is_empty());
        assert!(!later_attempted);
        assert!(target_root.join("committed.jpg").exists());
    }

    #[test]
    fn post_copy_source_hash_read_maps_unc_disconnect_without_reclassifying_target_io() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(64))
            }
        }
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(64))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        assert!(matches!(
            hash_source_reader_cancellable(&mut FailingReader, true, &AtomicBool::new(false)),
            Err(CopyError::SourceDisconnected)
        ));
        assert!(matches!(
            copy_stream_to(
                &mut std::io::Cursor::new(b"source"),
                &mut FailingWriter,
                true,
                false,
                &AtomicBool::new(false),
                &mut |_| {},
            ),
            Err(CopyError::Io(_))
        ));
    }

    #[test]
    fn target_write_and_hash_faults_pause_only_for_unc_targets() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(64))
            }
        }
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(64))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        assert!(matches!(
            copy_stream_to(
                &mut std::io::Cursor::new(b"source"),
                &mut FailingWriter,
                false,
                true,
                &AtomicBool::new(false),
                &mut |_| {},
            ),
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(64),
                    operation: TargetOperation::Write,
                },
                ..
            })
        ));
        assert!(matches!(
            hash_target_reader_cancellable(&mut FailingReader, true, &AtomicBool::new(false),),
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(64),
                    operation: TargetOperation::Hash,
                },
                ..
            })
        ));
        assert!(matches!(
            hash_target_reader_cancellable(&mut FailingReader, false, &AtomicBool::new(false),),
            Err(CopyError::Io(_))
        ));
    }

    #[test]
    fn target_open_reopen_sync_metadata_and_commit_faults_share_the_unc_context_gate() {
        for operation in [
            TargetOperation::OpenTarget,
            TargetOperation::Reopen,
            TargetOperation::Sync,
            TargetOperation::Metadata,
            TargetOperation::Commit,
        ] {
            let disconnected = contextualize_target_error(
                CopyError::Io(std::io::Error::from_raw_os_error(121)),
                true,
                operation,
            );
            assert!(
                matches!(
                    disconnected,
                    CopyError::TargetDisconnected {
                        info: TargetDisconnectInfo {
                            windows_code: Some(121),
                            operation: actual,
                        },
                        ..
                    } if actual == operation
                ),
                "{operation:?}"
            );
            let local = contextualize_target_error(
                CopyError::Io(std::io::Error::from_raw_os_error(121)),
                false,
                operation,
            );
            assert!(!matches!(local, CopyError::TargetDisconnected { .. }));
        }
    }

    #[test]
    fn missing_unc_target_child_remains_creatable_instead_of_looking_disconnected() {
        let error = map_target_child_open_error(
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing target child"),
            true,
            TargetOperation::Traverse,
        );

        assert!(matches!(
            error,
            CopyError::Io(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn preflight_source_hash_disconnect_returns_a_blocking_report() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(121))
            }
        }

        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        let source = source_root.join("IMG_0007.JPG");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(&source, b"trusted source").unwrap();
        let plan = copy_plan(
            &source_root,
            target_root,
            vec![selected(source, PathBuf::from("IMG_0007.JPG"))],
        );

        let report =
            preflight_copy_cancellable_using(&plan, &AtomicBool::new(false), |_, _, cancelled| {
                hash_source_reader_cancellable(&mut FailingReader, true, cancelled)
            })
            .unwrap();

        assert!(report.source_disconnected);
        assert!(!report.source_changed);
    }

    #[test]
    fn preflight_json_without_terminal_target_flag_remains_compatible() {
        let mut value = serde_json::to_value(PreflightReport::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("terminalTargetChanged");

        let report: PreflightReport = serde_json::from_value(value).unwrap();

        assert!(!report.terminal_target_changed);
    }

    #[test]
    fn preflight_json_without_target_disconnect_fields_remains_compatible() {
        let mut value = serde_json::to_value(PreflightReport::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("targetDisconnected");
        object.remove("targetDisconnect");
        object.remove("pendingTargetParts");

        let report: PreflightReport = serde_json::from_value(value).unwrap();

        assert!(!report.target_disconnected);
        assert!(report.target_disconnect.is_none());
        assert!(report.pending_target_parts.is_empty());
    }

    #[test]
    fn target_disconnect_info_serializes_without_paths_and_round_trips() {
        let info = TargetDisconnectInfo {
            windows_code: Some(53),
            operation: TargetOperation::OpenTarget,
        };
        let value = serde_json::to_value(info).unwrap();

        assert_eq!(value["windowsCode"], 53);
        assert_eq!(value["operation"], "open-target");
        assert_eq!(
            serde_json::from_value::<TargetDisconnectInfo>(value).unwrap(),
            info
        );
        assert_eq!(
            serde_json::from_value::<TargetDisconnectInfo>(serde_json::json!({
                "operation": "future-operation"
            }))
            .unwrap(),
            TargetDisconnectInfo {
                windows_code: None,
                operation: TargetOperation::Unknown,
            }
        );
    }

    #[test]
    fn preflight_rejects_target_outside_canonical_root() {
        let dir = test_tempdir();
        let root = dir.path().join("output");
        let source = dir.path().join("source.jpg");
        fs::write(&source, b"photo-bytes").unwrap();
        let plan = copy_plan(
            dir.path(),
            root,
            vec![selected(source, dir.path().join("escaped.jpg"))],
        );

        assert!(matches!(
            preflight_copy(&plan),
            Err(CopyError::InvalidTarget)
        ));
        assert!(!dir.path().join("escaped.jpg").exists());
    }

    #[test]
    fn cancelled_preflight_returns_without_scanning_the_plan() {
        let cancelled = AtomicBool::new(true);
        let plan = CopyPlan {
            source_root: "/path/that/must/not/be/opened".into(),
            source_root_identity: FileIdentity::default(),
            target_root: "/path/that/must/not/be/opened".into(),
            files: vec![],
        };

        assert!(matches!(
            preflight_copy_cancellable(&plan, &cancelled),
            Err(CopyError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_a_source_replaced_after_scan_even_with_same_metadata() {
        use crate::matching::{default_extensions, scan_source_index};

        let dir = test_tempdir();
        let source_root = dir.path().join("source");
        let target_root = dir.path().join("output");
        let source = source_root.join("IMG_0007.JPG");
        let moved = source_root.join("original.JPG");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(&source, b"same-size").unwrap();
        let index = scan_source_index(&source_root, None, &default_extensions()).unwrap();
        let indexed = index.files[0].clone();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();

        fs::rename(&source, &moved).unwrap();
        fs::write(&source, b"evil-byte").unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();

        let report = preflight_copy(&CopyPlan {
            source_root: index.root,
            source_root_identity: index.root_identity,
            target_root: target_root.clone(),
            files: vec![SelectedFile {
                canonical_number: "7".into(),
                source,
                target: target_root.join("IMG_0007.JPG"),
                fingerprint: indexed.fingerprint,
            }],
        })
        .unwrap();

        assert!(report.source_changed);
        assert!(!target_root.join("IMG_0007.JPG").exists());
    }

    #[cfg(unix)]
    #[test]
    fn candidate_preview_rejects_a_source_root_swapped_at_the_same_path() {
        use crate::matching::{default_extensions, scan_source_index};

        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let moved_root = directory.path().join("moved");
        fs::create_dir(&source_root).unwrap();
        fs::write(source_root.join("IMG_0007.JPG"), b"trusted-preview").unwrap();
        let index = scan_source_index(&source_root, None, &default_extensions()).unwrap();
        let indexed = index.files[0].clone();
        fs::rename(&source_root, &moved_root).unwrap();
        fs::create_dir(&source_root).unwrap();
        fs::write(source_root.join("IMG_0007.JPG"), b"replacement").unwrap();
        let selected = SelectedFile {
            canonical_number: indexed.canonical_number,
            source: indexed.path,
            target: PathBuf::new(),
            fingerprint: indexed.fingerprint,
        };

        assert!(matches!(
            read_bound_source_file(
                &index.root,
                &index.root_identity,
                &selected,
                24 * 1024 * 1024
            ),
            Err(CopyError::SourceChanged)
        ));
    }

    #[test]
    fn preflight_skips_identical_and_blocks_different_regular_targets() {
        let dir = test_tempdir();
        let root = dir.path().join("output");
        let identical_source = dir.path().join("same-source.jpg");
        let different_source = dir.path().join("different-source.jpg");
        let identical_target = root.join("same.jpg");
        let different_target = root.join("different.jpg");
        fs::create_dir_all(&root).unwrap();
        fs::write(&identical_source, b"same").unwrap();
        fs::write(&different_source, b"source").unwrap();
        fs::write(&identical_target, b"same").unwrap();
        fs::write(&different_target, b"target").unwrap();

        let report = preflight_copy(&copy_plan(
            dir.path(),
            root,
            vec![
                selected(identical_source, identical_target.clone()),
                selected(different_source, different_target.clone()),
            ],
        ))
        .unwrap();

        assert_eq!(report.identical.len(), 1);
        assert_eq!(report.conflicts.len(), 1);
        assert!(Path::new(&report.identical[0]).ends_with(Path::new("output").join("same.jpg")));
        assert!(
            Path::new(&report.conflicts[0]).ends_with(Path::new("output").join("different.jpg"))
        );
        assert_eq!(fs::read(different_target).unwrap(), b"target");
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_dangling_target_symlink() {
        use std::os::unix::fs::symlink;

        let dir = test_tempdir();
        let root = dir.path().join("output");
        let source = dir.path().join("source.jpg");
        let target = root.join("target.jpg");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source, b"photo-bytes").unwrap();
        symlink(dir.path().join("missing.jpg"), &target).unwrap();

        let result = preflight_copy(&copy_plan(dir.path(), root, vec![selected(source, target)]));

        assert!(matches!(result, Err(CopyError::InvalidTarget)));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_parent_symlink_even_when_it_points_inside_root() {
        use std::os::unix::fs::symlink;

        let dir = test_tempdir();
        let root = dir.path().join("output");
        let source = dir.path().join("source.jpg");
        let real_directory = root.join("real");
        let symlinked_directory = root.join("linked");
        fs::create_dir_all(&real_directory).unwrap();
        fs::write(&source, b"photo-bytes").unwrap();
        symlink(&real_directory, &symlinked_directory).unwrap();

        let result = preflight_copy(&copy_plan(
            dir.path(),
            root,
            vec![selected(source, symlinked_directory.join("target.jpg"))],
        ));

        assert!(matches!(result, Err(CopyError::InvalidTarget)));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_a_symlink_as_the_target_root() {
        use std::os::unix::fs::symlink;

        let dir = test_tempdir();
        let real_root = dir.path().join("real-output");
        let linked_root = dir.path().join("linked-output");
        let source = dir.path().join("source.jpg");
        fs::create_dir_all(&real_root).unwrap();
        fs::write(&source, b"photo-bytes").unwrap();
        symlink(&real_root, &linked_root).unwrap();

        let result = preflight_copy(&copy_plan(
            dir.path(),
            linked_root.clone(),
            vec![selected(source, linked_root.join("target.jpg"))],
        ));

        assert!(matches!(result, Err(CopyError::InvalidTarget)));
        assert!(!real_root.join("target.jpg").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_parent_with_symlink_during_copy_cannot_redirect_commit() {
        use std::os::unix::fs::symlink;

        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let parent = dir.path().join("output/album");
        let moved_parent = dir.path().join("output/album-moved");
        let outside = dir.path().join("outside");
        let target = parent.join("target.jpg");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(&source, b"photo-bytes").unwrap();

        let mut swapped = false;
        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            if swapped {
                return;
            }
            swapped = true;
            fs::rename(&parent, &moved_parent).unwrap();
            symlink(&outside, &parent).unwrap();
        });

        assert!(result.is_ok(), "{result:?}");
        assert!(!outside.join("target.jpg").exists());
        assert_eq!(
            fs::read(moved_parent.join("target.jpg")).unwrap(),
            b"photo-bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn removing_source_name_after_open_copies_the_opened_object() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"photo-bytes").unwrap();

        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            fs::remove_file(&source).unwrap();
        });

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fs::read(&target).unwrap(), b"photo-bytes");
        assert_no_part_files(target.parent().unwrap());
    }

    #[test]
    fn target_created_during_copy_is_never_overwritten() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"photo-bytes").unwrap();

        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            fs::write(&target, b"existing").unwrap();
        });

        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        assert_no_part_files(target.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_visible_part_cannot_change_committed_bytes() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"trusted-photo").unwrap();

        let mut replacement_attempted = false;
        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            if replacement_attempted {
                return;
            }
            replacement_attempted = true;
            if let Some(part) = fs::read_dir(target.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.to_string_lossy().contains(".part-"))
            {
                fs::remove_file(&part).unwrap();
                fs::write(part, b"evil-payload!").unwrap();
            }
        });

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fs::read(&target).unwrap(), b"trusted-photo");
        assert_no_part_files(target.parent().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exclusive_rename_fallback_commits_without_hardlinks_or_reflinks() {
        let dir = test_tempdir();
        let output = dir.path().join("output");
        fs::create_dir_all(&output).unwrap();
        let root = open_target_root(&output).unwrap();
        let target = locate_target(&root, Path::new("target.jpg"), false).unwrap();
        let mut part = PendingPart::create(&target.parent).unwrap();
        part.file_mut().write_all(b"photo-bytes").unwrap();
        part.file_mut().sync_all().unwrap();

        let committed = part.commit_macos_exclusive_rename(&target);
        part.finish(committed).unwrap();

        assert_eq!(fs::read(output.join("target.jpg")).unwrap(), b"photo-bytes");
        assert!(fs::read_dir(&output).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".stage-")));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_source_name_after_open_copies_the_opened_object() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let original = dir.path().join("opened-source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"trusted-photo").unwrap();
        let original_modified = fs::metadata(&source).unwrap().modified().unwrap();

        let mut swapped = false;
        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            if swapped {
                return;
            }
            swapped = true;
            fs::rename(&source, &original).unwrap();
            fs::write(&source, b"evil-payload!").unwrap();
            File::options()
                .write(true)
                .open(&source)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(original_modified))
                .unwrap();
        });

        assert!(result.is_ok());
        assert_eq!(fs::read(&target).unwrap(), b"trusted-photo");
        assert_eq!(fs::read(&source).unwrap(), b"evil-payload!");
        assert_no_part_files(target.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn deny_delete_source_handle_blocks_name_replacement_without_breaking_copy() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let renamed = dir.path().join("renamed-source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"trusted-photo").unwrap();

        let mut rename_error = None;
        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            rename_error = fs::rename(&source, &renamed).err();
        });

        assert!(result.is_ok(), "{result:?}");
        rename_error.expect("deny-delete handle must reject rename");
        assert_eq!(fs::read(&target).unwrap(), b"trusted-photo");
        assert_eq!(fs::read(&source).unwrap(), b"trusted-photo");
        assert!(!renamed.exists());
        assert_no_part_files(target.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_rename_uses_a_verified_path_and_zero_root_directory() {
        use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

        let path = r"F:\\photos\\IMG_0007.JPG"
            .encode_utf16()
            .collect::<Vec<_>>();
        let (root, target) = windows_rename_target(path.clone()).unwrap();
        assert_eq!(target, path);
        assert!(root.is_null());

        let (mut buffer, byte_len) = windows_rename_info_buffer(root, &target).unwrap();
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            assert!(!(*info).Anonymous.ReplaceIfExists);
            assert!((*info).RootDirectory.is_null());
            assert_eq!((*info).FileNameLength as usize, (target.len() + 1) * 2);
            let actual: &[u16] = std::slice::from_raw_parts(
                std::ptr::addr_of!((*info).FileName).cast(),
                target.len(),
            );
            assert_eq!(actual, target);
            let offset = std::ptr::addr_of!((*info).FileName) as usize - info.cast::<u8>() as usize;
            assert_eq!(byte_len as usize, offset + (target.len() + 1) * 2);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_rename_rejects_empty_or_nul_paths() {
        for invalid in ["", "target\0.jpg"] {
            assert!(
                windows_rename_target(invalid.encode_utf16().collect()).is_err(),
                "{invalid:?} must not be accepted as a Windows rename path"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn committed_probe_is_deleted_through_its_open_handle() {
        let directory = test_tempdir();
        let root = open_target_root(directory.path()).unwrap();
        let target = locate_target(&root, Path::new(".photo-selector-probe-test"), false).unwrap();
        let mut part = PendingPart::create_for_target_context(&target).unwrap();
        part.file_mut().write_all(b"probe").unwrap();

        let committed = part
            .commit(&target)
            .and_then(|()| windows_delete_file_on_close(part.file_mut()).map_err(CopyError::Io));
        part.finish(committed).unwrap();

        assert!(!target.display.exists());
        assert_no_part_files(directory.path());
    }

    #[cfg(windows)]
    #[test]
    fn local_rename_info_uses_zero_root_directory() {
        use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

        let path = r"F:\\photos\\target.jpg".encode_utf16().collect::<Vec<_>>();
        let (request_root, request_name) = windows_rename_target(path.clone()).unwrap();
        assert!(request_root.is_null());
        assert_eq!(request_name, path);
        let (mut buffer, _) = windows_rename_info_buffer(request_root, &request_name).unwrap();
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            assert!((*info).RootDirectory.is_null());
            assert!(!(*info).Anonymous.ReplaceIfExists);
        }
    }

    #[test]
    fn windows_smb_commit_never_reconstructs_a_path_from_a_handle() {
        let forbidden = ["GetFinalPathName", "ByHandleW"].concat();
        assert!(!include_str!("copy_engine.rs").contains(&forbidden));
    }

    #[cfg(unix)]
    #[test]
    fn existing_target_is_opened_without_following_the_leaf_symlink() {
        use std::os::unix::fs::symlink;

        let dir = test_tempdir();
        let output = dir.path().join("output");
        let outside = dir.path().join("outside.jpg");
        let target = output.join("target.jpg");
        fs::create_dir_all(&output).unwrap();
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &target).unwrap();
        let root = open_target_root(&output).unwrap();
        let location = locate_target(&root, Path::new("target.jpg"), false).unwrap();

        assert!(matches!(
            open_existing_target_nofollow(&location),
            Err(CopyError::InvalidTarget)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn existing_fifo_target_is_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, sync::mpsc, thread, time::Duration};

        let dir = test_tempdir();
        let output = dir.path().join("output");
        let target = output.join("target.fifo");
        fs::create_dir_all(&output).unwrap();
        let target_path = CString::new(target.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(target_path.as_ptr(), 0o600) }, 0);

        let root = open_target_root(&output).unwrap();
        let location = locate_target(&root, Path::new("target.fifo"), false).unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            sender
                .send(open_existing_target_nofollow(&location))
                .unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(Err(CopyError::InvalidTarget))
        ));
    }

    #[test]
    fn post_create_failure_is_cleaned_by_pending_part_guard() {
        let dir = test_tempdir();
        let root = open_target_root(dir.path()).unwrap();

        let result = PendingPart::create_with(&root.dir, |_| Err(CopyError::Cancelled));

        assert!(matches!(result, Err(CopyError::Cancelled)));
        assert_no_part_files(dir.path());
    }

    #[test]
    fn identity_capture_failure_after_create_is_cleaned_by_pending_part_guard() {
        let dir = test_tempdir();
        let root = open_target_root(dir.path()).unwrap();
        let target = locate_target(&root, Path::new("target.JPG"), true).unwrap();

        let result = PendingPart::create_for_target_with_identity(&target, |_| {
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(53),
                    operation: TargetOperation::Metadata,
                },
                pending_part: None,
            })
        });

        assert!(matches!(
            result,
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(53),
                    operation: TargetOperation::Metadata,
                },
                pending_part: None,
            })
        ));
        assert_no_part_files(dir.path());
    }

    #[test]
    fn progress_panic_is_reported_and_part_is_removed() {
        let dir = test_tempdir();
        let source = dir.path().join("source.jpg");
        let target = dir.path().join("out/target.jpg");
        fs::write(&source, b"photo-bytes").unwrap();

        let result = copy_one(&source, &target, &AtomicBool::new(false), |_| {
            panic!("tauri progress callback panicked");
        });

        assert!(matches!(result, Err(CopyError::ProgressPanicked)));
        assert!(!target.exists());
        assert_no_part_files(target.parent().unwrap());
    }

    #[test]
    fn cleanup_failure_preserves_the_original_operation_error() {
        let dir = test_tempdir();
        let root = open_target_root(dir.path()).unwrap();

        let result = cleanup_part(
            &root.dir,
            OsStr::new("missing.part"),
            Err(CopyError::Cancelled),
            false,
            None,
        );

        match result {
            Err(CopyError::CleanupFailed { operation, cleanup }) => {
                assert!(operation.contains("取消"));
                assert!(!cleanup.is_empty());
            }
            other => panic!("unexpected cleanup result: {other:?}"),
        }
    }

    #[test]
    fn successful_windows_commit_does_not_clean_the_old_part_name() {
        let cleanup_called = Cell::new(false);

        let result = finish_windows_pending_part_with(Ok(()), |_| {
            cleanup_called.set(true);
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(3),
                    operation: TargetOperation::Cleanup,
                },
                pending_part: None,
            })
        });

        assert!(result.is_ok());
        assert!(!cleanup_called.get());
    }

    #[test]
    fn failed_unc_partial_cleanup_preserves_its_exact_safe_record_for_recheck() {
        let record = PendingTargetPart {
            target: PathBuf::from(r"\\nas\share\target\IMG_0007.JPG"),
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };
        let original = TargetDisconnectInfo {
            windows_code: Some(64),
            operation: TargetOperation::Write,
        };

        let result = merge_part_cleanup_result(
            Err(CopyError::TargetDisconnected {
                info: original,
                pending_part: None,
            }),
            Err(std::io::Error::from_raw_os_error(53)),
            true,
            Some(record.clone()),
        );

        assert!(matches!(
            result,
            Err(CopyError::TargetDisconnected {
                info,
                pending_part: Some(actual)
            }) if actual == record && info == original
        ));
    }

    #[test]
    fn unconfirmed_cleanup_keeps_original_disconnect_and_exact_pending_record() {
        let record = PendingTargetPart {
            target: PathBuf::from(r"\\nas\share\target\IMG_0008.JPG"),
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };
        let original = TargetDisconnectInfo {
            windows_code: Some(121),
            operation: TargetOperation::Sync,
        };

        for cleanup in [
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            std::io::Error::new(std::io::ErrorKind::Other, "not confirmed"),
        ] {
            let result = merge_part_cleanup_result(
                Err(CopyError::TargetDisconnected {
                    info: original,
                    pending_part: None,
                }),
                Err(cleanup),
                true,
                Some(record.clone()),
            );
            assert!(matches!(
                result,
                Err(CopyError::TargetDisconnected {
                    info,
                    pending_part: Some(actual),
                }) if info == original && actual == record
            ));
        }
    }

    #[test]
    fn confirmed_cleanup_clears_pending_record_but_preserves_original_disconnect() {
        let record = PendingTargetPart {
            target: PathBuf::from(r"\\nas\share\target\IMG_0009.JPG"),
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };
        let original = TargetDisconnectInfo {
            windows_code: Some(64),
            operation: TargetOperation::Hash,
        };

        for cleanup in [
            Ok(()),
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "already absent",
            )),
        ] {
            let result = merge_part_cleanup_result(
                Err(CopyError::TargetDisconnected {
                    info: original,
                    pending_part: Some(record.clone()),
                }),
                cleanup,
                true,
                Some(record.clone()),
            );
            assert!(matches!(
                result,
                Err(CopyError::TargetDisconnected {
                    info,
                    pending_part: None,
                }) if info == original
            ));
        }
    }

    #[test]
    fn unc_raw_code3_cleanup_is_a_disconnect_and_keeps_the_pending_record() {
        let record = PendingTargetPart {
            target: PathBuf::from(r"\\nas\share\target\IMG_0010.JPG"),
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };
        let original = TargetDisconnectInfo {
            windows_code: Some(64),
            operation: TargetOperation::Write,
        };

        let result = merge_part_cleanup_result(
            Err(CopyError::TargetDisconnected {
                info: original,
                pending_part: None,
            }),
            Err(std::io::Error::from_raw_os_error(3)),
            true,
            Some(record.clone()),
        );

        assert!(matches!(
            result,
            Err(CopyError::TargetDisconnected {
                info,
                pending_part: Some(actual),
            }) if info == original && actual == record
        ));
    }

    #[test]
    fn cleanup_error_mapping_prioritizes_unc_raw3_but_keeps_local_enoent_absent() {
        for operation in [
            TargetOperation::OpenRoot,
            TargetOperation::Traverse,
            TargetOperation::OpenTarget,
        ] {
            assert!(matches!(
                map_target_cleanup_error(std::io::Error::from_raw_os_error(3), true, operation),
                CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(3),
                        operation: actual,
                    },
                    ..
                } if actual == operation
            ));
        }
        assert!(matches!(
            map_target_cleanup_error(
                std::io::Error::from_raw_os_error(2),
                false,
                TargetOperation::OpenTarget,
            ),
            CopyError::Io(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn unc_create_raw3_retries_absolute_child_after_parent_proves_connected() {
        let parent_revalidated = Cell::new(false);
        let child_enumerated = Cell::new(false);
        let fallback_attempted = Cell::new(false);

        let result = recover_unc_create_code3_with(
            std::io::Error::from_raw_os_error(3),
            true,
            || {
                parent_revalidated.set(true);
                Ok(())
            },
            || {
                child_enumerated.set(true);
                Ok(false)
            },
            || panic!("successful handle enumeration must not need an absolute child probe"),
            || {
                fallback_attempted.set(true);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert!(parent_revalidated.get());
        assert!(child_enumerated.get());
        assert!(fallback_attempted.get());
    }

    #[test]
    fn unc_create_raw3_accepts_parent_enumeration_when_handle_metadata_returns_raw3() {
        let result = recover_unc_create_code3_with(
            std::io::Error::from_raw_os_error(3),
            true,
            || {
                Err(CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(3),
                        operation: TargetOperation::Metadata,
                    },
                    pending_part: None,
                })
            },
            || Ok(false),
            || panic!("successful handle enumeration must not need an absolute child probe"),
            || Ok(()),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn unc_create_raw3_uses_the_authorized_absolute_parent_when_smb_handle_probes_return_raw3() {
        let fallback_attempted = Cell::new(false);

        let result = recover_unc_create_code3_with(
            std::io::Error::from_raw_os_error(3),
            true,
            || {
                Err(CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(3),
                        operation: TargetOperation::Metadata,
                    },
                    pending_part: None,
                })
            },
            || Err(std::io::Error::from_raw_os_error(3)),
            || Ok(true),
            || {
                fallback_attempted.set(true);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert!(fallback_attempted.get());
    }

    #[test]
    fn unc_child_metadata_raw3_enters_the_guarded_missing_child_creation_path() {
        let error = map_target_child_open_error(
            std::io::Error::from_raw_os_error(3),
            true,
            TargetOperation::Traverse,
        );

        assert!(is_missing_target_child_open_error(&error, true));
    }

    #[test]
    fn unc_existing_child_raw3_falls_back_to_the_authorized_absolute_directory_open() {
        let directory = test_tempdir();
        let child = directory.path().join("待精修的原片");
        fs::create_dir(&child).unwrap();
        let fallback_opened = Cell::new(false);

        let opened = reopen_created_target_child_with(
            Err(map_target_child_open_error(
                std::io::Error::from_raw_os_error(3),
                true,
                TargetOperation::Traverse,
            )),
            true,
            || {
                fallback_opened.set(true);
                Dir::open_ambient_dir(&child, ambient_authority()).map_err(CopyError::Io)
            },
        )
        .unwrap();

        assert!(opened.dir_metadata().unwrap().is_dir());
        assert!(fallback_opened.get());
    }

    #[test]
    fn unc_target_root_raw3_accepts_successful_native_handle_probe() {
        let probed = Cell::new(false);
        let result =
            confirm_open_target_root_with(Err(std::io::Error::from_raw_os_error(3)), true, || {
                probed.set(true);
                Ok(())
            });

        assert!(result.is_ok());
        assert!(probed.get());
    }

    #[test]
    fn unc_target_root_raw3_keeps_a_failed_handle_probe_disconnected() {
        let result =
            confirm_open_target_root_with(Err(std::io::Error::from_raw_os_error(3)), true, || {
                Err(std::io::Error::from_raw_os_error(53))
            });

        assert!(matches!(
            result,
            Err(CopyError::TargetDisconnected {
                info: TargetDisconnectInfo {
                    windows_code: Some(53),
                    operation: TargetOperation::Metadata,
                },
                pending_part: None,
            })
        ));
    }

    #[test]
    fn unc_target_root_accepts_open_handle_when_absolute_probe_returns_raw3() {
        let result = probe_target_root_with_fallbacks(
            || Err(std::io::Error::from_raw_os_error(3)),
            || Ok(()),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn absolute_target_probe_accepts_a_real_directory() {
        let directory = test_tempdir();

        probe_target_directory_path(directory.path()).unwrap();
    }

    #[test]
    fn absolute_target_probe_proves_a_missing_child_under_a_real_parent() {
        let directory = test_tempdir();

        assert!(probe_target_child_absent(&directory.path().join("missing.ARW")).unwrap());
    }

    #[test]
    fn absolute_target_probe_does_not_call_an_existing_file_missing() {
        let directory = test_tempdir();
        let file = directory.path().join("existing.ARW");
        fs::write(&file, b"existing").unwrap();

        assert!(!probe_target_child_absent(&file).unwrap());
    }

    #[test]
    fn verbatim_unc_normalization_collapses_extra_separator_after_prefix() {
        let input = r"\\?\UNC\\server\share\folder"
            .encode_utf16()
            .collect::<Vec<_>>();
        let expected = r"\\server\share\folder".encode_utf16().collect::<Vec<_>>();

        assert_eq!(normalize_verbatim_unc_wide(&input), Some(expected));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_unc_targets_use_a_standard_unc_io_path() {
        assert_eq!(
            target_io_path(Path::new(r"\\?\UNC\server\share\folder")),
            PathBuf::from(r"\\server\share\folder")
        );
        assert_eq!(
            target_io_path(Path::new(r"\\?\unc\server\share\folder")),
            PathBuf::from(r"\\server\share\folder")
        );
    }

    #[cfg(windows)]
    #[test]
    fn standard_unc_target_io_paths_are_unchanged() {
        let path = Path::new(r"\\server\share\folder");
        assert_eq!(target_io_path(path), path);
    }

    #[cfg(unix)]
    #[test]
    fn absolute_target_probe_rejects_a_directory_symlink() {
        use std::os::unix::fs::symlink;

        let directory = test_tempdir();
        let link = directory.path().join("link");
        symlink(directory.path(), &link).unwrap();

        assert_eq!(
            probe_target_directory_path(&link).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_directory_handle_probe_accepts_an_open_directory() {
        let directory = test_tempdir();
        let handle = Dir::open_ambient_dir(directory.path(), ambient_authority()).unwrap();

        probe_open_directory_handle(&handle).unwrap();
    }

    fn cleanup_child_operations() -> [TargetOperation; 3] {
        [
            TargetOperation::Traverse,
            TargetOperation::OpenTarget,
            TargetOperation::Cleanup,
        ]
    }

    #[test]
    fn child_raw3_post_parent_revalidation_failure_is_a_disconnect() {
        for operation in cleanup_child_operations() {
            let enumerated = Cell::new(false);
            let result = cleanup_child_result_after_parent_proof_with_postcheck(
                Err::<(), _>(std::io::Error::from_raw_os_error(3)),
                true,
                operation,
                || {
                    Err(CopyError::TargetDisconnected {
                        info: TargetDisconnectInfo {
                            windows_code: Some(3),
                            operation: TargetOperation::Metadata,
                        },
                        pending_part: None,
                    })
                },
                || {
                    enumerated.set(true);
                    Ok(false)
                },
            );

            assert!(matches!(result, Err(CopyError::TargetDisconnected { .. })));
            assert!(!enumerated.get());
        }
    }

    #[test]
    fn child_raw3_post_parent_enumeration_disconnect_is_not_missing() {
        for operation in cleanup_child_operations() {
            let revalidated = Cell::new(false);
            let result = cleanup_child_result_after_parent_proof_with_postcheck(
                Err::<(), _>(std::io::Error::from_raw_os_error(3)),
                true,
                operation,
                || {
                    revalidated.set(true);
                    Ok(())
                },
                || Err(std::io::Error::from_raw_os_error(3)),
            );

            assert!(revalidated.get());
            assert!(matches!(
                result,
                Err(CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(3),
                        operation: TargetOperation::Metadata,
                    },
                    ..
                })
            ));
        }
    }

    #[test]
    fn child_raw3_post_parent_enumeration_finding_exact_entry_is_not_missing() {
        for operation in cleanup_child_operations() {
            let result = cleanup_child_result_after_parent_proof_with_postcheck(
                Err::<(), _>(std::io::Error::from_raw_os_error(3)),
                true,
                operation,
                || Ok(()),
                || Ok(true),
            );

            assert!(matches!(result, Err(CopyError::InvalidTarget)));
        }
    }

    #[test]
    fn child_raw3_stable_parent_enumeration_proving_exact_entry_absent_is_missing() {
        for operation in cleanup_child_operations() {
            let result = cleanup_child_result_after_parent_proof_with_postcheck(
                Err::<(), _>(std::io::Error::from_raw_os_error(3)),
                true,
                operation,
                || Ok(()),
                || Ok(false),
            );

            assert!(matches!(result, Ok(None)));
        }
    }

    #[test]
    fn other_disconnect_codes_do_not_run_the_raw3_absence_postcheck() {
        for operation in [
            TargetOperation::Traverse,
            TargetOperation::OpenTarget,
            TargetOperation::Cleanup,
        ] {
            assert!(matches!(
                cleanup_child_result_after_parent_proof_with_postcheck(
                    Err::<(), _>(std::io::Error::from_raw_os_error(53)),
                    true,
                    operation,
                    || panic!("raw code 53 must not run the raw3 parent revalidation"),
                    || panic!("raw code 53 must not enumerate the parent"),
                ),
                Err(CopyError::TargetDisconnected {
                    info: TargetDisconnectInfo {
                        windows_code: Some(53),
                        operation: actual,
                    },
                    ..
                }) if actual == operation
            ));
        }
    }

    #[test]
    fn windows_disconnect_codes_beat_a_not_found_kind_on_every_platform() {
        for raw_os_error in [3, 53, 64, 67, 121, 1231, 2250] {
            assert!(!is_missing_cleanup_child_error(
                Some(raw_os_error),
                std::io::ErrorKind::NotFound,
                true,
            ));
        }
        assert!(is_missing_cleanup_child_error(
            Some(2),
            std::io::ErrorKind::NotFound,
            true,
        ));
        assert!(is_missing_cleanup_child_error(
            Some(53),
            std::io::ErrorKind::NotFound,
            false,
        ));
        assert!(!is_missing_cleanup_child_error(
            Some(53),
            std::io::ErrorKind::PermissionDenied,
            true,
        ));
    }

    #[test]
    fn local_not_found_child_remains_missing_without_a_parent_postcheck() {
        for operation in cleanup_child_operations() {
            let result = cleanup_child_result_after_parent_proof_with_postcheck(
                Err::<(), _>(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "local child is absent",
                )),
                false,
                operation,
                || panic!("local NotFound must not revalidate the parent"),
                || panic!("local NotFound must not enumerate the parent"),
            );

            assert!(matches!(result, Ok(None)));
        }
    }

    #[test]
    fn cleanup_parent_enumeration_matches_only_the_exact_os_name() {
        let directory = test_tempdir();
        let exact_name = OsStr::new(".photo-selector.part-Exact");
        fs::write(directory.path().join(exact_name), b"part").unwrap();
        let parent = Dir::open_ambient_dir(directory.path(), ambient_authority()).unwrap();

        assert!(cleanup_parent_contains_exact_child(&parent, exact_name).unwrap());
        assert!(!cleanup_parent_contains_exact_child(
            &parent,
            OsStr::new(".photo-selector.part-exact"),
        )
        .unwrap());
        assert!(
            !cleanup_parent_contains_exact_child(&parent, OsStr::new("unrelated.part")).unwrap()
        );
    }

    #[test]
    fn local_missing_cleanup_root_is_not_created_until_full_preflight() {
        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("missing-target");
        fs::create_dir_all(&source_root).unwrap();
        let source = source_root.join("IMG_0011.JPG");
        fs::write(&source, b"trusted").unwrap();
        let target = target_root.join("IMG_0011.JPG");
        let plan = copy_plan(
            &source_root,
            target_root.clone(),
            vec![selected(source, target.clone())],
        );
        let pending = PendingTargetPart {
            target,
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };

        cleanup_pending_target_parts(&plan, &[pending]).unwrap();

        assert!(!target_root.exists());
        preflight_copy(&plan).unwrap();
        assert!(target_root.exists());
    }

    #[test]
    fn missing_intermediate_cleanup_directory_is_not_created_until_full_preflight() {
        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let source = source_root.join("IMG_0012.JPG");
        fs::write(&source, b"trusted").unwrap();
        let target = target_root.join("missing-child/IMG_0012.JPG");
        let plan = copy_plan(
            &source_root,
            target_root.clone(),
            vec![selected(source, target.clone())],
        );
        let pending = PendingTargetPart {
            target,
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };

        cleanup_pending_target_parts(&plan, &[pending]).unwrap();

        assert!(!target_root.join("missing-child").exists());
        preflight_copy(&plan).unwrap();
        assert!(target_root.join("missing-child").is_dir());
    }

    #[test]
    fn missing_part_under_opened_target_directory_is_confirmed_absent() {
        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let source = source_root.join("IMG_0013.JPG");
        fs::write(&source, b"trusted").unwrap();
        let target = target_root.join("IMG_0013.JPG");
        let plan = copy_plan(
            &source_root,
            target_root,
            vec![selected(source, target.clone())],
        );
        let pending = PendingTargetPart {
            target,
            part_name: ".photo-selector.part-exact".into(),
            identity: FileIdentity::default(),
        };

        cleanup_pending_target_parts(&plan, &[pending]).unwrap();
    }

    #[test]
    fn reconnect_cleanup_removes_only_the_exact_owned_partial_with_matching_identity() {
        let directory = test_tempdir();
        let source_root = directory.path().join("source");
        let target_root = directory.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        let source = source_root.join("IMG_0007.JPG");
        fs::write(&source, b"trusted").unwrap();
        let target = target_root.join("IMG_0007.JPG");
        let plan = copy_plan(
            &source_root,
            target_root.clone(),
            vec![selected(source, target.clone())],
        );
        let part_name = ".photo-selector.part-owned";
        let part_path = target_root.join(part_name);
        fs::write(&part_path, b"partial").unwrap();
        let identity = capture_file_fingerprint(&mut File::open(&part_path).unwrap())
            .unwrap()
            .identity;
        let record = PendingTargetPart {
            target: target.clone(),
            part_name: part_name.into(),
            identity,
        };

        cleanup_pending_target_parts(&plan, std::slice::from_ref(&record)).unwrap();
        assert!(!part_path.exists());

        fs::write(&part_path, b"user replacement").unwrap();
        let replacement = fs::read(&part_path).unwrap();
        assert!(matches!(
            cleanup_pending_target_parts(&plan, std::slice::from_ref(&record)),
            Err(CopyError::InvalidTarget)
        ));
        assert_eq!(fs::read(&part_path).unwrap(), replacement);

        let unauthorized = PendingTargetPart {
            target: target_root.join("not-in-plan.JPG"),
            part_name: part_name.into(),
            identity: record.identity.clone(),
        };
        assert!(matches!(
            cleanup_pending_target_parts(&plan, &[unauthorized]),
            Err(CopyError::InvalidTarget)
        ));
        assert!(part_path.exists());

        let broad_name = PendingTargetPart {
            target,
            part_name: "../user-file".into(),
            identity: record.identity.clone(),
        };
        assert!(matches!(
            cleanup_pending_target_parts(&plan, &[broad_name]),
            Err(CopyError::InvalidTarget)
        ));
        assert!(part_path.exists());
    }

    fn assert_no_part_files(directory: &Path) {
        assert!(fs::read_dir(directory).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".part-")));
    }
}
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use cap_fs_ext::OpenOptionsFollowExt;
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_primitives::fs::FollowSymlinks;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions as CapOpenOptions},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{FileFingerprint, FileIdentity, SelectedFile};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetConflict {
    Identical,
    Different,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetOperation {
    OpenRoot,
    Traverse,
    CreateDirectory,
    OpenTarget,
    CreatePart,
    Write,
    Sync,
    Metadata,
    Seek,
    Hash,
    Reopen,
    Commit,
    Cleanup,
    AvailableSpace,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TargetDisconnectInfo {
    pub windows_code: Option<u32>,
    pub operation: TargetOperation,
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("目标路径无效")]
    InvalidTarget,
    #[error("源文件不在已授权目录内")]
    InvalidSource,
    #[error("源文件不可用")]
    SourceDisconnected,
    #[error("目标目录不可用")]
    TargetDisconnected {
        info: TargetDisconnectInfo,
        pending_part: Option<PendingTargetPart>,
    },
    #[error("复制已取消")]
    Cancelled,
    #[error("复制校验失败")]
    HashMismatch,
    #[error("复制期间源文件发生变化")]
    SourceChanged,
    #[error("复制进度回调异常")]
    ProgressPanicked,
    #[error("当前文件系统不支持安全的原子提交，请改用支持的磁盘或人工处理")]
    AtomicCommitUnsupported,
    #[error("目标文件已存在，未执行覆盖")]
    TargetAlreadyExists,
    #[error("临时文件清理失败（原操作：{operation}；清理：{cleanup}）")]
    CleanupFailed { operation: String, cleanup: String },
}

pub struct CopyPlan {
    pub source_root: PathBuf,
    pub source_root_identity: FileIdentity,
    pub target_root: PathBuf,
    pub files: Vec<SelectedFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    pub ambiguous: Vec<String>,
    pub partial: Vec<String>,
    pub missing: Vec<String>,
    pub conflicts: Vec<String>,
    pub identical: Vec<String>,
    pub permission_denied: bool,
    pub insufficient_space: bool,
    pub atomic_commit_unsupported: bool,
    pub source_changed: bool,
    pub source_disconnected: bool,
    #[serde(default)]
    pub target_disconnected: bool,
    #[serde(default)]
    pub target_disconnect: Option<TargetDisconnectInfo>,
    #[serde(default)]
    pub pending_target_parts: Vec<PendingTargetPart>,
    #[serde(default)]
    pub terminal_target_changed: bool,
}

impl PreflightReport {
    pub fn is_complete_for_terminal_validation(&self) -> bool {
        !self.permission_denied
            && !self.atomic_commit_unsupported
            && !self.source_changed
            && !self.source_disconnected
            && !self.target_disconnected
    }

    pub fn has_blocking_issue(&self) -> bool {
        !self.ambiguous.is_empty()
            || !self.partial.is_empty()
            || !self.missing.is_empty()
            || !self.conflicts.is_empty()
            || self.permission_denied
            || self.insufficient_space
            || self.source_changed
            || self.source_disconnected
            || self.target_disconnected
            || self.terminal_target_changed
            || self.atomic_commit_unsupported
    }
}

impl CopyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                "permission-denied"
            }
            Self::Io(_) => "io",
            Self::InvalidTarget => "invalid-target",
            Self::InvalidSource => "invalid-source",
            Self::SourceDisconnected => "source-disconnected",
            Self::TargetDisconnected { .. } => "target-disconnected",
            Self::Cancelled => "cancelled",
            Self::HashMismatch => "hash-mismatch",
            Self::SourceChanged => "source-changed",
            Self::ProgressPanicked => "progress-panicked",
            Self::AtomicCommitUnsupported => "atomic-commit-unsupported",
            Self::TargetAlreadyExists => "target-conflict",
            Self::CleanupFailed { .. } => "cleanup-failed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum CopyItemOutcome {
    Started,
    Copied { source_hash: String },
    SkippedIdentical { source_hash: String },
    Interrupted,
    Failed { code: String, summary: String },
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyReport {
    pub copied: Vec<String>,
    pub skipped_identical: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub cancelled: bool,
    pub stop_reason: Option<CopyStopReason>,
    pub target_disconnect: Option<TargetDisconnectInfo>,
    pub pending_target_parts: Vec<PendingTargetPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CopyStopReason {
    SourceDisconnected,
    TargetDisconnected,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTargetPart {
    pub target: PathBuf,
    pub part_name: String,
    pub identity: FileIdentity,
}

fn record_preflight_target_disconnect(
    report: &mut PreflightReport,
    info: TargetDisconnectInfo,
    pending_part: Option<PendingTargetPart>,
) {
    report.target_disconnected = true;
    report.target_disconnect = Some(info);
    if let Some(part) = pending_part {
        report.pending_target_parts.push(part);
    }
}

fn record_copy_target_disconnect(
    report: &mut CopyReport,
    info: TargetDisconnectInfo,
    pending_part: Option<PendingTargetPart>,
) {
    report.stop_reason = Some(CopyStopReason::TargetDisconnected);
    report.target_disconnect = Some(info);
    if let Some(part) = pending_part {
        report.pending_target_parts.push(part);
    }
}

#[cfg(test)]
fn hash_file(path: &Path) -> Result<blake3::Hash, CopyError> {
    hash_reader(File::open(path)?)
}

#[cfg(test)]
fn open_source(path: &Path) -> Result<File, CopyError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        return Ok(File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)?);
    }
    #[cfg(not(windows))]
    Ok(File::open(path)?)
}

#[cfg(test)]
fn hash_reader(mut file: impl Read) -> Result<blake3::Hash, CopyError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

fn hash_source_reader_cancellable(
    mut file: impl Read,
    source_is_unc: bool,
    cancelled: &AtomicBool,
) -> Result<blake3::Hash, CopyError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| map_source_open_error(error, source_is_unc))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

fn hash_target_reader_cancellable(
    mut file: impl Read,
    target_is_unc: bool,
    cancelled: &AtomicBool,
) -> Result<blake3::Hash, CopyError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Hash))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
fn compare_open_files(
    mut source: File,
    mut target: impl Read + Seek,
) -> Result<TargetConflict, CopyError> {
    if source.metadata()?.len() != target.seek(SeekFrom::End(0))? {
        return Ok(TargetConflict::Different);
    }
    source.seek(SeekFrom::Start(0))?;
    target.seek(SeekFrom::Start(0))?;
    Ok(if hash_reader(source)? == hash_reader(target)? {
        TargetConflict::Identical
    } else {
        TargetConflict::Different
    })
}

fn compare_planned_source_to_target(
    mut source: File,
    mut target: impl Read + Seek,
    expected: &FileFingerprint,
    source_is_unc: bool,
    target_is_unc: bool,
    cancelled: &AtomicBool,
) -> Result<TargetConflict, CopyError> {
    if !source_metadata_matches_for_operation(&source, expected, source_is_unc)? {
        return Err(CopyError::SourceChanged);
    }
    let before = source_snapshot_for_operation(&source, source_is_unc)?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| map_source_open_error(error, source_is_unc))?;
    let source_hash = hash_source_reader_cancellable(&mut source, source_is_unc, cancelled)?;
    if !expected.content_hash.is_empty() && source_hash.to_hex().as_str() != expected.content_hash {
        return Err(CopyError::SourceChanged);
    }
    let result = if source
        .metadata()
        .map_err(|error| map_source_open_error(error, source_is_unc))?
        .len()
        != target
            .seek(SeekFrom::End(0))
            .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Seek))?
    {
        TargetConflict::Different
    } else {
        target
            .seek(SeekFrom::Start(0))
            .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Seek))?;
        if source_hash == hash_target_reader_cancellable(&mut target, target_is_unc, cancelled)? {
            TargetConflict::Identical
        } else {
            TargetConflict::Different
        }
    };
    if source_snapshot_for_operation(&source, source_is_unc)? != before
        || !source_metadata_matches_for_operation(&source, expected, source_is_unc)?
    {
        return Err(CopyError::SourceChanged);
    }
    Ok(result)
}

#[cfg(test)]
fn compare_existing(source: &Path, target: &Path) -> Result<TargetConflict, CopyError> {
    let target = absolute_path(target)?;
    let parent = target.parent().ok_or(CopyError::InvalidTarget)?;
    let name = target.file_name().ok_or(CopyError::InvalidTarget)?;
    let location = TargetLocation {
        parent: Dir::open_ambient_dir(parent, ambient_authority())?,
        name: name.to_os_string(),
        display: target,
        is_unc: false,
    };
    compare_existing_at(source, &location)
}

fn copy_stream_to(
    input: &mut impl Read,
    output: &mut impl Write,
    source_is_unc: bool,
    target_is_unc: bool,
    cancelled: &AtomicBool,
    progress: &mut (impl FnMut(u64) + ?Sized),
) -> Result<blake3::Hash, CopyError> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let read = input
            .read(&mut buffer)
            .map_err(|error| map_source_open_error(error, source_is_unc))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Write))?;
        hasher.update(&buffer[..read]);
        progress(read as u64);
    }
    Ok(hasher.finalize())
}

struct TargetRoot {
    dir: Dir,
    requested_root: PathBuf,
    display_root: PathBuf,
    is_unc: bool,
}

struct SourceRoot {
    dir: Dir,
    requested_root: PathBuf,
    is_unc: bool,
}

struct TargetLocation {
    parent: Dir,
    name: OsString,
    display: PathBuf,
    is_unc: bool,
}

#[derive(PartialEq, Eq)]
struct SourceSnapshot {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
}

fn source_snapshot(file: &File) -> Result<SourceSnapshot, CopyError> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    let (volume_serial_number, file_index) = windows_file_identity(file)?;

    Ok(SourceSnapshot {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(windows)]
        volume_serial_number,
        #[cfg(windows)]
        file_index,
    })
}

fn source_snapshot_for_operation(
    file: &File,
    source_is_unc: bool,
) -> Result<SourceSnapshot, CopyError> {
    source_snapshot(file).map_err(|error| contextualize_source_error(error, source_is_unc))
}

fn source_fingerprint(file: &File) -> Result<FileFingerprint, CopyError> {
    let metadata = file.metadata()?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_millis().min(u64::MAX as u128) as u64);
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            device: metadata.dev(),
            file_index: metadata.ino(),
        }
    };
    #[cfg(windows)]
    let identity = {
        let (device, file_index) = windows_file_identity(file)?;
        FileIdentity {
            device: u64::from(device),
            file_index,
        }
    };
    #[cfg(not(any(unix, windows)))]
    let identity = FileIdentity::default();
    Ok(FileFingerprint {
        identity,
        size: metadata.len(),
        modified_ms,
        content_hash: String::new(),
    })
}

pub(crate) fn capture_file_fingerprint(file: &mut File) -> Result<FileFingerprint, CopyError> {
    let mut fingerprint = source_fingerprint(file)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(fingerprint.size as usize);
    file.read_to_end(&mut bytes)?;
    fingerprint.content_hash = blake3::hash(&bytes).to_hex().to_string();
    file.seek(SeekFrom::Start(0))?;
    Ok(fingerprint)
}

pub(crate) fn capture_directory_identity(path: &Path) -> Result<FileIdentity, CopyError> {
    let directory = Dir::open_ambient_dir(path, ambient_authority())?;
    Ok(source_fingerprint(&directory.into_std_file())?.identity)
}

fn source_metadata_matches(file: &File, expected: &FileFingerprint) -> Result<bool, CopyError> {
    let actual = source_fingerprint(file)?;
    Ok(actual.identity == expected.identity
        && actual.size == expected.size
        && actual.modified_ms == expected.modified_ms)
}

fn source_metadata_matches_for_operation(
    file: &File,
    expected: &FileFingerprint,
    source_is_unc: bool,
) -> Result<bool, CopyError> {
    source_metadata_matches(file, expected)
        .map_err(|error| contextualize_source_error(error, source_is_unc))
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u32, u64), CopyError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a valid handle for the duration of the call and
    // `information` points to writable storage of the required structure.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
}

fn absolute_path(path: &Path) -> Result<PathBuf, CopyError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn open_source_root(
    source_root: &Path,
    expected_identity: &FileIdentity,
) -> Result<SourceRoot, CopyError> {
    let requested_root = absolute_path(source_root)?;
    let is_unc = is_windows_unc_path(&requested_root);
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    let mut reached_normal_component = false;
    for component in requested_root.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !reached_normal_component => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                reached_normal_component = true;
                names.push(name.to_os_string());
            }
            _ => return Err(CopyError::InvalidSource),
        }
    }
    if anchor.as_os_str().is_empty() {
        return Err(CopyError::InvalidSource);
    }
    let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())
        .map_err(|error| map_source_open_error(error, is_unc))?;
    for name in names {
        let parent = dir
            .try_clone()
            .map_err(|error| map_source_open_error(error, is_unc))?
            .into_std_file();
        let child = cap_primitives::fs::open_dir_nofollow(&parent, Path::new(&name))
            .map_err(|error| map_source_open_error(error, is_unc))?;
        dir = Dir::from_std_file(child);
    }
    let actual = source_fingerprint(
        &dir.try_clone()
            .map_err(|error| map_source_open_error(error, is_unc))?
            .into_std_file(),
    )
    .map_err(|error| contextualize_source_error(error, is_unc))?
    .identity;
    if actual != *expected_identity {
        return Err(CopyError::SourceChanged);
    }
    Ok(SourceRoot {
        dir,
        requested_root,
        is_unc,
    })
}

fn open_planned_source(root: &SourceRoot, selected: &SelectedFile) -> Result<File, CopyError> {
    let relative = selected
        .source
        .strip_prefix(&root.requested_root)
        .map_err(|_| CopyError::InvalidSource)?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CopyError::InvalidSource);
    }
    let mut components = relative.components().peekable();
    let mut parent = root
        .dir
        .try_clone()
        .map_err(|error| map_source_open_error(error, root.is_unc))?;
    let mut leaf = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(CopyError::InvalidSource);
        };
        if components.peek().is_none() {
            leaf = Some(name.to_os_string());
            break;
        }
        let parent_file = parent
            .try_clone()
            .map_err(|error| map_source_open_error(error, root.is_unc))?
            .into_std_file();
        parent = Dir::from_std_file(
            cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(name))
                .map_err(|error| map_source_open_error(error, root.is_unc))?,
        );
    }
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }
    let file = parent
        .open_with(leaf.as_ref().ok_or(CopyError::InvalidSource)?, &options)
        .map_err(|error| map_source_open_error(error, root.is_unc))?
        .into_std();
    if !file
        .metadata()
        .map_err(|error| map_source_open_error(error, root.is_unc))?
        .is_file()
        || !source_metadata_matches_for_operation(&file, &selected.fingerprint, root.is_unc)?
    {
        return Err(CopyError::SourceChanged);
    }
    Ok(file)
}

pub(crate) fn read_bound_source_file(
    source_root: &Path,
    source_root_identity: &FileIdentity,
    selected: &SelectedFile,
    maximum_bytes: u64,
) -> Result<Vec<u8>, CopyError> {
    if selected.fingerprint.size > maximum_bytes {
        return Err(CopyError::InvalidSource);
    }
    let root = open_source_root(source_root, source_root_identity)?;
    let mut file = open_planned_source(&root, selected)?;
    let mut bytes = Vec::with_capacity(selected.fingerprint.size as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| map_source_open_error(error, root.is_unc))?;
    if bytes.len() as u64 != selected.fingerprint.size {
        return Err(CopyError::SourceChanged);
    }
    if blake3::hash(&bytes).to_hex().as_str() != selected.fingerprint.content_hash {
        return Err(CopyError::HashMismatch);
    }
    Ok(bytes)
}

fn is_windows_unc_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        return matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
        );
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn map_source_open_error(error: std::io::Error, is_unc: bool) -> CopyError {
    if is_unc
        && matches!(
            error.raw_os_error(),
            Some(3 | 53 | 64 | 67 | 121 | 1231 | 2250)
        )
    {
        CopyError::SourceDisconnected
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        CopyError::Io(error)
    } else {
        CopyError::SourceChanged
    }
}

fn map_target_io_error(
    error: std::io::Error,
    is_unc: bool,
    operation: TargetOperation,
) -> CopyError {
    if is_target_disconnect_error(&error, is_unc) {
        CopyError::TargetDisconnected {
            info: TargetDisconnectInfo {
                windows_code: error
                    .raw_os_error()
                    .and_then(|code| u32::try_from(code).ok()),
                operation,
            },
            pending_part: None,
        }
    } else {
        CopyError::Io(error)
    }
}

fn map_target_cleanup_error(
    error: std::io::Error,
    is_unc: bool,
    operation: TargetOperation,
) -> CopyError {
    // Cleanup must classify UNC disconnect codes before interpreting
    // ErrorKind::NotFound. In particular, Windows code 3 can mean the
    // network path vanished rather than that the exact part was deleted.
    map_target_io_error(error, is_unc, operation)
}

fn recover_unc_create_code3_with(
    error: std::io::Error,
    is_unc: bool,
    revalidate_parent: impl FnOnce() -> Result<(), CopyError>,
    enumerate_exact_child: impl FnOnce() -> std::io::Result<bool>,
    probe_absolute_child_absent: impl FnOnce() -> std::io::Result<bool>,
    retry_absolute_create: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), CopyError> {
    if !is_unc || error.raw_os_error() != Some(3) {
        return Err(map_target_io_error(
            error,
            is_unc,
            TargetOperation::CreateDirectory,
        ));
    }
    match enumerate_exact_child() {
        Ok(true) => Ok(()),
        Ok(false) => {
            if let Err(error) = revalidate_parent() {
                let metadata_code3 = matches!(
                    error,
                    CopyError::TargetDisconnected {
                        info: TargetDisconnectInfo {
                            windows_code: Some(3),
                            operation: TargetOperation::Metadata,
                        },
                        pending_part: None,
                    }
                );
                if !metadata_code3 {
                    return Err(error);
                }
            }
            match retry_absolute_create() {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) if error.raw_os_error() == Some(3) => Err(CopyError::Io(error)),
                Err(error) => Err(map_target_io_error(
                    error,
                    is_unc,
                    TargetOperation::CreateDirectory,
                )),
            }
        }
        Err(error) if error.raw_os_error() == Some(3) => {
            if let Err(error) = revalidate_parent() {
                let metadata_code3 = matches!(
                    error,
                    CopyError::TargetDisconnected {
                        info: TargetDisconnectInfo {
                            windows_code: Some(3),
                            operation: TargetOperation::Metadata,
                        },
                        pending_part: None,
                    }
                );
                if !metadata_code3 {
                    return Err(error);
                }
            }
            match probe_absolute_child_absent() {
                Ok(false) => Ok(()),
                Ok(true) => match retry_absolute_create() {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(error) if error.raw_os_error() == Some(3) => Err(CopyError::Io(error)),
                    Err(error) => Err(map_target_io_error(
                        error,
                        is_unc,
                        TargetOperation::CreateDirectory,
                    )),
                },
                Err(error) => Err(map_target_io_error(
                    error,
                    is_unc,
                    TargetOperation::Metadata,
                )),
            }
        }
        Err(error) => {
            revalidate_parent()?;
            Err(map_target_io_error(
                error,
                is_unc,
                TargetOperation::Metadata,
            ))
        }
    }
}

fn cleanup_parent_contains_exact_child(parent: &Dir, child_name: &OsStr) -> std::io::Result<bool> {
    for entry in parent.entries()? {
        if entry?.file_name().as_os_str() == child_name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cleanup_child_result_after_parent_proof_with_postcheck<T>(
    result: Result<T, std::io::Error>,
    is_unc: bool,
    operation: TargetOperation,
    revalidate_parent: impl FnOnce() -> Result<(), CopyError>,
    enumerate_exact_child: impl FnOnce() -> std::io::Result<bool>,
) -> Result<Option<T>, CopyError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_unc && error.raw_os_error() == Some(3) => {
            revalidate_parent()?;
            match enumerate_exact_child() {
                Ok(false) => Ok(None),
                Ok(true) => Err(CopyError::InvalidTarget),
                Err(error) => Err(map_target_cleanup_error(
                    error,
                    is_unc,
                    TargetOperation::Metadata,
                )),
            }
        }
        Err(error) if is_target_disconnect_error(&error, is_unc) => {
            Err(map_target_cleanup_error(error, is_unc, operation))
        }
        Err(error)
            if is_missing_cleanup_child_error(error.raw_os_error(), error.kind(), is_unc) =>
        {
            Ok(None)
        }
        Err(error) => Err(map_target_cleanup_error(error, is_unc, operation)),
    }
}

fn cleanup_child_result_after_parent_proof<T>(
    result: Result<T, std::io::Error>,
    parent: &Dir,
    child_name: &OsStr,
    is_unc: bool,
    operation: TargetOperation,
) -> Result<Option<T>, CopyError> {
    cleanup_child_result_after_parent_proof_with_postcheck(
        result,
        is_unc,
        operation,
        || confirm_cleanup_parent_connected(parent, is_unc),
        || cleanup_parent_contains_exact_child(parent, child_name),
    )
}

fn map_target_child_open_error(
    error: std::io::Error,
    is_unc: bool,
    operation: TargetOperation,
) -> CopyError {
    if is_missing_target_child_io_error(&error, is_unc) {
        CopyError::Io(error)
    } else {
        map_target_io_error(error, is_unc, operation)
    }
}

fn is_missing_target_child_io_error(error: &std::io::Error, is_unc: bool) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || (is_unc && matches!(error.raw_os_error(), Some(2 | 3)))
}

fn is_missing_target_child_open_error(error: &CopyError, is_unc: bool) -> bool {
    matches!(
        error,
        CopyError::Io(error) if is_missing_target_child_io_error(error, is_unc)
    )
}

fn contextualize_source_error(error: CopyError, is_unc: bool) -> CopyError {
    match error {
        CopyError::Io(error) => map_source_open_error(error, is_unc),
        error => error,
    }
}

fn contextualize_target_error(
    error: CopyError,
    is_unc: bool,
    operation: TargetOperation,
) -> CopyError {
    match error {
        CopyError::Io(error) => map_target_io_error(error, is_unc, operation),
        error => error,
    }
}

fn open_child_dir_nofollow(
    parent: &Dir,
    name: &OsStr,
    target_is_unc: bool,
) -> Result<Dir, CopyError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CopyError::InvalidTarget);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(map_target_child_open_error(
                error,
                target_is_unc,
                TargetOperation::Traverse,
            ));
        }
    }
    let parent_file = parent
        .try_clone()
        .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Traverse))?
        .into_std_file();
    let child =
        cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(name)).map_err(|error| {
            map_target_child_open_error(error, target_is_unc, TargetOperation::Traverse)
        })?;
    Ok(Dir::from_std_file(child))
}

fn open_cleanup_child_dir_nofollow(
    parent: &Dir,
    name: &OsStr,
    target_is_unc: bool,
) -> Result<Option<Dir>, CopyError> {
    let Some(metadata) = cleanup_child_result_after_parent_proof(
        parent.symlink_metadata(name),
        parent,
        name,
        target_is_unc,
        TargetOperation::Traverse,
    )?
    else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() {
        return Err(CopyError::InvalidTarget);
    }
    let parent_file = parent
        .try_clone()
        .map_err(|error| map_target_cleanup_error(error, target_is_unc, TargetOperation::Traverse))?
        .into_std_file();
    let Some(child) = cleanup_child_result_after_parent_proof(
        cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(name)),
        parent,
        name,
        target_is_unc,
        TargetOperation::Traverse,
    )?
    else {
        return Ok(None);
    };
    let child = Dir::from_std_file(child);
    let Some(metadata) = cleanup_child_result_after_parent_proof(
        child.dir_metadata(),
        parent,
        name,
        target_is_unc,
        TargetOperation::Metadata,
    )?
    else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Err(CopyError::InvalidTarget);
    }
    let Some(child_file) = cleanup_child_result_after_parent_proof(
        child.try_clone().map(Dir::into_std_file),
        parent,
        name,
        target_is_unc,
        TargetOperation::Metadata,
    )?
    else {
        return Ok(None);
    };
    match source_fingerprint(&child_file) {
        Ok(_) => {}
        Err(CopyError::Io(error)) => {
            let Some(()) = cleanup_child_result_after_parent_proof(
                Err(error),
                parent,
                name,
                target_is_unc,
                TargetOperation::Metadata,
            )?
            else {
                return Ok(None);
            };
        }
        Err(error) => {
            return Err(contextualize_target_error(
                error,
                target_is_unc,
                TargetOperation::Metadata,
            ));
        }
    }
    Ok(Some(child))
}

fn confirm_cleanup_parent_connected(parent: &Dir, target_is_unc: bool) -> Result<(), CopyError> {
    confirm_open_target_root_with(
        parent.dir_metadata().map(|metadata| metadata.is_dir()),
        target_is_unc,
        || probe_open_directory_handle(parent),
    )
}

fn reopen_created_target_child_with(
    relative_open: Result<Dir, CopyError>,
    target_is_unc: bool,
    open_absolute: impl FnOnce() -> Result<Dir, CopyError>,
) -> Result<Dir, CopyError> {
    match relative_open {
        Ok(child) => Ok(child),
        Err(error) if is_missing_target_child_open_error(&error, target_is_unc) => open_absolute(),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_absolute_target_directory(path: &Path) -> std::io::Result<Dir> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Ok(Dir::from_std_file(file))
}

#[cfg(not(windows))]
fn open_absolute_target_directory(path: &Path) -> std::io::Result<Dir> {
    Dir::open_ambient_dir(path, ambient_authority())
}

fn open_absolute_target_directory_nofollow(
    path: &Path,
    target_is_unc: bool,
) -> Result<Dir, CopyError> {
    probe_target_directory_path(path)
        .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Metadata))?;
    let directory = open_absolute_target_directory(path)
        .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Traverse))?;
    probe_target_directory_path(path)
        .map_err(|error| map_target_io_error(error, target_is_unc, TargetOperation::Metadata))?;
    Ok(directory)
}

fn open_or_create_child_dir(
    parent: &Dir,
    parent_path: &Path,
    name: &OsStr,
    target_is_unc: bool,
) -> Result<Dir, CopyError> {
    match open_child_dir_nofollow(parent, name, target_is_unc) {
        Ok(child) => Ok(child),
        Err(error) if is_missing_target_child_open_error(&error, target_is_unc) => {
            let CopyError::Io(_) = error else {
                unreachable!("missing target child errors preserve their I/O error")
            };
            match parent.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    recover_unc_create_code3_with(
                        error,
                        target_is_unc,
                        || confirm_cleanup_parent_connected(parent, target_is_unc),
                        || cleanup_parent_contains_exact_child(parent, name),
                        || probe_target_child_absent(&parent_path.join(name)),
                        || std::fs::create_dir(parent_path.join(name)),
                    )?;
                }
            }
            reopen_created_target_child_with(
                open_child_dir_nofollow(parent, name, target_is_unc),
                target_is_unc,
                || open_absolute_target_directory_nofollow(&parent_path.join(name), target_is_unc),
            )
        }
        Err(error) => Err(error),
    }
}

fn confirm_open_target_root_with(
    metadata_is_directory: std::io::Result<bool>,
    target_is_unc: bool,
    probe_open_handle: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), CopyError> {
    match metadata_is_directory {
        Ok(true) => Ok(()),
        Ok(false) => Err(CopyError::InvalidTarget),
        Err(error) if target_is_unc && error.raw_os_error() == Some(3) => probe_open_handle()
            .map_err(|probe_error| {
                map_target_io_error(probe_error, target_is_unc, TargetOperation::Metadata)
            }),
        Err(error) => Err(map_target_io_error(
            error,
            target_is_unc,
            TargetOperation::Metadata,
        )),
    }
}

fn probe_target_root_with_fallbacks(
    probe_absolute_path: impl FnOnce() -> std::io::Result<()>,
    probe_open_handle: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    match probe_absolute_path() {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(3) => probe_open_handle(),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn probe_open_directory_handle(dir: &Dir) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a valid handle for the duration of the call and
    // `information` points to writable storage of the required structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "opened target handle is not a directory",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn probe_open_directory_handle(dir: &Dir) -> std::io::Result<()> {
    let mut entries = dir.entries()?;
    match entries.next() {
        Some(Err(error)) => Err(error),
        Some(Ok(_)) | None => Ok(()),
    }
}

#[cfg(windows)]
fn windows_target_attributes(path: &Path) -> std::io::Result<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};

    let mut path_wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if path_wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path contains an interior NUL",
        ));
    }
    path_wide.push(0);
    // SAFETY: `path_wide` is NUL-terminated and remains alive for the call.
    let attributes = unsafe { GetFileAttributesW(path_wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(std::io::Error::last_os_error());
    }
    Ok(attributes)
}

#[cfg(windows)]
fn probe_target_directory_path(path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let attributes = windows_target_attributes(path)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "target path is not a non-reparse directory",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn probe_target_child_absent(path: &Path) -> std::io::Result<bool> {
    match windows_target_attributes(path) {
        Ok(_) => Ok(false),
        Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => {
            probe_target_directory_path(path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target child has no parent",
                )
            })?)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn probe_target_directory_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "target path is not a non-symlink directory",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn probe_target_child_absent(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            probe_target_directory_path(path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target child has no parent",
                )
            })?)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(windows, test))]
fn normalize_verbatim_unc_wide(wide: &[u16]) -> Option<Vec<u16>> {
    let verbatim_unc_prefix = [
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    let has_verbatim_unc_prefix = wide.len() >= verbatim_unc_prefix.len()
        && wide
            .iter()
            .zip(verbatim_unc_prefix)
            .all(|(actual, expected)| {
                if (b'A' as u16..=b'Z' as u16).contains(&expected) {
                    *actual == expected || *actual == expected + u16::from(b'a' - b'A')
                } else {
                    *actual == expected
                }
            });
    if !has_verbatim_unc_prefix {
        return None;
    }

    let suffix = &wide[verbatim_unc_prefix.len()..];
    let suffix = &suffix[suffix.iter().position(|unit| *unit != b'\\' as u16)?..];
    let mut normalized = vec![b'\\' as u16, b'\\' as u16];
    normalized.extend_from_slice(suffix);
    Some(normalized)
}

#[cfg(windows)]
fn target_io_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let Some(normalized) = normalize_verbatim_unc_wide(&wide) else {
        return path.to_path_buf();
    };
    PathBuf::from(OsString::from_wide(&normalized))
}

#[cfg(not(windows))]
fn target_io_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn open_target_root(target_root: &Path) -> Result<TargetRoot, CopyError> {
    let requested_root = absolute_path(target_root)?;
    let is_unc = is_windows_unc_path(&requested_root);
    let display_root = target_io_path(&requested_root);
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    let mut reached_normal_component = false;
    for component in display_root.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !reached_normal_component => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                reached_normal_component = true;
                names.push(name.to_os_string());
            }
            _ => return Err(CopyError::InvalidTarget),
        }
    }
    if anchor.as_os_str().is_empty() {
        return Err(CopyError::InvalidTarget);
    }

    // The filesystem namespace root is not replaceable through a parent
    // directory. Ambient authority is therefore used only for this anchor
    // (`/` on Unix, a volume/share root on Windows); every user-controlled
    // component is resolved from an already-open directory without following
    // a final symlink.
    let mut dir = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|error| map_target_io_error(error, is_unc, TargetOperation::OpenRoot))?;
    let mut current_path = anchor.clone();
    for name in &names {
        dir = open_or_create_child_dir(&dir, &current_path, name, is_unc)?;
        current_path.push(name);
    }
    confirm_open_target_root_with(
        dir.dir_metadata().map(|metadata| metadata.is_dir()),
        is_unc,
        || {
            probe_target_root_with_fallbacks(
                || probe_target_directory_path(&display_root),
                || probe_open_directory_handle(&dir),
            )
        },
    )?;

    Ok(TargetRoot {
        dir,
        requested_root,
        display_root,
        is_unc,
    })
}

fn open_cleanup_target_root(target_root: &Path) -> Result<Option<TargetRoot>, CopyError> {
    let requested_root = absolute_path(target_root)?;
    let is_unc = is_windows_unc_path(&requested_root);
    let display_root = target_io_path(&requested_root);
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    let mut reached_normal_component = false;
    for component in display_root.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !reached_normal_component => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                reached_normal_component = true;
                names.push(name.to_os_string());
            }
            _ => return Err(CopyError::InvalidTarget),
        }
    }
    if anchor.as_os_str().is_empty() {
        return Err(CopyError::InvalidTarget);
    }

    let mut dir = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|error| map_target_cleanup_error(error, is_unc, TargetOperation::OpenRoot))?;
    // This is the first online-parent proof. A raw code 3 before this point
    // remains a disconnect; only child operations after this proof may treat
    // code 3 as a confirmed missing child.
    confirm_cleanup_parent_connected(&dir, is_unc)?;
    for name in &names {
        dir = match open_cleanup_child_dir_nofollow(&dir, name, is_unc)? {
            Some(child) => child,
            None => return Ok(None),
        };
    }

    Ok(Some(TargetRoot {
        dir,
        requested_root,
        display_root,
        is_unc,
    }))
}

fn relative_target(root: &TargetRoot, requested: &Path) -> Result<PathBuf, CopyError> {
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(&root.requested_root)
            .map_err(|_| CopyError::InvalidTarget)?
            .to_path_buf()
    } else {
        requested.to_path_buf()
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CopyError::InvalidTarget);
    }
    Ok(relative)
}

fn locate_target(
    root: &TargetRoot,
    requested: &Path,
    create_parents: bool,
) -> Result<TargetLocation, CopyError> {
    let relative = relative_target(root, requested)?;
    let mut components = relative.components().peekable();
    let mut parent = root
        .dir
        .try_clone()
        .map_err(|error| map_target_io_error(error, root.is_unc, TargetOperation::Traverse))?;
    let mut parent_path = root.display_root.clone();
    let mut name = None;
    while let Some(component) = components.next() {
        let Component::Normal(component_name) = component else {
            return Err(CopyError::InvalidTarget);
        };
        if components.peek().is_none() {
            name = Some(component_name.to_os_string());
            break;
        }
        parent = if create_parents {
            open_or_create_child_dir(&parent, &parent_path, component_name, root.is_unc)?
        } else {
            open_child_dir_nofollow(&parent, component_name, root.is_unc)?
        };
        parent_path.push(component_name);
    }

    Ok(TargetLocation {
        parent,
        name: name.ok_or(CopyError::InvalidTarget)?,
        display: root.display_root.join(relative),
        is_unc: root.is_unc,
    })
}

fn locate_cleanup_target(
    root: &TargetRoot,
    requested: &Path,
) -> Result<Option<TargetLocation>, CopyError> {
    let relative = relative_target(root, requested)?;
    let mut components = relative.components().peekable();
    let mut parent = root
        .dir
        .try_clone()
        .map_err(|error| map_target_cleanup_error(error, root.is_unc, TargetOperation::Traverse))?;
    let mut name = None;
    while let Some(component) = components.next() {
        let Component::Normal(component_name) = component else {
            return Err(CopyError::InvalidTarget);
        };
        if components.peek().is_none() {
            name = Some(component_name.to_os_string());
            break;
        }
        parent = match open_cleanup_child_dir_nofollow(&parent, component_name, root.is_unc)? {
            Some(child) => child,
            None => return Ok(None),
        };
    }

    Ok(Some(TargetLocation {
        parent,
        name: name.ok_or(CopyError::InvalidTarget)?,
        display: root.display_root.join(relative),
        is_unc: root.is_unc,
    }))
}

#[cfg(test)]
fn compare_existing_at(
    source: &Path,
    target: &TargetLocation,
) -> Result<TargetConflict, CopyError> {
    let file = open_existing_target_nofollow(target)?.ok_or_else(|| {
        CopyError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "target does not exist",
        ))
    })?;
    compare_open_files(open_source(source)?, file)
}

fn open_existing_target_nofollow(target: &TargetLocation) -> Result<Option<File>, CopyError> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }

    let file = match target.parent.open_with(&target.name, &options) {
        Ok(file) => file.into_std(),
        Err(open_error) => {
            if target.is_unc
                && (open_error.kind() == std::io::ErrorKind::NotFound
                    || open_error.raw_os_error() == Some(3))
            {
                match probe_target_child_absent(&target.display) {
                    Ok(true) => return Ok(None),
                    Ok(false) => {}
                    Err(error) => {
                        return Err(map_target_io_error(
                            error,
                            target.is_unc,
                            TargetOperation::Metadata,
                        ));
                    }
                }
            }
            return match target.parent.symlink_metadata(&target.name) {
                Ok(metadata) if metadata.file_type().is_symlink() => Err(CopyError::InvalidTarget),
                Err(metadata_error)
                    if open_error.kind() == std::io::ErrorKind::NotFound
                        && metadata_error.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(None)
                }
                Err(metadata_error)
                    if !is_target_disconnect_error(&open_error, target.is_unc)
                        && is_target_disconnect_error(&metadata_error, target.is_unc) =>
                {
                    Err(map_target_io_error(
                        metadata_error,
                        target.is_unc,
                        TargetOperation::Metadata,
                    ))
                }
                _ => Err(map_target_io_error(
                    open_error,
                    target.is_unc,
                    TargetOperation::OpenTarget,
                )),
            };
        }
    };
    if !file
        .metadata()
        .map_err(|error| map_target_io_error(error, target.is_unc, TargetOperation::Metadata))?
        .is_file()
    {
        return Err(CopyError::InvalidTarget);
    }
    Ok(Some(file))
}

fn open_cleanup_file_nofollow(target: &TargetLocation) -> Result<Option<File>, CopyError> {
    confirm_cleanup_parent_connected(&target.parent, target.is_unc)?;
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }

    let file = match target.parent.open_with(&target.name, &options) {
        Ok(file) => file.into_std(),
        Err(open_error) => {
            if open_error.kind() == std::io::ErrorKind::NotFound
                || (target.is_unc && open_error.raw_os_error() == Some(3))
            {
                let _confirmed_absent = cleanup_child_result_after_parent_proof(
                    Err::<(), _>(open_error),
                    &target.parent,
                    &target.name,
                    target.is_unc,
                    TargetOperation::OpenTarget,
                )?;
                return Ok(None);
            }
            let Some(metadata) = cleanup_child_result_after_parent_proof(
                target.parent.symlink_metadata(&target.name),
                &target.parent,
                &target.name,
                target.is_unc,
                TargetOperation::Metadata,
            )?
            else {
                return Ok(None);
            };
            if metadata.file_type().is_symlink() {
                return Err(CopyError::InvalidTarget);
            }
            return Err(map_target_cleanup_error(
                open_error,
                target.is_unc,
                TargetOperation::OpenTarget,
            ));
        }
    };
    let Some(metadata) = cleanup_child_result_after_parent_proof(
        file.metadata(),
        &target.parent,
        &target.name,
        target.is_unc,
        TargetOperation::Metadata,
    )?
    else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(CopyError::InvalidTarget);
    }
    Ok(Some(file))
}

fn cleanup_file_identity(
    file: &File,
    parent: &Dir,
    name: &OsStr,
    target_is_unc: bool,
) -> Result<Option<FileIdentity>, CopyError> {
    confirm_cleanup_parent_connected(parent, target_is_unc)?;
    match source_fingerprint(file) {
        Ok(fingerprint) => Ok(Some(fingerprint.identity)),
        Err(CopyError::Io(error)) => cleanup_child_result_after_parent_proof(
            Err::<FileIdentity, _>(error),
            parent,
            name,
            target_is_unc,
            TargetOperation::Metadata,
        ),
        Err(error) => Err(contextualize_target_error(
            error,
            target_is_unc,
            TargetOperation::Metadata,
        )),
    }
}

fn cleanup_part(
    parent: &Dir,
    part_name: &OsStr,
    result: Result<(), CopyError>,
    target_is_unc: bool,
    pending_part: Option<PendingTargetPart>,
) -> Result<(), CopyError> {
    merge_part_cleanup_result(
        result,
        parent.remove_file(part_name),
        target_is_unc,
        pending_part,
    )
}

#[cfg(any(windows, test))]
fn finish_windows_pending_part_with(
    result: Result<(), CopyError>,
    cleanup_uncommitted: impl FnOnce(Result<(), CopyError>) -> Result<(), CopyError>,
) -> Result<(), CopyError> {
    if result.is_ok() {
        // SetFileInformationByHandle renames the opened part itself. After a
        // successful commit, the old UUID name no longer belongs to this
        // operation and must not be removed. In particular, some SMB servers
        // report Windows code 3 for that now-absent relative name.
        result
    } else {
        cleanup_uncommitted(result)
    }
}

fn merge_part_cleanup_result(
    result: Result<(), CopyError>,
    cleanup_result: Result<(), std::io::Error>,
    target_is_unc: bool,
    pending_part: Option<PendingTargetPart>,
) -> Result<(), CopyError> {
    let result = match result {
        Err(CopyError::TargetDisconnected {
            info,
            pending_part: original_pending,
        }) => {
            return match cleanup_result {
                Ok(()) => Err(CopyError::TargetDisconnected {
                    info,
                    pending_part: None,
                }),
                Err(cleanup) if is_target_disconnect_error(&cleanup, target_is_unc) => {
                    Err(CopyError::TargetDisconnected {
                        info,
                        pending_part: pending_part.or(original_pending),
                    })
                }
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {
                    Err(CopyError::TargetDisconnected {
                        info,
                        pending_part: None,
                    })
                }
                Err(_) => Err(CopyError::TargetDisconnected {
                    info,
                    pending_part: pending_part.or(original_pending),
                }),
            };
        }
        result => result,
    };

    match cleanup_result {
        Ok(()) => result,
        Err(cleanup) if is_target_disconnect_error(&cleanup, target_is_unc) => {
            match map_target_cleanup_error(cleanup, target_is_unc, TargetOperation::Cleanup) {
                CopyError::TargetDisconnected { info, .. } => {
                    Err(CopyError::TargetDisconnected { info, pending_part })
                }
                _ => unreachable!("disconnect classification was checked above"),
            }
        }
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound && result.is_ok() => result,
        Err(cleanup) => Err(CopyError::CleanupFailed {
            operation: result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "目标已提交".to_owned()),
            cleanup: cleanup.to_string(),
        }),
    }
}

fn is_target_disconnect_code(raw_os_error: Option<i32>, target_is_unc: bool) -> bool {
    target_is_unc && matches!(raw_os_error, Some(3 | 53 | 64 | 67 | 121 | 1231 | 2250))
}

fn is_target_disconnect_error(error: &std::io::Error, target_is_unc: bool) -> bool {
    is_target_disconnect_code(error.raw_os_error(), target_is_unc)
}

fn is_missing_cleanup_child_error(
    raw_os_error: Option<i32>,
    kind: std::io::ErrorKind,
    target_is_unc: bool,
) -> bool {
    kind == std::io::ErrorKind::NotFound && !is_target_disconnect_code(raw_os_error, target_is_unc)
}

struct PendingPart {
    parent: Dir,
    #[cfg(target_os = "macos")]
    staging: Option<Dir>,
    #[cfg(target_os = "macos")]
    staging_name: Option<OsString>,
    file: Option<File>,
    name: Option<OsString>,
    target_is_unc: bool,
    pending_target_part: Option<PendingTargetPart>,
}

impl PendingPart {
    #[cfg(test)]
    fn create(parent: &Dir) -> Result<Self, CopyError> {
        Self::create_with_context(parent, false, None, |_| Ok(()))
    }

    fn create_for_target(target: &TargetLocation) -> Result<Self, CopyError> {
        Self::create_with_context(
            &target.parent,
            target.is_unc,
            Some(target.display.as_path()),
            |_| Ok(()),
        )
    }

    #[cfg(test)]
    fn create_for_target_with_identity(
        target: &TargetLocation,
        capture_identity: impl FnOnce(&File) -> Result<FileIdentity, CopyError>,
    ) -> Result<Self, CopyError> {
        Self::create_with_context_and_identity(
            &target.parent,
            target.is_unc,
            Some(target.display.as_path()),
            capture_identity,
            |_| Ok(()),
        )
    }

    fn create_for_target_context(target: &TargetLocation) -> Result<Self, CopyError> {
        Self::create_with_context(&target.parent, target.is_unc, None, |_| Ok(()))
    }

    #[cfg(test)]
    fn create_with(
        parent: &Dir,
        after_create: impl FnOnce(&mut Self) -> Result<(), CopyError>,
    ) -> Result<Self, CopyError> {
        Self::create_with_context(parent, false, None, after_create)
    }

    fn create_with_context(
        parent: &Dir,
        target_is_unc: bool,
        tracked_target: Option<&Path>,
        after_create: impl FnOnce(&mut Self) -> Result<(), CopyError>,
    ) -> Result<Self, CopyError> {
        Self::create_with_context_and_identity(
            parent,
            target_is_unc,
            tracked_target,
            |file| {
                source_fingerprint(file)
                    .map_err(|error| {
                        contextualize_target_error(error, target_is_unc, TargetOperation::Metadata)
                    })
                    .map(|fingerprint| fingerprint.identity)
            },
            after_create,
        )
    }

    fn create_with_context_and_identity(
        parent: &Dir,
        target_is_unc: bool,
        tracked_target: Option<&Path>,
        capture_identity: impl FnOnce(&File) -> Result<FileIdentity, CopyError>,
        after_create: impl FnOnce(&mut Self) -> Result<(), CopyError>,
    ) -> Result<Self, CopyError> {
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = (
                parent,
                target_is_unc,
                tracked_target,
                capture_identity,
                after_create,
            );
            return Err(CopyError::AtomicCommitUnsupported);
        }

        #[cfg(any(target_os = "macos", windows))]
        {
            // Clone the parent before creating a named object. Once the file
            // exists, `part` owns both its handle and cleanup responsibility.
            let parent = parent.try_clone().map_err(|error| {
                map_target_io_error(error, target_is_unc, TargetOperation::CreatePart)
            })?;
            #[cfg(target_os = "macos")]
            let (staging, staging_name, name) = {
                use std::os::unix::fs::PermissionsExt;
                let staging_name =
                    OsString::from(format!(".photo-selector.stage-{}", Uuid::new_v4()));
                parent.create_dir(&staging_name).map_err(|error| {
                    map_target_io_error(error, target_is_unc, TargetOperation::CreatePart)
                })?;
                let staging = match open_child_dir_nofollow(&parent, &staging_name, target_is_unc) {
                    Ok(staging) => staging,
                    Err(error) => {
                        let _ = parent.remove_dir(&staging_name);
                        return Err(error);
                    }
                };
                // exFAT/SMB may not implement POSIX modes. The UUID-named
                // directory is still bound by its open handle; tighten local
                // permissions where the filesystem supports it.
                let _ = staging.set_permissions(
                    ".",
                    cap_std::fs::Permissions::from_std(std::fs::Permissions::from_mode(0o700)),
                );
                (
                    Some(staging),
                    Some(staging_name),
                    OsString::from("payload.part"),
                )
            };
            #[cfg(windows)]
            let name = OsString::from(format!(".photo-selector.part-{}", Uuid::new_v4()));
            let mut options = CapOpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(windows)]
            {
                use cap_std::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
                };
                options
                    .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
                    .share_mode(FILE_SHARE_READ);
            }
            #[cfg(target_os = "macos")]
            let file = staging
                .as_ref()
                .expect("macOS staging directory exists")
                .open_with(&name, &options)
                .map_err(|error| {
                    map_target_io_error(error, target_is_unc, TargetOperation::CreatePart)
                })?
                .into_std();
            #[cfg(windows)]
            let file = parent
                .open_with(&name, &options)
                .map_err(|error| {
                    map_target_io_error(error, target_is_unc, TargetOperation::CreatePart)
                })?
                .into_std();
            let mut part = Self {
                parent,
                #[cfg(target_os = "macos")]
                staging,
                #[cfg(target_os = "macos")]
                staging_name,
                file: Some(file),
                name: Some(name),
                target_is_unc,
                pending_target_part: None,
            };
            if let Some(target) = tracked_target {
                let identity = match capture_identity(
                    part.file.as_ref().expect("pending file is available"),
                ) {
                    Ok(identity) => identity,
                    Err(error) => {
                        return match part.finish(Err(error)) {
                            Err(error) => Err(error),
                            Ok(()) => unreachable!("finishing an error cannot succeed"),
                        };
                    }
                };
                part.pending_target_part = Some(PendingTargetPart {
                    target: target.to_path_buf(),
                    part_name: part
                        .name
                        .as_ref()
                        .expect("pending file name is available")
                        .to_string_lossy()
                        .into_owned(),
                    identity,
                });
            }
            if let Err(error) = after_create(&mut part) {
                return match part.finish(Err(error)) {
                    Err(error) => Err(error),
                    Ok(()) => unreachable!("finishing an error cannot succeed"),
                };
            }
            Ok(part)
        }
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("pending file is available")
    }

    #[cfg(target_os = "macos")]
    fn commit_macos_exclusive_rename(&self, target: &TargetLocation) -> Result<(), CopyError> {
        use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

        let target_name =
            std::ffi::CString::new(target.name.as_bytes()).map_err(|_| CopyError::InvalidTarget)?;
        let staging = self.staging.as_ref().ok_or(CopyError::InvalidTarget)?;
        let part_name = self.name.as_ref().ok_or(CopyError::InvalidTarget)?;
        let part_name =
            std::ffi::CString::new(part_name.as_bytes()).map_err(|_| CopyError::InvalidTarget)?;
        let result = unsafe {
            libc::renameatx_np(
                staging.as_raw_fd(),
                part_name.as_ptr(),
                self.parent.as_raw_fd(),
                target_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(map_atomic_commit_error(std::io::Error::last_os_error()))
        }
    }

    fn commit(&self, target: &TargetLocation) -> Result<(), CopyError> {
        let file = self.file.as_ref().expect("pending file is available");

        #[cfg(target_os = "macos")]
        {
            use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

            let target_name = std::ffi::CString::new(target.name.as_bytes())
                .map_err(|_| CopyError::InvalidTarget)?;
            // SAFETY: both descriptors remain owned and open for the duration
            // of the call; `target_name` is NUL-terminated and has no interior
            // NUL. fclonefileat creates a new destination and never replaces it.
            let result = unsafe {
                libc::fclonefileat(
                    file.as_raw_fd(),
                    self.parent.as_raw_fd(),
                    target_name.as_ptr(),
                    0,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let clone_error = map_atomic_commit_error(std::io::Error::last_os_error());
            if !matches!(clone_error, CopyError::AtomicCommitUnsupported) {
                return Err(contextualize_target_error(
                    clone_error,
                    target.is_unc,
                    TargetOperation::Commit,
                ));
            }
            // `staging` and the target parent are both open directory handles.
            // RENAME_EXCL is an atomic no-replace move and works without
            // hardlinks/reflinks on volumes that advertise exclusive rename.
            return self.commit_macos_exclusive_rename(target).map_err(|error| {
                contextualize_target_error(error, target.is_unc, TargetOperation::Commit)
            });
        }

        #[cfg(windows)]
        {
            return windows_commit_handle(file, target).map_err(|error| {
                contextualize_target_error(error, target.is_unc, TargetOperation::Commit)
            });
        }

        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = (file, target);
            Err(CopyError::AtomicCommitUnsupported)
        }
    }

    fn finish(mut self, result: Result<(), CopyError>) -> Result<(), CopyError> {
        self.file.take();
        #[cfg(target_os = "macos")]
        {
            let result = match (self.staging.as_ref(), self.name.take()) {
                (Some(staging), Some(name)) => cleanup_part(
                    staging,
                    &name,
                    result,
                    self.target_is_unc,
                    self.pending_target_part.clone(),
                ),
                _ => result,
            };
            self.staging.take();
            return match self.staging_name.take() {
                Some(name) => match self.parent.remove_dir(&name) {
                    Ok(()) => result,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => result,
                    Err(cleanup) => Err(CopyError::CleanupFailed {
                        operation: result
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "目标已提交".to_owned()),
                        cleanup: cleanup.to_string(),
                    }),
                },
                None => result,
            };
        }
        #[cfg(windows)]
        match self.name.take() {
            Some(name) => finish_windows_pending_part_with(result, |result| {
                cleanup_part(
                    &self.parent,
                    &name,
                    result,
                    self.target_is_unc,
                    self.pending_target_part.take(),
                )
            }),
            None => result,
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        match self.name.take() {
            Some(name) => cleanup_part(
                &self.parent,
                &name,
                result,
                self.target_is_unc,
                self.pending_target_part.take(),
            ),
            None => result,
        }
    }
}

impl Drop for PendingPart {
    fn drop(&mut self) {
        self.file.take();
        #[cfg(target_os = "macos")]
        {
            if let (Some(staging), Some(name)) = (self.staging.as_ref(), self.name.take()) {
                let _ = staging.remove_file(name);
            }
            self.staging.take();
            if let Some(name) = self.staging_name.take() {
                let _ = self.parent.remove_dir(name);
            }
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(name) = self.name.take() {
            let _ = self.parent.remove_file(name);
        }
    }
}

fn map_atomic_commit_error(error: std::io::Error) -> CopyError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return CopyError::TargetAlreadyExists;
    }
    #[cfg(target_os = "macos")]
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) | Some(libc::EINVAL)
    ) {
        return CopyError::AtomicCommitUnsupported;
    }
    #[cfg(windows)]
    if error.kind() == std::io::ErrorKind::Unsupported
        || matches!(error.raw_os_error(), Some(1 | 17 | 50 | 87))
    {
        return CopyError::AtomicCommitUnsupported;
    }
    CopyError::Io(error)
}

#[cfg(windows)]
fn windows_rename_target(
    path: Vec<u16>,
) -> Result<(std::os::windows::io::RawHandle, Vec<u16>), std::io::Error> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Windows atomic commit target must be a single file name",
        )
    };
    if path.is_empty() || path.contains(&0) {
        return Err(invalid());
    }
    Ok((std::ptr::null_mut(), path))
}

#[cfg(windows)]
fn windows_rename_info_buffer(
    root: std::os::windows::io::RawHandle,
    name: &[u16],
) -> Result<(Vec<usize>, u32), std::io::Error> {
    use std::mem::size_of;
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    let name_bytes = name
        .len()
        .checked_add(1)
        .and_then(|length| length.checked_mul(size_of::<u16>()))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name too long")
        })?;
    let uninit = std::mem::MaybeUninit::<FILE_RENAME_INFO>::uninit();
    let base = uninit.as_ptr();
    let file_name_offset =
        unsafe { std::ptr::addr_of!((*base).FileName) as usize - base.cast::<u8>() as usize };
    let buffer_size = file_name_offset.checked_add(name_bytes).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name too long")
    })?;
    let words = buffer_size
        .checked_add(size_of::<usize>() - 1)
        .map(|value| value / size_of::<usize>())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "target name too long")
        })?;
    let mut buffer = vec![0_usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = root as _;
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    Ok((buffer, buffer_size as u32))
}

#[cfg(windows)]
fn windows_commit_handle(file: &File, target: &TargetLocation) -> Result<(), CopyError> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};

    fn rename_to(
        file: &File,
        root: std::os::windows::io::RawHandle,
        name: &[u16],
    ) -> Result<(), std::io::Error> {
        use windows_sys::Win32::Storage::FileSystem::{
            FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO,
        };

        let (mut buffer, buffer_size) = windows_rename_info_buffer(root, name)?;
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            if SetFileInformationByHandle(
                file.as_raw_handle() as _,
                FileRenameInfo,
                info.cast(),
                buffer_size,
            ) != 0
            {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }

    let target_path = target.display.as_os_str().encode_wide().collect::<Vec<_>>();
    let (root, name) = windows_rename_target(target_path).map_err(map_atomic_commit_error)?;
    rename_to(file, root, &name).map_err(map_atomic_commit_error)
}

#[cfg(windows)]
fn windows_delete_file_on_close(file: &File) -> std::io::Result<()> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfo,
            std::ptr::addr_of!(disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn probe_atomic_commit(root: &TargetRoot) -> Result<(), CopyError> {
    let target_name = OsString::from(format!(".photo-selector-probe-{}", Uuid::new_v4()));
    let target = TargetLocation {
        parent: root
            .dir
            .try_clone()
            .map_err(|error| map_target_io_error(error, root.is_unc, TargetOperation::Traverse))?,
        name: target_name.clone(),
        display: root.display_root.join(&target_name),
        is_unc: root.is_unc,
    };
    let mut part = PendingPart::create_for_target_context(&target)?;
    part.file_mut()
        .write_all(b"atomic-commit-probe")
        .map_err(|error| map_target_io_error(error, root.is_unc, TargetOperation::Write))?;
    part.file_mut()
        .sync_all()
        .map_err(|error| map_target_io_error(error, root.is_unc, TargetOperation::Sync))?;
    let committed = part.commit(&target);
    #[cfg(windows)]
    let committed = committed.and_then(|()| {
        windows_delete_file_on_close(part.file_mut())
            .map_err(|error| map_target_io_error(error, root.is_unc, TargetOperation::Cleanup))
    });
    let committed = part.finish(committed);
    #[cfg(windows)]
    if committed.is_ok() {
        return match probe_target_child_absent(&target.display) {
            Ok(true) => Ok(()),
            Ok(false) => Err(CopyError::InvalidTarget),
            Err(error) => Err(map_target_io_error(
                error,
                root.is_unc,
                TargetOperation::Cleanup,
            )),
        };
    }
    #[cfg(not(windows))]
    if committed.is_ok() {
        if let Err(cleanup) = root.dir.remove_file(&target_name) {
            if is_target_disconnect_error(&cleanup, root.is_unc) {
                return Err(map_target_io_error(
                    cleanup,
                    root.is_unc,
                    TargetOperation::Cleanup,
                ));
            }
            return Err(CopyError::CleanupFailed {
                operation: "原子提交能力探针已创建".to_owned(),
                cleanup: cleanup.to_string(),
            });
        }
    }
    committed
}

fn copy_one_at(
    mut source_file: File,
    expected_source: &FileFingerprint,
    target: &TargetLocation,
    source_is_unc: bool,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<String, CopyError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(CopyError::Cancelled);
    }
    match target.parent.symlink_metadata(&target.name) {
        Ok(_) => return Err(CopyError::InvalidTarget),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error)
            if target.is_unc
                && error.raw_os_error() == Some(3)
                && probe_target_child_absent(&target.display).map_err(|probe_error| {
                    map_target_io_error(probe_error, target.is_unc, TargetOperation::Metadata)
                })? => {}
        Err(error) => {
            return Err(map_target_io_error(
                error,
                target.is_unc,
                TargetOperation::Metadata,
            ));
        }
    }

    if !source_file
        .metadata()
        .map_err(|error| map_source_open_error(error, source_is_unc))?
        .is_file()
    {
        return Err(CopyError::InvalidSource);
    }
    if !source_metadata_matches_for_operation(&source_file, expected_source, source_is_unc)? {
        return Err(CopyError::SourceChanged);
    }
    let before = source_snapshot_for_operation(&source_file, source_is_unc)?;
    let mut part = PendingPart::create_for_target(target)?;

    let result = match catch_unwind(AssertUnwindSafe(|| -> Result<String, CopyError> {
        let copied_hash = copy_stream_to(
            &mut source_file,
            part.file_mut(),
            source_is_unc,
            target.is_unc,
            cancelled,
            &mut progress,
        )?;
        if !expected_source.content_hash.is_empty()
            && copied_hash.to_hex().as_str() != expected_source.content_hash
        {
            return Err(CopyError::SourceChanged);
        }
        part.file_mut()
            .sync_all()
            .map_err(|error| map_target_io_error(error, target.is_unc, TargetOperation::Sync))?;
        part.file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|error| map_target_io_error(error, target.is_unc, TargetOperation::Reopen))?;
        if hash_target_reader_cancellable(part.file_mut(), target.is_unc, cancelled)? != copied_hash
        {
            return Err(CopyError::HashMismatch);
        }
        if source_snapshot_for_operation(&source_file, source_is_unc)? != before {
            return Err(CopyError::SourceChanged);
        }
        if !source_metadata_matches_for_operation(&source_file, expected_source, source_is_unc)? {
            return Err(CopyError::SourceChanged);
        }
        part.commit(target)?;
        Ok(copied_hash.to_hex().to_string())
    })) {
        Ok(result) => result,
        Err(_) => Err(CopyError::ProgressPanicked),
    };
    match result {
        Ok(source_hash) => {
            part.finish(Ok(()))?;
            Ok(source_hash)
        }
        Err(error) => match part.finish(Err(error)) {
            Err(error) => Err(error),
            Ok(()) => unreachable!("finishing an error cannot succeed"),
        },
    }
}

#[cfg(test)]
fn copy_one(
    source: &Path,
    target: &Path,
    cancelled: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<(), CopyError> {
    let source_file = open_source(source)?;
    let mut fingerprint = source_fingerprint(&source_file)?;
    fingerprint.content_hash = hash_reader(open_source(source)?)?.to_hex().to_string();
    let parent = target.parent().ok_or(CopyError::InvalidTarget)?;
    let root = open_target_root(parent)?;
    let file_name = target.file_name().ok_or(CopyError::InvalidTarget)?;
    let location = locate_target(&root, Path::new(file_name), true)?;
    copy_one_at(
        source_file,
        &fingerprint,
        &location,
        false,
        cancelled,
        progress,
    )
    .map(|_| ())
}

pub(crate) fn cleanup_pending_target_parts(
    plan: &CopyPlan,
    pending_parts: &[PendingTargetPart],
) -> Result<(), CopyError> {
    if pending_parts.is_empty() {
        return Ok(());
    }
    let Some(root) = open_cleanup_target_root(&plan.target_root)? else {
        return Ok(());
    };
    for pending in pending_parts {
        if !plan.files.iter().any(|file| file.target == pending.target) {
            return Err(CopyError::InvalidTarget);
        }
        let part_name = Path::new(&pending.part_name);
        if !pending.part_name.starts_with(".photo-selector.part-")
            || part_name.components().count() != 1
            || !matches!(part_name.components().next(), Some(Component::Normal(_)))
        {
            return Err(CopyError::InvalidTarget);
        }
        let Some(planned_target) = locate_cleanup_target(&root, &pending.target)? else {
            continue;
        };
        let part = TargetLocation {
            parent: planned_target.parent.try_clone().map_err(|error| {
                map_target_cleanup_error(error, root.is_unc, TargetOperation::Cleanup)
            })?,
            name: OsString::from(&pending.part_name),
            display: planned_target
                .display
                .parent()
                .ok_or(CopyError::InvalidTarget)?
                .join(&pending.part_name),
            is_unc: root.is_unc,
        };
        let Some(file) = open_cleanup_file_nofollow(&part)? else {
            continue;
        };
        let Some(actual) = cleanup_file_identity(&file, &part.parent, &part.name, root.is_unc)?
        else {
            continue;
        };
        if actual != pending.identity {
            return Err(CopyError::InvalidTarget);
        }
        drop(file);
        confirm_cleanup_parent_connected(&part.parent, root.is_unc)?;
        let _removed = cleanup_child_result_after_parent_proof(
            part.parent.remove_file(&part.name),
            &part.parent,
            &part.name,
            root.is_unc,
            TargetOperation::Cleanup,
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn preflight_copy(plan: &CopyPlan) -> Result<PreflightReport, CopyError> {
    preflight_copy_cancellable(plan, &AtomicBool::new(false))
}

pub fn preflight_copy_cancellable(
    plan: &CopyPlan,
    cancelled: &AtomicBool,
) -> Result<PreflightReport, CopyError> {
    preflight_copy_cancellable_using(plan, cancelled, |source, source_is_unc, cancelled| {
        hash_source_reader_cancellable(source, source_is_unc, cancelled)
    })
}

fn preflight_copy_cancellable_using(
    plan: &CopyPlan,
    cancelled: &AtomicBool,
    mut hash_source: impl FnMut(&mut File, bool, &AtomicBool) -> Result<blake3::Hash, CopyError>,
) -> Result<PreflightReport, CopyError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(CopyError::Cancelled);
    }
    let mut report = PreflightReport::default();
    let source_root = match open_source_root(&plan.source_root, &plan.source_root_identity) {
        Ok(root) => root,
        Err(CopyError::SourceDisconnected) => {
            report.source_disconnected = true;
            return Ok(report);
        }
        Err(CopyError::InvalidSource | CopyError::SourceChanged) => {
            report.source_changed = true;
            return Ok(report);
        }
        Err(error) => return Err(error),
    };
    let target_root = match open_target_root(&plan.target_root) {
        Ok(root) => root,
        Err(CopyError::TargetDisconnected { info, pending_part }) => {
            record_preflight_target_disconnect(&mut report, info, pending_part);
            return Ok(report);
        }
        Err(error) => return Err(error),
    };
    match probe_atomic_commit(&target_root) {
        Ok(()) => {}
        Err(CopyError::AtomicCommitUnsupported) => {
            report.atomic_commit_unsupported = true;
            return Ok(report);
        }
        Err(CopyError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            report.permission_denied = true;
            return Ok(report);
        }
        Err(CopyError::TargetDisconnected { info, pending_part }) => {
            record_preflight_target_disconnect(&mut report, info, pending_part);
            return Ok(report);
        }
        Err(error) => return Err(error),
    }

    let mut required = 0_u64;
    for item in &plan.files {
        if cancelled.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let source_file = match open_planned_source(&source_root, item) {
            Ok(file) => file,
            Err(CopyError::SourceDisconnected) => {
                report.source_disconnected = true;
                return Ok(report);
            }
            Err(CopyError::InvalidSource | CopyError::SourceChanged) => {
                report.source_changed = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut source_file = source_file;
        let source_changed = (|| -> Result<bool, CopyError> {
            if item.fingerprint.content_hash.is_empty() {
                return Ok(false);
            }
            let before = source_snapshot_for_operation(&source_file, source_root.is_unc)?;
            let source_hash = hash_source(&mut source_file, source_root.is_unc, cancelled)?;
            let changed = source_hash.to_hex().as_str() != item.fingerprint.content_hash
                || source_snapshot_for_operation(&source_file, source_root.is_unc)? != before
                || !source_metadata_matches_for_operation(
                    &source_file,
                    &item.fingerprint,
                    source_root.is_unc,
                )?;
            source_file
                .seek(SeekFrom::Start(0))
                .map_err(|error| map_source_open_error(error, source_root.is_unc))?;
            Ok(changed)
        })();
        match source_changed {
            Ok(true) | Err(CopyError::InvalidSource | CopyError::SourceChanged) => {
                report.source_changed = true;
                continue;
            }
            Err(CopyError::SourceDisconnected) => {
                report.source_disconnected = true;
                return Ok(report);
            }
            Err(error) => return Err(error),
            Ok(false) => {}
        }
        let target = match locate_target(&target_root, &item.target, true) {
            Ok(target) => target,
            Err(CopyError::TargetDisconnected { info, pending_part }) => {
                record_preflight_target_disconnect(&mut report, info, pending_part);
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
        let existing = match open_existing_target_nofollow(&target) {
            Ok(existing) => existing,
            Err(CopyError::TargetDisconnected { info, pending_part }) => {
                record_preflight_target_disconnect(&mut report, info, pending_part);
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
        match existing {
            Some(target_file) => match compare_planned_source_to_target(
                source_file,
                target_file,
                &item.fingerprint,
                source_root.is_unc,
                target.is_unc,
                cancelled,
            ) {
                Ok(TargetConflict::Identical) => report
                    .identical
                    .push(target.display.to_string_lossy().into_owned()),
                Ok(TargetConflict::Different) => report
                    .conflicts
                    .push(target.display.to_string_lossy().into_owned()),
                Err(CopyError::SourceDisconnected) => {
                    report.source_disconnected = true;
                    return Ok(report);
                }
                Err(CopyError::TargetDisconnected { info, pending_part }) => {
                    record_preflight_target_disconnect(&mut report, info, pending_part);
                    return Ok(report);
                }
                Err(CopyError::InvalidSource | CopyError::SourceChanged) => {
                    report.source_changed = true
                }
                Err(error) => return Err(error),
            },
            None => {
                required = required.saturating_add(item.fingerprint.size);
            }
        }
    }
    report.insufficient_space =
        match fs2::available_space(&target_root.display_root).map_err(|error| {
            map_target_io_error(error, target_root.is_unc, TargetOperation::AvailableSpace)
        }) {
            Ok(available) => available < required,
            Err(CopyError::TargetDisconnected { info, pending_part }) => {
                record_preflight_target_disconnect(&mut report, info, pending_part);
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
    Ok(report)
}

pub fn execute_copy_with_updates(
    plan: &CopyPlan,
    cancelled: &AtomicBool,
    progress: impl FnMut(&Path, u64),
    item_update: impl FnMut(&SelectedFile, CopyItemOutcome),
) -> CopyReport {
    execute_copy_with_updates_using(
        plan,
        cancelled,
        progress,
        item_update,
        |source_file, item, target, source_is_unc, cancelled, progress| {
            copy_one_at(
                source_file,
                &item.fingerprint,
                target,
                source_is_unc,
                cancelled,
                progress,
            )
        },
    )
}

fn execute_copy_with_updates_using(
    plan: &CopyPlan,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(&Path, u64),
    mut item_update: impl FnMut(&SelectedFile, CopyItemOutcome),
    mut copy_missing: impl FnMut(
        File,
        &SelectedFile,
        &TargetLocation,
        bool,
        &AtomicBool,
        &mut dyn FnMut(u64),
    ) -> Result<String, CopyError>,
) -> CopyReport {
    let mut report = CopyReport::default();
    let source_root = match open_source_root(&plan.source_root, &plan.source_root_identity) {
        Ok(root) => root,
        Err(CopyError::SourceDisconnected) => {
            report.stop_reason = Some(CopyStopReason::SourceDisconnected);
            return report;
        }
        Err(error) => {
            for item in &plan.files {
                item_update(
                    item,
                    CopyItemOutcome::Failed {
                        code: error.code().into(),
                        summary: error.to_string(),
                    },
                );
            }
            report.failed.push((
                plan.source_root.to_string_lossy().into_owned(),
                error.to_string(),
            ));
            report.stop_reason = Some(CopyStopReason::Other);
            return report;
        }
    };
    let target_root = match open_target_root(&plan.target_root) {
        Ok(root) => root,
        Err(CopyError::TargetDisconnected { info, pending_part }) => {
            record_copy_target_disconnect(&mut report, info, pending_part);
            return report;
        }
        Err(error) => {
            for item in &plan.files {
                item_update(
                    item,
                    CopyItemOutcome::Failed {
                        code: error.code().into(),
                        summary: error.to_string(),
                    },
                );
            }
            report.failed.push((
                plan.target_root.to_string_lossy().into_owned(),
                error.to_string(),
            ));
            report.stop_reason = Some(CopyStopReason::Other);
            return report;
        }
    };
    for item in &plan.files {
        item_update(item, CopyItemOutcome::Started);
        if cancelled.load(Ordering::Relaxed) {
            item_update(item, CopyItemOutcome::Cancelled);
            report.cancelled = true;
            break;
        }
        let source_file = match open_planned_source(&source_root, item) {
            Ok(file) => file,
            Err(CopyError::SourceDisconnected) => {
                item_update(item, CopyItemOutcome::Interrupted);
                report.stop_reason = Some(CopyStopReason::SourceDisconnected);
                break;
            }
            Err(error) => {
                item_update(
                    item,
                    CopyItemOutcome::Failed {
                        code: error.code().into(),
                        summary: error.to_string(),
                    },
                );
                report.failed.push((
                    item.source.to_string_lossy().into_owned(),
                    error.to_string(),
                ));
                report.stop_reason = Some(CopyStopReason::Other);
                break;
            }
        };
        let target = match locate_target(&target_root, &item.target, true) {
            Ok(target) => target,
            Err(CopyError::TargetDisconnected { info, pending_part }) => {
                item_update(item, CopyItemOutcome::Interrupted);
                record_copy_target_disconnect(&mut report, info, pending_part);
                break;
            }
            Err(error) => {
                item_update(
                    item,
                    CopyItemOutcome::Failed {
                        code: error.code().into(),
                        summary: error.to_string(),
                    },
                );
                report.failed.push((
                    item.target.to_string_lossy().into_owned(),
                    error.to_string(),
                ));
                report.stop_reason = Some(CopyStopReason::Other);
                break;
            }
        };
        match open_existing_target_nofollow(&target) {
            Ok(Some(target_file)) => {
                match compare_planned_source_to_target(
                    source_file,
                    target_file,
                    &item.fingerprint,
                    source_root.is_unc,
                    target.is_unc,
                    cancelled,
                ) {
                    Ok(TargetConflict::Identical) => {
                        item_update(
                            item,
                            CopyItemOutcome::SkippedIdentical {
                                source_hash: item.fingerprint.content_hash.clone(),
                            },
                        );
                        report
                            .skipped_identical
                            .push(target.display.to_string_lossy().into_owned());
                        continue;
                    }
                    Ok(TargetConflict::Different) => {
                        item_update(
                            item,
                            CopyItemOutcome::Failed {
                                code: "target-conflict".into(),
                                summary: "target exists with different contents".into(),
                            },
                        );
                        report.failed.push((
                            target.display.to_string_lossy().into_owned(),
                            "target-conflict".into(),
                        ));
                        report.stop_reason = Some(CopyStopReason::Other);
                    }
                    Err(CopyError::SourceDisconnected) => {
                        item_update(item, CopyItemOutcome::Interrupted);
                        report.stop_reason = Some(CopyStopReason::SourceDisconnected);
                    }
                    Err(CopyError::TargetDisconnected { info, pending_part }) => {
                        item_update(item, CopyItemOutcome::Interrupted);
                        record_copy_target_disconnect(&mut report, info, pending_part);
                    }
                    Err(error) => {
                        item_update(
                            item,
                            CopyItemOutcome::Failed {
                                code: error.code().into(),
                                summary: error.to_string(),
                            },
                        );
                        report.failed.push((
                            target.display.to_string_lossy().into_owned(),
                            error.to_string(),
                        ));
                        report.stop_reason = Some(CopyStopReason::Other);
                    }
                }
            }
            Ok(None) => {
                let mut item_progress = |bytes| progress(&item.source, bytes);
                match open_planned_source(&source_root, item).and_then(|source_file| {
                    copy_missing(
                        source_file,
                        item,
                        &target,
                        source_root.is_unc,
                        cancelled,
                        &mut item_progress,
                    )
                }) {
                    Ok(source_hash) => {
                        item_update(item, CopyItemOutcome::Copied { source_hash });
                        report
                            .copied
                            .push(target.display.to_string_lossy().into_owned())
                    }
                    Err(CopyError::Cancelled) => {
                        item_update(item, CopyItemOutcome::Cancelled);
                        report.cancelled = true;
                        break;
                    }
                    Err(CopyError::SourceDisconnected) => {
                        item_update(item, CopyItemOutcome::Interrupted);
                        report.stop_reason = Some(CopyStopReason::SourceDisconnected);
                        break;
                    }
                    Err(CopyError::TargetDisconnected { info, pending_part }) => {
                        item_update(item, CopyItemOutcome::Interrupted);
                        record_copy_target_disconnect(&mut report, info, pending_part);
                        break;
                    }
                    Err(error) => {
                        item_update(
                            item,
                            CopyItemOutcome::Failed {
                                code: error.code().into(),
                                summary: error.to_string(),
                            },
                        );
                        report.failed.push((
                            target.display.to_string_lossy().into_owned(),
                            error.to_string(),
                        ));
                        report.stop_reason = Some(CopyStopReason::Other);
                        break;
                    }
                }
                continue;
            }
            Err(CopyError::TargetDisconnected { info, pending_part }) => {
                item_update(item, CopyItemOutcome::Interrupted);
                record_copy_target_disconnect(&mut report, info, pending_part);
                break;
            }
            Err(error) => {
                item_update(
                    item,
                    CopyItemOutcome::Failed {
                        code: error.code().into(),
                        summary: error.to_string(),
                    },
                );
                report.failed.push((
                    target.display.to_string_lossy().into_owned(),
                    error.to_string(),
                ));
                report.stop_reason = Some(CopyStopReason::Other);
                break;
            }
        }
        if report.stop_reason.is_some() {
            break;
        }
    }
    report
}
