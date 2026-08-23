#[cfg(feature = "gui")]
use dirsync::gui;
use dirsync::{cli, cli_ui, completions, config, drive, paths, progress, sync};

use anyhow::{bail, Result};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Handle `completions <SHELL>` before lexopt sees the args.
    {
        let raw: Vec<String> = std::env::args().collect();
        // A bare invocation is a request for orientation, not an error: print
        // the same text as `--help` and exit 0. Partially specified runs (a
        // flag but no SRC/DST) still fail below, because those are mistakes.
        if raw.len() == 1 {
            cli::print_help();
            return Ok(());
        }
        if raw.get(1).map(|s| s.as_str()) == Some("completions") {
            let shell = raw.get(2).map(|s| s.as_str()).unwrap_or("");
            completions::print(shell);
            return Ok(());
        }
    }

    let args = cli::parse();

    // Load config, merge CLI excludes
    let mut config = config::AppConfig::load();
    if !args.exclude.is_empty() {
        config = config.with_extra_excludes(args.exclude);
    }
    if let Some(port) = args.port {
        config.port = port;
    }

    if args.gui {
        #[cfg(feature = "gui")]
        {
            let port = config.port;
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
            return Ok(());
        }
        #[cfg(not(feature = "gui"))]
        bail!("This binary was compiled without GUI support. Rebuild with the `gui` feature.");
    }

    // CLI mode
    let src = args.src.unwrap_or_else(|| {
        eprintln!("Error: SRC and DST required in CLI mode.");
        std::process::exit(1);
    });
    let dst = args.dst.unwrap_or_else(|| {
        eprintln!("Error: SRC and DST required in CLI mode.");
        std::process::exit(1);
    });

    // Same guards the GUI applies: canonicalize, reject system-critical
    // endpoints (unless --yolo), and reject nested SRC/DST pairs. Without the
    // nesting check a SRC inside DST deletes every DST sibling as an orphan,
    // and a DST inside SRC copies its own output one level deeper every run.
    // Validation resolves the pair internally; the engine keeps the paths the
    // user typed, matching the GUI and keeping `\\?\`-prefixed canonical forms
    // out of every log line and error message.
    if let Err(e) = paths::validate_endpoints(&src, &dst, args.yolo) {
        bail!("{e}");
    }

    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // Ctrl-C → cancel; a second Ctrl-C force-exits. Registering the handler
    // disables the default terminate disposition for the rest of the process
    // lifetime, so the task must keep listening: a one-shot forward would
    // leave every later Ctrl-C silently discarded.
    {
        let cancel_tx2 = cancel_tx.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = cancel_tx2.send(true);
            eprintln!("\nCancelling… (press Ctrl-C again to force quit)");
            tokio::signal::ctrl_c().await.ok();
            std::process::exit(130);
        });
    }

    // Probed here rather than inside preview() so the message prints before
    // the walk starts: the CLI progress UI doesn't render log events.
    let (drives, drive_msg) = drive::probe(&src, &dst);
    println!("{drive_msg}");

    let (progress, _) = progress::new_progress_channel();
    let config = Arc::new(config);
    let engine = sync::SyncEngine::new(src, dst, config).with_drives(drives);

    let scan_rx = progress.subscribe();
    let scan_ui = tokio::spawn(cli_ui::CliUi::scan(scan_rx));
    let plan = engine
        .preview(Some(progress.clone()), Some(cancel_rx.clone()))
        .await?;
    scan_ui.await.ok();
    println!("{}", plan.summary());

    if plan.is_noop() {
        println!("Nothing to do.");
        return Ok(());
    }

    if args.dry_run {
        println!("(dry-run - no changes made)");
        return Ok(());
    }

    let rx = progress.subscribe();
    let ui = cli_ui::CliUi::new();

    let sync_handle = tokio::spawn({
        let progress = progress.clone();
        async move { engine.run(plan, progress, false, pause_rx, cancel_rx).await }
    });

    ui.run(progress, rx).await;

    let skip_log = sync_handle.await?;
    skip_log.print_summary();

    Ok(())
}
