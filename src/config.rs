//! Persistent configuration: downstream MCP server definitions + tuning knobs.
//!
//! Stored as JSON at `~/.config/caby/config.json` (or `$CABY_CONFIG` /
//! `--config`). Written atomically (tmp + rename).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::util::{build_argv, caby_config_home, ensure_dir};

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_RESTART_MAX: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDef {
    pub name: String,
    /// Executable or full command line (may be shell-split).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl ServerDef {
    pub fn argv(&self) -> Vec<String> {
        build_argv(&self.command, &self.args)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// How many skills `discover_skills` returns per query.
    #[serde(default = "default_top_k")]
    pub discover_top_k: usize,
    /// Minimum match score for a skill to be returned.
    #[serde(default = "default_threshold")]
    pub match_threshold: f64,
    /// Per downstream tool call timeout.
    #[serde(default = "default_call_timeout")]
    pub call_timeout_secs: u64,
    /// Minify downstream schemas at ingestion time.
    #[serde(default = "default_true")]
    pub minify_schemas: bool,
    /// Max automatic restarts of a crashed downstream server.
    #[serde(default = "default_restart_max")]
    pub restart_max: u32,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_top_k() -> usize {
    3
}
fn default_threshold() -> f64 {
    0.001
}
fn default_call_timeout() -> u64 {
    DEFAULT_CALL_TIMEOUT_SECS
}
fn default_restart_max() -> u32 {
    DEFAULT_RESTART_MAX
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            log_level: default_log_level(),
            discover_top_k: default_top_k(),
            match_threshold: default_threshold(),
            call_timeout_secs: default_call_timeout(),
            minify_schemas: default_true(),
            restart_max: default_restart_max(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub servers: Vec<ServerDef>,
    #[serde(default)]
    pub settings: Settings,
}

fn default_config_version() -> u32 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            servers: Vec::new(),
            settings: Settings::default(),
        }
    }
}

impl Config {
    pub fn server(&self, name: &str) -> Option<&ServerDef> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn enabled_servers(&self) -> Vec<&ServerDef> {
        self.servers.iter().filter(|s| s.enabled).collect()
    }
}

/// Resolve the config path respecting: explicit flag > $CABY_CONFIG > default.
pub fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("CABY_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    caby_config_home().join("config.json")
}

pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let cfg: Config = serde_json::from_str(&raw)
                .with_context(|| format!("invalid config file {}", path.display()))?;
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(anyhow::anyhow!(
            "failed to read config {}: {e}",
            path.display()
        )),
    }
}

pub fn save_config(path: &Path, cfg: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let raw = serde_json::to_string_pretty(cfg)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))?;
    Ok(())
}

/// Parse an env flag `K=V` / `K=V1=V2...` (splits on first `=`).
pub fn parse_env_flag(flag: &str) -> anyhow::Result<(String, String)> {
    match flag.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(anyhow::anyhow!(
            "invalid --env '{flag}' (expected KEY=VALUE)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::shell_split;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("caby-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut cfg = Config::default();
        cfg.servers.push(ServerDef {
            name: "github".into(),
            command: "github-mcp-server".into(),
            args: vec![],
            env: Default::default(),
            cwd: None,
            enabled: true,
        });
        save_config(&path, &cfg).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.servers[0].name, "github");
        assert_eq!(loaded.settings.discover_top_k, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_config_is_default() {
        let path = Path::new("/nonexistent/caby/config.json");
        let cfg = load_config(path).unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn shell_split_respects_quotes() {
        assert_eq!(
            shell_split("docker run -i --rm mcp/postgres \"postgresql://localhost/db\""),
            vec![
                "docker",
                "run",
                "-i",
                "--rm",
                "mcp/postgres",
                "postgresql://localhost/db"
            ]
        );
        assert_eq!(shell_split("echo 'a b'"), vec!["echo", "a b"]);
        assert_eq!(shell_split("cmd"), vec!["cmd"]);
    }

    #[test]
    fn env_flag_parsing() {
        let (k, v) = parse_env_flag("GITHUB_TOKEN=ghp_xxx").unwrap();
        assert_eq!(k, "GITHUB_TOKEN");
        assert_eq!(v, "ghp_xxx");
        assert!(parse_env_flag("no-equals").is_err());
    }
}
