#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn staged_scan_io_error_exposes_only_fixed_diagnostic_metadata() {
        let error = MatchError::scan_io(
            ScanFailureStage::FileOpen,
            std::io::Error::from_raw_os_error(3),
        );

        assert_eq!(error.diagnostic_stage(), Some("scan-file-open"));
        assert_eq!(error.raw_os_error(), Some(3));

        let missing = MatchError::scan_io(
            ScanFailureStage::RootCanonicalize,
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        );
        assert_eq!(missing.io_kind(), Some(std::io::ErrorKind::NotFound));
    }

    #[test]
    fn scan_directory_entries_return_names_and_nofollow_types() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("IMG_0007.JPG"), b"jpg").unwrap();
        let directory = open_absolute_dir_nofollow(&root.path().canonicalize().unwrap()).unwrap();

        let entries = scan_directory_entries(&directory, root.path()).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, OsStr::new("IMG_0007.JPG"));
        assert!(entries[0].is_file);
    }

    #[test]
    fn hashing_does_not_require_a_one_megabyte_thread_stack() {
        let root = tempdir().unwrap();
        let path = root.path().join("IMG_0007.JPG");
        fs::write(&path, vec![7_u8; 2 * 1024 * 1024]).unwrap();
        let file = File::open(path).unwrap();

        let digest = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || hash_open_file_cancellable(&file, &AtomicBool::new(false)))
            .unwrap()
            .join()
            .expect("hashing must not overflow a small Windows-sized stack")
            .unwrap();

        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn number_filtered_scan_indexes_only_requested_candidates() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("IMG_0007.JPG"), b"wanted").unwrap();
        fs::write(root.path().join("IMG_0099.JPG"), b"unrelated").unwrap();
        let wanted = HashSet::from(["7".to_string()]);

        let index = scan_source_index_for_numbers_cancellable(
            root.path(),
            None,
            &default_extensions(),
            &wanted,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0].canonical_number, "7");
        assert!(
            index.files[0].fingerprint.content_hash.is_empty(),
            "filename-only screening must not read and hash photo contents"
        );
    }

    #[test]
    fn filename_scan_reports_checked_and_matched_file_progress() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("IMG_0007.JPG"), b"wanted").unwrap();
        fs::write(root.path().join("nested/IMG_0099.JPG"), b"unrelated").unwrap();
        let wanted = HashSet::from(["7".to_string()]);
        let mut updates = Vec::new();

        let index = scan_source_index_for_numbers_with_progress_cancellable(
            root.path(),
            None,
            &default_extensions(),
            &wanted,
            &AtomicBool::new(false),
            |progress| updates.push(progress),
        )
        .unwrap();

        assert_eq!(index.files.len(), 1);
        assert_eq!(
            updates.last(),
            Some(&ScanProgressUpdate {
                checked_files: 2,
                matched_files: 1,
                scanned_directories: 2,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn filename_scan_never_opens_unrelated_photo_contents() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        fs::write(root.path().join("IMG_0007.JPG"), b"wanted").unwrap();
        let unrelated = root.path().join("IMG_0099.JPG");
        fs::write(&unrelated, b"must not be opened").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o000)).unwrap();

        let result = scan_source_index_for_numbers_cancellable(
            root.path(),
            None,
            &default_extensions(),
            &HashSet::from(["7".to_string()]),
            &AtomicBool::new(false),
        );

        fs::set_permissions(&unrelated, fs::Permissions::from_mode(0o600)).unwrap();
        let index = result.expect("an unrelated filename must not be opened");
        assert_eq!(index.files.len(), 1);
    }

    #[test]
    fn scans_nested_raw_and_jpg_but_excludes_target() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("RAW")).unwrap();
        fs::create_dir_all(root.path().join("JPG")).unwrap();
        fs::create_dir_all(root.path().join("照片成片/待精修的原片")).unwrap();
        fs::write(root.path().join("RAW/IMG_01234.CR3"), b"raw").unwrap();
        fs::write(root.path().join("JPG/IMG_01234.JPG"), b"jpg").unwrap();
        fs::write(
            root.path().join("照片成片/待精修的原片/IMG_01234.JPG"),
            b"old",
        )
        .unwrap();

        let files = scan_source(
            root.path(),
            Some(&root.path().join("照片成片/待精修的原片")),
            &default_extensions(),
        )
        .unwrap();

        assert_eq!(files.len(), 2);
        let matched = match_numbers(&["1234".into()], &files);
        assert_eq!(matched[0].status, MatchStatus::Complete);
        assert_eq!(matched[0].groups[0].files.len(), 2);
    }

    #[test]
    fn preserves_leading_zero_normalization_end_to_end() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("IMG_001234.JPG"), b"jpg").unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();
        let matched = match_numbers(&["1234".into()], &files);

        assert_eq!(files[0].canonical_number, "1234");
        assert_eq!(matched[0].status, MatchStatus::Complete);
    }

    #[test]
    fn a_single_available_raw_or_jpg_format_is_a_complete_match() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("IMG_0007.CR3"), b"raw").unwrap();
        fs::write(root.path().join("IMG_0008.JPG"), b"jpg").unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();
        let matched = match_numbers(&["7".into(), "8".into()], &files);

        assert_eq!(matched[0].status, MatchStatus::Complete);
        assert_eq!(matched[0].groups[0].files.len(), 1);
        assert_eq!(matched[1].status, MatchStatus::Complete);
        assert_eq!(matched[1].groups[0].files.len(), 1);
    }

    #[test]
    fn indexes_numbers_in_names_with_internal_periods() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("archive.IMG-0012.JPG"), b"jpg").unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();
        let matched = match_numbers(&["12".into()], &files);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].canonical_number, "12");
        assert_eq!(matched[0].status, MatchStatus::Complete);
    }

    #[test]
    fn never_picks_first_duplicate_silently() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("shoot-a")).unwrap();
        fs::create_dir_all(root.path().join("shoot-b")).unwrap();
        fs::write(root.path().join("shoot-a/IMG_0007.JPG"), b"a").unwrap();
        fs::write(root.path().join("shoot-b/IMG_0007.JPG"), b"b").unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();
        let matched = match_numbers(&["7".into()], &files);
        assert_eq!(matched[0].status, MatchStatus::Ambiguous);
    }

    #[cfg(unix)]
    #[test]
    fn scan_digest_detects_in_place_rewrite_with_unchanged_metadata() {
        use crate::{
            copy_engine::{preflight_copy, CopyPlan},
            models::SelectedFile,
        };

        let root = tempdir().unwrap();
        let source = root.path().join("IMG_0007.JPG");
        fs::write(&source, b"trusted-").unwrap();
        let index = scan_source_index(root.path(), None, &default_extensions()).unwrap();
        let indexed = index.files[0].clone();
        let target_root = index.root.join("output");
        let target = target_root.join("IMG_0007.JPG");
        let modified = fs::metadata(&source).unwrap().modified().unwrap();

        fs::write(&source, b"changed-").unwrap();
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();

        let report = preflight_copy(&CopyPlan {
            source_root: index.root,
            source_root_identity: index.root_identity,
            target_root,
            files: vec![SelectedFile {
                canonical_number: "7".into(),
                source: indexed.path,
                target,
                fingerprint: indexed.fingerprint,
            }],
        })
        .unwrap();

        assert!(report.source_changed);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let linked_directory = tempdir().unwrap();
        fs::write(linked_directory.path().join("IMG_0007.JPG"), b"linked").unwrap();
        symlink(
            linked_directory.path(),
            root.path().join("linked-directory"),
        )
        .unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();

        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_index_a_source_file_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("IMG_0007.JPG"), b"outside").unwrap();
        symlink(
            outside.path().join("IMG_0007.JPG"),
            root.path().join("IMG_0007.JPG"),
        )
        .unwrap();

        let files = scan_source(root.path(), None, &default_extensions()).unwrap();

        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_scan_an_output_directory_through_a_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let output = tempdir().unwrap();
        fs::write(output.path().join("IMG_0008.JPG"), b"old output").unwrap();
        symlink(output.path(), root.path().join("output-link")).unwrap();

        let files = scan_source(root.path(), Some(output.path()), &default_extensions()).unwrap();

        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn candidate_preview_read_does_not_follow_replaced_parent_or_file_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(root.path().join("JPG")).unwrap();
        fs::write(root.path().join("JPG/IMG_0007.JPG"), b"preview").unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        assert_eq!(
            read_file_nofollow(root.path(), &root.path().join("JPG/IMG_0007.JPG"), 1024).unwrap(),
            b"preview"
        );

        fs::remove_file(root.path().join("JPG/IMG_0007.JPG")).unwrap();
        symlink(
            outside.path().join("secret"),
            root.path().join("JPG/IMG_0007.JPG"),
        )
        .unwrap();
        assert!(
            read_file_nofollow(root.path(), &root.path().join("JPG/IMG_0007.JPG"), 1024).is_err()
        );
    }
}
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

