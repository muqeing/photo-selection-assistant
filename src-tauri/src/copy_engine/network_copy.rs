//! Mac SMB cannot always publish a staged file with a no-replace rename.
//! Reserve the final name exclusively and keep a persistent incomplete record
//! until a fresh read of that name has passed verification. Never delete a
//! partially written final file, including during unwinding or cancellation.
use super::*;

const RECORD_DIR: &str = ".photo-selector-incomplete";

pub(super) fn is_smb(dir: &Dir) -> Result<bool, CopyError> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CStr, mem::MaybeUninit, os::fd::AsRawFd};
        let mut stats = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the live directory owns the fd and statfs initializes stats.
        if unsafe { libc::fstatfs(dir.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stats = unsafe { stats.assume_init() };
        let name = unsafe { CStr::from_ptr(stats.f_fstypename.as_ptr()) };
        Ok(name.to_bytes() == b"smbfs")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = dir;
        Ok(false)
    }
}

fn records(target: &TargetLocation, create: bool) -> Result<Option<Dir>, CopyError> {
    match target.parent.symlink_metadata(RECORD_DIR) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err(CopyError::InvalidTarget),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(None);
            }
            match target.parent.create_dir(RECORD_DIR) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    open_child_dir_nofollow(&target.parent, OsStr::new(RECORD_DIR), target.is_unc).map(Some)
}

pub(super) fn has_incomplete(target: &TargetLocation) -> Result<bool, CopyError> {
    has_incomplete_inner(target).map_err(|error| {
        contextualize_target_error(error, target.is_unc, TargetOperation::Metadata)
    })
}

fn has_incomplete_inner(target: &TargetLocation) -> Result<bool, CopyError> {
    let Some(dir) = records(target, false)? else {
        return Ok(false);
    };
    // Merely existing is sufficient: never follow, parse or trust an old record.
    // The same basename also preserves the volume's case/Unicode name semantics.
    match dir.symlink_metadata(&target.name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn create_exclusive(dir: &Dir, name: &OsStr) -> Result<File, CopyError> {
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    dir.open_with(name, &options)
        .map(|file| file.into_std())
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CopyError::TargetAlreadyExists
            } else {
                error.into()
            }
        })
}

#[cfg(target_os = "macos")]
fn reopen_same(dir: &Dir, name: &OsStr, original: &File) -> Result<File, CopyError> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let current = dir.open_with(name, &options)?.into_std();
    if !current.metadata()?.is_file()
        || source_fingerprint(&current)?.identity != source_fingerprint(original)?.identity
    {
        return Err(CopyError::InvalidTarget);
    }
    Ok(current)
}

#[cfg(target_os = "macos")]
pub(super) fn probe(root: &TargetRoot) -> Result<(), CopyError> {
    let name = OsString::from(format!(".photo-selector-probe-{}", Uuid::new_v4()));
    let mut file = create_exclusive(&root.dir, &name)?;
    let result = (|| {
        file.write_all(b"network-copy-probe")?;
        sync_target_file(&file)?;
        // Check exclusive creation against the server, not just client metadata.
        match create_exclusive(&root.dir, &name) {
            Err(CopyError::TargetAlreadyExists) => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(CopyError::AtomicCommitUnsupported),
        }
        let mut readback = Vec::new();
        reopen_same(&root.dir, &name, &file)?.read_to_end(&mut readback)?;
        if readback != b"network-copy-probe" {
            return Err(CopyError::HashMismatch);
        }
        Ok(())
    })();
    // Only remove the anonymous file if its current name still binds our handle.
    reopen_same(&root.dir, &name, &file)?;
    root.dir.remove_file(&name)?;
    result
}

