use std::path::{Path, PathBuf};

#[cfg(any(windows, test))]
use std::{ffi::c_void, mem::size_of};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED as ERROR_ACCESS_DENIED_CODE,
    ERROR_ALREADY_ASSIGNED as ERROR_ALREADY_ASSIGNED_CODE,
    ERROR_BAD_NET_NAME as ERROR_BAD_NET_NAME_CODE, ERROR_BAD_USERNAME as ERROR_BAD_USERNAME_CODE,
    ERROR_CANCELLED as ERROR_CANCELLED_CODE,
    ERROR_CONNECTION_UNAVAIL as ERROR_CONNECTION_UNAVAIL_CODE,
    ERROR_INVALID_NAME as ERROR_INVALID_NAME_CODE,
    ERROR_INVALID_PASSWORD as ERROR_INVALID_PASSWORD_CODE,
    ERROR_LOGON_FAILURE as ERROR_LOGON_FAILURE_CODE, ERROR_MORE_DATA as ERROR_MORE_DATA_CODE,
    ERROR_NETWORK_UNREACHABLE as ERROR_NETWORK_UNREACHABLE_CODE,
    ERROR_NOT_CONNECTED as ERROR_NOT_CONNECTED_CODE,
    ERROR_NOT_ENOUGH_MEMORY as ERROR_NOT_ENOUGH_MEMORY_CODE,
    ERROR_NO_MORE_ITEMS as ERROR_NO_MORE_ITEMS_CODE,
    ERROR_SESSION_CREDENTIAL_CONFLICT as ERROR_SESSION_CREDENTIAL_CONFLICT_CODE,
    NO_ERROR as NO_ERROR_CODE,
};

#[cfg(all(not(windows), test))]
const NO_ERROR_CODE: u32 = 0;
#[cfg(all(not(windows), test))]
const ERROR_ACCESS_DENIED_CODE: u32 = 5;
#[cfg(all(not(windows), test))]
const ERROR_BAD_NET_NAME_CODE: u32 = 67;
#[cfg(all(not(windows), test))]
const ERROR_ALREADY_ASSIGNED_CODE: u32 = 85;
#[cfg(all(not(windows), test))]
const ERROR_INVALID_PASSWORD_CODE: u32 = 86;
#[cfg(all(not(windows), test))]
const ERROR_NOT_ENOUGH_MEMORY_CODE: u32 = 8;
#[cfg(all(not(windows), test))]
const ERROR_INVALID_NAME_CODE: u32 = 123;
#[cfg(all(not(windows), test))]
const ERROR_MORE_DATA_CODE: u32 = 234;
#[cfg(all(not(windows), test))]
const ERROR_NO_MORE_ITEMS_CODE: u32 = 259;
#[cfg(all(not(windows), test))]
const ERROR_CONNECTION_UNAVAIL_CODE: u32 = 1201;
#[cfg(all(not(windows), test))]
const ERROR_SESSION_CREDENTIAL_CONFLICT_CODE: u32 = 1219;
#[cfg(all(not(windows), test))]
const ERROR_CANCELLED_CODE: u32 = 1223;
#[cfg(all(not(windows), test))]
const ERROR_NETWORK_UNREACHABLE_CODE: u32 = 1231;
#[cfg(all(not(windows), test))]
const ERROR_LOGON_FAILURE_CODE: u32 = 1326;
#[cfg(all(not(windows), test))]
const ERROR_BAD_USERNAME_CODE: u32 = 2202;
#[cfg(all(not(windows), test))]
const ERROR_NOT_CONNECTED_CODE: u32 = 2250;

#[cfg(windows)]
const INTERACTIVE_CONNECT_FLAGS: u32 =
    windows_sys::Win32::NetworkManagement::WNet::CONNECT_INTERACTIVE
        | windows_sys::Win32::NetworkManagement::WNet::CONNECT_PROMPT
        | windows_sys::Win32::NetworkManagement::WNet::CONNECT_UPDATE_PROFILE
        | windows_sys::Win32::NetworkManagement::WNet::CONNECT_CMD_SAVECRED;
#[cfg(all(not(windows), test))]
const INTERACTIVE_CONNECT_FLAGS: u32 = 8 | 16 | 1 | 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPathError {
    Cancelled,
    CredentialConflict,
    AccessDenied(u32),
    Unavailable(u32),
    InvalidPath,
}