use cap_fs_ext::OpenOptionsFollowExt;
use cap_primitives::fs::{open_dir_nofollow, FollowSymlinks};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions as CapOpenOptions},
};

use crate::{
    models::{
        CandidateGroup, FileFingerprint, FileIdentity, IndexedFile, MatchStatus, NumberMatch,
        SourceIndex,
    },
    numbers::extract_number,
};

#[derive(Debug, thiserror::Error)]
pub enum MatchError {
    #[error("无法读取文件或目录：{0}")]
    Io(#[from] std::io::Error),
    #[error("无法读取文件或目录：{source}")]
    StagedIo {
        stage: ScanFailureStage,
        #[source]
        source: std::io::Error,
    },
    #[error("文件名不是有效文本")]
    InvalidName,
    #[error("扫描已取消")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanFailureStage {
    RootCanonicalize,
    RootOpen,
    RootIdentity,
    DirectoryEnumerate,
    EntryRead,
    EntryType,
    ChildDirectoryOpen,
    FileOpen,
    FileMetadata,
    FileHash,
    FileIdentity,
}

impl ScanFailureStage {
    pub(crate) fn diagnostic_stage(self) -> &'static str {
        match self {
            Self::RootCanonicalize => "scan-root-canonicalize",
            Self::RootOpen => "scan-root-open",
            Self::RootIdentity => "scan-root-identity",
            Self::DirectoryEnumerate => "scan-directory-enumerate",
            Self::EntryRead => "scan-entry-read",
            Self::EntryType => "scan-entry-type",
            Self::ChildDirectoryOpen => "scan-child-directory-open",
            Self::FileOpen => "scan-file-open",
            Self::FileMetadata => "scan-file-metadata",
            Self::FileHash => "scan-file-hash",
            Self::FileIdentity => "scan-file-identity",
        }
    }
}

