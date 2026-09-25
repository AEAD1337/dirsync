#[cfg(feature = "gui")]
use dirsync::gui;
use dirsync::{cli, cli_ui, completions, config, drive, paths, progress, sync};

use anyhow::Result;
use std::sync::Arc;

// Returns rather than calling `process::exit` on the common paths, so
// destructors and the coverage runtime's exit hooks still run.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            cli::exit_code_for(&e)
        }
    };
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Print a usage error and exit with the usage status.
fn usage_error(msg: impl std::fmt::Display) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(cli::EXIT_USAGE);
}

async fn real_main() -> Result<i32> {
    // Handle `completions <SHELL>` before lexopt sees the args.
    {
        let raw: Vec<String> = std::env::args().collect();
        // A bare invocation is a request for orientation, not an error: print
        // the same text as `--help` and exit 0. Partially specified runs (a
        // flag but no SRC/DST) still fail below, because those are mistakes.
        if raw.len() == 1 {
            cli::print_help();
            return Ok(0);
        }
        if raw.get(1).map(|s| s.as_str()) == Some("completions") {
            let shell = raw.get(2).map(|s| s.as_str()).unwrap_or("");
            completions::print(shell);
            return Ok(0);
        }
    }

    let args = cli::parse();

    // Load config (from --config when given), merge CLI excludes. The loaded
    // path travels with the config so every later save() lands in the same file.
    // A named file that cannot be loaded is a usage error, never defaults:
    // defaults drop its excludes, and the DST content they protect would be
    // planned for deletion.
    let mut config = match &args.config {
        Some(path) => config::AppConfig::load_explicit(path).unwrap_or_else(|e| usage_error(e)),
        None => config::AppConfig::load(),
    };
    // Session-only: applied to this run, stripped again by every save().
    if !args.exclude.is_empty() {
        config = config.with_extra_excludes(args.exclude);
    }

    if args.gui {
        #[cfg(feature = "gui")]
        {
            // Passed to the server directly instead of written into the
            // config, which the GUI saves on every preview.
            let port = args.port.unwrap_or(config.port);
            let auto_preview = args.src.as_ref().is_some_and(|p| p.is_dir())
                && args.dst.as_ref().is_some_and(|p| p.is_dir());
            if let Some(p) = args.src {
                config.last_src = Some(p);
            }
            if let Some(p) = args.dst {
                config.last_dst = Some(p);
            }
            let (state, _rx) = gui::state::AppState::new(config, auto_preview, args.yolo);
            gui::start(state, port).await?;
            return Ok(0);
        }
        #[cfg(not(feature = "gui"))]
        usage_error(
            "This binary was compiled without GUI support. Rebuild with the `gui` feature.",
        );
    }

    // CLI mode
    if args.port.is_some() {
        eprintln!("Warning: --port only applies to --gui; ignored.");
    }
    let (Some(src), Some(dst)) = (args.src, args.dst) else {
        usage_error("SRC and DST required in CLI mode.");
    };

    // Same guards the GUI applies: canonicalize, reject system-critical
    // endpoints (unless --yolo), and reject nested SRC/DST pairs. Without the
    // nesting check a SRC inside DST deletes every DST sibling as an orphan,
    // and a DST inside SRC copies its own output one level deeper every run.
    // Validation resolves the pair internally; the engine keeps the paths the
    // user typed, matching the GUI and keeping `\\?\`-prefixed canonical forms
    // out of every log line and error message.
    let (canon_src, canon_dst) =
        paths::validate_endpoints(&src, &dst, args.yolo).unwrap_or_else(|e| usage_error(e));

    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (progress, _) = progress::new_progress_channel();

    // Ctrl-C → cancel; a second Ctrl-C force-exits. Registering the handler
    // disables the default terminate disposition for the rest of the process
    // lifetime, so the task must keep listening: a one-shot forward would
    // leave every later Ctrl-C silently discarded. The notice goes through
    // the progress channel so the CLI UI can print it above its bars instead
    // of eprintln! splitting a repaint.
    {
        let cancel_tx2 = cancel_tx.clone();
        let progress = progress.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = cancel_tx2.send(true);
            progress.emit_log(
                progress::LogLevel::Warning,
                "Cancelling... (press Ctrl-C again to force quit)".to_owned(),
            );
            tokio::signal::ctrl_c().await.ok();
            std::process::exit(cli::EXIT_CANCELLED);
        });
    }

    // Probed here rather than inside preview() so the message prints before
    // the walk starts. The canonical forms are used so a relative path still
    // resolves to its drive; the engine keeps the paths the user typed.
    let (drives, drive_msg) = drive::probe(&canon_src, &canon_dst);
    println!("{drive_msg}");

    let config = Arc::new(config);
    let engine = sync::SyncEngine::new(src, dst, config).with_drives(drives);

    let scan_rx = progress.subscribe();
    let scan_ui = tokio::spawn(cli_ui::CliUi::scan(scan_rx));
    let plan = engine
        .preview(Some(progress.clone()), Some(cancel_rx.clone()))
        .await?;
    scan_ui.await.ok();
    println!("{}", plan.summary());

    // Unreadable paths shield their DST counterparts from deletion, but the
    // sync is still incomplete: say so on stderr whatever the terminal
    // state, and never exit 0 over it.
    let incomplete = !plan.walk_errors.is_empty();
    if incomplete {
        eprintln!(
            "
{} path(s) could not be read; nothing below them was changed:",
            plan.walk_errors.len()
        );
        for e in &plan.walk_errors {
            eprintln!("  {}: {}", e.path.display(), e.message);
        }
    }

    let partial_or_ok = |failed: bool| if failed { cli::EXIT_PARTIAL } else { 0 };
    if plan.is_noop() {
        println!("Nothing to do.");
        return Ok(partial_or_ok(incomplete));
    }

    if args.dry_run {
        println!("(dry-run - no changes made)");
        return Ok(partial_or_ok(incomplete));
    }

    let rx = progress.subscribe();
    let ui = cli_ui::CliUi::new();

    let sync_handle = tokio::spawn({
        let progress = progress.clone();
        async move { engine.run(plan, progress, false, pause_rx, cancel_rx).await }
    });

    ui.run(progress.clone(), rx).await;

    let skip_log = sync_handle.await?;
    skip_log.print_summary();

    // Exit status is the contract with scripts: a cancelled run and a run
    // that skipped files must both be distinguishable from success.
    let cancelled = *progress.status.read().unwrap() == progress::SyncStatus::Cancelled;
    if cancelled {
        return Ok(cli::EXIT_CANCELLED);
    }
    Ok(partial_or_ok(incomplete || !skip_log.is_empty()))
}
