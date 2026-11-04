//! Flotilla: native Windows client for a Flotilla workspace on your VPS.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod connect;
mod embedded;
mod hosts;
mod runtime;
mod secrets;
mod selftest;
mod settings;
mod updater;

use std::path::PathBuf;

fn main() {
    if let Some(path) =
        std::env::args().find_map(|a| a.strip_prefix("--self-test=").map(PathBuf::from))
    {
        std::process::exit(selftest::run(&path));
    }
    init_logging();
    // A replacement may have been interrupted by a crash or a failed
    // self-check: confirm the new build or put the previous one back and run it.
    if updater::startup_recovery() {
        return;
    }
    let rt = runtime::start();
    app::run(rt.handle().clone());
}

/// Log to `%LOCALAPPDATA%\Flotilla\logs\flotilla.log` (filter with FLOTILLA_LOG).
fn init_logging() {
    let Some(dir) = dirs::data_local_dir().map(|d| d.join("Flotilla").join("logs")) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("flotilla.log"))
    else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("FLOTILLA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
}
