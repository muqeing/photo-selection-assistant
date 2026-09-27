const KEYRING_SERVICE: &str = "com.muliai.photo-selection-assistant";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("系统密钥库错误：{0}")]
    Keyring(#[from] keyring::Error),
}

pub fn save_api_key(profile_id: &str, secret: &str) -> Result<(), SecretError> {
    with_entry(profile_id, |entry| entry.set_password(secret))
}

pub fn load_api_key(profile_id: &str) -> Result<String, SecretError> {
    with_entry(profile_id, keyring::Entry::get_password)
}

/// Removes the credential for a profile from the platform keyring.
pub fn delete_api_key(profile_id: &str) -> Result<(), SecretError> {
    with_entry(profile_id, keyring::Entry::delete_credential)
}

fn with_entry<T>(
    profile_id: &str,
    operation: impl FnOnce(&keyring::Entry) -> keyring::Result<T>,
) -> Result<T, SecretError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id)?;
    run_keyring_operation(|| operation(&entry))
}

fn run_keyring_operation<T>(
    operation: impl FnOnce() -> keyring::Result<T>,
) -> Result<T, SecretError> {
    Ok(operation()?)
}

#[cfg(test)]
mod tests {
    use super::{run_keyring_operation, SecretError, KEYRING_SERVICE};

    #[test]
    fn key_name_is_stable() {
        assert_eq!(KEYRING_SERVICE, "com.muliai.photo-selection-assistant");
    }

    #[test]
    fn keyring_errors_are_returned_without_creating_a_credential() {
        let result: Result<(), SecretError> =
            run_keyring_operation(|| Err(keyring::Error::NoEntry));

        assert!(matches!(
            result,
            Err(SecretError::Keyring(keyring::Error::NoEntry))
        ));
    }

    // This fails to compile if the macOS-native Cargo feature is removed.
    // Constructing a credential does not write anything to the Keychain.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_build_enables_the_native_keychain_store() {
        let constructor: fn(
            Option<&str>,
            &str,
            &str,
        ) -> keyring::Result<keyring::macos::MacCredential> =
            keyring::macos::MacCredential::new_with_target;
        drop(constructor(None, KEYRING_SERVICE, "feature-check").unwrap());
    }

    // This fails to compile if the Windows-native Cargo feature is removed.
    // Constructing a credential does not write anything to Credential Manager.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_build_enables_the_native_credential_store() {
        let constructor: fn(
            Option<&str>,
            &str,
            &str,
        ) -> keyring::Result<keyring::windows::WinCredential> =
            keyring::windows::WinCredential::new_with_target;
        drop(constructor(None, KEYRING_SERVICE, "feature-check").unwrap());
    }
}
