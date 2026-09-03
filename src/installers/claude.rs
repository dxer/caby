//! Claude Code installer.
//!
//! Global:   `~/.claude.json`   (the "mcpServers" key)
//! Project:  `./.mcp.json`      (Claude Code project scope)

use std::path::PathBuf;

pub fn config_path(project: bool) -> anyhow::Result<PathBuf> {
    if project {
        let p = std::env::current_dir()?.join(".mcp.json");
        if !p.exists() {
            let _ = &p;
            // fine to create in project mode
        }
        return Ok(p);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut p = home.join(".claude.json");
    if !p.exists() {
        // some setups use ~/.config/claude-code/... ; .claude.json is the classic one
        let alt = dirs::config_dir()
            .unwrap_or_else(|| home.clone())
            .join("claude-code")
            .join("config.json");
        if alt.exists() {
            p = alt;
        }
    }
    Ok(p)
}
