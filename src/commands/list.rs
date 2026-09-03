//! `caby list` — status tree of servers + skills.
//!
//! By default downstream servers are live-probed (initialize + tools/list) so
//! the tool counts and minification savings are real. `--offline` skips probing.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command as ProcCommand, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use crate::cli::ListArgs;
use crate::config::{load_config, resolve_config_path, ServerDef};
use crate::core::minifier::minify_schema;
use crate::core::skillstore::SkillStore;
use crate::util::{display_path, log_info};

#[derive(Debug, Clone)]
struct Probe {
    tool_count: usize,
    chars_before: usize,
    chars_after: usize,
    fields_removed: usize,
    error: Option<String>,
}

impl Probe {
    fn reduction_pct(&self) -> f64 {
        if self.chars_before == 0 {
            return 0.0;
        }
        100.0 * (1.0 - self.chars_after as f64 / self.chars_before as f64)
    }
}

pub fn run(args: &ListArgs) -> anyhow::Result<()> {
    let path = resolve_config_path(args.config.as_deref());
    let cfg = load_config(&path)?;

    // --- probe servers (unless offline) ---
    let mut probes: BTreeMap<String, Probe> = BTreeMap::new();
    if !args.offline {
        for def in cfg.enabled_servers() {
            log_info!("probing server '{}'...", def.name);
            probes.insert(def.name.clone(), probe_server(def));
        }
    } else {
        for def in cfg.enabled_servers() {
            probes.insert(
                def.name.clone(),
                Probe {
                    tool_count: 0,
                    chars_before: 0,
                    chars_after: 0,
                    fields_removed: 0,
                    error: None,
                },
            );
        }
    }

    // --- skills ---
    let mut store = SkillStore::new();
    store.scan_paths();

    if args.json {
        print_json(&cfg, &probes, &store);
        return Ok(());
    }

    print_tree(&cfg, &probes, &store, args.offline);
    Ok(())
}

fn print_json(cfg: &crate::config::Config, probes: &BTreeMap<String, Probe>, store: &SkillStore) {
    let servers: Vec<Value> = cfg
        .enabled_servers()
        .iter()
        .map(|s| {
            let p = probes.get(&s.name);
            json_server(s, p)
        })
        .collect();
    let skills: Vec<Value> = store
        .all_skills()
        .iter()
        .map(|s| {
            json!({
                "name": s.name(),
                "path": display_path(&s.path),
                "priority": if s.priority == 0 { "project" } else { "global" },
                "fallback": s.is_fallback(),
                "allowed_tools": s.meta.allowed_tools,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "config": cfg,
            "servers": servers,
            "skills": skills,
        }))
        .unwrap()
    );
}

fn json_server(def: &ServerDef, probe: Option<&Probe>) -> Value {
    match probe {
        Some(p) if p.error.is_none() => json!({
            "name": def.name,
            "state": "running",
            "tools": p.tool_count,
            "schema_minified": true,
            "reduction_pct": p.reduction_pct(),
            "fields_removed": p.fields_removed,
        }),
        Some(p) => json!({
            "name": def.name,
            "state": "down",
            "error": p.error,
        }),
        None => json!({
            "name": def.name,
            "state": "configured",
            "tools": 0,
        }),
    }
}

fn print_tree(
    cfg: &crate::config::Config,
    probes: &BTreeMap<String, Probe>,
    store: &SkillStore,
    offline: bool,
) {
    let enabled: Vec<&ServerDef> = cfg.enabled_servers();
    let healthy = enabled
        .iter()
        .filter(|s| {
            probes
                .get(&s.name)
                .map(|p| p.error.is_none())
                .unwrap_or(false)
        })
        .count();
    if offline {
        println!("Servers ({n} configured, --offline)", n = enabled.len());
    } else {
        println!("Servers ({healthy} running)");
    }
    let lines: Vec<String> = enabled
        .iter()
        .map(|s| {
            let p = probes.get(&s.name);
            match p {
                Some(p) if p.error.is_none() => {
                    let pct = p.reduction_pct();
                    format!(
                        "{} ({} tools registered, schema minified -{:.0}%)",
                        s.name, p.tool_count, pct
                    )
                }
                Some(p) => format!("{} (down — {})", s.name, p.error.as_deref().unwrap_or("")),
                None => format!("{} (configured)", s.name),
            }
        })
        .collect();
    render_branches(&lines, "├── ", "└── ");

    if offline {
        println!();
        println!("  (--offline: tool counts hidden, run `caby list` live for real numbers)");
    }

    let skills = store.all_skills();
    println!();
    println!("Skills ({} active)", skills.len());
    let skill_lines: Vec<String> = skills
        .iter()
        .map(|s| {
            let tools = s.meta.allowed_tools.clone();
            let auth = tools.len();
            let mut label = format!(
                "{} (authorized: {} tool{})",
                display_path(&s.path),
                auth,
                if auth == 1 { "" } else { "s" }
            );
            if s.is_fallback() {
                label.push_str(", fallback");
            }
            label
        })
        .collect();
    render_branches(&skill_lines, "├── ", "└── ");
}