impl MatchError {
    fn scan_io(stage: ScanFailureStage, source: std::io::Error) -> Self {
        Self::StagedIo { stage, source }
    }

    fn with_scan_stage(self, stage: ScanFailureStage) -> Self {
        match self {
            Self::Io(source) | Self::StagedIo { source, .. } => Self::scan_io(stage, source),
            error => error,
        }
    }

    pub(crate) fn diagnostic_stage(&self) -> Option<&'static str> {
        match self {
            Self::StagedIo { stage, .. } => Some(stage.diagnostic_stage()),
            _ => None,
        }
    }

    pub(crate) fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Io(source) | Self::StagedIo { source, .. } => source.raw_os_error(),
            _ => None,
        }
    }

    pub(crate) fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::Io(source) | Self::StagedIo { source, .. } => Some(source.kind()),
            _ => None,
        }
    }
}

pub fn default_extensions() -> HashSet<String> {
    [
        "cr2", "cr3", "nef", "arw", "raf", "dng", "rw2", "orf", "jpg", "jpeg",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[cfg(test)]
fn scan_source(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
) -> Result<Vec<IndexedFile>, MatchError> {
    Ok(scan_source_index(root, excluded_target, extensions)?.files)
}

#[cfg(test)]
pub(crate) fn scan_source_index(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
) -> Result<SourceIndex, MatchError> {
    scan_source_index_cancellable(root, excluded_target, extensions, &AtomicBool::new(false))
}

pub fn scan_source_index_cancellable(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
    cancelled: &AtomicBool,
) -> Result<SourceIndex, MatchError> {
    scan_source_index_impl(
        root,
        excluded_target,
        extensions,
        None,
        true,
        cancelled,
        &mut |_| {},
    )
}

pub fn scan_source_index_for_numbers_cancellable(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
    wanted_numbers: &HashSet<String>,
    cancelled: &AtomicBool,
) -> Result<SourceIndex, MatchError> {
    scan_source_index_for_numbers_with_progress_cancellable(
        root,
        excluded_target,
        extensions,
        wanted_numbers,
        cancelled,
        |_| {},
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgressUpdate {
    pub checked_files: usize,
    pub matched_files: usize,
    pub scanned_directories: usize,
}

pub fn scan_source_index_for_numbers_with_progress_cancellable(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
    wanted_numbers: &HashSet<String>,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(ScanProgressUpdate),
) -> Result<SourceIndex, MatchError> {
    scan_source_index_impl(
        root,
        excluded_target,
        extensions,
        Some(wanted_numbers),
        false,
        cancelled,
        &mut progress,
    )
}

fn scan_source_index_impl(
    root: &Path,
    excluded_target: Option<&Path>,
    extensions: &HashSet<String>,
    wanted_numbers: Option<&HashSet<String>>,
    hash_contents: bool,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(ScanProgressUpdate),
) -> Result<SourceIndex, MatchError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(MatchError::Cancelled);
    }
    let root = root
        .canonicalize()
        .map_err(|source| MatchError::scan_io(ScanFailureStage::RootCanonicalize, source))?;
    let root_dir = open_absolute_dir_nofollow(&root)
        .map_err(|error| error.with_scan_stage(ScanFailureStage::RootOpen))?;
    let root_file = root_dir
        .try_clone()
        .map_err(|source| MatchError::scan_io(ScanFailureStage::RootIdentity, source))?
        .into_std_file();
    let root_identity = file_identity(&root_file)
        .map_err(|error| error.with_scan_stage(ScanFailureStage::RootIdentity))?;
    let excluded = excluded_target.and_then(|path| {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        resolved.strip_prefix(&root).ok().map(Path::to_path_buf)
    });
    let mut pending = vec![(root_dir, PathBuf::new())];
    let mut found = Vec::new();
    let mut progress_update = ScanProgressUpdate {
        checked_files: 0,
        matched_files: 0,
        scanned_directories: 0,
    };

    while let Some((dir, relative_dir)) = pending.pop() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(MatchError::Cancelled);
        }
        if excluded
            .as_ref()
            .is_some_and(|target| relative_dir.starts_with(target))
        {
            continue;
        }
        progress_update.scanned_directories += 1;
        progress(progress_update);
        let absolute_dir = root.join(&relative_dir);
        for entry in scan_directory_entries(&dir, &absolute_dir)? {
            if cancelled.load(Ordering::Relaxed) {
                return Err(MatchError::Cancelled);
            }
            if entry.is_symlink {
                continue;
            }
            let name = entry.name;
            let relative = relative_dir.join(&name);
            if entry.is_dir {
                let child = open_child_dir_nofollow(&dir, &name)
                    .map_err(|error| error.with_scan_stage(ScanFailureStage::ChildDirectoryOpen))?;
                pending.push((child, relative));
            } else if entry.is_file {
                progress_update.checked_files += 1;
                if !filename_is_candidate(&relative, extensions, wanted_numbers)? {
                    progress(progress_update);
                    continue;
                }
                let mut options = CapOpenOptions::new();
                options.read(true).follow(FollowSymlinks::No);
                let file = dir
                    .open_with(&name, &options)
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::FileOpen, source))?
                    .into_std();
                if !file
                    .metadata()
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::FileMetadata, source))?
                    .is_file()
                {
                    continue;
                }
                let path = root.join(&relative);
                if let Some(indexed) = index_open_file(
                    path,
                    &file,
                    extensions,
                    wanted_numbers,
                    hash_contents,
                    cancelled,
                )? {
                    found.push(indexed);
                    progress_update.matched_files += 1;
                }
                progress(progress_update);
            }
        }
    }

    Ok(SourceIndex {
        root,
        root_identity,
        files: found,
    })
}

