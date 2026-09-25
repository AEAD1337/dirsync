use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"))
}

fn main() {
    // Always re-run if these change
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package-lock.json");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    // Vite inputs that live outside frontend/src: without these, editing them
    // alone leaves build.rs un-run, so Vite never re-runs and the binary keeps
    // shipping the previous bundle.
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/tsconfig.json");
    println!("cargo:rerun-if-changed=frontend/svelte.config.js");
    // Vite copies frontend/public into dist verbatim (favicons), so a change
    // there alone must re-run Vite as well.
    println!("cargo:rerun-if-changed=frontend/public");
    // Toggling the skip must re-run build.rs: otherwise unsetting it leaves
    // the Vite build skipped until some unrelated input changes.
    println!("cargo:rerun-if-env-changed=SKIP_FRONTEND_BUILD");

    // Inject build time and version
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let build_time = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    println!("cargo:rustc-env=APP_VERSION={version}");
    println!("cargo:rustc-env=BUILD_TIME={build_time}");
    println!("cargo:rustc-env=BUILD_PROFILE={profile}");

    // Embed icon into the Windows executable
    // Not under cfg(test): tests/build_licenses.rs compiles this file as a
    // module, and winres is a build-dependency the test crate cannot see.
    #[cfg(all(windows, not(test)))]
    embed_icon();

    let root = manifest_dir();

    // Sync version into README.md badge and frontend files unconditionally
    // (these don't require the gui feature flag).
    sync_readme_version(&root, &version);

    // Only build frontend when the gui feature is enabled
    if std::env::var("CARGO_FEATURE_GUI").is_err() {
        return;
    }

    let frontend = root.join("frontend");

    sync_npm_version(&frontend, &version);
    sync_package_lock_version(&frontend, &version);
    if !frontend.exists() {
        println!("cargo:warning=frontend/ directory not found, skipping Svelte build");
        return;
    }

    // Check if npm is available
    let npm_cmd = if cfg!(target_os = "windows") {
        "npm.cmd"
    } else {
        "npm"
    };
    if Command::new(npm_cmd).arg("--version").output().is_err() {
        println!(
            "cargo:warning=npm not found - skipping frontend build. Install Node.js to build the GUI (https://nodejs.org/en/download)."
        );
        return;
    }

    // Run npm ci if node_modules is absent
    let node_modules = frontend.join("node_modules");
    if !node_modules.exists() {
        run_npm(&frontend, npm_cmd, &["ci"]);
    }

    // Generate the license data TypeScript file from live Cargo + npm metadata.
    // Must happen before vite build so the import resolves.
    generate_licenses_ts(&root, &frontend);

    // Skip vite build if env var set (e.g. CI matrix after building once)
    if std::env::var("SKIP_FRONTEND_BUILD").is_ok() {
        return;
    }

    // Always invoke Vite: it has its own incremental build cache and is fast.
    // The old mtime-sentinel heuristic was incomplete: editing any file other
    // than App.svelte silently skipped the rebuild and shipped stale assets.
    run_npm_with_env(
        &frontend,
        npm_cmd,
        &["run", "build"],
        &[("APP_PROFILE", &profile)],
    );
}

// ---------------------------------------------------------------------------
// License data generation
// ---------------------------------------------------------------------------

pub struct LicEntry {
    pub name: String,
    pub version: String,
    pub license: String,
    pub copyright: String,
    pub url: String,
}

/// The targets the release workflow ships. The Rust half of the table is
/// filtered to these, so it describes the published binaries and comes out
/// the same whichever host runs the build.
const RELEASE_TARGETS: &[&str] = &["x86_64-pc-windows-msvc", "x86_64-unknown-linux-musl"];

/// npm packages whose code lands in the bundle without being imported by
/// frontend/src: Vite injects its modulepreload polyfill into the entry
/// chunk. Listed without their dependencies, which stay build-time only.
const NPM_INJECTED: &[&str] = &["vite"];

fn generate_licenses_ts(root: &Path, frontend: &Path) {
    let dest = frontend.join("src/lib/licenses_generated.ts");
    write_licenses_ts(
        &dest,
        collect_rust_packages(root),
        collect_npm_packages(frontend),
    );
}

