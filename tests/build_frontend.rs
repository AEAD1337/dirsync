//! `SKIP_FRONTEND_BUILD`: the frontend was built in another job, so build.rs
//! runs no npm at all and embeds the bundle already in `frontend/dist`. The
//! release workflow relies on it to keep npm package code out of the jobs
//! that compile the binaries. A missing bundle must stop the build instead
//! of producing a binary with an empty GUI.
#![cfg(feature = "gui")]

#[path = "../build.rs"]
#[allow(dead_code)]
mod build_script;

use build_script::prebuilt_frontend;
use tempfile::TempDir;

#[test]
fn a_prebuilt_bundle_is_accepted() {
    let frontend = TempDir::new().unwrap();
    std::fs::create_dir(frontend.path().join("dist")).unwrap();
    std::fs::write(
        frontend.path().join("dist").join("index.html"),
        "<!doctype html>",
    )
    .unwrap();

    assert!(prebuilt_frontend(frontend.path()).is_ok());
}

#[test]
fn a_missing_bundle_stops_the_build_with_a_hint() {
    let frontend = TempDir::new().unwrap();

    let err = prebuilt_frontend(frontend.path()).unwrap_err();

    assert!(err.contains("index.html"), "{err}");
    assert!(err.contains("SKIP_FRONTEND_BUILD"), "{err}");
}