#[derive(Debug)]
struct ScanDirectoryEntry {
    name: OsString,
    is_symlink: bool,
    is_dir: bool,
    is_file: bool,
}

fn scan_directory_entries(
    directory: &Dir,
    absolute_path: &Path,
) -> Result<Vec<ScanDirectoryEntry>, MatchError> {
    #[cfg(windows)]
    {
        // cap-std 4.0.2 reconstructs a Windows path from the directory handle
        // before calling std::fs::read_dir. For UNC handles it strips `\\?\`
        // from `\\?\UNC\...` without restoring the leading UNC separators,
        // producing a relative `UNC\...` path and Windows error 3 on SMB.
        //
        // Enumerate names through the already-authorized canonical path, then
        // continue to open every child relative to `directory` with no-follow
        // semantics. Names can therefore never redirect file access outside
        // the opened source directory.
        let _ = directory;
        let entries = std::fs::read_dir(absolute_path)
            .map_err(|source| MatchError::scan_io(ScanFailureStage::DirectoryEnumerate, source))?;
        entries
            .map(|entry| {
                let entry = entry
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::EntryRead, source))?;
                let file_type = entry
                    .file_type()
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::EntryType, source))?;
                Ok(ScanDirectoryEntry {
                    name: entry.file_name(),
                    is_symlink: file_type.is_symlink(),
                    is_dir: file_type.is_dir(),
                    is_file: file_type.is_file(),
                })
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = absolute_path;
        let entries = directory
            .entries()
            .map_err(|source| MatchError::scan_io(ScanFailureStage::DirectoryEnumerate, source))?;
        entries
            .map(|entry| {
                let entry = entry
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::EntryRead, source))?;
                let file_type = entry
                    .file_type()
                    .map_err(|source| MatchError::scan_io(ScanFailureStage::EntryType, source))?;
                Ok(ScanDirectoryEntry {
                    name: entry.file_name(),
                    is_symlink: file_type.is_symlink(),
                    is_dir: file_type.is_dir(),
                    is_file: file_type.is_file(),
                })
            })
            .collect()
    }
}