/// Write the table only when both halves were collected. The file is
/// committed, so a partial table (say, `cargo metadata` failing offline)
/// would silently drop every Rust entry from the About dialog: keeping the
/// previous complete file is the better failure.
pub fn write_licenses_ts(
    dest: &Path,
    rust: Result<Vec<LicEntry>, String>,
    npm: Result<Vec<LicEntry>, String>,
) {
    let (mut entries, npm) = match (rust, npm) {
        (Ok(r), Ok(n)) => (r, n),
        (Err(e), _) | (_, Err(e)) => {
            println!(
                "cargo:warning=License data incomplete ({e}), keeping the existing licenses_generated.ts"
            );
            return;
        }
    };
    entries.extend(npm);
    let out = render_licenses_ts(entries);

    // Only write if the content changed to avoid spurious vite rebuilds
    let existing = std::fs::read_to_string(dest).unwrap_or_default();
    if existing != out {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Err(e) = std::fs::write(dest, &out) {
            println!("cargo:warning=Could not write licenses_generated.ts: {e}");
        }
    }
}

/// Render the TypeScript module. Sorted and merged here, so the output does
/// not depend on the order the collectors produced entries in.
pub fn render_licenses_ts(mut entries: Vec<LicEntry>) -> String {
    // Sort by name (case-insensitive), then name, then version, so the order
    // is total and versions are in order.
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.name.cmp(&b.name))
            .then(a.version.cmp(&b.version))
    });
    // Remove exact duplicates (same name + version from both Rust and npm scans).
    entries.dedup_by(|a, b| a.name == b.name && a.version == b.version);

    // Merge multiple versions of the same package into one row.
    // `version` becomes a comma-separated list; other fields taken from first occurrence.
    let mut merged: Vec<LicEntry> = Vec::new();
    for e in entries {
        if let Some(last) = merged.last_mut()
            && last.name == e.name
        {
            last.version.push_str(", ");
            last.version.push_str(&e.version);
            continue;
        }
        merged.push(e);
    }

    let mut out = String::from(
        "// AUTO-GENERATED by build.rs on every `cargo build`: do not edit manually.\n\
         // Source: `cargo metadata` filtered to the release targets, plus the npm\n\
         // packages frontend/src imports, resolved through frontend/package-lock.json.\n\n\
         export interface LicenseEntry {\n\
           name: string;\n\
           version: string;\n\
           license: string;\n\
           copyright: string;\n\
           url: string;\n\
         }\n\n\
         export const licenses: LicenseEntry[] = [\n",
    );

    for e in &merged {
        out.push_str(&format!(
            "  {{ name: {}, version: {}, license: {}, copyright: {}, url: {} }},\n",
            ts_str(&e.name),
            ts_str(&e.version),
            ts_str(&e.license),
            ts_str(&e.copyright),
            ts_str(&e.url),
        ));
    }
    out.push_str("];\n");
    out
}

fn ts_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// Rust packages: parsed from `cargo metadata` JSON output
// ---------------------------------------------------------------------------

/// BFS over the resolve graph starting from every workspace member, following
/// only edges whose `dep_kinds` contains at least one entry with `kind: null`
/// (i.e. normal runtime dependency). Returns the set of package IDs that are
/// actually linked into the final binary.
fn runtime_package_ids(json: &serde_json::Value) -> std::collections::HashSet<String> {
    // Build a map: package-id → node (for quick lookup)
    let nodes: std::collections::HashMap<&str, &serde_json::Value> = json["resolve"]["nodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n["id"].as_str().map(|id| (id, n)))
                .collect()
        })
        .unwrap_or_default();

    // Seed the BFS with workspace members (our own crates)
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    if let Some(members) = json["workspace_members"].as_array() {
        for m in members {
            if let Some(id) = m.as_str()
                && visited.insert(id.to_string())
            {
                queue.push_back(id.to_string());
            }
        }
    }

    while let Some(id) = queue.pop_front() {
        let node = match nodes.get(id.as_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(deps) = node["deps"].as_array() {
            for dep in deps {
                // dep_kinds is an array of {kind, target} objects.
                // kind is null for normal deps, "dev" for dev-deps,
                // "build" for build-deps.
                let is_runtime = dep["dep_kinds"]
                    .as_array()
                    .map(|kinds| kinds.iter().any(|k| k["kind"].is_null()))
                    .unwrap_or(false);
                if is_runtime
                    && let Some(pkg_id) = dep["pkg"].as_str()
                    && visited.insert(pkg_id.to_string())
                {
                    queue.push_back(pkg_id.to_string());
                }
            }
        }
    }

    visited
}