impl NetworkPathError {
    pub fn windows_code(self) -> Option<u32> {
        match self {
            Self::Cancelled => Some(1223),
            Self::CredentialConflict => Some(1219),
            Self::AccessDenied(code) | Self::Unavailable(code) => Some(code),
            Self::InvalidPath => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedDirectoryKind {
    Local,
    MappedDrive,
    Unc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDirectory {
    pub path: PathBuf,
    pub kind: PreparedDirectoryKind,
    pub network_target: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareDirectoryError {
    pub error: NetworkPathError,
    pub kind: PreparedDirectoryKind,
    pub network_target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    SilentOnly,
    AllowCredentialPrompt { owner: isize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetDirectoryState {
    Existing(PathBuf),
    Missing,
}

#[cfg(any(windows, test))]
trait WNetApi {
    fn universal_name(&self, path: &Path) -> Result<PathBuf, u32>;

    fn remembered_remote(&self, local_device: &Path) -> Result<Option<PathBuf>, u32>;

    #[allow(dead_code)]
    fn connect(&self, owner: isize, remote: &Path, flags: u32) -> u32;
}

#[cfg(any(windows, test))]
enum ResolvedLocation {
    Local,
    Network {
        path: PathBuf,
        kind: PreparedDirectoryKind,
    },
}

#[cfg(any(windows, test))]
fn resolve_location_with_api_diagnostics(
    path: &Path,
    api: &impl WNetApi,
) -> Result<ResolvedLocation, PrepareDirectoryError> {
    match classify(path) {
        PathKind::Unc => valid_unc(path)
            .map(|path| ResolvedLocation::Network {
                path,
                kind: PreparedDirectoryKind::Unc,
            })
            .map_err(|error| PrepareDirectoryError {
                error,
                kind: PreparedDirectoryKind::Unc,
                network_target: Some(path.to_path_buf()),
            }),
        PathKind::DriveLetter => match api.universal_name(path) {
            Ok(resolved) => {
                let network_target = Some(resolved.clone());
                valid_unc(&resolved)
                    .map(|path| ResolvedLocation::Network {
                        path,
                        kind: PreparedDirectoryKind::MappedDrive,
                    })
                    .map_err(|error| PrepareDirectoryError {
                        error,
                        kind: PreparedDirectoryKind::MappedDrive,
                        network_target,
                    })
            }
            Err(code @ (ERROR_NOT_CONNECTED_CODE | ERROR_CONNECTION_UNAVAIL_CODE)) => {
                let local_device = drive_local_device(path).ok_or(PrepareDirectoryError {
                    error: NetworkPathError::InvalidPath,
                    kind: PreparedDirectoryKind::Local,
                    network_target: None,
                })?;
                let remote =
                    api.remembered_remote(&local_device)
                        .map_err(|code| PrepareDirectoryError {
                            error: map_network_error(code),
                            kind: PreparedDirectoryKind::Local,
                            network_target: None,
                        })?;
                let Some(remote) = remote else {
                    return if code == ERROR_NOT_CONNECTED_CODE {
                        Ok(ResolvedLocation::Local)
                    } else {
                        Err(PrepareDirectoryError {
                            error: NetworkPathError::Unavailable(code),
                            kind: PreparedDirectoryKind::Local,
                            network_target: None,
                        })
                    };
                };
                let network_target = Some(remote.clone());
                join_remembered_remote(&remote, path)
                    .map(|path| ResolvedLocation::Network {
                        path,
                        kind: PreparedDirectoryKind::MappedDrive,
                    })
                    .map_err(|error| PrepareDirectoryError {
                        error,
                        kind: PreparedDirectoryKind::MappedDrive,
                        network_target,
                    })
            }
            Err(code) => Err(PrepareDirectoryError {
                error: map_network_error(code),
                kind: PreparedDirectoryKind::Local,
                network_target: None,
            }),
        },
        PathKind::Local => Ok(ResolvedLocation::Local),
    }
}

#[cfg(any(windows, test))]
fn resolve_location_with_api(
    path: &Path,
    api: &impl WNetApi,
) -> Result<ResolvedLocation, NetworkPathError> {
    resolve_location_with_api_diagnostics(path, api).map_err(|error| error.error)
}

#[cfg(any(windows, test))]
fn resolve_network_path_with_api(
    path: &Path,
    api: &impl WNetApi,
) -> Result<PathBuf, NetworkPathError> {
    resolve_network_path_with_api_and_validator(path, api, validate_directory)
}

#[cfg(any(windows, test))]
fn resolve_network_path_with_api_and_validator(
    path: &Path,
    api: &impl WNetApi,
    validate: impl FnOnce(&Path) -> Result<PathBuf, NetworkPathError>,
) -> Result<PathBuf, NetworkPathError> {
    match resolve_location_with_api(path, api)? {
        ResolvedLocation::Local => validate(path),
        ResolvedLocation::Network { path, .. } => Ok(path),
    }
}

#[cfg(any(windows, test))]
fn prepare_directory_with_api_diagnostics(
    path: &Path,
    mode: ConnectMode,
    api: &impl WNetApi,
    validate: impl FnOnce(&Path) -> Result<PathBuf, NetworkPathError>,
) -> Result<PreparedDirectory, PrepareDirectoryError> {
    let (network_path, kind) = match resolve_location_with_api_diagnostics(path, api)? {
        ResolvedLocation::Local => {
            return validate(path)
                .map(|path| PreparedDirectory {
                    path,
                    kind: PreparedDirectoryKind::Local,
                    network_target: None,
                })
                .map_err(|error| PrepareDirectoryError {
                    error,
                    kind: PreparedDirectoryKind::Local,
                    network_target: None,
                });
        }
        ResolvedLocation::Network { path, kind } => (path, kind),
    };
    let network_target = Some(network_path.clone());
    let share_root = unc_share_root(&network_path).ok_or_else(|| PrepareDirectoryError {
        error: NetworkPathError::InvalidPath,
        kind,
        network_target: network_target.clone(),
    })?;

    connect_share_with_api(api, &share_root, mode).map_err(|error| PrepareDirectoryError {
        error,
        kind,
        network_target: network_target.clone(),
    })?;
    validate(&network_path)
        .map(|path| PreparedDirectory {
            path,
            kind,
            network_target,
        })
        .map_err(|error| PrepareDirectoryError {
            error,
            kind,
            network_target: Some(network_path),
        })
}

#[cfg(any(windows, test))]
fn connect_share_with_api(
    api: &impl WNetApi,
    share_root: &Path,
    mode: ConnectMode,
) -> Result<(), NetworkPathError> {
    match api.connect(0, share_root, 0) {
        NO_ERROR_CODE | ERROR_ALREADY_ASSIGNED_CODE => Ok(()),
        code if is_authentication_error(code) => match mode {
            ConnectMode::SilentOnly => Err(map_connection_error(code)),
            ConnectMode::AllowCredentialPrompt { owner } => {
                map_connection_result(api.connect(owner, share_root, INTERACTIVE_CONNECT_FLAGS))
            }
        },
        code => Err(map_connection_error(code)),
    }
}

#[cfg(any(windows, test))]
fn prepare_target_directory_with_api_diagnostics(
    path: &Path,
    mode: ConnectMode,
    api: &impl WNetApi,
    inspect: impl FnOnce(&Path) -> Result<TargetDirectoryState, NetworkPathError>,
    validate_network_base: impl FnOnce(&Path) -> Result<PathBuf, NetworkPathError>,
) -> Result<PreparedDirectory, PrepareDirectoryError> {
    let (target_path, kind, share_root) = match resolve_location_with_api_diagnostics(path, api)? {
        ResolvedLocation::Local => {
            return inspect(path)
                .map(|state| PreparedDirectory {
                    path: match state {
                        TargetDirectoryState::Existing(path) => path,
                        TargetDirectoryState::Missing => path.to_path_buf(),
                    },
                    kind: PreparedDirectoryKind::Local,
                    network_target: None,
                })
                .map_err(|error| PrepareDirectoryError {
                    error,
                    kind: PreparedDirectoryKind::Local,
                    network_target: None,
                });
        }
        ResolvedLocation::Network { path, kind } => {
            let share_root = unc_share_root(&path).ok_or_else(|| PrepareDirectoryError {
                error: NetworkPathError::InvalidPath,
                kind,
                network_target: Some(path.clone()),
            })?;
            (path, kind, share_root)
        }
    };
    let network_target = Some(target_path.clone());
    connect_share_with_api(api, &share_root, mode).map_err(|error| PrepareDirectoryError {
        error,
        kind,
        network_target: network_target.clone(),
    })?;
    let prepared_path = match inspect(&target_path) {
        Ok(TargetDirectoryState::Existing(path)) => path,
        Ok(TargetDirectoryState::Missing) => {
            validate_network_base(&share_root).map_err(|error| PrepareDirectoryError {
                error,
                kind,
                network_target: network_target.clone(),
            })?;
            target_path.clone()
        }
        Err(error) => {
            return Err(PrepareDirectoryError {
                error,
                kind,
                network_target,
            });
        }
    };
    Ok(PreparedDirectory {
        path: prepared_path,
        kind,
        network_target: Some(target_path),
    })
}

#[cfg(any(windows, test))]
fn prepare_directory_with_api(
    path: &Path,
    mode: ConnectMode,
    api: &impl WNetApi,
    validate: impl FnOnce(&Path) -> Result<PathBuf, NetworkPathError>,
) -> Result<PathBuf, NetworkPathError> {
    prepare_directory_with_api_diagnostics(path, mode, api, validate)
        .map(|prepared| prepared.path)
        .map_err(|error| error.error)
}

#[cfg(any(windows, test))]
fn is_authentication_error(code: u32) -> bool {
    matches!(
        code,
        ERROR_ACCESS_DENIED_CODE
            | ERROR_INVALID_PASSWORD_CODE
            | ERROR_LOGON_FAILURE_CODE
            | ERROR_BAD_USERNAME_CODE
    )
}

#[cfg(any(windows, test))]
fn map_connection_result(code: u32) -> Result<(), NetworkPathError> {
    match code {
        NO_ERROR_CODE | ERROR_ALREADY_ASSIGNED_CODE => Ok(()),
        code => Err(map_connection_error(code)),
    }
}

#[cfg(any(windows, test))]
fn map_connection_error(code: u32) -> NetworkPathError {
    match code {
        ERROR_CANCELLED_CODE => NetworkPathError::Cancelled,
        ERROR_SESSION_CREDENTIAL_CONFLICT_CODE => NetworkPathError::CredentialConflict,
        ERROR_ACCESS_DENIED_CODE
        | ERROR_INVALID_PASSWORD_CODE
        | ERROR_LOGON_FAILURE_CODE
        | ERROR_BAD_USERNAME_CODE => NetworkPathError::AccessDenied(code),
        code => NetworkPathError::Unavailable(code),
    }
}

#[cfg(any(windows, test))]
fn drive_local_device(path: &Path) -> Option<PathBuf> {
    let value = path.to_string_lossy();
    is_absolute_drive_letter_path(&value).then(|| PathBuf::from(&value[..2]))
}

#[cfg(any(windows, test))]
fn join_remembered_remote(remote: &Path, original: &Path) -> Result<PathBuf, NetworkPathError> {
    valid_unc(remote)?;
    let value = original.to_string_lossy();
    let relative = value
        .get(2..)
        .ok_or(NetworkPathError::InvalidPath)?
        .trim_start_matches(|character| character == '\\' || character == '/');
    let remote = remote.to_string_lossy();
    let combined = if relative.is_empty() {
        remote.into_owned()
    } else {
        format!(
            r"{}\{relative}",
            remote.trim_end_matches(|character| character == '\\' || character == '/')
        )
    };
    valid_unc(Path::new(&combined))
}

#[cfg(any(windows, test))]
fn valid_unc(path: &Path) -> Result<PathBuf, NetworkPathError> {
    unc_share_root(path)
        .is_some()
        .then(|| path.to_path_buf())
        .ok_or(NetworkPathError::InvalidPath)
}

fn validate_directory(path: &Path) -> Result<PathBuf, NetworkPathError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| NetworkPathError::InvalidPath)?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(NetworkPathError::InvalidPath)
    }
}

fn inspect_target_directory_with(
    path: &Path,
    metadata: impl FnOnce(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<TargetDirectoryState, NetworkPathError> {
    match metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(NetworkPathError::InvalidPath)
        }
        Ok(_) => validate_directory(path).map(TargetDirectoryState::Existing),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || matches!(error.raw_os_error(), Some(2 | 3)) =>
        {
            Ok(TargetDirectoryState::Missing)
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(
            NetworkPathError::AccessDenied(error.raw_os_error().unwrap_or(5) as u32),
        ),
        Err(error) => Err(error
            .raw_os_error()
            .map(|code| NetworkPathError::Unavailable(code as u32))
            .unwrap_or(NetworkPathError::InvalidPath)),
    }
}

fn inspect_target_directory(path: &Path) -> Result<TargetDirectoryState, NetworkPathError> {
    inspect_target_directory_with(path, |path| path.symlink_metadata())
}

#[cfg(any(windows, test))]
fn map_network_error(code: u32) -> NetworkPathError {
    match code {
        ERROR_CANCELLED_CODE => NetworkPathError::Cancelled,
        ERROR_SESSION_CREDENTIAL_CONFLICT_CODE => NetworkPathError::CredentialConflict,
        ERROR_ACCESS_DENIED_CODE
        | ERROR_INVALID_PASSWORD_CODE
        | ERROR_LOGON_FAILURE_CODE
        | ERROR_BAD_USERNAME_CODE => NetworkPathError::AccessDenied(code),
        ERROR_INVALID_NAME_CODE => NetworkPathError::InvalidPath,
        ERROR_BAD_NET_NAME_CODE
        | ERROR_NETWORK_UNREACHABLE_CODE
        | ERROR_CONNECTION_UNAVAIL_CODE => NetworkPathError::Unavailable(code),
        code => NetworkPathError::Unavailable(code),
    }
}

pub fn resolve_network_path(path: &Path) -> Result<PathBuf, NetworkPathError> {
    #[cfg(windows)]
    {
        resolve_network_path_with_api(path, &SystemWNet)
    }

    #[cfg(not(windows))]
    {
        validate_directory(path)
    }
}

pub fn prepare_directory(path: &Path, mode: ConnectMode) -> Result<PathBuf, NetworkPathError> {
    prepare_directory_with_diagnostics(path, mode)
        .map(|prepared| prepared.path)
        .map_err(|error| error.error)
}

pub fn prepare_directory_with_diagnostics(
    path: &Path,
    mode: ConnectMode,
) -> Result<PreparedDirectory, PrepareDirectoryError> {
    #[cfg(windows)]
    {
        prepare_directory_with_api_diagnostics(path, mode, &SystemWNet, validate_directory)
    }

    #[cfg(not(windows))]
    {
        let _ = mode;
        validate_directory(path)
            .map(|path| PreparedDirectory {
                path,
                kind: PreparedDirectoryKind::Local,
                network_target: None,
            })
            .map_err(|error| PrepareDirectoryError {
                error,
                kind: PreparedDirectoryKind::Local,
                network_target: None,
            })
    }
}

pub fn prepare_target_directory_with_diagnostics(
    path: &Path,
    mode: ConnectMode,
) -> Result<PreparedDirectory, PrepareDirectoryError> {
    #[cfg(windows)]
    {
        prepare_target_directory_with_api_diagnostics(
            path,
            mode,
            &SystemWNet,
            inspect_target_directory,
            validate_directory,
        )
    }

    #[cfg(not(windows))]
    {
        let _ = mode;
        inspect_target_directory(path)
            .map(|state| PreparedDirectory {
                path: match state {
                    TargetDirectoryState::Existing(path) => path,
                    TargetDirectoryState::Missing => path.to_path_buf(),
                },
                kind: PreparedDirectoryKind::Local,
                network_target: None,
            })
            .map_err(|error| PrepareDirectoryError {
                error,
                kind: PreparedDirectoryKind::Local,
                network_target: None,
            })
    }
}

#[cfg(any(windows, test))]
fn universal_name_with(
    path: &Path,
    mut get_universal_name: impl FnMut(*const u16, *mut c_void, &mut u32) -> u32,
) -> Result<PathBuf, u32> {
    const INITIAL_BUFFER_BYTES: usize = 2 * 1024;
    const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

    let mut input = encode_path(path);
    if input.contains(&0) {
        return Err(ERROR_INVALID_NAME_CODE);
    }
    input.push(0);

    // Some Windows network providers reject a null-buffer size probe with
    // ERROR_INVALID_PARAMETER even for a valid, connected mapped drive. Start
    // with a real aligned buffer and grow it only when the provider asks.
    let mut requested_bytes = INITIAL_BUFFER_BYTES;
    loop {
        if requested_bytes < size_of::<*mut u16>() || requested_bytes > MAX_BUFFER_BYTES {
            return Err(ERROR_INVALID_NAME_CODE);
        }

        let word_size = size_of::<usize>();
        let word_count = requested_bytes
            .checked_add(word_size - 1)
            .ok_or(ERROR_NOT_ENOUGH_MEMORY_CODE)?
            / word_size;
        let mut buffer = Vec::<usize>::new();
        buffer
            .try_reserve_exact(word_count)
            .map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
        buffer.resize(word_count, 0);
        let allocated_bytes = word_count
            .checked_mul(word_size)
            .ok_or(ERROR_NOT_ENOUGH_MEMORY_CODE)?;

        let mut supplied_bytes =
            u32::try_from(allocated_bytes).map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
        let result = get_universal_name(
            input.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut supplied_bytes,
        );
        if result == ERROR_MORE_DATA_CODE {
            let required_bytes =
                usize::try_from(supplied_bytes).map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
            if required_bytes <= allocated_bytes {
                return Err(ERROR_INVALID_NAME_CODE);
            }
            requested_bytes = required_bytes;
            continue;
        }
        if result != 0 {
            return Err(result);
        }

        return path_from_universal_name_buffer(&buffer, allocated_bytes);
    }
}

#[cfg(any(windows, test))]
fn path_from_universal_name_buffer(
    buffer: &[usize],
    supplied_bytes: usize,
) -> Result<PathBuf, u32> {
    let buffer_start = buffer.as_ptr() as usize;
    let buffer_end = buffer_start
        .checked_add(supplied_bytes)
        .ok_or(ERROR_INVALID_NAME_CODE)?;
    let name = unsafe { buffer.as_ptr().cast::<*mut u16>().read() };
    let name_address = name as usize;
    if name.is_null()
        || name_address < buffer_start + size_of::<*mut u16>()
        || name_address >= buffer_end
        || name_address % size_of::<u16>() != 0
    {
        return Err(ERROR_INVALID_NAME_CODE);
    }

    let available_units = (buffer_end - name_address) / size_of::<u16>();
    let units = unsafe { std::slice::from_raw_parts(name, available_units) };
    let terminator = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(ERROR_INVALID_NAME_CODE)?;
    path_from_wide(&units[..terminator])
}

#[cfg(any(windows, test))]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawNetResourceW {
    scope: u32,
    resource_type: u32,
    display_type: u32,
    usage: u32,
    local_name: *mut u16,
    remote_name: *mut u16,
    comment: *mut u16,
    provider: *mut u16,
}

#[cfg(windows)]
const _: [(); size_of::<RawNetResourceW>()] =
    [(); size_of::<windows_sys::Win32::NetworkManagement::WNet::NETRESOURCEW>()];
#[cfg(windows)]
const _: [(); std::mem::align_of::<RawNetResourceW>()] =
    [(); std::mem::align_of::<windows_sys::Win32::NetworkManagement::WNet::NETRESOURCEW>()];

#[cfg(any(windows, test))]
struct EnumHandleGuard<'a, C: FnMut(isize) -> u32> {
    handle: isize,
    close: &'a mut C,
}

#[cfg(any(windows, test))]
impl<C: FnMut(isize) -> u32> Drop for EnumHandleGuard<'_, C> {
    fn drop(&mut self) {
        (self.close)(self.handle);
    }
}

#[cfg(any(windows, test))]
fn remembered_remote_with(
    local_device: &Path,
    mut open_enum: impl FnMut(&mut isize) -> u32,
    mut enum_resource: impl FnMut(isize, &mut u32, *mut c_void, &mut u32) -> u32,
    mut close_enum: impl FnMut(isize) -> u32,
) -> Result<Option<PathBuf>, u32> {
    const INITIAL_BUFFER_BYTES: usize = 16 * 1024;
    const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

    let mut handle = 0isize;
    let open_result = open_enum(&mut handle);
    if open_result != NO_ERROR_CODE {
        return Err(open_result);
    }
    if handle == 0 {
        return Err(ERROR_INVALID_NAME_CODE);
    }
    let _guard = EnumHandleGuard {
        handle,
        close: &mut close_enum,
    };

    let target = local_device.to_string_lossy();
    let mut requested_bytes = INITIAL_BUFFER_BYTES;
    loop {
        if requested_bytes < size_of::<RawNetResourceW>() || requested_bytes > MAX_BUFFER_BYTES {
            return Err(ERROR_NOT_ENOUGH_MEMORY_CODE);
        }

        let word_size = size_of::<usize>();
        let word_count = requested_bytes
            .checked_add(word_size - 1)
            .ok_or(ERROR_NOT_ENOUGH_MEMORY_CODE)?
            / word_size;
        let mut buffer = Vec::<usize>::new();
        buffer
            .try_reserve_exact(word_count)
            .map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
        buffer.resize(word_count, 0);
        let allocated_bytes = word_count
            .checked_mul(word_size)
            .ok_or(ERROR_NOT_ENOUGH_MEMORY_CODE)?;
        let mut supplied_bytes =
            u32::try_from(requested_bytes).map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
        let mut count = u32::MAX;

        let result = enum_resource(
            handle,
            &mut count,
            buffer.as_mut_ptr().cast(),
            &mut supplied_bytes,
        );
        if result == ERROR_MORE_DATA_CODE {
            let required =
                usize::try_from(supplied_bytes).map_err(|_| ERROR_NOT_ENOUGH_MEMORY_CODE)?;
            if required <= requested_bytes {
                return Err(ERROR_INVALID_NAME_CODE);
            }
            requested_bytes = required;
            continue;
        }
        if result == ERROR_NO_MORE_ITEMS_CODE {
            return Ok(None);
        }
        if result != NO_ERROR_CODE {
            return Err(result);
        }
        if count == 0 {
            return Err(ERROR_INVALID_NAME_CODE);
        }

        let entry_count = usize::try_from(count).map_err(|_| ERROR_INVALID_NAME_CODE)?;
        let records_bytes = entry_count
            .checked_mul(size_of::<RawNetResourceW>())
            .ok_or(ERROR_INVALID_NAME_CODE)?;
        if records_bytes > allocated_bytes {
            return Err(ERROR_INVALID_NAME_CODE);
        }
        let buffer_start = buffer.as_ptr() as usize;
        let strings_start = buffer_start
            .checked_add(records_bytes)
            .ok_or(ERROR_INVALID_NAME_CODE)?;
        let buffer_end = buffer_start
            .checked_add(allocated_bytes)
            .ok_or(ERROR_INVALID_NAME_CODE)?;

        for index in 0..entry_count {
            let resource = unsafe { buffer.as_ptr().cast::<RawNetResourceW>().add(index).read() };
            if resource.local_name.is_null() {
                continue;
            }
            let local =
                path_from_wide_pointer_in_buffer(resource.local_name, strings_start, buffer_end)?;
            if local.to_string_lossy().eq_ignore_ascii_case(&target) {
                let remote = path_from_wide_pointer_in_buffer(
                    resource.remote_name,
                    strings_start,
                    buffer_end,
                )?;
                return Ok(Some(remote));
            }
        }
    }
}

#[cfg(any(windows, test))]
fn path_from_wide_pointer_in_buffer(
    pointer: *mut u16,
    strings_start: usize,
    buffer_end: usize,
) -> Result<PathBuf, u32> {
    let address = pointer as usize;
    if pointer.is_null()
        || address < strings_start
        || address >= buffer_end
        || address % size_of::<u16>() != 0
    {
        return Err(ERROR_INVALID_NAME_CODE);
    }
    let available_units = (buffer_end - address) / size_of::<u16>();
    let units = unsafe { std::slice::from_raw_parts(pointer, available_units) };
    let terminator = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(ERROR_INVALID_NAME_CODE)?;
    path_from_wide(&units[..terminator])
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().collect()
}

#[cfg(all(not(windows), test))]
fn encode_path(path: &Path) -> Vec<u16> {
    path.to_string_lossy().encode_utf16().collect()
}

#[cfg(windows)]
fn path_from_wide(units: &[u16]) -> Result<PathBuf, u32> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    Ok(PathBuf::from(OsString::from_wide(units)))
}

#[cfg(all(not(windows), test))]
fn path_from_wide(units: &[u16]) -> Result<PathBuf, u32> {
    String::from_utf16(units)
        .map(PathBuf::from)
        .map_err(|_| ERROR_INVALID_NAME_CODE)
}

#[cfg(windows)]
struct SystemWNet;

#[cfg(windows)]
impl WNetApi for SystemWNet {
    fn universal_name(&self, path: &Path) -> Result<PathBuf, u32> {
        use windows_sys::Win32::NetworkManagement::WNet::{
            WNetGetUniversalNameW, UNIVERSAL_NAME_INFO_LEVEL,
        };

        universal_name_with(path, |input, buffer, size| unsafe {
            WNetGetUniversalNameW(input, UNIVERSAL_NAME_INFO_LEVEL, buffer, size)
        })
    }

