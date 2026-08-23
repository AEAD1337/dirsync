//! Sync-endpoint validation shared by both entrypoints.
//!
//! These checks used to live in `gui/handlers.rs` and therefore applied only to
//! the GUI. CLI mode reached the engine with no validation at all, which made
//! two destructive configurations reachable from a single command: a SRC nested
//! inside DST (every DST sibling becomes an orphan and is deleted) and a DST
//! nested inside SRC (each run copies the previous run's output one level
//! deeper, without bound). The guards now live here and are called by both.

use std::path::{Path, PathBuf};

/// Forward-slash string form of a path: the wire format the frontend
/// consumes for every rel-path (tree keys, exclude matching, dir-size maps).
pub fn to_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `root`-relative, forward-slash form of `p`; falls back to the full path
/// when `p` is not under `root`. The frontend string-matches these values
/// across events (op rows vs `ops_completed`), so every producer must go
/// through this one implementation.
pub fn rel_to_root(p: &Path, root: &Path) -> String {
    to_slash(p.strip_prefix(root).unwrap_or(p))
}

/// Canonicalize `path` as far as possible.  For paths that do not (fully)
/// exist yet: e.g. a DST that will be created on run: walk up to the deepest
/// ancestor that exists, canonicalize that, then re-append the remaining
/// components.  This resolves `..`, symlinks, and Windows case differences for
/// the portion of the path that is already on disk.
pub fn canonicalize_or_partial(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    // Find the deepest existing ancestor.
    let mut base = path;
    while let Some(parent) = base.parent() {
        if let Ok(c) = std::fs::canonicalize(parent) {
            // Re-append the components that were stripped.
            let suffix = path.strip_prefix(parent).unwrap_or(path);
            return c.join(suffix);
        }
        base = parent;
    }
    path.to_path_buf()
}

/// Render a path for a user-facing message.
///
/// `std::fs::canonicalize` returns extended-length paths on Windows
/// (`\\?\C:\Users\Default`). That prefix is meaningful to the OS but noise in
/// an error message, so strip it for display only: never for comparison.
pub fn display_path(path: &Path) -> String {
    let s = path.display().to_string();
    #[cfg(windows)]
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        return stripped.to_owned();
    }
    s
}

/// Returns an error string if `path` should be rejected as a sync endpoint.
/// `must_exist`: pass `true` when the path must already be a readable directory
/// (e.g. SRC, or DST when a preview is requested against an existing tree).
/// Pass `false` when the path may not yet exist.
///
/// `path` is expected to be canonical already: see [`canonicalize_or_partial`].
/// Passing a raw path lets `..` traversal forms slip past `is_system_critical`.
pub fn validate_sync_path(path: &Path, must_exist: bool, yolo: bool) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(format!("'{}' is not a directory", display_path(path)));
            }
            if !yolo && is_system_critical(path) {
                return Err(format!(
                    "'{}' is a system-critical path and cannot be used as a sync endpoint",
                    display_path(path)
                ));
            }
        }
        Err(e) if must_exist => {
            return Err(format!("Cannot access '{}': {}", display_path(path), e));
        }
        Err(_) => {} // path does not exist: caller decides whether that is an error
    }

    Ok(())
}

/// Returns true if `path` is (or is inside) a well-known system directory that
/// should never be used as a sync source or destination.
pub fn is_system_critical(path: &Path) -> bool {
    // Unix filesystem root ("/") has no parent. On Windows we do NOT use
    // parent()==None because bare drive-letter paths like "D:" (no backslash)
    // also satisfy that condition even though they are not the drive root:
    // the Windows branch below uses component counting to catch "D:\" correctly.
    #[cfg(not(windows))]
    if path.parent().is_none() {
        return true;
    }

    #[cfg(windows)]
    {
        let p = path.to_string_lossy().to_lowercase();
        let p = p.replace('\\', "/");
        // std::fs::canonicalize on Windows returns \\?\-prefixed paths
        // (extended-length format). Strip the normalised form "//?/" so the
        // prefix-matching below works on canonical paths as well as raw ones.
        let p = p.strip_prefix("//?/").unwrap_or(&p);

        // Block C:\ (the system drive root) and known system directories under it.
        // Other drive roots (D:\, E:\, …) are permitted as sync endpoints.
        // Use component-aware matching: "c:/windows" must match "c:/windows" and
        // "c:/windows/system32" but NOT "c:/windows123".
        if p == "c:/" {
            return true;
        }
        let critical = [
            "c:/windows",
            "c:/program files",
            "c:/program files (x86)",
            "c:/programdata",
            "c:/system volume information",
            "c:/recovery",
            "c:/users/default",
        ];
        critical
            .iter()
            .any(|c| p == *c || p.starts_with(&format!("{c}/")))
    }

    #[cfg(not(windows))]
    {
        let critical = [
            "/proc",
            "/sys",
            "/dev",
            "/etc",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib",
            "/boot",
            "/root",
        ];
        critical.iter().any(|c| path.starts_with(c))
    }
}