fn filename_is_candidate(
    path: &Path,
    extensions: &HashSet<String>,
    wanted_numbers: Option<&HashSet<String>>,
) -> Result<bool, MatchError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !extensions.contains(&extension) {
        return Ok(false);
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(MatchError::InvalidName)?;
    let Some(number) = extract_number(file_name) else {
        return Ok(false);
    };
    Ok(wanted_numbers.is_none_or(|wanted| wanted.contains(&number.canonical)))
}

#[cfg(test)]
pub fn read_file_nofollow(
    root: &Path,
    path: &Path,
    maximum_bytes: u64,
) -> Result<Vec<u8>, MatchError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "preview is outside source root",
        )
    })?;
    let canonical_root = root.canonicalize()?;
    let mut components = relative.components().peekable();
    let mut directory = open_absolute_dir_nofollow(&canonical_root)?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview path is not normalized",
            )
            .into());
        };
        if components.peek().is_some() {
            directory = open_child_dir_nofollow(&directory, name)?;
            continue;
        }
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = directory.open_with(name, &options)?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > maximum_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "preview file is invalid or too large",
            )
            .into());
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)?;
        return Ok(bytes);
    }
    Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "preview path is empty").into())
}

pub fn match_numbers(numbers: &[String], files: &[IndexedFile]) -> Vec<NumberMatch> {
    numbers
        .iter()
        .map(|number| {
            let mut grouped: BTreeMap<(String, PathBuf), Vec<IndexedFile>> = BTreeMap::new();
            for file in files.iter().filter(|file| &file.canonical_number == number) {
                grouped
                    .entry((file.stem.to_ascii_lowercase(), file.family_key.clone()))
                    .or_default()
                    .push(file.clone());
            }
            let groups: Vec<CandidateGroup> = grouped
                .into_iter()
                .map(|((stem, family), files)| CandidateGroup {
                    id: {
                        let digest =
                            blake3::hash(format!("{}\0{stem}", family.display()).as_bytes())
                                .to_hex()
                                .to_string();
                        format!("candidate-{}", &digest[..16])
                    },
                    files,
                })
                .collect();
            let status = match groups.as_slice() {
                [] => MatchStatus::Missing,
                [_] => MatchStatus::Complete,
                _ => MatchStatus::Ambiguous,
            };
            NumberMatch {
                canonical_number: number.clone(),
                status,
                groups,
            }
        })
        .collect()
}