fn collect_rust_packages(root: &Path) -> Result<Vec<LicEntry>, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(&cargo);
    cmd.args(["metadata", "--format-version", "1"]);
    for target in RELEASE_TARGETS {
        cmd.args(["--filter-platform", target]);
    }
    let output = match cmd.current_dir(root).output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => return Err(format!("`cargo metadata` failed (exit {})", o.status)),
        Err(e) => return Err(format!("could not run `cargo metadata`: {e}")),
    };

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("could not parse `cargo metadata` output: {e}"))?;

    // Walk the resolve graph from the workspace root(s), following only
    // normal (kind: null) edges. This excludes dev-dependencies and
    // build-dependencies that are not also normal deps.
    let runtime_ids = runtime_package_ids(&json);

    let root_name = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    let packages = json["packages"]
        .as_array()
        .ok_or("`cargo metadata` output has no packages")?;

    let mut entries = Vec::new();
    for pkg in packages {
        let name = match pkg["name"].as_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let id = pkg["id"].as_str().unwrap_or("");
        // Skip our own crate, local path-only deps, and anything not
        // reachable via normal (non-dev, non-build) dependency edges.
        if name == root_name || pkg["source"].is_null() || !runtime_ids.contains(id) {
            continue;
        }
        let version = pkg["version"].as_str().unwrap_or("").to_string();
        let license = pkg["license"].as_str().unwrap_or("unknown").to_string();
        let repo_url = pkg["repository"].as_str().unwrap_or("").to_string();
        let authors: Vec<String> = pkg["authors"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(strip_email)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let url = if !repo_url.is_empty() {
            clean_repo_url(&repo_url)
        } else {
            format!("https://crates.io/crates/{name}")
        };

        let crate_dir = pkg["manifest_path"]
            .as_str()
            .and_then(|m| Path::new(m).parent());
        let copyright = find_rust_copyright(&authors, crate_dir);

        entries.push(LicEntry {
            name,
            version,
            license,
            copyright,
            url,
        });
    }
    Ok(entries)
}

/// Try to extract a copyright line from the crate's own LICENSE files,
/// falling back to a formatted authors string. `cargo metadata` has just
/// downloaded and unpacked every package it reports, and `manifest_path`
/// points into that unpacked source, so the result does not depend on which
/// crates an earlier build happened to leave in the registry.
fn find_rust_copyright(authors: &[String], crate_dir: Option<&Path>) -> String {
    if let Some(c) = crate_dir.and_then(extract_copyright_from_dir) {
        return c;
    }
    // Fall back: format authors list
    if authors.is_empty() {
        String::new()
    } else {
        format!("© {}", authors.join(", "))
    }
}

// ---------------------------------------------------------------------------
// npm packages: selected through frontend/package-lock.json
// ---------------------------------------------------------------------------

/// The npm half of the table: the packages frontend/src imports plus their
/// transitive dependencies, as the lockfile resolves them, plus
/// [`NPM_INJECTED`]. Every frontend package is a devDependency (Vite compiles
/// the Svelte runtime into the bundle), so the lockfile's `dev` flag cannot
/// tell shipped code from build tools; the imports can. Reading the lockfile
/// instead of listing node_modules keeps build tools and the host's
/// platform-specific binaries out, so every machine produces the same table.
fn collect_npm_packages(frontend: &Path) -> Result<Vec<LicEntry>, String> {
    let lock_path = frontend.join("package-lock.json");
    let lock_text = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("could not read frontend/package-lock.json: {e}"))?;
    let lock: serde_json::Value = serde_json::from_str(&lock_text)
        .map_err(|e| format!("could not parse frontend/package-lock.json: {e}"))?;

    let mut roots = std::collections::BTreeSet::new();
    collect_imports(&frontend.join("src"), &mut roots);
    let roots: Vec<String> = roots.into_iter().collect();
    let injected: Vec<String> = NPM_INJECTED.iter().map(|s| s.to_string()).collect();

    let mut entries = Vec::new();
    for key in npm_runtime_packages(&lock, &roots, &injected) {
        let pkg_dir = frontend.join(&key);
        match parse_npm_pkg(&pkg_dir) {
            Some(NpmPkg::Entry(e)) => entries.push(e),
            Some(NpmPkg::PlatformSpecific) => {}
            None => {
                return Err(format!(
                    "{key} is in package-lock.json but not installed (run `npm ci`)"
                ));
            }
        }
    }
    Ok(entries)
}