fn render_branches(lines: &[String], prefix: &str, last_prefix: &str) {
    let n = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let p = if i + 1 == n { last_prefix } else { prefix };
        println!("{p}{line}");
    }
    if n == 0 {
        println!("{last_prefix}(none)");
    }
}

// --- live probing (sync, bounded) ------------------------------------------

fn probe_server(def: &ServerDef) -> Probe {
    let argv = def.argv();
    let mut child = match ProcCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Probe {
                tool_count: 0,
                chars_before: 0,
                chars_after: 0,
                fields_removed: 0,
                error: Some(format!("spawn failed: {e}")),
            }
        }
    };

    let stderr = child.stderr.take().expect("piped stderr");
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let stdout = child.stdout.take().expect("piped stdout");
    let probe = probe_session(&mut child, stdout);
    let _ = child.kill();
    let _ = child.wait();
    probe
}

fn probe_session(child: &mut Child, stdout: std::process::ChildStdout) -> Probe {
    // writer thread handling both requests
    let stdin = child.stdin.take();
    let reqs: Vec<Vec<u8>> = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"caby-list","version":env!("CARGO_PKG_VERSION")}
        }}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ]
    .iter()
    .map(|v| {
        let mut b = serde_json::to_vec(v).unwrap();
        b.push(b'\n');
        b
    })
    .collect();

    std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            for r in &reqs {
                let _ = stdin.write_all(r);
                let _ = stdin.flush();
            }
        }
    });

    let (tx, rx) = std::sync::mpsc::channel::<(u64, Value)>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut fr = crate::core::jsonrpc::FrameReader::new();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            #[allow(clippy::redundant_guards)]
            match reader.fill_buf() {
                Ok(b) if b.is_empty() => break,
                Ok(b) => {
                    fr.push(b);
                    let len = b.len();
                    reader.consume(len);
                }
                Err(_) => break,
            }
            while let Ok(Some(frame)) = fr.next_frame() {
                if let Ok(msg) = serde_json::from_slice::<Value>(&frame) {
                    let id = msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                    let _ = tx.send((id, msg));
                }
            }
        }
    });

    let mut init_ok = false;
    let mut tools: Vec<Value> = Vec::new();
    let mut chars_before = 0usize;
    let mut chars_after = 0usize;
    let mut fields_removed = 0usize;

    let deadline = Duration::from_secs(6);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        #[allow(clippy::redundant_guards)]
        match rx.recv_timeout(remaining) {
            Ok((1, msg)) => {
                if msg.pointer("/error").is_none() {
                    init_ok = true;
                } else {
                    let e = msg
                        .pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("initialize error");
                    return Probe {
                        tool_count: 0,
                        chars_before: 0,
                        chars_after: 0,
                        fields_removed: 0,
                        error: Some(e.to_string()),
                    };
                }
            }
            Ok((2, msg)) => {
                if let Some(err) = msg.pointer("/error") {
                    let e = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("tools/list error");
                    return Probe {
                        tool_count: 0,
                        chars_before: 0,
                        chars_after: 0,
                        fields_removed: 0,
                        error: Some(e.to_string()),
                    };
                }
                if let Some(arr) = msg.pointer("/result/tools").and_then(|t| t.as_array()) {
                    tools = arr.clone();
                }
                if init_ok && !tools.is_empty() {
                    break; // got what we need
                }
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if !init_ok {
                    return Probe {
                        tool_count: 0,
                        chars_before: 0,
                        chars_after: 0,
                        fields_removed: 0,
                        error: Some("process exited before handshake".into()),
                    };
                }
                break;
            }
        }
    }

    if !init_ok {
        return Probe {
            tool_count: 0,
            chars_before: 0,
            chars_after: 0,
            fields_removed: 0,
            error: Some("no response to initialize".into()),
        };
    }
    for tool in &tools {
        if let Some(schema) = tool.get("inputSchema") {
            let before = schema.to_string().len();
            let (min, stats) = minify_schema(schema);
            chars_before += before;
            chars_after += min.to_string().len();
            fields_removed += stats.fields_removed;
        }
    }
    Probe {
        tool_count: tools.len(),
        chars_before,
        chars_after,
        fields_removed,
        error: None,
    }
}
