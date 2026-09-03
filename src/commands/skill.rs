//! `caby skill new` / `caby skill install` — skill pack management.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;

use crate::cli::{SkillArgs, SkillCmd, SkillDir, SkillInstallArgs, SkillNewArgs};
use crate::config::{load_config, resolve_config_path};
use crate::core::yaml_fm::split_front_matter;
use crate::util::{ensure_dir, global_skills_dir, project_skills_dir};

const SKILL_TEMPLATE: &str = r#"---
name: {name}
description: 当需要……时使用 (describe when this skill applies)
keywords:
  - {name}
allowed_tools:
  - server_name:tool_name
---
# 执行准则与安全规范 (SOP)

1. 第一个步骤……
2. 必须执行的检查……
3. 禁止事项 / 红线……
"#;

pub fn run(args: &SkillArgs) -> anyhow::Result<()> {
    match &args.cmd {
        SkillCmd::New(a) => skill_new(a),
        SkillCmd::Install(a) => skill_install(a),
    }
}

fn skills_dir_for(dir: &SkillDir) -> PathBuf {
    match dir {
        SkillDir::Project => project_skills_dir(),
        SkillDir::Global => global_skills_dir(),
    }
}

/// File-stem for a skill: keeps unicode letters/CJK (they are valid filename
/// chars and keep the file readable), lowercases ASCII, drops unsafe chars.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in name.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            let ch = if c.is_ascii() {
                c.to_ascii_lowercase()
            } else {
                c
            };
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// --- new -------------------------------------------------------------------

fn skill_new(args: &SkillNewArgs) -> anyhow::Result<()> {
    let dir = skills_dir_for(&args.dir);
    ensure_dir(&dir)?;
    let stem = slugify(&args.name);
    if stem.is_empty() {
        bail!("invalid skill name '{}'", args.name);
    }
    let path = dir.join(format!("{stem}.md"));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let content = SKILL_TEMPLATE.replace("{name}", &args.name);
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    println!("created skill template: {}", path.display());
    println!();
    println!("edit it to define the SOP + allowed_tools, then it is live (hot-reload).");
    Ok(())
}

// --- install ----------------------------------------------------------------

fn skill_install(args: &SkillInstallArgs) -> anyhow::Result<()> {
    let spec = args.spec.trim();
    let fetched = fetch_skill_spec(spec)?;
    let mut any = false;
    for (source, content) in fetched {
        let (meta, _body) = match split_front_matter(&content) {
            Some(pair) => pair,
            None => {
                println!("warning: {} has no front-matter — skipped", source);
                continue;
            }
        };
        any = true;
        let dir = skills_dir_for(&args.dir);
        ensure_dir(&dir)?;
        let stem = slugify(&meta.name);
        let path = dir.join(format!("{stem}.md"));
        std::fs::write(&path, &content).with_context(|| format!("write {}", path.display()))?;
        println!(
            "installed skill '{}' ({}) -> {}",
            meta.name,
            source,
            path.display()
        );
        handle_missing_servers(&meta, args);
    }
    if !any {
        bail!("nothing installable found for spec '{spec}'");
    }
    Ok(())
}

