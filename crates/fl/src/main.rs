mod cli;
mod client;
mod commands;
mod error;
mod output;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, OutputFormat};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json || cli.output == OutputFormat::Json;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error::CliError::internal(format!("cannot start the async runtime: {error}")).print(json);
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(commands::run(&cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error.print(json);
            ExitCode::FAILURE
        }
    }
}
