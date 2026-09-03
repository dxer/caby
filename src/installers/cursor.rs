//! Cursor installer.
//!
//! Global:   `~/.cursor/mcp.json`
//! Project:  `./.cursor/mcp.json`

use std::path::PathBuf;

pub fn config_path(project: bool) -> anyhow::Result<PathBuf> {
    if project {
        return Ok(std::env::current_dir()?.join(".cursor").join("mcp.json"));
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    Ok(home.join(".cursor").join("mcp.json"))
}