/// Resolve a spec to (source-label, content) pairs.
fn fetch_skill_spec(spec: &str) -> anyhow::Result<Vec<(String, String)>> {
    // 1. local file
    let local = PathBuf::from(spec);
    if local.is_file() {
        let content = std::fs::read_to_string(&local)?;
        return Ok(vec![(spec.to_string(), content)]);
    }
    // 2. plain https URL
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return Ok(vec![(spec.to_string(), fetch_url(spec)?)]);
    }
    // 3. github:user/repo[/path]
    if let Some(rest) = spec.strip_prefix("github:") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 2 {
            bail!("spec '{spec}' malformed — expected github:user/repo[/path]");
        }
        let user = parts[0];
        let repo = parts[1];
        let path_part: Option<String> = parts.get(2..).map(|p| p.join("/"));

        let mut tried: Vec<String> = Vec::new();
        let mut found: Vec<(String, String)> = Vec::new();
        if let Some(p) = &path_part {
            let candidates = [format!("{p}.md"), format!("skills/{p}.md"), p.to_string()];
            for cand in candidates {
                let url = format!("https://raw.githubusercontent.com/{user}/{repo}/HEAD/{cand}");
                tried.push(url.clone());
                match fetch_url(&url) {
                    Ok(content) => {
                        found.push((url, content));
                        break;
                    }
                    Err(e) if e.to_string().contains("404") => continue,
                    Err(e) => return Err(e.context("github fetch failed")),
                }
            }
            if found.is_empty() {
                bail!(
                    "no skill file found at {user}/{repo} for path '{}' (tried {})",
                    path_part.as_deref().unwrap_or(""),
                    tried.join(", ")
                );
            }
            return Ok(found);
        }
        // bare repo: list skills/ via the GitHub contents API
        let api = format!("https://api.github.com/repos/{user}/{repo}/contents/skills");
        match fetch_url(&api) {
            Ok(body) => {
                let entries: Value =
                    serde_json::from_str(&body).context("github contents response unparseable")?;
                if let Some(arr) = entries.as_array() {
                    for entry in arr {
                        let path = entry.get("path").and_then(|p| p.as_str()).unwrap_or("");
                        if path.ends_with(".md")
                            && entry.get("type").and_then(|t| t.as_str()) == Some("file")
                        {
                            let raw = format!(
                                "https://raw.githubusercontent.com/{user}/{repo}/HEAD/{path}"
                            );
                            match fetch_url(&raw) {
                                Ok(c) => found.push((raw, c)),
                                Err(_) => continue,
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // fallback: repo root <repo>.md
                let url = format!("https://raw.githubusercontent.com/{user}/{repo}/HEAD/{repo}.md");
                match fetch_url(&url) {
                    Ok(c) => found.push((url, c)),
                    Err(_) => bail!("no skills/<name>.md or root '<repo>.md' in {user}/{repo}"),
                }
            }
        }
        if found.is_empty() {
            bail!("no skill files found under skills/ in {user}/{repo}");
        }
        return Ok(found);
    }
    // 4. github shorthand user/repo
    if let Some((user, repo)) = spec.split_once('/') {
        if !user.is_empty() && !repo.is_empty() && !repo.contains('/') {
            return fetch_skill_spec(&format!("github:{user}/{repo}"));
        }
    }
    bail!(
        "cannot resolve skill spec '{spec}' — use github:user/repo[/path], an https:// URL, or a local .md path"
    )
}

fn fetch_url(url: &str) -> anyhow::Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(&format!("caby/{}", env!("CARGO_PKG_VERSION")))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let status = resp.status();
    if status == 404 {
        bail!("404 not found")
    }
    if !(200..300).contains(&status) {
        bail!("HTTP {status}")
    }
    let mut body = String::new();
    resp.into_reader()
        .read_to_string(&mut body)
        .context("read response body")?;
    Ok(body)
}

/// Compare skill's allowed servers against the config; suggest / interactively
/// add missing downstream servers.
fn handle_missing_servers(meta: &crate::core::yaml_fm::SkillMeta, args: &SkillInstallArgs) {
    let cfg_path = resolve_config_path(args.config.as_deref());
    let cfg = match load_config(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            println!("  (could not read config: {e})");
            return;
        }
    };
    let mut missing: Vec<String> = Vec::new();
    for entry in &meta.allowed_tools {
        if let Some((server, _)) = entry.split_once(':') {
            if cfg.server(server).is_none() && !missing.iter().any(|m| m == server) {
                missing.push(server.to_string());
            }
        }
    }
    if missing.is_empty() {
        return;
    }
    println!("  note: this skill needs server(s): {}", missing.join(", "));
    for server in missing {
        if args.yes || !std::io::stdin().is_terminal() {
            println!("    -> not configured: caby add {server} --command \"<cmd>\"");
            continue;
        }
        print!("    add server '{server}' now? caby add {server} --command ");
        std::io::stdout().flush().ok();
        let mut cmd = String::new();
        if std::io::stdin().read_line(&mut cmd).is_ok() {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let add_args = crate::cli::AddArgs {
                    name: server.clone(),
                    command: cmd.to_string(),
                    extra_args: vec![],
                    env: vec![],
                    cwd: None,
                    no_verify: false,
                    config: args.config.clone(),
                };
                if let Err(e) = crate::commands::add::run(&add_args) {
                    println!("    failed to add '{server}': {e}");
                }
            }
        }
    }
}