/// Package names `import`ed anywhere under `dir` (.ts, .js, .svelte).
fn collect_imports(dir: &Path, out: &mut std::collections::BTreeSet<String>) {
    let Ok(iter) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in iter.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_imports(&path, out);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // The generated table is output, not input: scanning it would let
        // its own text feed back into the next run.
        let generated = path
            .file_name()
            .is_some_and(|n| n == "licenses_generated.ts");
        if generated || !matches!(ext, "ts" | "js" | "mjs" | "svelte") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.extend(bare_imports(&text));
        }
    }
}

/// Package names of the bare module specifiers in `text`: `from 'x'`,
/// `import 'x'` and `import('x')`, with subpaths reduced to the package
/// (`svelte/store` -> `svelte`, `@scope/pkg/sub` -> `@scope/pkg`). Relative
/// and absolute specifiers are skipped. A textual scan, not a parser: a
/// spurious match is harmless because only names the lockfile knows count.
pub fn bare_imports(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["from", "import"] {
        let mut rest = text;
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let after = rest.trim_start();
            let after = after.strip_prefix('(').unwrap_or(after).trim_start();
            let Some(quote) = after.chars().next().filter(|c| matches!(c, '\'' | '"')) else {
                continue;
            };
            let Some(end) = after[1..].find(quote) else {
                continue;
            };
            let spec = &after[1..1 + end];
            if spec.is_empty() || spec.starts_with('.') || spec.starts_with('/') {
                continue;
            }
            let mut parts = spec.split('/');
            let first = parts.next().unwrap_or("");
            let name = if first.starts_with('@') {
                match parts.next() {
                    Some(second) => format!("{first}/{second}"),
                    None => continue,
                }
            } else {
                first.to_string()
            };
            out.push(name);
        }
    }
    out
}

/// Lockfile keys (`node_modules/...` paths) of the packages that ship:
/// `roots` with their `dependencies` and `optionalDependencies`, followed
/// transitively, plus `injected` on their own. Packages the lockfile marks
/// with an `os` or `cpu` restriction are skipped, as are names it does not
/// contain. Sorted, so the result is deterministic.
pub fn npm_runtime_packages(
    lock: &serde_json::Value,
    roots: &[String],
    injected: &[String],
) -> Vec<String> {
    let Some(packages) = lock["packages"].as_object() else {
        return Vec::new();
    };
    let restricted = |key: &str| {
        let p = &packages[key];
        !p["os"].is_null() || !p["cpu"].is_null()
    };

    let mut found = std::collections::BTreeSet::new();
    for name in injected {
        if let Some(key) = resolve_npm_dep(packages, "", name)
            && !restricted(&key)
        {
            found.insert(key);
        }
    }

    let mut queue: std::collections::VecDeque<String> = roots
        .iter()
        .filter_map(|name| resolve_npm_dep(packages, "", name))
        .collect();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(key) = queue.pop_front() {
        if !visited.insert(key.clone()) || restricted(&key) {
            continue;
        }
        let pkg = &packages[&key];
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(deps) = pkg[field].as_object() {
                for dep in deps.keys() {
                    if let Some(dep_key) = resolve_npm_dep(packages, &key, dep) {
                        queue.push_back(dep_key);
                    }
                }
            }
        }
        found.insert(key);
    }
    found.into_iter().collect()
}

