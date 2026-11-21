//! Project-wide npm policy in `frontend/.npmrc`. Every npm and npx call
//! (build.rs, make.ps1, make.sh, CI) runs from `frontend/` and reads it, so
//! a setting dropped here is dropped everywhere at once.

fn npmrc() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/frontend/.npmrc");
    std::fs::read_to_string(path).expect("frontend/.npmrc is missing")
}

/// The value of `key=value`, ignoring comments and surrounding blanks.
fn setting(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_owned())
        })
}

#[test]
fn package_install_scripts_never_run() {
    // npm ci runs inside the jobs that compile and upload the release
    // binaries, and on the maintainer's machine: a compromised package's
    // install script would run there with access to both.
    assert_eq!(setting(&npmrc(), "ignore-scripts").as_deref(), Some("true"));
}

#[test]
fn only_week_old_versions_are_resolved() {
    assert_eq!(setting(&npmrc(), "min-release-age").as_deref(), Some("7"));
}
