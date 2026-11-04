//! One place decides JSON versus text so every command honors `--json`.
use serde::Serialize;

use crate::error::CliError;

#[derive(Debug, Clone, Copy)]
pub struct Reporter {
    pub json: bool,
}

impl Reporter {
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    pub fn value<T: Serialize>(&self, value: &T) -> Result<(), CliError> {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }

    pub fn text(&self, text: impl std::fmt::Display) {
        println!("{text}");
    }
}
