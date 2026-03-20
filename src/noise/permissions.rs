//! File permission checks for secure key/token storage.
//!
//! Enforces:
//! - Config directory: mode 700 (owner rwx only)
//! - Key files: mode 600 (owner rw only)
//! - Token files: mode 600 (owner rw only)
//!
//! Refuses to start if permissions are wrong.

use super::{NoiseError, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Required mode for config directory (~/.llama-mesh/)
pub const DIR_MODE: u32 = 0o700;

/// Required mode for secret files (keys, tokens)
pub const FILE_MODE: u32 = 0o600;

/// Check that a file has the required permissions.
/// Returns error if permissions are wrong.
pub fn check_file_mode(path: &Path, expected: u32) -> Result<()> {
    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode() & 0o777;

    if mode != expected {
        return Err(NoiseError::PermissionDenied {
            path: path.display().to_string(),
            actual: mode,
            expected,
        });
    }

    Ok(())
}

/// Check that a directory has the required permissions.
pub fn check_dir_mode(path: &Path, expected: u32) -> Result<()> {
    check_file_mode(path, expected)
}

/// Ensure the config directory exists with correct permissions.
/// Creates it if it doesn't exist.
pub fn ensure_config_dir(path: &Path) -> Result<()> {
    if path.exists() {
        // Check permissions
        check_dir_mode(path, DIR_MODE)?;
    } else {
        // Create with correct permissions
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))?;
        tracing::info!(
            path = %path.display(),
            mode = format!("{:o}", DIR_MODE),
            "Created config directory"
        );
    }

    Ok(())
}

/// Set secure permissions on a file after writing.
pub fn secure_file(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn test_check_file_mode_correct() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file");
        File::create(&file_path).unwrap();
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(check_file_mode(&file_path, 0o600).is_ok());
    }

    #[test]
    fn test_check_file_mode_wrong() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file");
        File::create(&file_path).unwrap();
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o644)).unwrap();

        let result = check_file_mode(&file_path, 0o600);
        assert!(result.is_err());
        match result.unwrap_err() {
            NoiseError::PermissionDenied {
                actual, expected, ..
            } => {
                assert_eq!(actual, 0o644);
                assert_eq!(expected, 0o600);
            }
            _ => panic!("Expected PermissionDenied error"),
        }
    }

    #[test]
    fn test_ensure_config_dir_creates() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("new_config");

        assert!(!config_path.exists());
        ensure_config_dir(&config_path).unwrap();
        assert!(config_path.exists());

        let mode = fs::metadata(&config_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, DIR_MODE);
    }

    #[test]
    fn test_ensure_config_dir_checks_existing() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("existing_config");
        fs::create_dir(&config_path).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o755)).unwrap();

        let result = ensure_config_dir(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_secure_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file");
        File::create(&file_path).unwrap();

        secure_file(&file_path).unwrap();

        let mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, FILE_MODE);
    }
}
