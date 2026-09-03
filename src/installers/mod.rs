//! Agent client installers: register `caby serve` into the config of
//! Claude Code, Cursor, or Cline with zero manual JSON editing.

pub mod claude;
pub mod cline;
pub mod cursor;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use std::path::Path;

use crate::cli::AgentTarget;

/// Merge `{"mcpServers": {"caby": {...}}}` into an existing JSON config file.
/// Creates the file if missing; preserves every other key.
pub fn merge_mcp_servers(path: &Path, server_entry: Value, create: bool) -> anyhow::Result<()> {
    if !path.exists() {
        if !create {
            bail!(
                "config {} does not exist — rerun with --yes to create it",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut root = Map::new();
        root.insert(
            "mcpServers".into(),
            json!({ "caby": server_entry }),
        );
        std::fs::write(path, serde_json::to_string_pretty(&Value::Object(root))?)?;
        return Ok(());
    }

    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut root: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "{} is not valid JSON. Caby refused to touch it to avoid corrupting your agent config.\n\
             Tip: export the config via your client and re-run, or fix the JSON manually.",
            path.display()
        )
    })?;

    let obj = root.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("{} does not contain a JSON object at the top level", path.display())
    })?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers_obj = servers.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("'mcpServers' in {} is not an object", path.display())
    })?;
    servers_obj.insert("caby".into(), server_entry);

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Default `caby` entry recorded into client configs.
pub fn default_server_entry(command: &str) -> Value {
    json!({
        "command": command,
        "args": ["serve"]
    })
}

/// Resolve which binary path to record: explicit flag > current exe > "caby".
pub fn resolve_recorded_command(override_cmd: Option<&str>) -> String {
    if let Some(c) = override_cmd {
        return c.to_string();
    }
    if let Ok(exe) = std::env::current_exe() {
        // Skip cargo's temp build dirs (target/debug) — those paths vanish.
        let s = exe.display().to_string();
        if !s.contains("/target/debug/") && !s.contains("/target/release/") {
            return s;
        }
    }
    "caby".to_string()
}

pub fn run_install(target: AgentTarget, project: bool, yes: bool, command: Option<&str>) -> anyhow::Result<()> {
    let entry = default_server_entry(&resolve_recorded_command(command));
    // Global user configs (claude-code / cursor) are safe to create by default;
    // cline has several candidate locations, so creation requires --yes.
    let (path, create) = match target {
        AgentTarget::ClaudeCode => (claude::config_path(project)?, true),
        AgentTarget::Cursor => (cursor::config_path(project)?, true),
        AgentTarget::Cline => (cline::config_path(yes)?, yes),
    };
    merge_mcp_servers(&path, entry, create)?;
    println!(
        "registered caby in {} ({})",
        path.display(),
        match target {
            AgentTarget::ClaudeCode => "claude-code",
            AgentTarget::Cursor => "cursor",
            AgentTarget::Cline => "cline",
        }
    );
    println!("  -> command: {} serve", resolve_recorded_command(command));
    println!("restart your client to pick up the new MCP server.");
    Ok(())
}