fn family_key(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(""));
    let name = parent
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(name.as_str(), "raw" | "jpg" | "jpeg" | "原片" | "预览") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn index_open_file(
    path: PathBuf,
    file: &File,
    extensions: &HashSet<String>,
    wanted_numbers: Option<&HashSet<String>>,
    hash_contents: bool,
    cancelled: &AtomicBool,
) -> Result<Option<IndexedFile>, MatchError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !extensions.contains(&extension) {
        return Ok(None);
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(MatchError::InvalidName)?
        .to_string();
    let Some(number) = extract_number(&file_name) else {
        return Ok(None);
    };
    if wanted_numbers.is_some_and(|wanted| !wanted.contains(&number.canonical)) {
        return Ok(None);
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or(MatchError::InvalidName)?
        .to_string();
    let metadata = file
        .metadata()
        .map_err(|source| MatchError::scan_io(ScanFailureStage::FileMetadata, source))?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_millis().min(u64::MAX as u128) as u64);

    let content_hash = if hash_contents {
        hash_open_file_cancellable(file, cancelled)
            .map_err(|error| error.with_scan_stage(ScanFailureStage::FileHash))?
    } else {
        String::new()
    };
    let fingerprint = FileFingerprint {
        identity: file_identity(file)
            .map_err(|error| error.with_scan_stage(ScanFailureStage::FileIdentity))?,
        size: metadata.len(),
        modified_ms,
        content_hash,
    };
    Ok(Some(IndexedFile {
        family_key: family_key(&path),
        path,
        stem,
        extension,
        canonical_number: number.canonical,
        size: metadata.len(),
        modified_ms,
        fingerprint,
    }))
}

fn hash_open_file_cancellable(file: &File, cancelled: &AtomicBool) -> Result<String, MatchError> {
    let mut reader = file.try_clone()?;
    let before = file.metadata()?;
    let mut hasher = blake3::Hasher::new();
    // Keep the 1 MiB chunk on the heap. The default Windows main-thread stack
    // is commonly 1 MiB, so a stack array of the same size terminates the
    // scanner with STATUS_STACK_OVERFLOW before the first file can be hashed.
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(MatchError::Cancelled);
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(
            std::io::Error::new(std::io::ErrorKind::Other, "source changed while hashing").into(),
        );
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn open_absolute_dir_nofollow(path: &Path) -> Result<Dir, MatchError> {
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    let mut reached_normal_component = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir if !reached_normal_component => {
                anchor.push(component.as_os_str());
            }
            Component::Normal(name) => {
                reached_normal_component = true;
                names.push(name.to_os_string());
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "source root is not an absolute normalized path",
                )
                .into())
            }
        }
    }
    let mut dir = Dir::open_ambient_dir(anchor, ambient_authority())?;
    for name in names {
        dir = open_child_dir_nofollow(&dir, &name)?;
    }
    Ok(dir)
}

fn open_child_dir_nofollow(parent: &Dir, name: &OsStr) -> Result<Dir, MatchError> {
    let parent_file = parent.try_clone()?.into_std_file();
    Ok(Dir::from_std_file(open_dir_nofollow(
        &parent_file,
        Path::new(name),
    )?))
}

fn file_identity(file: &File) -> Result<FileIdentity, MatchError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(FileIdentity {
            device: metadata.dev(),
            file_index: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` remains open and owns this valid handle for the call.
        let succeeded =
            unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(FileIdentity {
            device: u64::from(information.dwVolumeSerialNumber),
            file_index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(FileIdentity::default())
    }
}
