//! Cline installer.
//!
//! Cline stores MCP settings inside the VS Code extension's global storage:
//!   `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`
//!
//! We search a few known locations and prefer an existing file; otherwise the
//! standard Linux path is used (created only with `--yes`).

use std::path::PathBuf;

pub fn config_path(create_default: bool) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut candidates: Vec<PathBuf> = Vec::new();
    for editor in ["Code", "Code - OSS", "VSCodium", "cursor"] {
        candidates.push(
            home.join(".config")
                .join(editor)
                .join("User")
                .join("globalStorage")
                .join("saoudrizwan.claude-dev")
                .join("settings")
                .join("cline_mcp_settings.json"),
        );
    }
    // older style
    candidates.push(
        home.join(".config")
            .join("claude-dev")
            .join("settings")
            .join("cline_mcp_settings.json"),
    );

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    let default = &candidates[0];
    if create_default {
        Ok(default.clone())
    } else {
        anyhow::bail!(
            "no existing Cline settings found (looked at {}). Re-run with --yes to create the default location: {}",
            candidates.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", "),
            default.display()
        )
    }
}
