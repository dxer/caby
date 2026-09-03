//! `caby install --target <client>` — one-shot registration into agent clients.

use crate::cli::InstallArgs;
use crate::installers::run_install;

pub fn run(args: &InstallArgs) -> anyhow::Result<()> {
    run_install(
        args.target.clone(),
        args.project,
        args.yes,
        args.command.as_deref(),
    )
}