/// Node's resolution over lockfile keys: look for `dep` in `from`'s own
/// node_modules, then in each enclosing package's, then at the top level.
fn resolve_npm_dep(
    packages: &serde_json::Map<String, serde_json::Value>,
    from: &str,
    dep: &str,
) -> Option<String> {
    let mut base = from.to_string();
    loop {
        let candidate = if base.is_empty() {
            format!("node_modules/{dep}")
        } else {
            format!("{base}/node_modules/{dep}")
        };
        if packages.contains_key(&candidate) {
            return Some(candidate);
        }
        if base.is_empty() {
            return None;
        }
        // "node_modules/a/node_modules/b" -> "node_modules/a" -> "".
        let cut = base.rfind("node_modules/").unwrap_or(0);
        base.truncate(cut.saturating_sub(1));
    }
}

enum NpmPkg {
    Entry(LicEntry),
    /// Declares `os` or `cpu` in its package.json: a platform binary whose
    /// presence depends on the installing host, never part of the table.
    PlatformSpecific,
}

fn parse_npm_pkg(pkg_dir: &Path) -> Option<NpmPkg> {
    let content = std::fs::read_to_string(pkg_dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    if !json["os"].is_null() || !json["cpu"].is_null() {
        return Some(NpmPkg::PlatformSpecific);
    }

    let name = json["name"].as_str()?.to_string();
    let version = json["version"].as_str().unwrap_or("").to_string();

    let license = match &json["license"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => o
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().or_else(|| v["type"].as_str()))
            .collect::<Vec<_>>()
            .join(" OR "),
        _ => match &json["licenses"] {
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().or_else(|| v["type"].as_str()))
                .collect::<Vec<_>>()
                .join(" OR "),
            _ => "unknown".to_string(),
        },
    };

    // Copyright: prefer LICENSE file, fall back to author field
    let copyright = extract_copyright_from_dir(pkg_dir)
        .or_else(|| npm_author_copyright(&json))
        .unwrap_or_default();

    // URL: prefer homepage, then repository
    let url = if let Some(hp) = json["homepage"].as_str() {
        hp.to_string()
    } else {
        match &json["repository"] {
            serde_json::Value::String(s) => normalize_npm_url(s),
            serde_json::Value::Object(o) => {
                let u = o.get("url").and_then(|v| v.as_str()).unwrap_or("");
                normalize_npm_url(u)
            }
            _ => String::new(),
        }
    };

    Some(NpmPkg::Entry(LicEntry {
        name,
        version,
        license,
        copyright,
        url,
    }))
}

