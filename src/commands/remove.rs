//! `caby remove <name>` — detach a downstream server.

use crate::cli::RemoveArgs;
use crate::config::{load_config, resolve_config_path, save_config};
use crate::util::display_path;
use anyhow::bail;

pub fn run(args: &RemoveArgs) -> anyhow::Result<()> {
    let path = resolve_config_path(args.config.as_deref());
    let mut cfg = load_config(&path)?;
    let before = cfg.servers.len();
    cfg.servers.retain(|s| s.name != args.name);
    if cfg.servers.len() == before {
        bail!("server '{}' is not configured", args.name);
    }
    save_config(&path, &cfg)?;
    println!(
        "removed server '{}' from {}",
        args.name,
        display_path(&path)
    );
    println!("note: a running `caby serve` detaches it automatically (config hot-reload).");
    Ok(())
}
