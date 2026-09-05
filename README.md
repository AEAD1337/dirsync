<div align="center">

[![Version](https://img.shields.io/badge/Version-1.1.5-blue?style=flat)](https://github.com/AEAD1337/dirsync/releases) &nbsp;
[![Release](https://github.com/AEAD1337/dirsync/actions/workflows/release.yml/badge.svg)](https://github.com/AEAD1337/dirsync/actions/workflows/release.yml) &nbsp;
[![Audit](https://github.com/AEAD1337/dirsync/actions/workflows/audit.yml/badge.svg)](https://github.com/AEAD1337/dirsync/actions/workflows/audit.yml) &nbsp;
[![CodeQL](https://github.com/AEAD1337/dirsync/actions/workflows/github-code-scanning/codeql/badge.svg)](https://github.com/AEAD1337/dirsync/security/code-scanning) &nbsp;
[![Coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/AEAD1337/dirsync/main/.github/badges/coverage.json)](https://github.com/AEAD1337/dirsync/actions/workflows/coverage.yml) &nbsp;
[![License](https://img.shields.io/badge/License-GPL--3.0--only-blue)](https://github.com/AEAD1337/dirsync/blob/main/LICENSE) &nbsp;
[![Rust](https://img.shields.io/badge/Rust-edition%202024-000000?style=flat&logo=rust&logoColor=white)](https://www.rust-lang.org/) &nbsp;
[![Svelte](https://img.shields.io/badge/Svelte-5-FF3E00?style=flat&logo=svelte&logoColor=white)](https://svelte.dev/) &nbsp;
[![TypeScript](https://img.shields.io/badge/TypeScript-6%20strict-3178C6?style=flat&logo=typescript&logoColor=white)](frontend/tsconfig.json) &nbsp;
[![Node](https://img.shields.io/badge/Node-%E2%89%A5%2020.19-5FA04E?style=flat&logo=nodedotjs&logoColor=white)](https://nodejs.org/) &nbsp;
[![Clippy](https://img.shields.io/badge/Clippy-deny%20warnings-000000?style=flat&logo=rust&logoColor=white)](https://github.com/AEAD1337/dirsync/blob/main/.github/workflows/release.yml) &nbsp;
[![SBOM](https://img.shields.io/badge/SBOM-CycloneDX-394EFF?style=flat)](https://github.com/AEAD1337/dirsync/releases/tag/rolling) &nbsp;
![Linux](https://img.shields.io/badge/Linux-FCC624.svg?style=flat&logo=linux&logoColor=black) &nbsp;
![macOS](https://img.shields.io/badge/macOS-000000.svg?style=flat&logo=apple&logoColor=white) &nbsp;
![Windows](https://img.shields.io/badge/Windows-0078D6.svg?style=flat&logo=windows&logoColor=white)

</div>

<div align="center">
  <h1>dirsync</h1>
  <img src="frontend/public/favicon.svg" width="80" alt="dirsync logo" />
  <p>One-way directory mirror sync with smart rename/move detection to minimize writes.</p>
</div>

Mirrors a source directory into a destination directory: copying new files, overwriting changed ones, removing orphans, and detecting renames/moves within the destination so they become cheap in-place operations instead of a delete + re-copy.

Available as both a CLI tool and an optional local web GUI. Runs on Windows, macOS, and Linux. CLI/core written in Rust, frontend written with Svelte and TypeScript.

### GUI
<div align="center">
  <img width="770" alt="GUI Example" src="https://github.com/user-attachments/assets/2303793a-927c-4471-9fc6-0354422b7b87" style="border: 1px solid #bbb; border-radius: 8px;"/>
</div>

### CLI
```
.\dirsync.exe D:\dirsync\SRC\ D:\dirsync\DST\
Drive detection: SRC=SSD, DST=SSD → parallel I/O
copy=1 overwrite=0 move=2 delete=1 symlink=0 identical=1 touch=0 (24.0 KB to transfer)
  [========================================] 100% Done
```

## Features

- **Smart rename/move detection**: files moved or renamed within DST are identified by content hash and renamed in-place rather than deleted and re-copied
- **Symlink preservation**: symlinks are mirrored as symlinks (not followed); unchanged symlinks are skipped, changed targets are re-created
- **Dry-run mode**: preview the full operation plan without touching any files
- **Automatic drive detection**: each endpoint is probed before every run; hashing runs serially on a spinning endpoint and across all cores on an SSD, and copies drop to serial as soon as either side is an HDD (Windows: TRIM query; Linux/macOS: sysinfo)
- **Glob exclude patterns**: skip files or directories matching shell-style glob patterns (repeatable)
- **SHA-256 fingerprinting**: partial hashing (head + tail) for large files keeps analysis fast
- **Web GUI**: browser-based interface with file tree, progress tracking, pause/cancel, and light/dark theme
- **CLI mode**: scriptable, with an interactive progress display and Ctrl-C cancel support
- **Persistent config**: last-used paths, exclude patterns, theme, and port saved automatically

## Installation

```
cargo build --release
```

The `gui` feature is enabled by default and embeds the web frontend into the binary, enabling `--gui` mode. To build a smaller CLI-only binary without the embedded frontend:

```
cargo build --release --no-default-features
```

The compiled binary is at `target/release/dirsync`.

## Usage

### CLI

```
dirsync [SRC] [DST] [OPTIONS]
dirsync --gui [OPTIONS]
dirsync completions <SHELL>

Options:
  -n, --dry-run              Analyse only, make no changes
  -e, --exclude <PATTERN>    Exclude files/dirs matching glob (repeatable)
      --yolo                 Disable system-critical path checks
      --config <PATH>        Path to config file
      --port <PORT>          Override GUI server port (1024-65535)
      --gui                  Launch the web GUI
  -h, --help                 Print this help and exit
  -V, --version              Print version and exit

Subcommands:
  completions <SHELL>        Print a shell completion script
                             (bash, zsh, fish, powershell)
```

Exclude patterns match individual path components, not the whole relative
path: `*.tmp` and `node_modules` work, `build/temp` never matches.

**Safety checks**

Both the CLI and the GUI refuse a sync when:

- SRC and DST are the same directory, or one is nested inside the other:
  a source inside its destination would delete everything else in the
  destination as an orphan, and a destination inside its source would copy
  its own output one level deeper on every run
- either endpoint is a system-critical directory (`C:\Windows`,
  `C:\Program Files`, `/etc`, `/boot`, …), pass `--yolo` to override

Paths are canonicalized before these checks, so `..` traversal forms cannot
slip past them.

**Examples**

```sh
# Mirror /data/photos → /backup/photos
dirsync /data/photos /backup/photos

# Preview without writing
dirsync /data/photos /backup/photos --dry-run

# Exclude build artifacts and hidden files
dirsync ./src ./dst -e "target" -e ".*"
```

### GUI

```sh
dirsync --gui
```

Opens `http://127.0.0.1:7373` in your default browser. The GUI is served locally; no data leaves the machine.

Passing SRC and DST alongside `--gui` pre-fills both paths and runs a preview
immediately, provided both already exist as directories:

```sh
dirsync /data/photos /backup/photos --gui
```

## Config file

Saved automatically to the platform config directory:

| Platform | Path |
|----------|------|
| Windows  | `%APPDATA%\dirsync\config.toml` |
| macOS    | `~/Library/Application Support/dirsync/config.toml` |
| Linux    | `~/.config/dirsync/config.toml` |

```toml
port = 7373
theme = "system"          # "light" | "dark" | "system"
exclude_patterns = []
last_src = "/path/to/src"
last_dst = "/path/to/dst"
```

CLI flags override config values for that run but do not write back to the file.

## How it works

1. **Detect**: each endpoint's drive type is probed (Windows: TRIM query; Linux/macOS: sysinfo). The result is kept per endpoint: it sets that side's hashing to serial (HDD) or all-cores (SSD), and forces serial copies if *either* side is spinning media
2. **Walk**: both trees are scanned concurrently, always, whatever the drive types; each walk reads only its own endpoint, so the two never contend for one spindle. Each entry records size, mtime, and symlink target; no hashing happens here
3. **Match**: SRC files are matched against DST files, and the SHA-256 fingerprints are computed here, only for the candidates that actually need one; identical files (same path and size with mtimes within a 3 s tolerance, or a confirmed hash match) are skipped; files with matching hash but diverged mtime get their DST mtime corrected in-place; files present only in DST are marked for deletion
4. **Rename detection**: DST-only files whose hash matches a SRC-only file are turned into a `Move` operation instead of `Delete` + `Copy`
5. **Execute**: operations run in dependency order: directories created first, files copied/moved/overwritten, orphaned files deleted, empty directories removed last

## Coverage

The **Coverage** badge reports line coverage of the CLI build
(`--no-default-features`), measured by `cargo llvm-cov` on every push that can
change the code. The GUI modules are excluded on purpose: the axum server,
WebSocket layer and embedded assets have no test harness, so counting them
would only dilute the figure for the sync engine that the tests do exercise.

Reproduce it locally with:

```
.\make.ps1 coverage    # ./make.sh coverage on non-Windows shells
```

CI writes the same numbers (lines, functions, regions) into each run summary
and stores the badge payload at `.github/badges/coverage.json`, which shields.io
reads directly: no third-party coverage service and no upload token.

## Documentation

- [Architecture](doc/architecture.md): module layout, data flow, and key design choices
- [Design Decisions](doc/decisions.md): rationale behind non-obvious implementation choices
- [GUI Protocol](doc/gui-protocol.md): WebSocket message format between the backend and Svelte frontend

## License

Copyright (c) 2026 AEAD1337  
GPL-3.0-only, see [LICENSE](LICENSE) for details.  
Third-party dependency licenses are listed in **Help → Third-Party Licenses** in the GUI.
