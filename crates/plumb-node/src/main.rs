//! `plumb`: build and query a Plumb Search index. See [`plumb_node`] for the
//! commands.

use std::process::ExitCode;

use clap::Parser;
use plumb_node::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    plumb_node::init_logging();
    match plumb_node::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