    fn remembered_remote(&self, local_device: &Path) -> Result<Option<PathBuf>, u32> {
        use windows_sys::Win32::{
            Foundation::HANDLE,
            NetworkManagement::WNet::{
                WNetCloseEnum, WNetEnumResourceW, WNetOpenEnumW, RESOURCETYPE_DISK,
                RESOURCE_REMEMBERED,
            },
        };

        remembered_remote_with(
            local_device,
            |raw_handle| {
                let mut handle: HANDLE = std::ptr::null_mut();
                let result = unsafe {
                    WNetOpenEnumW(
                        RESOURCE_REMEMBERED,
                        RESOURCETYPE_DISK,
                        0,
                        std::ptr::null(),
                        &mut handle,
                    )
                };
                *raw_handle = handle as isize;
                result
            },
            |raw_handle, count, buffer, size| unsafe {
                WNetEnumResourceW(raw_handle as HANDLE, count, buffer, size)
            },
            |raw_handle| unsafe { WNetCloseEnum(raw_handle as HANDLE) },
        )
    }

    fn connect(&self, owner: isize, remote: &Path, flags: u32) -> u32 {
        use windows_sys::Win32::{
            Foundation::HWND,
            NetworkManagement::WNet::{WNetAddConnection3W, NETRESOURCEW, RESOURCETYPE_DISK},
        };

        let mut remote_name = encode_path(remote);
        if remote_name.contains(&0) {
            return ERROR_INVALID_NAME_CODE;
        }
        remote_name.push(0);

        let resource = NETRESOURCEW {
            dwType: RESOURCETYPE_DISK,
            lpLocalName: std::ptr::null_mut(),
            lpRemoteName: remote_name.as_mut_ptr(),
            ..Default::default()
        };

        unsafe {
            WNetAddConnection3W(
                owner as HWND,
                &resource,
                std::ptr::null(),
                std::ptr::null(),
                flags,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        mem::size_of,
        path::{Path, PathBuf},
    };

    use super::{
        classify, inspect_target_directory_with, prepare_directory_with_api,
        prepare_directory_with_api_diagnostics, prepare_target_directory_with_api_diagnostics,
        remembered_remote_with, resolve_network_path_with_api,
        resolve_network_path_with_api_and_validator, unc_share_root, universal_name_with,
        ConnectMode, NetworkPathError, PathKind, PreparedDirectoryKind, TargetDirectoryState,
        WNetApi, ERROR_INVALID_NAME_CODE, ERROR_MORE_DATA_CODE, ERROR_NOT_CONNECTED_CODE,
    };
    #[cfg(not(windows))]
    use super::{prepare_directory, resolve_network_path};

    struct FakeWNet {
        universal_result: Result<PathBuf, u32>,
        remembered_result: Result<Option<PathBuf>, u32>,
        universal_calls: Cell<usize>,
        remembered_calls: Cell<usize>,
        connect_calls: Cell<usize>,
        connect_results: RefCell<VecDeque<u32>>,
        connection_args: RefCell<Vec<(isize, PathBuf, u32)>>,
    }

    impl FakeWNet {
        fn returning(result: Result<PathBuf, u32>) -> Self {
            Self {
                universal_result: result,
                remembered_result: Ok(None),
                universal_calls: Cell::new(0),
                remembered_calls: Cell::new(0),
                connect_calls: Cell::new(0),
                connect_results: RefCell::new(VecDeque::new()),
                connection_args: RefCell::new(Vec::new()),
            }
        }

        fn connecting(results: impl IntoIterator<Item = u32>) -> Self {
            Self {
                universal_result: Ok(PathBuf::new()),
                remembered_result: Ok(None),
                universal_calls: Cell::new(0),
                remembered_calls: Cell::new(0),
                connect_calls: Cell::new(0),
                connect_results: RefCell::new(results.into_iter().collect()),
                connection_args: RefCell::new(Vec::new()),
            }
        }

        fn disconnected_mapping(
            remembered_result: Result<Option<PathBuf>, u32>,
            connect_results: impl IntoIterator<Item = u32>,
        ) -> Self {
            Self {
                universal_result: Err(1201),
                remembered_result,
                universal_calls: Cell::new(0),
                remembered_calls: Cell::new(0),
                connect_calls: Cell::new(0),
                connect_results: RefCell::new(connect_results.into_iter().collect()),
                connection_args: RefCell::new(Vec::new()),
            }
        }
    }

    impl WNetApi for FakeWNet {
        fn universal_name(&self, _path: &Path) -> Result<PathBuf, u32> {
            self.universal_calls.set(self.universal_calls.get() + 1);
            self.universal_result.clone()
        }

        fn remembered_remote(&self, local_device: &Path) -> Result<Option<PathBuf>, u32> {
            self.remembered_calls.set(self.remembered_calls.get() + 1);
            assert!(local_device.to_string_lossy().ends_with(':'));
            self.remembered_result.clone()
        }

        fn connect(&self, owner: isize, remote: &Path, flags: u32) -> u32 {
            self.connect_calls.set(self.connect_calls.get() + 1);
            self.connection_args
                .borrow_mut()
                .push((owner, remote.to_path_buf(), flags));
            self.connect_results
                .borrow_mut()
                .pop_front()
                .expect("unexpected WNet connection attempt")
        }
    }

    #[test]
    fn an_unmapped_drive_is_validated_as_local_without_connecting() {
        let mut api = FakeWNet::returning(Err(2250));
        api.remembered_result = Ok(None);
        let expected = PathBuf::from("/canonical/local/photos");

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"C:\photos"),
                ConnectMode::AllowCredentialPrompt { owner: 42 },
                &api,
                |path| {
                    assert_eq!(path, Path::new(r"C:\photos"));
                    Ok(expected.clone())
                },
            ),
            Ok(expected)
        );
        assert_eq!(api.universal_calls.get(), 1);
        assert_eq!(api.remembered_calls.get(), 1);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[test]
    fn runtime_prepare_diagnostics_distinguish_local_and_mapped_drive_letters() {
        let local_api = FakeWNet::returning(Err(2250));
        let local = prepare_directory_with_api_diagnostics(
            Path::new(r"C:\photos"),
            ConnectMode::SilentOnly,
            &local_api,
            |_| Ok(PathBuf::from("/canonical/local/photos")),
        )
        .unwrap();

        assert_eq!(local.kind, PreparedDirectoryKind::Local);
        assert_eq!(local.network_target, None);

        let mut mapped_api = FakeWNet::connecting([0]);
        mapped_api.universal_result = Ok(PathBuf::from(r"\\nas-box\photos\wedding\raw"));
        let mapped = prepare_directory_with_api_diagnostics(
            Path::new(r"Z:\wedding\raw"),
            ConnectMode::SilentOnly,
            &mapped_api,
            |_| Ok(PathBuf::from(r"\\nas-box\photos\wedding\raw")),
        )
        .unwrap();

        assert_eq!(mapped.kind, PreparedDirectoryKind::MappedDrive);
        assert_eq!(
            mapped.network_target,
            Some(PathBuf::from(r"\\nas-box\photos\wedding\raw"))
        );

        let direct_api = FakeWNet::connecting([0]);
        let direct = prepare_directory_with_api_diagnostics(
            Path::new(r"\\nas-box\photos\wedding\raw"),
            ConnectMode::SilentOnly,
            &direct_api,
            |_| Ok(PathBuf::from(r"\\nas-box\photos\wedding\raw")),
        )
        .unwrap();

        assert_eq!(direct.kind, PreparedDirectoryKind::Unc);
        assert_eq!(
            direct.network_target,
            Some(PathBuf::from(r"\\nas-box\photos\wedding\raw"))
        );
    }