#[cfg(target_os = "macos")]
pub(super) fn copy(
    source: File,
    expected: &FileFingerprint,
    target: &TargetLocation,
    source_is_unc: bool,
    cancelled: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<String, CopyError> {
    copy_with_sync(
        source,
        expected,
        target,
        source_is_unc,
        cancelled,
        progress,
        sync_target_file,
    )
}

#[cfg(target_os = "macos")]
fn copy_with_sync(
    mut source: File,
    expected: &FileFingerprint,
    target: &TargetLocation,
    source_is_unc: bool,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64),
    sync: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<String, CopyError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(CopyError::Cancelled);
    }
    if !source_metadata_matches_for_operation(&source, expected, source_is_unc)? {
        return Err(CopyError::SourceChanged);
    }
    let before = source_snapshot_for_operation(&source, source_is_unc)?;
    if has_incomplete(target)? {
        return Err(CopyError::NetworkCopyIncomplete {
            reason: "存在上次未完成的复制记录".into(),
        });
    }
    // Keep this directory permanently. Removing an empty directory would race
    // another process holding its handle and could orphan that process's record.
    let records = records(target, true)?.ok_or(CopyError::InvalidTarget)?;
    let mut record = create_exclusive(&records, &target.name).map_err(|error| match error {
        CopyError::TargetAlreadyExists => CopyError::NetworkCopyIncomplete {
            reason: "另一个复制任务已占用此目标".into(),
        },
        error => error,
    })?;
    let record_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "status": "incomplete",
        "owner": Uuid::new_v4().to_string(),
        "sourceFingerprint": expected,
        "instruction": "Do not use this target until verified. Retain the target and this record on failure.",
    })).map_err(|error| CopyError::Io(std::io::Error::other(error)))?;
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<String, CopyError> {
        record.write_all(&record_bytes)?;
        sync_target_file(&record)?;
        // Verify the marker directory is still attached before making a target.
        let current_records = self::records(target, false)?.ok_or(CopyError::InvalidTarget)?;
        verify_record(&current_records, &target.name, &record, &record_bytes)?;
        let mut output = create_exclusive(&target.parent, &target.name)?;
        let copied_hash = copy_stream_to(
            &mut source,
            &mut output,
            source_is_unc,
            target.is_unc,
            cancelled,
            &mut progress,
        )?;
        if !expected.content_hash.is_empty()
            && copied_hash.to_hex().as_str() != expected.content_hash
        {
            return Err(CopyError::SourceChanged);
        }
        sync(&output)?;
        let mut readback = reopen_same(&target.parent, &target.name, &output)?;
        if hash_target_reader_cancellable(&mut readback, target.is_unc, cancelled)? != copied_hash
            || readback.metadata()?.len() != before.len
        {
            return Err(CopyError::HashMismatch);
        }
        if source_snapshot_for_operation(&source, source_is_unc)? != before
            || !source_metadata_matches_for_operation(&source, expected, source_is_unc)?
        {
            return Err(CopyError::SourceChanged);
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        reopen_same(&target.parent, &target.name, &output)?;
        finish_record(target, &record, &record_bytes, |dir, name| {
            dir.remove_file(name)
        })?;
        Ok(copied_hash.to_hex().to_string())
    }));
    match result {
        Ok(Ok(hash)) => Ok(hash),
        // Even a cancelled copy can have created the final name. Persist failure
        // with an actionable message, rather than a harmless cancellation label.
        Ok(Err(error)) => Err(CopyError::NetworkCopyIncomplete {
            reason: error.to_string(),
        }),
        Err(_) => Err(CopyError::NetworkCopyIncomplete {
            reason: "复制进度回调异常".into(),
        }),
    }
}

#[cfg(target_os = "macos")]
fn finish_record(
    target: &TargetLocation,
    record: &File,
    expected: &[u8],
    remove: impl FnOnce(&Dir, &OsStr) -> std::io::Result<()>,
) -> Result<(), CopyError> {
    let current = records(target, false)?.ok_or(CopyError::InvalidTarget)?;
    verify_record(&current, &target.name, record, expected)?;
    let identity = source_fingerprint(&current.try_clone()?.into_std_file())?.identity;
    let removal = remove(&current, &target.name);
    // A server can apply deletion and then lose the reply. Read back the result
    // without retrying deletion. The target is already completely verified here.
    let after = records(target, false)?.ok_or(CopyError::InvalidTarget)?;
    if source_fingerprint(&after.try_clone()?.into_std_file())?.identity != identity {
        return Err(CopyError::InvalidTarget);
    }
    match after.symlink_metadata(&target.name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => match removal {
            Err(error) => Err(error.into()),
            Ok(()) => Err(CopyError::InvalidTarget),
        },
    }
}

