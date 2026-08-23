// Library entrypoint for integration tests.
pub mod cli;
pub mod cli_ui;
pub mod completions;
pub mod config;
pub mod drive;
pub mod error;
pub mod fmt;
pub mod paths;
pub mod progress;
pub mod sync;

#[cfg(feature = "gui")]
pub mod gui;
// trivial PR test comment
