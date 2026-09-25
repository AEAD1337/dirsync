use dirsync::paths::is_system_critical;
use std::path::Path;

#[cfg(windows)]
#[test]
fn a_parent_of_a_system_directory_is_critical_too() {
    // Mirroring into C:\Users deletes every profile, C:\Users\Default included.
    assert!(is_system_critical(Path::new(r"C:\Users")));
    assert!(!is_system_critical(Path::new(r"C:\Users\someone\Backup")));
}

#[cfg(not(windows))]
#[test]
fn a_parent_of_a_system_directory_is_critical_too() {
    // A DST of /usr mirror-deletes /usr/bin, which is itself blocked.
    assert!(is_system_critical(Path::new("/usr")));
    assert!(is_system_critical(Path::new("/var")));
    assert!(!is_system_critical(Path::new("/home/someone/backup")));
}

#[cfg(not(windows))]
#[test]
fn canonical_macos_and_usrmerge_forms_are_critical() {
    // canonicalize() turns /etc into /private/etc on macOS, and /lib64 into
    // /usr/lib64 on usrmerge Linux: the guard runs on canonical paths.
    for p in [
        "/private/etc",
        "/private/var/db",
        "/usr/lib64",
        "/System/Library",
    ] {
        assert!(is_system_critical(Path::new(p)), "{p}");
    }
}
