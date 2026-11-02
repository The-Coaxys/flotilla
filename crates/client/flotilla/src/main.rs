//! Flotilla: native Windows client for a Flotilla workspace on your VPS.
#![cfg_attr(windows, windows_subsystem = "windows")]
// Task 14 wires the remaining items into the UI and removes this allow.
#![allow(dead_code)]

mod embedded;
mod hosts;
mod runtime;
mod secrets;
mod selftest;

use std::path::PathBuf;

fn main() {
    if let Some(path) =
        std::env::args().find_map(|a| a.strip_prefix("--self-test=").map(PathBuf::from))
    {
        std::process::exit(selftest::run(&path));
    }
    eprintln!("flotilla: the UI shell arrives in Task 14");
}