#[cfg(target_os = "macos")]
fn verify_record(
    dir: &Dir,
    name: &OsStr,
    original: &File,
    expected: &[u8],
) -> Result<(), CopyError> {
    let current = reopen_same(dir, name, original)?;
    let mut bytes = Vec::new();
    current
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes != expected {
        return Err(CopyError::InvalidTarget);
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{Arc, Barrier},
    };

    struct Fixture {
        dir: tempfile::TempDir,
        source: PathBuf,
        target: TargetLocation,
        fingerprint: FileFingerprint,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir_in("/private/tmp").unwrap();
            let source = dir.path().join("source.JPG");
            fs::write(&source, vec![17; COPY_BUFFER_BYTES + 99]).unwrap();
            let mut fingerprint = source_fingerprint(&File::open(&source).unwrap()).unwrap();
            fingerprint.content_hash = hash_file(&source).unwrap().to_hex().to_string();
            let target = TargetLocation {
                parent: Dir::open_ambient_dir(dir.path(), ambient_authority()).unwrap(),
                name: OsString::from("target.JPG"),
                display: dir.path().join("target.JPG"),
                is_unc: false,
            };
            Self {
                dir,
                source,
                target,
                fingerprint,
            }
        }

        fn run(
            &self,
            cancelled: &AtomicBool,
            progress: impl FnMut(u64),
        ) -> Result<String, CopyError> {
            copy(
                File::open(&self.source).unwrap(),
                &self.fingerprint,
                &self.target,
                false,
                cancelled,
                progress,
            )
        }

        fn record_path(&self) -> PathBuf {
            self.dir.path().join(RECORD_DIR).join(&self.target.name)
        }
    }

    #[test]
    fn network_success_verifies_and_removes_only_record() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run(&AtomicBool::new(false), |_| {}).unwrap(),
            fixture.fingerprint.content_hash
        );
        assert_eq!(
            hash_file(&fixture.source).unwrap(),
            hash_file(&fixture.target.display).unwrap()
        );
        assert!(!fixture.record_path().exists());
        assert!(fixture.record_path().parent().unwrap().is_dir());
    }

    #[test]
    fn network_existing_target_is_never_overwritten() {
        let fixture = Fixture::new();
        fs::write(&fixture.target.display, b"owned by another writer").unwrap();
        let error = fixture.run(&AtomicBool::new(false), |_| {}).unwrap_err();
        assert_eq!(error.code(), "network-copy-incomplete");
        assert_eq!(
            fs::read(&fixture.target.display).unwrap(),
            b"owned by another writer"
        );
        assert!(fixture.record_path().exists());
    }

    #[test]
    fn network_records_block_identical_different_and_missing_targets() {
        for content in [
            None,
            Some(b"different".as_slice()),
            Some(vec![17; COPY_BUFFER_BYTES + 99].as_slice()),
        ] {
            let fixture = Fixture::new();
            fs::create_dir(fixture.record_path().parent().unwrap()).unwrap();
            fs::write(fixture.record_path(), b"previous unfinished operation").unwrap();
            if let Some(content) = content {
                fs::write(&fixture.target.display, content).unwrap();
            }
            assert!(matches!(
                open_existing_target_nofollow(&fixture.target),
                Err(CopyError::NetworkCopyIncomplete { .. })
            ));
            assert_eq!(
                fixture
                    .run(&AtomicBool::new(false), |_| {})
                    .unwrap_err()
                    .code(),
                "network-copy-incomplete"
            );
            assert_eq!(
                fs::read(fixture.record_path()).unwrap(),
                b"previous unfinished operation"
            );
        }
    }

    #[test]
    fn network_failures_retain_target_and_record_without_success() {
        for failure in [
            "cancel",
            "panic",
            "sync",
            "hash",
            "source",
            "record",
            "target-replaced",
        ] {
            let fixture = Fixture::new();
            let cancelled = AtomicBool::new(false);
            let mut injected = false;
            let error = copy_with_sync(
                File::open(&fixture.source).unwrap(),
                &fixture.fingerprint,
                &fixture.target,
                false,
                &cancelled,
                |_| {
                    if injected {
                        return;
                    }
                    injected = true;
                    match failure {
                        "cancel" => cancelled.store(true, Ordering::Relaxed),
                        "panic" => panic!("synthetic callback failure"),
                        "hash" => {
                            fs::write(&fixture.target.display, b"corrupt").unwrap();
                        }
                        "source" => {
                            fs::write(&fixture.source, b"changed source").unwrap();
                        }
                        "record" => {
                            fs::write(fixture.record_path(), b"different owner").unwrap();
                        }
                        "target-replaced" => {
                            fs::rename(
                                &fixture.target.display,
                                fixture.dir.path().join("retained-partial"),
                            )
                            .unwrap();
                            fs::write(&fixture.target.display, b"other owner").unwrap();
                        }
                        _ => {}
                    }
                },
                |file| {
                    if failure == "sync" {
                        Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
                    } else {
                        sync_target_file(file)
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.code(), "network-copy-incomplete", "{failure}");
            assert!(fixture.target.display.exists(), "{failure}");
            assert!(fixture.record_path().exists(), "{failure}");
            assert!(network_copy::has_incomplete(&fixture.target).unwrap());
            if failure == "record" {
                assert_eq!(fs::read(fixture.record_path()).unwrap(), b"different owner");
            }
            if failure == "target-replaced" {
                assert_eq!(fs::read(&fixture.target.display).unwrap(), b"other owner");
            }
            if failure != "source" {
                assert_eq!(
                    hash_file(&fixture.source).unwrap().to_hex().as_str(),
                    fixture.fingerprint.content_hash
                );
            }
        }
    }

    #[test]
    fn network_never_follows_record_symlinks_or_replaces_reserved_files() {
        use std::os::unix::fs::symlink;
        for kind in ["directory-link", "record-link", "directory-file"] {
            let fixture = Fixture::new();
            let outside = tempfile::tempdir_in("/private/tmp").unwrap();
            let sentinel = outside.path().join("sentinel");
            fs::write(&sentinel, b"untouched").unwrap();
            match kind {
                "directory-link" => {
                    symlink(outside.path(), fixture.record_path().parent().unwrap()).unwrap()
                }
                "record-link" => {
                    fs::create_dir(fixture.record_path().parent().unwrap()).unwrap();
                    symlink(&sentinel, fixture.record_path()).unwrap();
                }
                _ => fs::write(fixture.record_path().parent().unwrap(), b"user file").unwrap(),
            }
            assert!(fixture.run(&AtomicBool::new(false), |_| {}).is_err());
            assert!(!fixture.target.display.exists());
            assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        }
    }

    #[test]
    fn network_concurrent_writer_cannot_take_over_active_target() {
        let fixture = Arc::new(Fixture::new());
        let entered = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let fixture = Arc::clone(&fixture);
            let entered = Arc::clone(&entered);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                let mut first = true;
                fixture.run(&AtomicBool::new(false), |_| {
                    if first {
                        first = false;
                        entered.wait();
                        resume.wait();
                    }
                })
            })
        };
        entered.wait();
        let second = fixture.run(&AtomicBool::new(false), |_| {
            panic!("second writer must not write")
        });
        resume.wait();
        assert_eq!(second.unwrap_err().code(), "network-copy-incomplete");
        assert!(worker.join().unwrap().is_ok());
        assert_eq!(
            hash_file(&fixture.source).unwrap(),
            hash_file(&fixture.target.display).unwrap()
        );
        assert!(!fixture.record_path().exists());
    }

    #[test]
    fn network_record_deletion_reads_back_unknown_result_without_retry() {
        for applied in [false, true] {
            let fixture = Fixture::new();
            let dir = records(&fixture.target, true).unwrap().unwrap();
            let mut record = create_exclusive(&dir, &fixture.target.name).unwrap();
            record.write_all(b"owned test record").unwrap();
            let calls = std::cell::Cell::new(0);
            let result = finish_record(
                &fixture.target,
                &record,
                b"owned test record",
                |dir, name| {
                    calls.set(calls.get() + 1);
                    if applied {
                        dir.remove_file(name)?;
                    }
                    Err(std::io::Error::from_raw_os_error(libc::EIO))
                },
            );
            assert_eq!(calls.get(), 1);
            assert_eq!(result.is_ok(), applied);
            assert_eq!(fixture.record_path().exists(), !applied);
        }
    }
}
