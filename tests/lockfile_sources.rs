//! Every locked package comes from its public registry, as itself.
//!
//! The 7-day cooldown (`scripts/cargo-cooldown.js`, `frontend/.npmrc`) only
//! judges registry versions. A lockfile edit that points a package at a git
//! repository, another registry or another package's tarball would slip past
//! it, and `cargo --locked` and `npm ci` install whatever the lockfile says.
//! This runs with every `cargo test`, locally and on every pull request.

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
const NPM_REGISTRY: &str = "https://registry.npmjs.org/";

fn read(path: &str) -> String {
    let full = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

#[test]
fn every_crate_comes_from_crates_io() {
    let lock: toml::Table = toml::from_str(&read("Cargo.lock")).unwrap();
    let packages = lock["package"].as_array().expect("no [[package]] entries");
    let mut foreign = Vec::new();
    for package in packages {
        let name = package["name"].as_str().unwrap();
        let version = package["version"].as_str().unwrap();
        match package.get("source").and_then(|s| s.as_str()) {
            // No source: a workspace member, built from this repository.
            None => assert_eq!(name, "dirsync", "{name} {version} has no source"),
            Some(CRATES_IO) => assert!(
                package.get("checksum").is_some(),
                "{name} {version} has no checksum"
            ),
            Some(other) => foreign.push(format!("{name} {version}: {other}")),
        }
    }
    assert!(
        foreign.is_empty(),
        "crates not from crates.io: {foreign:#?}"
    );
}

/// The npm package name an entry installs: its `name` field for an alias,
/// otherwise the path after the last `node_modules/`.
fn npm_name<'a>(key: &'a str, entry: &'a serde_json::Value) -> &'a str {
    entry["name"]
        .as_str()
        .unwrap_or_else(|| key.rsplit_once("node_modules/").map_or(key, |(_, n)| n))
}

#[test]
fn every_npm_package_is_the_registry_tarball_of_its_own_name_and_version() {
    let lock: serde_json::Value =
        serde_json::from_str(&read("frontend/package-lock.json")).unwrap();
    let packages = lock["packages"].as_object().expect("no packages map");
    let mut wrong = Vec::new();
    for (key, entry) in packages {
        // "" is the frontend itself, built from this repository.
        if key.is_empty() {
            continue;
        }
        if entry["link"].as_bool() == Some(true) {
            wrong.push(format!("{key}: a link to a local folder"));
            continue;
        }
        let name = npm_name(key, entry);
        let version = entry["version"].as_str().unwrap_or("");
        let basename = name.rsplit('/').next().unwrap();
        let expected = format!("{NPM_REGISTRY}{name}/-/{basename}-{version}.tgz");
        if entry["resolved"].as_str() != Some(expected.as_str()) {
            wrong.push(format!(
                "{key}: {} (expected {expected})",
                entry["resolved"]
            ));
        }
        if !entry["integrity"]
            .as_str()
            .is_some_and(|i| i.starts_with("sha512-"))
        {
            wrong.push(format!("{key}: no sha512 integrity"));
        }
    }
    assert!(
        wrong.is_empty(),
        "npm packages not from the registry: {wrong:#?}"
    );
}
