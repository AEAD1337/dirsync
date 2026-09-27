//! The Windows version resource names each executable. The CLI-only build is
//! published as dirsync-cli.exe, so its resource must say so: scanners and
//! VirusTotal report the embedded original filename.
#![cfg(feature = "gui")]

#[path = "../build.rs"]
#[allow(dead_code)]
mod build_script;

#[test]
fn the_gui_build_is_named_dirsync() {
    assert_eq!(build_script::exe_names(true), ("dirsync.exe", "dirsync"));
}

#[test]
fn the_cli_only_build_is_named_dirsync_cli() {
    assert_eq!(
        build_script::exe_names(false),
        ("dirsync-cli.exe", "dirsync-cli")
    );
}
