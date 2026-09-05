//! End-to-end tests that run the compiled binary.
//!
//! `main.rs`, the `--help` / `--version` exits in `cli.rs` and the unknown
//! shell exit in `completions.rs` all end in `std::process::exit`, so they can
//! only be exercised from the outside. Spawning the binary also proves the
//! argument handling and the CLI wiring agree with each other.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_dirsync");

fn run(args: &[&str]) -> Output {
    Command::new(EXE)
        .args(args)
        .output()
        .expect("failed to run the dirsync binary")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write_file(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn bare_invocation_prints_help_and_succeeds() {
    let out = run(&[]);

    // A bare run is a request for orientation, not a usage error: exit 0.
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("USAGE:"));
}

#[test]
fn help_flag_prints_help_and_exits_zero() {
    for flag in ["-h", "--help"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        let text = stdout(&out);
        assert!(text.contains("USAGE:"), "{flag} printed: {text}");
        assert!(text.contains("SUBCOMMANDS:"));
        assert!(text.contains("EXCLUDE PATTERNS:"));
    }
}

#[test]
fn version_flag_prints_version_and_license() {
    for flag in ["-V", "--version"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        let text = stdout(&out);
        assert!(text.starts_with("dirsync "), "{flag} printed: {text}");
        assert!(text.contains("License: GPL-3.0-only"));
    }
}

#[test]
fn completions_print_a_script_for_every_supported_shell() {
    for (shell, marker) in [
        ("bash", "complete -F _dirsync dirsync"),
        ("zsh", "#compdef dirsync"),
        ("fish", "complete -c dirsync"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let out = run(&["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        assert!(
            stdout(&out).contains(marker),
            "{shell} script missing {marker:?}"
        );
    }
}

#[test]
fn completions_reject_an_unknown_shell() {
    let out = run(&["completions", "tcsh"]);

    assert_eq!(out.status.code(), Some(2));
    let text = stderr(&out);
    assert!(text.contains("Unknown shell"));
    assert!(text.contains("bash, zsh, fish, powershell"));
}

#[test]
fn completions_without_a_shell_argument_is_rejected() {
    // The empty shell name takes the same branch as an unknown one.
    let out = run(&["completions"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Unknown shell"));
}

#[test]
fn missing_src_and_dst_is_a_usage_error() {
    let out = run(&["--dry-run"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("SRC and DST required"));
}

#[test]
fn a_lone_src_is_a_usage_error() {
    let src = TempDir::new().unwrap();
    let out = run(&[src.path().to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("SRC and DST required"));
}

#[test]
fn an_out_of_range_port_is_rejected() {
    let out = run(&["--port", "80"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Error:"));
}

#[test]
fn a_non_numeric_port_is_rejected() {
    let out = run(&["--port", "not-a-port"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("invalid port number"));
}

#[test]
fn an_unknown_flag_is_rejected() {
    let out = run(&["--definitely-not-a-flag"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("Error:"));
}

#[test]
fn a_third_positional_argument_is_rejected() {
    let out = run(&["one", "two", "three"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unexpected positional argument"));
}

#[test]
fn nested_endpoints_are_rejected() {
    let src = TempDir::new().unwrap();
    let dst = src.path().join("inside");
    fs::create_dir(&dst).unwrap();

    let out = run(&[src.path().to_str().unwrap(), dst.to_str().unwrap()]);

    assert!(!out.status.success());
    // A DST inside SRC would copy its own output one level deeper every run.
    assert!(
        stderr(&out).contains("is inside source"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn identical_directories_report_nothing_to_do() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.txt", b"same");
    write_file(dst.path(), "a.txt", b"same");

    let out = run(&[src.path().to_str().unwrap(), dst.path().to_str().unwrap()]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Nothing to do."));
}

#[test]
fn dry_run_reports_the_plan_without_touching_dst() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "new.txt", b"payload");

    let out = run(&[
        "--dry-run",
        src.path().to_str().unwrap(),
        dst.path().to_str().unwrap(),
    ]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("dry-run"));
    assert!(
        !dst.path().join("new.txt").exists(),
        "a dry run must not write anything"
    );
}

#[test]
fn a_real_run_mirrors_src_into_dst() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.txt", b"payload");
    write_file(src.path(), "sub/b.txt", b"nested");
    write_file(dst.path(), "orphan.txt", b"delete me");

    let out = run(&[src.path().to_str().unwrap(), dst.path().to_str().unwrap()]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(fs::read(dst.path().join("a.txt")).unwrap(), b"payload");
    assert_eq!(fs::read(dst.path().join("sub/b.txt")).unwrap(), b"nested");
    assert!(
        !dst.path().join("orphan.txt").exists(),
        "orphans are removed by a mirror sync"
    );
}

#[test]
fn excludes_from_the_command_line_are_applied() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "keep.txt", b"keep");
    write_file(src.path(), "skip.tmp", b"skip");

    let out = run(&[
        "-e",
        "*.tmp",
        src.path().to_str().unwrap(),
        dst.path().to_str().unwrap(),
    ]);

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(dst.path().join("keep.txt").exists());
    assert!(!dst.path().join("skip.tmp").exists());
}

#[test]
#[cfg(not(feature = "gui"))]
fn the_cli_only_binary_refuses_gui_mode() {
    let out = run(&["--gui"]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("compiled without GUI support"));
}