/// Fully validate a SRC/DST pair and return their canonical forms.
///
/// Performs, in order: canonicalization (so `..` forms cannot bypass the
/// system-critical guard), per-endpoint validation, and the three nesting
/// checks. Both endpoints must already exist: the engine creates the DST root
/// on run, but a DST that does not exist yet cannot be meaningfully previewed
/// and both entrypoints have always required it.
///
/// The nesting checks are the reason this function exists: SRC and DST must be
/// disjoint subtrees or a one-way mirror either deletes the non-overlapping
/// remainder of DST or recursively copies into itself.
pub fn validate_endpoints(
    src: &Path,
    dst: &Path,
    yolo: bool,
) -> Result<(PathBuf, PathBuf), String> {
    let canon_src = std::fs::canonicalize(src)
        .map_err(|e| format!("Cannot resolve '{}': {}", src.display(), e))?;
    let canon_dst = canonicalize_or_partial(dst);

    validate_sync_path(&canon_src, true, yolo)?;
    validate_sync_path(&canon_dst, true, yolo)?;

    if canon_src == canon_dst {
        return Err("Source and destination must be different paths".to_owned());
    }
    if canon_dst.starts_with(&canon_src) {
        return Err(format!(
            "Destination '{}' is inside source '{}' - each run would copy the previous run's output one level deeper",
            dst.display(),
            src.display()
        ));
    }
    if canon_src.starts_with(&canon_dst) {
        return Err(format!(
            "Source '{}' is inside destination '{}' - everything else in the destination would be deleted as an orphan",
            src.display(),
            dst.display()
        ));
    }

    Ok((canon_src, canon_dst))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- display_path ---

    #[test]
    #[cfg(windows)]
    fn test_display_path_strips_extended_length_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\Default")),
            r"C:\Users\Default"
        );
    }

    #[test]
    fn test_display_path_leaves_normal_paths_alone() {
        let p = if cfg!(windows) { r"D:\data" } else { "/data" };
        assert_eq!(display_path(Path::new(p)), p);
    }

    // --- is_system_critical ---

    #[test]
    #[cfg(windows)]
    fn test_is_system_critical_windows_blocks_c_root() {
        assert!(is_system_critical(Path::new("C:\\")));
        assert!(is_system_critical(Path::new("c:\\")));
    }

    #[test]
    #[cfg(windows)]
    fn test_is_system_critical_windows_blocks_known_dirs() {
        for path in &[
            "C:\\Windows",
            "C:\\Windows\\System32",
            "C:\\Program Files",
            "C:\\Program Files (x86)",
            "C:\\ProgramData",
            "C:\\Users\\Default",
        ] {
            assert!(
                is_system_critical(Path::new(path)),
                "{path} should be system-critical"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn test_is_system_critical_windows_allows_other_drives() {
        assert!(!is_system_critical(Path::new("D:\\")));
        assert!(!is_system_critical(Path::new("D:\\MyData")));
        assert!(!is_system_critical(Path::new("E:\\Backup")));
    }

    /// "c:/windows" must not match "c:/windows123": component-aware matching.
    #[test]
    #[cfg(windows)]
    fn test_is_system_critical_windows_no_prefix_false_positive() {
        assert!(!is_system_critical(Path::new("C:\\Windows123")));
        assert!(!is_system_critical(Path::new("C:\\ProgramDataX")));
    }

    #[test]
    #[cfg(not(windows))]
    fn test_is_system_critical_unix_blocks_root() {
        assert!(is_system_critical(Path::new("/")));
    }

    #[test]
    #[cfg(not(windows))]
    fn test_is_system_critical_unix_blocks_known_dirs() {
        for path in &[
            "/proc", "/sys", "/dev", "/etc", "/etc/ssh", "/bin", "/usr/bin", "/boot",
        ] {
            assert!(
                is_system_critical(Path::new(path)),
                "{path} should be system-critical"
            );
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn test_is_system_critical_unix_allows_user_paths() {
        for path in &["/home/user", "/tmp", "/mnt/data", "/var/log"] {
            assert!(
                !is_system_critical(Path::new(path)),
                "{path} should not be system-critical"
            );
        }
    }

    // --- validate_sync_path ---

    #[test]
    fn test_validate_rejects_file_path() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("not_a_dir.txt");
        std::fs::write(&file, b"hi").unwrap();

        let err = validate_sync_path(&file, false, false).unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn test_validate_rejects_missing_when_must_exist() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does_not_exist");

        assert!(validate_sync_path(&missing, true, false).is_err());
    }

    #[test]
    fn test_validate_allows_missing_when_not_must_exist() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does_not_exist");

        assert!(validate_sync_path(&missing, false, false).is_ok());
    }

    #[test]
    fn test_validate_allows_normal_directory() {
        let dir = TempDir::new().unwrap();
        assert!(validate_sync_path(dir.path(), true, false).is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn test_validate_blocks_c_windows_without_yolo() {
        let p = Path::new("C:\\Windows");
        if p.is_dir() {
            let err = validate_sync_path(p, false, false).unwrap_err();
            assert!(err.contains("system-critical"), "got: {err}");
        }
    }

    #[test]
    #[cfg(windows)]
    fn test_validate_yolo_bypasses_system_critical() {
        let p = Path::new("C:\\Windows");
        if p.is_dir() {
            assert!(validate_sync_path(p, false, true).is_ok());
        }
    }

    /// Traversal forms that resolve to system-critical directories must be
    /// rejected once canonicalized: callers must canonicalize first.
    #[test]
    #[cfg(windows)]
    fn test_validate_traversal_to_system_dir_is_blocked() {
        for raw in &[
            "C:\\Users\\..\\Windows",
            "C:\\.\\Windows",
            "C:\\Windows\\..\\Windows\\System32",
        ] {
            let p = PathBuf::from(raw);
            if let Ok(canon) = std::fs::canonicalize(&p) {
                let err = validate_sync_path(&canon, false, false).unwrap_err();
                assert!(
                    err.contains("system-critical"),
                    "{raw} canonicalized to `{}` but was not blocked: {err}",
                    canon.display()
                );
            }
        }
    }

    // --- validate_endpoints: nesting ---

    #[test]
    fn test_endpoints_rejects_identical_paths() {
        let dir = TempDir::new().unwrap();
        let err = validate_endpoints(dir.path(), dir.path(), false).unwrap_err();
        assert!(err.contains("must be different"), "got: {err}");
    }

    #[test]
    fn test_endpoints_rejects_dst_inside_src() {
        let dir = TempDir::new().unwrap();
        let inner = dir.path().join("backup");
        std::fs::create_dir(&inner).unwrap();

        let err = validate_endpoints(dir.path(), &inner, false).unwrap_err();
        assert!(err.contains("is inside source"), "got: {err}");
    }

    #[test]
    fn test_endpoints_rejects_src_inside_dst() {
        let dir = TempDir::new().unwrap();
        let inner = dir.path().join("photos");
        std::fs::create_dir(&inner).unwrap();

        let err = validate_endpoints(&inner, dir.path(), false).unwrap_err();
        assert!(err.contains("is inside destination"), "got: {err}");
    }

    /// Nesting must be detected through `..` and `.` traversal forms too, which
    /// is why the check runs on canonical paths.
    #[test]
    fn test_endpoints_rejects_nesting_via_traversal() {
        let dir = TempDir::new().unwrap();
        let inner = dir.path().join("backup");
        std::fs::create_dir(&inner).unwrap();
        // dir/backup/.. == dir, so this is really src == dst.
        let traversal = inner.join("..");

        assert!(validate_endpoints(dir.path(), &traversal, false).is_err());
    }

    #[test]
    fn test_endpoints_accepts_disjoint_siblings() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();

        let (cs, cd) = validate_endpoints(&src, &dst, false).unwrap();
        assert!(cs.ends_with("src"));
        assert!(cd.ends_with("dst"));
    }

    /// A sibling whose name merely starts with the other's name must not be
    /// mistaken for a nested path ("dst2" is not inside "dst").
    #[test]
    fn test_endpoints_accepts_name_prefix_siblings() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("data");
        let dst = dir.path().join("data2");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();

        assert!(validate_endpoints(&src, &dst, false).is_ok());
    }

    #[test]
    fn test_endpoints_rejects_missing_src() {
        let dir = TempDir::new().unwrap();
        let dst = dir.path().join("dst");
        std::fs::create_dir(&dst).unwrap();

        let err = validate_endpoints(&dir.path().join("nope"), &dst, false).unwrap_err();
        assert!(err.contains("Cannot resolve"), "got: {err}");
    }
}
