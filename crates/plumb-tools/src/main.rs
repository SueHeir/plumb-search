//! `plumb-tools`: make the project's page sets and measure ranking. See
//! [`plumb_tools`] for the commands.

use std::process::ExitCode;

use clap::Parser;
use plumb_tools::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    plumb_node::init_logging();
    match plumb_tools::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
