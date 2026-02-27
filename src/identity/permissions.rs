//! Cross-platform private key file permissions.
//!
//! Ensures private key files have restrictive permissions to prevent
//! unauthorized access. On Unix, sets mode 0600 (owner read/write only).
//! On Windows, restricts ACL to owner and SYSTEM only.

use std::path::Path;
use tracing::warn;

use crate::error::Result;

/// Secure a private key file by setting restrictive permissions.
///
/// - **Unix**: Sets file mode to 0600 (owner read/write only)
/// - **Windows**: Restricts ACL to owner and SYSTEM only
/// - **Other platforms**: Logs a warning and returns Ok (graceful degradation)
///
/// This function should be called AFTER the file is written. Errors are logged
/// as warnings but do not fail the operation to allow graceful degradation on
/// systems where permission setting is not supported or fails.
pub fn secure_private_key(path: &Path) -> Result<()> {
    match secure_private_key_impl(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to set restrictive permissions on private key file"
            );
            Ok(())
        }
    }
}

#[cfg(unix)]
fn secure_private_key_impl(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(windows)]
fn secure_private_key_impl(path: &Path) -> std::io::Result<()> {
    use windows_acl::acl::ACL;
    use windows_acl::helper;

    // Get current user SID
    let current_user = helper::current_user()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    // Get SYSTEM SID
    let system_sid = helper::string_to_sid("S-1-5-18")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    // Create new ACL with only owner and SYSTEM having full control
    let path_str = path.to_string_lossy();
    let mut acl = ACL::from_file_path(&path_str, false)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    // Remove all existing entries
    let entries = acl.all().map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    })?;
    for entry in entries {
        let _ = acl.remove(&entry.sid, Some(entry.entry_type), None);
    }

    // Add owner with full control
    acl.add_entry(&current_user, windows_acl::acl::AceType::AccessAllow, 0x1F01FF)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    // Add SYSTEM with full control
    acl.add_entry(&system_sid, windows_acl::acl::AceType::AccessAllow, 0x1F01FF)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn secure_private_key_impl(path: &Path) -> std::io::Result<()> {
    warn!(
        path = %path.display(),
        "Platform does not support file permission setting; private key may be world-readable"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_nonexistent_file_graceful() {
        // Should not panic, just log warning and return Ok
        let result = secure_private_key(Path::new("/nonexistent/path/to/key"));
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn sets_mode_0600() {
            let dir = tempfile::tempdir().unwrap();
            let key_path = dir.path().join("test.key");

            // Create file with permissive mode
            std::fs::write(&key_path, b"secret").unwrap();
            let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&key_path, perms).unwrap();

            // Verify initial mode
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o644);

            // Apply secure permissions
            secure_private_key(&key_path).unwrap();

            // Verify mode is now 0600
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