/// Extract a copyright notice from `author`/`authors` JSON fields.
fn npm_author_copyright(json: &serde_json::Value) -> Option<String> {
    let name = match &json["author"] {
        serde_json::Value::String(s) => {
            // "Name <email> (url)" → "Name"
            let s = s.find('<').map_or(s.as_str(), |i| s[..i].trim());
            let s = s.find('(').map_or(s, |i| s[..i].trim());
            s.to_string()
        }
        serde_json::Value::Object(o) => o
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    if name.is_empty() {
        None
    } else {
        Some(format!("© {name}"))
    }
}

// ---------------------------------------------------------------------------
// Copyright extraction from LICENSE files
// ---------------------------------------------------------------------------

/// Scan a package directory for LICENSE* / LICENCE* / COPYRIGHT* files and
/// return the first copyright notice found, normalised to "© YEAR Name" form.
fn extract_copyright_from_dir(dir: &Path) -> Option<String> {
    // Preferred file names in priority order
    let candidates = [
        "LICENSE",
        "LICENSE.md",
        "LICENSE.txt",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "LICENSE-ISC",
        "LICENCE",
        "LICENCE.md",
        "COPYRIGHT",
        "COPYRIGHT.md",
    ];
    for &fname in &candidates {
        let path = dir.join(fname);
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Some(c) = extract_copyright_line(&content)
        {
            return Some(c);
        }
    }
    None
}

/// Extract and normalise the first copyright line from license text.
fn extract_copyright_line(text: &str) -> Option<String> {
    for line in text.lines().take(40) {
        let t = line.trim();
        if t.is_empty() || t.len() > 200 {
            continue;
        }

        let lower = t.to_lowercase();
        // Must look like an *owner* declaration, not a generic mention of "copyright"
        // in license boilerplate (e.g. Apache-2.0 "copyright notice that is included…").
        let looks_like = t.starts_with('©')
            || lower.starts_with("copyright (c)")
            || lower.starts_with("copyright (c) ")
            || lower.starts_with("copyright(c)")
            || lower.starts_with("copyright © ")
            || lower.starts_with("(c) copyright")
            // "Copyright YYYY": must be followed by a 4-digit year
            || (lower.starts_with("copyright ") && lower[10..].trim_start().starts_with(|c: char| c.is_ascii_digit()));

        if !looks_like {
            continue;
        }

        // Normalise common copyright prefixes to "©"
        let normalised = t
            .replace("Copyright (c) ", "© ")
            .replace("Copyright (C) ", "© ")
            .replace("copyright (c) ", "© ")
            .replace("copyright (C) ", "© ")
            .replace("COPYRIGHT (C) ", "© ")
            .replace("Copyright(c) ", "© ")
            .replace("Copyright(C) ", "© ")
            .replace("Copyright (c)", "© ") // no trailing space variants
            .replace("Copyright (C)", "© ")
            .replace("Copyright ", "© ")
            .replace("copyright ", "© ")
            .replace("COPYRIGHT ", "© ");

        // Remove markdown link syntax: [text](url) → text
        let normalised = remove_md_links(&normalised);

        // Collapse whitespace
        let normalised: String = normalised.split_whitespace().collect::<Vec<_>>().join(" ");

        if normalised.len() > 4 {
            return Some(normalised);
        }
    }
    None
}

/// Strip Markdown `[text](url)` links, keeping only the display text.
fn remove_md_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            // Collect link text
            let mut text = String::new();
            let mut found_close = false;
            for ch in chars.by_ref() {
                if ch == ']' {
                    found_close = true;
                    break;
                }
                text.push(ch);
            }
            if found_close && chars.peek() == Some(&'(') {
                // Skip the (url) part
                chars.next(); // consume '('
                let mut depth = 1;
                for ch in chars.by_ref() {
                    if ch == '(' {
                        depth += 1;
                    }
                    if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                out.push_str(&text);
            } else {
                out.push('[');
                out.push_str(&text);
                if found_close {
                    out.push(']');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// URL / string helpers
// ---------------------------------------------------------------------------

fn clean_repo_url(url: &str) -> String {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn normalize_npm_url(url: &str) -> String {
    let url = url.trim_start_matches("git+").trim_end_matches(".git");
    if url.starts_with("https://") || url.starts_with("http://") {
        return url.to_string();
    }
    if let Some(rest) = url.strip_prefix("github:") {
        return format!("https://github.com/{rest}");
    }
    if let Some(rest) = url.strip_prefix("gitlab:") {
        return format!("https://gitlab.com/{rest}");
    }
    if url.contains('/') && !url.contains(':') {
        return format!("https://github.com/{url}");
    }
    url.to_string()
}

fn strip_email(author: &str) -> String {
    let s = author.find('<').map_or(author, |i| &author[..i]);
    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn run_npm(cwd: &Path, npm_cmd: &str, args: &[&str]) {
    run_npm_with_env(cwd, npm_cmd, args, &[]);
}

fn run_npm_with_env(cwd: &Path, npm_cmd: &str, args: &[&str], env: &[(&str, &str)]) {
    let mut cmd = Command::new(npm_cmd);
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to run `npm {args:?}`: {e}"));
    if !status.success() {
        panic!("`npm {args:?}` exited with {status}");
    }
}

/// Update the version badge on the first line of README.md.
/// Matches `![version](https://img.shields.io/badge/version-X.Y.Z-blue)`.
fn sync_readme_version(root: &Path, cargo_version: &str) {
    let path = root.join("README.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            println!("cargo:warning=Could not read README.md: {e}");
            return;
        }
    };
    // The badge is not pinned to the first line: the badge block sits in a
    // centering <div>, so patch the first line that carries the marker.
    let mut patched = false;
    let updated: String = content
        .split_inclusive('\n')
        .map(|line| {
            if patched {
                return line.to_string();
            }
            let new_line = regex_replace_badge(line, cargo_version);
            if new_line != line {
                patched = true;
            }
            new_line
        })
        .collect();
    if updated != content
        && let Err(e) = std::fs::write(&path, &updated)
    {
        println!("cargo:warning=Could not update README.md: {e}");
    }
}

/// Replace `Version-X.Y.Z-blue` in a shields.io badge URL with the new version.
fn regex_replace_badge(line: &str, version: &str) -> String {
    // Find `Version-` followed by digits/dots then `-blue`
    let marker = "Version-";
    let suffix = "-blue";
    if let Some(start) = line.find(marker) {
        let after = &line[start + marker.len()..];
        if let Some(end) = after.find(suffix) {
            let old_ver = &after[..end];
            if old_ver != version {
                return line.replacen(
                    &format!("{marker}{old_ver}{suffix}"),
                    &format!("{marker}{version}{suffix}"),
                    1,
                );
            }
        }
    }
    line.to_string()
}

/// Update the top-level `"version"` field in frontend/package-lock.json.
/// npm stores it in the same `"version": "x.y.z"` format as package.json.
fn sync_package_lock_version(frontend: &Path, cargo_version: &str) {
    let path = frontend.join("package-lock.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            println!("cargo:warning=Could not read frontend/package-lock.json: {e}");
            return;
        }
    };
    // Replace the first two occurrences of `"version": "..."` (lockfile has
    // the project version at the top level and again under `packages.""`).
    let mut replaced = 0;
    let updated: String = content
        .lines()
        .map(|line| {
            if replaced < 2 && line.trim_start().starts_with("\"version\"") {
                replaced += 1;
                let indent = &line[..line.len() - line.trim_start().len()];
                // Keep the original line's trailing comma: hard-coding one
                // produces invalid JSON if "version" is ever the last key.
                let comma = if line.trim_end().ends_with(',') {
                    ","
                } else {
                    ""
                };
                format!("{indent}\"version\": \"{cargo_version}\"{comma}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if content.ends_with('\n') { "\n" } else { "" };
    if updated != content
        && let Err(e) = std::fs::write(&path, &updated)
    {
        println!("cargo:warning=Could not update frontend/package-lock.json: {e}");
    }
}

/// Write the Cargo version into frontend/package.json so that Vite's
/// `__APP_VERSION__` injection stays in sync without manual editing.
/// Uses a simple string replacement on the `"version":` line to avoid
/// reformatting the whole file.
fn sync_npm_version(frontend: &Path, cargo_version: &str) {
    let pkg_path = frontend.join("package.json");
    let content = match std::fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(e) => {
            println!("cargo:warning=Could not read frontend/package.json: {e}");
            return;
        }
    };

    // Replace the first `"version": "x.y.z"` line in the file.
    let mut replaced = false;
    let updated: String = content
        .lines()
        .map(|line| {
            if !replaced && line.trim_start().starts_with("\"version\"") {
                replaced = true;
                // Preserve leading indentation and the original trailing comma
                // (hard-coding one produces invalid JSON if "version" is last).
                let indent = &line[..line.len() - line.trim_start().len()];
                let comma = if line.trim_end().ends_with(',') { "," } else { "" };
                format!("{indent}\"version\": \"{cargo_version}\"{comma}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        // Restore trailing newline if original had one
        + if content.ends_with('\n') { "\n" } else { "" };

    if updated != content
        && let Err(e) = std::fs::write(&pkg_path, &updated)
    {
        println!("cargo:warning=Could not update frontend/package.json version: {e}");
    }
}

// ---------------------------------------------------------------------------
// Windows executable icon
// ---------------------------------------------------------------------------

#[cfg(all(windows, not(test)))]
fn embed_icon() {
    let icon = manifest_dir().join("assets/icon.ico");
    if !icon.exists() {
        println!("cargo:warning=assets/icon.ico not found - skipping icon embedding");
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set_icon(icon.to_str().unwrap_or("assets/icon.ico"));
    if let Err(e) = res.compile() {
        println!("cargo:warning=Failed to embed Windows icon: {e}");
    }
}