    #[test]
    fn unresolved_drive_failures_are_not_reported_as_mapped() {
        let api = FakeWNet::returning(Err(53));
        let error = prepare_directory_with_api_diagnostics(
            Path::new(r"Z:\wedding\raw"),
            ConnectMode::SilentOnly,
            &api,
            |_| panic!("filesystem validation must not run after resolution failure"),
        )
        .unwrap_err();

        assert_eq!(error.kind, PreparedDirectoryKind::Local);
        assert_eq!(error.network_target, None);
        assert_eq!(error.error.windows_code(), Some(53));
    }

    #[test]
    fn a_disconnected_remembered_drive_rebuilds_the_complete_unc_before_connecting() {
        let api = FakeWNet::disconnected_mapping(Ok(Some(PathBuf::from(r"\\nas-box\photos"))), [0]);
        let expected = PathBuf::from("/canonical/wedding/raw");

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"z:\wedding\raw"),
                ConnectMode::SilentOnly,
                &api,
                |path| {
                    assert_eq!(path, Path::new(r"\\nas-box\photos\wedding\raw"));
                    Ok(expected.clone())
                },
            ),
            Ok(expected)
        );
        assert_eq!(api.universal_calls.get(), 1);
        assert_eq!(api.remembered_calls.get(), 1);
        assert_eq!(
            *api.connection_args.borrow(),
            vec![(0, PathBuf::from(r"\\nas-box\photos"), 0)]
        );
        assert_eq!(api.connect_calls.get(), 1);
    }

    #[test]
    fn a_not_connected_remembered_drive_rebuilds_the_complete_unc_before_connecting() {
        let mut api = FakeWNet::connecting([0]);
        api.universal_result = Err(ERROR_NOT_CONNECTED_CODE);
        api.remembered_result = Ok(Some(PathBuf::from(r"\\nas-box\photos")));
        let expected = PathBuf::from("/canonical/wedding/raw");

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"z:\wedding\raw"),
                ConnectMode::SilentOnly,
                &api,
                |path| {
                    assert_eq!(path, Path::new(r"\\nas-box\photos\wedding\raw"));
                    Ok(expected.clone())
                },
            ),
            Ok(expected)
        );
        assert_eq!(api.universal_calls.get(), 1);
        assert_eq!(api.remembered_calls.get(), 1);
        assert_eq!(
            *api.connection_args.borrow(),
            vec![(0, PathBuf::from(r"\\nas-box\photos"), 0)]
        );
        assert_eq!(api.connect_calls.get(), 1);
    }

    #[test]
    fn a_disconnected_drive_without_a_remembered_mapping_is_unavailable() {
        let api = FakeWNet::disconnected_mapping(Ok(None), []);

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"Z:\wedding\raw"),
                ConnectMode::AllowCredentialPrompt { owner: 42 },
                &api,
                |_| panic!("filesystem validation must not run without a remembered mapping"),
            ),
            Err(NetworkPathError::Unavailable(1201))
        );
        assert_eq!(api.universal_calls.get(), 1);
        assert_eq!(api.remembered_calls.get(), 1);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[test]
    fn preparing_a_synthetic_local_path_skips_every_wnet_query() {
        let path = Path::new("photos/local");
        let expected = PathBuf::from("/canonical/photos/local");
        let api = FakeWNet::connecting([]);
        let validations = Cell::new(0);

        let prepared = prepare_directory_with_api(
            path,
            ConnectMode::AllowCredentialPrompt { owner: 42 },
            &api,
            |validated| {
                validations.set(validations.get() + 1);
                assert_eq!(validated, path);
                Ok(expected.clone())
            },
        );

        assert_eq!(prepared, Ok(expected));
        assert_eq!(validations.get(), 1);
        assert_eq!(api.universal_calls.get(), 0);
        assert_eq!(api.remembered_calls.get(), 0);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[test]
    fn silent_success_and_already_connected_validate_the_complete_directory() {
        for result in [0, 85] {
            let api = FakeWNet::connecting([result]);
            let expected = PathBuf::from("/canonical/full-directory");
            let path = Path::new(r"\\dynamic-server\dynamic-share\job\raw");
            let validations = Cell::new(0);

            let prepared =
                prepare_directory_with_api(path, ConnectMode::SilentOnly, &api, |validated| {
                    validations.set(validations.get() + 1);
                    assert_eq!(validated, path);
                    Ok(expected.clone())
                });

            assert_eq!(prepared, Ok(expected));
            assert_eq!(validations.get(), 1);
            assert_eq!(
                *api.connection_args.borrow(),
                vec![(0, PathBuf::from(r"\\dynamic-server\dynamic-share"), 0)]
            );
            assert_eq!(api.connect_calls.get(), 1);
        }
    }

    #[test]
    fn missing_unc_target_connects_and_validates_only_its_authorized_share_root() {
        let api = FakeWNet::connecting([0]);
        let target = Path::new(r"\\dynamic-server\dynamic-share\订单\照片成片\待精修的原片");
        let inspected = RefCell::new(Vec::new());
        let validated_bases = RefCell::new(Vec::new());

        let prepared = prepare_target_directory_with_api_diagnostics(
            target,
            ConnectMode::AllowCredentialPrompt { owner: 4242 },
            &api,
            |path| {
                inspected.borrow_mut().push(path.to_path_buf());
                Ok(TargetDirectoryState::Missing)
            },
            |path| {
                validated_bases.borrow_mut().push(path.to_path_buf());
                Ok(path.to_path_buf())
            },
        )
        .unwrap();

        assert_eq!(prepared.path, target);
        assert_eq!(prepared.kind, PreparedDirectoryKind::Unc);
        assert_eq!(*inspected.borrow(), vec![target.to_path_buf()]);
        assert_eq!(
            *validated_bases.borrow(),
            vec![PathBuf::from(r"\\dynamic-server\dynamic-share")]
        );
        assert_eq!(
            *api.connection_args.borrow(),
            vec![(0, PathBuf::from(r"\\dynamic-server\dynamic-share"), 0)]
        );
    }

    #[test]
    fn windows_path_not_found_code_marks_an_unc_target_as_missing_for_safe_creation() {
        let target = Path::new(r"\\dynamic-server\dynamic-share\订单\照片成片\待精修的原片");

        let state =
            inspect_target_directory_with(target, |_| Err(std::io::Error::from_raw_os_error(3)))
                .unwrap();

        assert_eq!(state, TargetDirectoryState::Missing);
    }

    #[test]
    fn missing_local_target_keeps_the_exact_authorized_path_without_connecting_or_creating() {
        let api = FakeWNet::connecting([]);
        let target = Path::new("authorized/local/missing-target");
        let base_validations = Cell::new(0);

        let prepared = prepare_target_directory_with_api_diagnostics(
            target,
            ConnectMode::AllowCredentialPrompt { owner: 4242 },
            &api,
            |path| {
                assert_eq!(path, target);
                Ok(TargetDirectoryState::Missing)
            },
            |_| {
                base_validations.set(base_validations.get() + 1);
                panic!("a missing local target must not authorize a broader directory")
            },
        )
        .unwrap();

        assert_eq!(prepared.path, target);
        assert_eq!(prepared.kind, PreparedDirectoryKind::Local);
        assert_eq!(api.connect_calls.get(), 0);
        assert_eq!(base_validations.get(), 0);
    }

    #[test]
    fn silent_auth_failures_allow_exactly_one_native_credential_attempt() {
        const EXPECTED_INTERACTIVE_FLAGS: u32 = 8 | 16 | 1 | 4096;

        for auth_failure in [5, 86, 1326, 2202] {
            let api = FakeWNet::connecting([auth_failure, 0]);
            let path = Path::new(r"\\nas-box\photos\wedding\raw");

            assert_eq!(
                prepare_directory_with_api(
                    path,
                    ConnectMode::AllowCredentialPrompt { owner: 4242 },
                    &api,
                    |_| Ok(PathBuf::from("/canonical/wedding/raw")),
                ),
                Ok(PathBuf::from("/canonical/wedding/raw"))
            );
            assert_eq!(
                *api.connection_args.borrow(),
                vec![
                    (0, PathBuf::from(r"\\nas-box\photos"), 0),
                    (
                        4242,
                        PathBuf::from(r"\\nas-box\photos"),
                        EXPECTED_INTERACTIVE_FLAGS,
                    ),
                ]
            );
            assert_eq!(api.connect_calls.get(), 2);
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct TestNetResourceW {
        scope: u32,
        resource_type: u32,
        display_type: u32,
        usage: u32,
        local_name: *mut u16,
        remote_name: *mut u16,
        comment: *mut u16,
        provider: *mut u16,
    }

    unsafe fn write_wide(cursor: &mut *mut u16, value: &str) -> *mut u16 {
        let start = *cursor;
        let units: Vec<u16> = value.encode_utf16().chain([0]).collect();
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), start, units.len());
            *cursor = start.add(units.len());
        }
        start
    }

    #[test]
    fn remembered_enumeration_resizes_matches_case_insensitively_and_closes() {
        let closes = Cell::new(0);
        let enum_calls = Cell::new(0);
        let required = 32 * 1024u32;

        let result = remembered_remote_with(
            Path::new("z:"),
            |handle| {
                *handle = 77;
                0
            },
            |handle, count, buffer, size| {
                assert_eq!(handle, 77);
                enum_calls.set(enum_calls.get() + 1);
                if enum_calls.get() == 1 {
                    *size = required;
                    return 234;
                }

                assert!(*size >= required);
                let entries = buffer.cast::<TestNetResourceW>();
                let mut strings = unsafe { entries.add(2).cast::<u16>() };
                unsafe {
                    entries.write(TestNetResourceW {
                        local_name: write_wide(&mut strings, "Y:"),
                        remote_name: write_wide(&mut strings, r"\\other\share"),
                        ..Default::default()
                    });
                    entries.add(1).write(TestNetResourceW {
                        local_name: write_wide(&mut strings, "Z:"),
                        remote_name: write_wide(&mut strings, r"\\nas-box\photos"),
                        ..Default::default()
                    });
                }
                *count = 2;
                0
            },
            |handle| {
                assert_eq!(handle, 77);
                closes.set(closes.get() + 1);
                0
            },
        );

        assert_eq!(result, Ok(Some(PathBuf::from(r"\\nas-box\photos"))));
        assert_eq!(enum_calls.get(), 2);
        assert_eq!(closes.get(), 1);
    }

    #[test]
    fn remembered_enumeration_returns_none_and_closes_at_end() {
        let closes = Cell::new(0);

        assert_eq!(
            remembered_remote_with(
                Path::new("Z:"),
                |handle| {
                    *handle = 88;
                    0
                },
                |_, _, _, _| 259,
                |handle| {
                    assert_eq!(handle, 88);
                    closes.set(closes.get() + 1);
                    0
                },
            ),
            Ok(None)
        );
        assert_eq!(closes.get(), 1);
    }

    #[test]
    fn remembered_enumeration_skips_deviceless_entries() {
        let calls = Cell::new(0);

        assert_eq!(
            remembered_remote_with(
                Path::new("Z:"),
                |handle| {
                    *handle = 89;
                    0
                },
                |_, count, buffer, _| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 2 {
                        return 259;
                    }
                    unsafe {
                        buffer.cast::<TestNetResourceW>().write(TestNetResourceW {
                            local_name: std::ptr::null_mut(),
                            remote_name: std::ptr::null_mut(),
                            ..Default::default()
                        });
                    }
                    *count = 1;
                    0
                },
                |_| 0,
            ),
            Ok(None)
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn remembered_enumeration_rejects_out_of_buffer_pointers_and_closes() {
        let closes = Cell::new(0);

        assert_eq!(
            remembered_remote_with(
                Path::new("Z:"),
                |handle| {
                    *handle = 99;
                    0
                },
                |_, count, buffer, _| {
                    unsafe {
                        buffer.cast::<TestNetResourceW>().write(TestNetResourceW {
                            local_name: 1usize as *mut u16,
                            remote_name: 2usize as *mut u16,
                            ..Default::default()
                        });
                    }
                    *count = 1;
                    0
                },
                |_| {
                    closes.set(closes.get() + 1);
                    0
                },
            ),
            Err(ERROR_INVALID_NAME_CODE)
        );
        assert_eq!(closes.get(), 1);
    }

    #[test]
    fn silent_only_auth_failure_does_not_open_a_credential_window() {
        let api = FakeWNet::connecting([1326]);

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"\\nas-box\photos\wedding"),
                ConnectMode::SilentOnly,
                &api,
                |_| panic!("filesystem validation must not run after connection failure"),
            ),
            Err(NetworkPathError::AccessDenied(1326))
        );
        assert_eq!(api.connect_calls.get(), 1);
        assert_eq!(api.connection_args.borrow()[0].2, 0);
    }

    #[test]
    fn silent_authentication_failures_preserve_the_exact_windows_code() {
        for code in [5, 86, 1326, 2202] {
            let api = FakeWNet::connecting([code]);
            let error = prepare_directory_with_api_diagnostics(
                Path::new(r"\\nas-box\photos\wedding"),
                ConnectMode::SilentOnly,
                &api,
                |_| panic!("filesystem validation must not run after authentication failure"),
            )
            .unwrap_err();

            assert_eq!(error.kind, PreparedDirectoryKind::Unc);
            assert_eq!(error.error, NetworkPathError::AccessDenied(code));
            assert_eq!(error.error.windows_code(), Some(code));
            assert_eq!(api.connect_calls.get(), 1);
        }
    }

    #[test]
    fn mapped_drive_connection_failure_keeps_runtime_kind_target_and_code() {
        let mut api = FakeWNet::connecting([86]);
        api.universal_result = Ok(PathBuf::from(r"\\nas-box\photos\wedding"));
        let error = prepare_directory_with_api_diagnostics(
            Path::new(r"Z:\wedding"),
            ConnectMode::SilentOnly,
            &api,
            |_| panic!("filesystem validation must not run after connection failure"),
        )
        .unwrap_err();

        assert_eq!(error.kind, PreparedDirectoryKind::MappedDrive);
        assert_eq!(
            error.network_target,
            Some(PathBuf::from(r"\\nas-box\photos\wedding"))
        );
        assert_eq!(error.error, NetworkPathError::AccessDenied(86));
        assert_eq!(error.error.windows_code(), Some(86));
    }

    #[test]
    fn interactive_failures_preserve_cancel_and_conflict_windows_codes() {
        for (interactive_code, expected) in [
            (1223, NetworkPathError::Cancelled),
            (1219, NetworkPathError::CredentialConflict),
        ] {
            let api = FakeWNet::connecting([5, interactive_code]);
            let error = prepare_directory_with_api_diagnostics(
                Path::new(r"\\nas-box\photos\wedding"),
                ConnectMode::AllowCredentialPrompt { owner: 7 },
                &api,
                |_| panic!("filesystem validation must not run after interactive failure"),
            )
            .unwrap_err();

            assert_eq!(error.kind, PreparedDirectoryKind::Unc);
            assert_eq!(error.error, expected);
            assert_eq!(error.error.windows_code(), Some(interactive_code));
            assert_eq!(api.connect_calls.get(), 2);
        }
    }

    #[test]
    fn cancelling_the_native_credential_window_returns_cancelled() {
        let api = FakeWNet::connecting([5, 1223]);

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"\\nas-box\photos\wedding"),
                ConnectMode::AllowCredentialPrompt { owner: 7 },
                &api,
                |_| panic!("filesystem validation must not run after cancellation"),
            ),
            Err(NetworkPathError::Cancelled)
        );
        assert_eq!(api.connect_calls.get(), 2);
    }

    #[test]
    fn credential_conflict_is_returned_without_retrying_or_disconnecting() {
        let api = FakeWNet::connecting([1219]);

        assert_eq!(
            prepare_directory_with_api(
                Path::new(r"\\nas-box\photos\wedding"),
                ConnectMode::AllowCredentialPrompt { owner: 7 },
                &api,
                |_| panic!("filesystem validation must not run after a conflict"),
            ),
            Err(NetworkPathError::CredentialConflict)
        );
        assert_eq!(api.connect_calls.get(), 1);
    }

    #[test]
    fn non_authentication_connection_errors_are_unavailable_without_retry() {
        for code in [53, 67, 123, 1201, 1231, 4321] {
            let api = FakeWNet::connecting([code]);

            assert_eq!(
                prepare_directory_with_api(
                    Path::new(r"\\nas-box\photos\wedding"),
                    ConnectMode::AllowCredentialPrompt { owner: 7 },
                    &api,
                    |_| panic!("filesystem validation must not run after connection failure"),
                ),
                Err(NetworkPathError::Unavailable(code))
            );
            assert_eq!(api.connect_calls.get(), 1);
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn public_prepare_directory_validates_local_directories_on_non_windows() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            prepare_directory(directory.path(), ConnectMode::SilentOnly),
            Ok(directory.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn extracts_share_roots_without_hard_coding_servers() {
        assert_eq!(
            unc_share_root(Path::new(r"\\nas-box\photos\job\raw")).unwrap(),
            PathBuf::from(r"\\nas-box\photos")
        );
        assert_eq!(
            unc_share_root(Path::new(r"\\192.168.1.50\share\raw")).unwrap(),
            PathBuf::from(r"\\192.168.1.50\share")
        );
        assert_eq!(
            unc_share_root(Path::new(r"\\[fe80::20]\share\raw")).unwrap(),
            PathBuf::from(r"\\[fe80::20]\share")
        );
    }

    #[test]
    fn normalizes_verbatim_unc_before_extracting_share_root() {
        assert_eq!(
            unc_share_root(Path::new(r"\\?\UNC\nas-box\photos\job\raw")),
            Some(PathBuf::from(r"\\nas-box\photos"))
        );
    }

    #[test]
    fn handles_verbatim_paths_only_when_they_are_unc() {
        assert_eq!(classify(Path::new(r"\\?\C:\photos")), PathKind::Local);
        assert_eq!(unc_share_root(Path::new(r"\\?\C:\photos")), None);
        assert_eq!(
            classify(Path::new(r"\\?\unc\server\share\raw")),
            PathKind::Unc
        );
        assert_eq!(
            unc_share_root(Path::new(r"\\?\unc\server\share\raw")),
            Some(PathBuf::from(r"\\server\share"))
        );
    }

    #[test]
    fn rejects_local_and_incomplete_unc_paths() {
        assert_eq!(unc_share_root(Path::new(r"C:\photos")), None);
        assert_eq!(unc_share_root(Path::new(r"\\nas-box")), None);
    }

    #[test]
    fn classifies_local_drive_letter_and_unc_paths() {
        assert_eq!(classify(Path::new(r"C:\photos")), PathKind::DriveLetter);
        assert_eq!(classify(Path::new(r"Z:\photos")), PathKind::DriveLetter);
        assert_eq!(classify(Path::new("D:/photos")), PathKind::DriveLetter);
        assert_eq!(classify(Path::new(r"\\nas-box\photos")), PathKind::Unc);
        assert_eq!(classify(Path::new("photos")), PathKind::Local);
    }

    #[test]
    fn resolves_a_mapped_drive_to_its_full_unc_directory() {
        let api = FakeWNet::returning(Ok(PathBuf::from(r"\\nas-box\photos\wedding\raw")));

        assert_eq!(
            resolve_network_path_with_api(Path::new(r"Z:\wedding\raw"), &api),
            Ok(PathBuf::from(r"\\nas-box\photos\wedding\raw"))
        );
        assert_eq!(api.universal_calls.get(), 1);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[test]
    fn maps_wnet_errors_without_a_network_server() {
        for (code, expected) in [
            (1223, NetworkPathError::Cancelled),
            (1219, NetworkPathError::CredentialConflict),
            (5, NetworkPathError::AccessDenied(5)),
            (53, NetworkPathError::Unavailable(53)),
        ] {
            let api = FakeWNet::returning(Err(code));

            assert_eq!(
                resolve_network_path_with_api(Path::new(r"Z:\wedding\raw"), &api),
                Err(expected)
            );
            assert_eq!(api.universal_calls.get(), 1);
            assert_eq!(api.connect_calls.get(), 0);
        }
    }

    #[test]
    fn more_data_allocates_the_reported_buffer_and_calls_again() {
        let local = r"Z:\wedding\raw";
        let wide_input: Vec<u16> = local.encode_utf16().chain([0]).collect();
        let expected = r"\\nas-box\photos\wedding\raw";
        let wide_name: Vec<u16> = expected.encode_utf16().chain([0]).collect();
        let mut calls = Vec::new();

        let resolved = universal_name_with(Path::new(local), |input, buffer, size| {
            assert_eq!(
                unsafe { std::slice::from_raw_parts(input, wide_input.len()) },
                wide_input
            );
            calls.push((buffer.is_null(), *size));
            if calls.len() == 1 {
                *size = size.saturating_mul(2);
                return ERROR_MORE_DATA_CODE;
            }

            let required_bytes = size_of::<*mut u16>() + wide_name.len() * size_of::<u16>();
            assert!((*size as usize) >= required_bytes);
            unsafe {
                let name = buffer.cast::<u8>().add(size_of::<*mut u16>()).cast::<u16>();
                std::ptr::copy_nonoverlapping(wide_name.as_ptr(), name, wide_name.len());
                buffer.cast::<*mut u16>().write(name);
            }
            0
        });

        assert_eq!(resolved, Ok(PathBuf::from(expected)));
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].0 && !calls[1].0);
        assert_eq!(calls[1].1, calls[0].1 * 2);
    }

    #[test]
    fn universal_name_uses_a_real_initial_buffer_for_providers_that_reject_null_probes() {
        let local = r"Z:\wedding\raw";
        let expected = r"\\nas-box\photos\wedding\raw";
        let wide_name: Vec<u16> = expected.encode_utf16().chain([0]).collect();
        let mut calls = 0usize;

        let resolved = universal_name_with(Path::new(local), |_, buffer, size| {
            calls += 1;
            if buffer.is_null() {
                return 87;
            }
            let required_bytes = size_of::<*mut u16>() + wide_name.len() * size_of::<u16>();
            assert!((*size as usize) >= required_bytes);
            unsafe {
                let name = buffer.cast::<u8>().add(size_of::<*mut u16>()).cast::<u16>();
                std::ptr::copy_nonoverlapping(wide_name.as_ptr(), name, wide_name.len());
                buffer.cast::<*mut u16>().write(name);
            }
            0
        });

        assert_eq!(resolved, Ok(PathBuf::from(expected)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn direct_unc_bypasses_wnet() {
        let api = FakeWNet::returning(Err(53));
        let path = Path::new(r"\\nas-box\photos\wedding\raw");

        assert_eq!(
            resolve_network_path_with_api(path, &api),
            Ok(path.to_path_buf())
        );
        assert_eq!(api.universal_calls.get(), 0);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[test]
    fn resolving_a_synthetic_local_path_skips_every_wnet_query() {
        let path = Path::new("photos/local");
        let expected = PathBuf::from("/canonical/photos/local");
        let api = FakeWNet::returning(Err(53));
        let validations = Cell::new(0);

        assert_eq!(
            resolve_network_path_with_api_and_validator(path, &api, |validated| {
                validations.set(validations.get() + 1);
                assert_eq!(validated, path);
                Ok(expected.clone())
            }),
            Ok(expected)
        );
        assert_eq!(validations.get(), 1);
        assert_eq!(api.universal_calls.get(), 0);
        assert_eq!(api.remembered_calls.get(), 0);
        assert_eq!(api.connect_calls.get(), 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn public_non_windows_resolver_validates_a_local_directory() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve_network_path(directory.path()),
            Ok(directory.path().canonicalize().unwrap())
        );
        assert_eq!(
            resolve_network_path(&directory.path().join("missing")),
            Err(NetworkPathError::InvalidPath)
        );
    }

    #[test]
    fn universal_name_rejects_embedded_nul_before_calling_wnet() {
        let calls = Cell::new(0);

        assert_eq!(
            universal_name_with(Path::new("Z:\\bad\0path"), |_, _, _| {
                calls.set(calls.get() + 1);
                0
            }),
            Err(ERROR_INVALID_NAME_CODE)
        );
        assert_eq!(calls.get(), 0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Local,
    DriveLetter,
    Unc,
}

pub fn classify(path: &Path) -> PathKind {
    let value = path.to_string_lossy();

    if let Some(suffix) = value.strip_prefix(r"\\?\") {
        if is_verbatim_unc_suffix(suffix) {
            PathKind::Unc
        } else {
            PathKind::Local
        }
    } else if value.starts_with(r"\\.\") {
        PathKind::Local
    } else if value.starts_with(r"\\") {
        PathKind::Unc
    } else if is_absolute_drive_letter_path(&value) {
        PathKind::DriveLetter
    } else {
        PathKind::Local
    }
}

pub fn unc_share_root(path: &Path) -> Option<PathBuf> {
    let value = path.to_string_lossy();
    let normalized = if let Some(suffix) = value.strip_prefix(r"\\?\") {
        if !is_verbatim_unc_suffix(suffix) {
            return None;
        }
        format!(r"\\{}", &suffix[4..])
    } else {
        if value.starts_with(r"\\.\") {
            return None;
        }
        value.into_owned()
    };
    let mut components = normalized
        .strip_prefix(r"\\")?
        .split('\\')
        .filter(|component| !component.is_empty());
    let server = components.next()?;
    let share = components.next()?;

    Some(PathBuf::from(format!(r"\\{server}\{share}")))
}

fn is_absolute_drive_letter_path(value: &str) -> bool {
    matches!(value.as_bytes(), [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic())
}

fn is_verbatim_unc_suffix(suffix: &str) -> bool {
    suffix
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"UNC\"))
}
