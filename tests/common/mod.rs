//! Helpers shared by several integration test binaries.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Makes `dir` unlistable for the duration of the guard.
///
/// Returns `None` when the platform or the current user cannot be denied
/// (root on Unix reads everything), so the caller skips instead of passing
/// vacuously.
pub fn deny_listing(dir: &Path) -> Option<DenyGuard> {
    let guard = DenyGuard(dir.to_path_buf());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).ok()?;
    }
    #[cfg(windows)]
    {
        // Deny "list folder" to Everyone: deny ACEs bind the owner too.
        let ok = std::process::Command::new("icacls")
            .arg(dir)
            .args(["/deny", "*S-1-1-0:(RD)"])
            .output()
            .ok()?
            .status
            .success();
        if !ok {
            return None;
        }
    }
    if std::fs::read_dir(dir).is_ok() {
        return None; // guard drops and restores
    }
    Some(guard)
}

pub struct DenyGuard(PathBuf);

impl Drop for DenyGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", "*S-1-1-0"])
                .output();
        }
    }
}

/// Windows' "A required privilege is not held by the client": the only
/// acceptable reason for a symlink creation to fail in these tests.
pub const WINDOWS_PRIVILEGE_NOT_HELD: i32 = 1314;

/// Create a file symlink at `link` pointing at `target`.
///
/// Returns `false` only when Windows refuses for lack of
/// SeCreateSymbolicLinkPrivilege (no Developer Mode, not elevated), and
/// asserts that this is exactly why it failed: any other error panics, so a
/// broken fixture cannot pass as a skipped test.
pub fn symlink_file_or_unprivileged(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    match result {
        Ok(()) => true,
        Err(e) => {
            assert_privilege_error(&e.to_string(), e.raw_os_error());
            eprintln!("skipped: symlink creation needs a privilege this user lacks");
            false
        }
    }
}

/// Assert that a failure is the Windows missing-privilege error and nothing
/// else. `message` is the error text, `code` the raw OS error when known.
pub fn assert_privilege_error(message: &str, code: Option<i32>) {
    if cfg!(not(windows)) {
        panic!("symlink creation must not fail on this platform: {message}");
    }
    let matches = match code {
        Some(c) => c == WINDOWS_PRIVILEGE_NOT_HELD,
        None => message.contains(&format!("(os error {WINDOWS_PRIVILEGE_NOT_HELD})")),
    };
    assert!(
        matches,
        "expected the missing-privilege error (os error {WINDOWS_PRIVILEGE_NOT_HELD}), got: {message}"
    );
}
