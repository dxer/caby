//! `caby add <name> --command ...` — attach a downstream MCP server.
//!
//! Verifies connectivity (stdio initialize handshake) by default, then persists
//! the definition to the config file.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command as ProcCommand, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;

use crate::cli::AddArgs;
use crate::config::{load_config, parse_env_flag, resolve_config_path, save_config, ServerDef};
use crate::core::jsonrpc::FrameReader;
use crate::util::{display_path, log_info};

pub fn run(args: &AddArgs) -> anyhow::Result<()> {
    let path = resolve_config_path(args.config.as_deref());
    let mut cfg = load_config(&path)?;

    if cfg.server(&args.name).is_some() {
        bail!(
            "server '{}' is already configured in {} — remove it first (caby remove {})",
            args.name,
            display_path(&path),
            args.name
        );
    }

    let mut env = std::collections::BTreeMap::new();
    for flag in &args.env {
        let (k, v) = parse_env_flag(flag)?;
        env.insert(k, v);
    }

    let def = ServerDef {
        name: args.name.clone(),
        command: args.command.clone(),
        args: args.extra_args.clone(),
        env,
        cwd: args.cwd.as_ref().map(|p| p.display().to_string()),
        enabled: true,
    };

    if !args.no_verify {
        match verify_connectivity(&def) {
            Ok(info) => log_info!(
                "connectivity ok — server '{}' answered initialize: {}",
                args.name,
                info
            ),
            Err(e) => {
                println!(
                    "warning: connectivity check failed for '{}': {e}",
                    args.name
                );
                println!("  (the server is saved anyway; use --no-verify to skip checks)");
            }
        }
    }

    cfg.servers.push(def);
    save_config(&path, &cfg)?;

    println!("added server '{}'", args.name);
    println!("  command : {}", args.command);
    if !args.env.is_empty() {
        println!("  env     : {} variable(s)", args.env.len());
    }
    println!("  config  : {}", display_path(&path));
    println!();
    println!(
        "next: run `caby serve` to start the gateway, or `caby skill install <pack>` to wire skills."
    );
    Ok(())
}

/// Spawn the candidate server, run the initialize handshake, kill it.
/// Returns the server's reported identity on success.
pub fn verify_connectivity(def: &ServerDef) -> anyhow::Result<String> {
    let argv = def.argv();
    if argv.is_empty() {
        bail!("empty command");
    }
    let mut child = ProcCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", argv.join(" ")))?;

    // drain stderr in the background so the child never blocks on it
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
    let result = handshake_with_timeout(&mut child, stdout, 6);

    let _ = child.kill();
    let _ = child.wait();
    result
}

fn handshake_with_timeout(
    child: &mut Child,
    stdout: std::process::ChildStdout,
    timeout_secs: u64,
) -> anyhow::Result<String> {
    // writer thread: initialize request
    let mut stdin = child.stdin.take().expect("piped stdin");
    let init_frame = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "caby-verify", "version": env!("CARGO_PKG_VERSION")}
        }
    }))
    .unwrap();
    let mut frame = init_frame;
    frame.push(b'\n');
    std::thread::spawn(move || {
        let _ = stdin.write_all(&frame);
        let _ = stdin.flush();
    });

    // reader thread: collect frames into a channel
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut fr = FrameReader::new();
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
                if tx.send(frame).is_err() {
                    return;
                }
            }
        }
    });

    let deadline = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            bail!("no response to initialize after {timeout_secs}s");
        }
        #[allow(clippy::redundant_guards)]
        match rx.recv_timeout(remaining) {
            Ok(frame) => {
                let msg: Value = serde_json::from_slice(&frame).map_err(|e| {
                    anyhow::anyhow!("non-JSON output on stdout (is this an MCP server?): {e}")
                })?;
                if msg.get("id").and_then(|i| i.as_u64()) == Some(1) {
                    let server_info = msg
                        .pointer("/result/serverInfo")
                        .cloned()
                        .unwrap_or_else(|| Value::String("?".into()));
                    return Ok(server_info.to_string());
                }
                // other frames (stray notifications) — keep reading
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!("no response to initialize after {timeout_secs}s")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("server process exited before answering initialize")
            }
        }
    }
}
