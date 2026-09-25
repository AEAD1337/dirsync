//! The license table build.rs generates is committed, so it must come out
//! the same on every machine: npm entries follow the lockfile rather than
//! whatever node_modules holds on this host, platform-specific packages are
//! left out, and a failed metadata collection leaves the committed file
//! alone instead of overwriting it with half the table.
//!
//! build.rs is compiled into this test crate as a module. The license table
//! only exists for the gui feature, and gating the file on it keeps build.rs
//! out of the CLI-only coverage figure.
#![cfg(feature = "gui")]

#[path = "../build.rs"]
#[allow(dead_code)]
mod build_script;

use build_script::{
    LicEntry, bare_imports, npm_runtime_packages, render_licenses_ts, write_licenses_ts,
};
use serde_json::json;

fn entry(name: &str, version: &str) -> LicEntry {
    LicEntry {
        name: name.to_string(),
        version: version.to_string(),
        license: "MIT".to_string(),
        copyright: String::new(),
        url: String::new(),
    }
}

fn lockfile() -> serde_json::Value {
    json!({
        "lockfileVersion": 3,
        "packages": {
            "": { "devDependencies": { "svelte": "^5", "vite": "^8" } },
            "node_modules/svelte": {
                "version": "5.0.0",
                "dev": true,
                "dependencies": { "clsx": "^2", "acorn": "^8" }
            },
            "node_modules/clsx": { "version": "2.1.1", "dev": true },
            // svelte needs acorn 8 but the hoisted copy is 7: npm nests 8
            // under svelte, and resolution must find the nested one.
            "node_modules/acorn": { "version": "7.0.0", "dev": true },
            "node_modules/svelte/node_modules/acorn": {
                "version": "8.0.0",
                "dev": true,
                "optionalDependencies": { "@acorn/binding-win32-x64-msvc": "1" }
            },
            "node_modules/@acorn/binding-win32-x64-msvc": {
                "version": "1.0.0",
                "dev": true,
                "optional": true,
                "os": ["win32"],
                "cpu": ["x64"]
            },
            "node_modules/vite": {
                "version": "8.0.0",
                "dev": true,
                "dependencies": { "rolldown": "1" }
            },
            "node_modules/rolldown": { "version": "1.0.0", "dev": true },
            "node_modules/typescript": { "version": "6.0.0", "dev": true }
        }
    })
}

#[test]
fn npm_selection_follows_the_imported_packages_through_the_lockfile() {
    let got = npm_runtime_packages(&lockfile(), &["svelte".to_string()], &[]);
    assert_eq!(
        got,
        [
            "node_modules/clsx",
            "node_modules/svelte",
            "node_modules/svelte/node_modules/acorn",
        ]
    );
}

#[test]
fn npm_selection_skips_os_and_cpu_restricted_packages() {
    let got = npm_runtime_packages(&lockfile(), &["svelte".to_string()], &[]);
    assert!(!got.iter().any(|k| k.contains("binding-win32")));
}

#[test]
fn npm_injected_packages_are_listed_without_their_dependencies() {
    // Vite ships its modulepreload polyfill in the bundle, but nothing it
    // depends on: build tools like rolldown must not follow it in.
    let got = npm_runtime_packages(&lockfile(), &[], &["vite".to_string()]);
    assert_eq!(got, ["node_modules/vite"]);
}

#[test]
fn npm_roots_missing_from_the_lockfile_are_ignored() {
    let got = npm_runtime_packages(&lockfile(), &["not-installed".to_string()], &[]);
    assert!(got.is_empty());
}

#[test]
fn bare_imports_finds_package_names_and_skips_relative_paths() {
    let src = r#"
        import { onMount } from 'svelte';
        import { writable } from "svelte/store";
        import type { Thing } from '@scope/pkg/sub/path';
        import './styles.css';
        import Local from '../lib/Local.svelte';
        import { licenses } from './licenses_generated';
        const lazy = await import('lazy-pkg');
        import 'side-effect-only';
    "#;
    let mut got = bare_imports(src);
    got.sort();
    got.dedup();
    assert_eq!(
        got,
        ["@scope/pkg", "lazy-pkg", "side-effect-only", "svelte"]
    );
}

#[test]
fn rendering_is_independent_of_the_input_order() {
    let a = render_licenses_ts(vec![
        entry("zeta", "1.0.0"),
        entry("Alpha", "2.0.0"),
        entry("alpha", "1.0.0"),
        entry("mid", "1.0.0"),
        entry("mid", "1.0.0"),
    ]);
    let b = render_licenses_ts(vec![
        entry("mid", "1.0.0"),
        entry("alpha", "1.0.0"),
        entry("zeta", "1.0.0"),
        entry("mid", "1.0.0"),
        entry("Alpha", "2.0.0"),
    ]);
    assert_eq!(a, b);
    // The exact duplicate collapses to one row.
    assert_eq!(a.matches("name: \"mid\"").count(), 1);
}

#[test]
fn a_failed_collection_keeps_the_existing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("licenses_generated.ts");
    std::fs::write(&dest, "previous complete table\n").unwrap();

    write_licenses_ts(
        &dest,
        Err("cargo metadata failed".to_string()),
        Ok(vec![entry("svelte", "5.0.0")]),
    );
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "previous complete table\n"
    );

    write_licenses_ts(
        &dest,
        Ok(vec![entry("serde", "1.0.0")]),
        Err("package-lock.json missing".to_string()),
    );
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "previous complete table\n"
    );
}

#[test]
fn a_successful_collection_writes_both_halves() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("licenses_generated.ts");
    write_licenses_ts(
        &dest,
        Ok(vec![entry("serde", "1.0.0")]),
        Ok(vec![entry("svelte", "5.0.0")]),
    );
    let text = std::fs::read_to_string(&dest).unwrap();
    assert!(text.contains("name: \"serde\""));
    assert!(text.contains("name: \"svelte\""));
}
