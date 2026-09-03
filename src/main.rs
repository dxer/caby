mod cli;
mod commands;
mod config;
mod core;
mod installers;
mod perf;
mod util;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(e) = cli::run(cli) {
        eprintln!("caby: {e:#}");
        std::process::exit(1);
    }
}
