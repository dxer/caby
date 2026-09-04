//! `caby serve` — the MCP meta-gateway runtime over stdio.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify};

use crate::cli::ServeArgs;
use crate::config::{load_config, resolve_config_path};
use crate::core::gateway::Gateway;
use crate::core::jsonrpc::{self, FrameReader, Message, PARSE_ERROR};
use crate::core::registry::Registry;
use crate::core::skillstore::{run_debounced_rescan, SkillStore};
use crate::util::{display_path, log_info, log_warn, set_log_level, LogLevel};

pub fn run(args: &ServeArgs) -> anyhow::Result<()> {
    let cfg_path = resolve_config_path(args.config.as_deref());
    let mut cfg = load_config(&cfg_path)?;

    if let Some(ll) = &args.log_level {
        cfg.settings.log_level = ll.clone();
    }
    if args.no_restart {
        cfg.settings.restart_max = 0;
    }
    if let Some(t) = args.timeout_secs {
        cfg.settings.call_timeout_secs = t;
    }
    set_log_level(LogLevel::parse(&cfg.settings.log_level)?);

    log_info!(
        "caby v{} starting — config {}",
        env!("CARGO_PKG_VERSION"),
        display_path(&cfg_path)
    );
    log_info!("downstream servers configured: {}", cfg.servers.len());
    log_info!(
        "resident tool list: 2 meta tools (~{} tokens est., real cl100k ≈ 200)",
        crate::core::gateway::meta_tools_token_estimate()
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async move {
        let registry = Registry::new();
        let spawned = registry.spawn_all(&cfg).await;
        log_info!("spawned {} server lifecycle task(s)", spawned.len());

        // --- skills ---
        let store = Arc::new(std::sync::Mutex::new(SkillStore::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        {
            let mut guard = store.lock().expect("skill store lock");
            guard.scan_paths();
            guard.start_watchers(Arc::clone(&dirty))?;
        }
        let rescan_shutdown = Arc::new(Notify::new());
        {
            let store = Arc::clone(&store);
            let dirty = Arc::clone(&dirty);
            let shutdown = Arc::clone(&rescan_shutdown);
            tokio::spawn(run_debounced_rescan(store, dirty, shutdown));
        }

        let settings = Arc::new(cfg.settings.clone());
        let (gateway, rx) = Gateway::new(
            Arc::clone(&registry),
            Arc::clone(&store),
            Arc::clone(&settings),
        );

        // stdout writer task
        let writer = tokio::spawn(stdout_writer(rx));

        // backend startup grace in the background; then tell the host the tool
        // list has settled
        {
            let registry = Arc::clone(&registry);
            let gw = Arc::clone(&gateway);
            tokio::spawn(async move {
                registry.await_startup(Duration::from_secs(8)).await;
                gw.notify_host(jsonrpc::notif("notifications/tools/list_changed", None))
                    .await;
            });
        }

        // config hot-reload: `caby add/remove` (or any edit of the config
        // file) takes effect in this running gateway — no restart. The config
        // is saved atomically (tmp + rename), so a plain signature check is
        // enough; unparseable states are skipped, never applied.
        // ponytail: fixed 250 ms poll; a file watcher would be fancier, but
        // config changes are rare and this is deterministic on every OS.
        {
            let registry = Arc::clone(&registry);
            let gw = Arc::clone(&gateway);
            let cfg_path = cfg_path.clone();
            let shutdown = Arc::clone(&rescan_shutdown);
            tokio::spawn(async move {
                let mut last: Option<(std::time::SystemTime, u64)> = None;
                loop {
                    tokio::select! {
                        _ = shutdown.notified() => break,
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {
                            let cur = config_signature(&cfg_path);
                            if cur != last {
                                last = cur;
                                match load_config(&cfg_path) {
                                    Ok(next) => {
                                        let events = registry.reconcile(&next).await;
                                        if !events.is_empty() {
                                            log_info!("config changed — {}", events.join(", "));
                                            gw.notify_host(jsonrpc::notif(
                                                "notifications/tools/list_changed",
                                                None,
                                            ))
                                            .await;
                                        }
                                    }
                                    Err(e) => log_warn!(
                                        "ignoring unreadable config {}: {e:#}",
                                        display_path(&cfg_path)
                                    ),
                                }
                            }
                        }
                    }
                }
            });
        }

        // --- host stdio loop ---
        let mut stdin = tokio::io::stdin();
        let mut fr = FrameReader::new();
        let mut buf = [0u8; 65536];

        let result: anyhow::Result<()> = loop {
            let n = {
                let read_fut = stdin.read(&mut buf);
                tokio::select! {
                    biased;
                    _ = tokio::signal::ctrl_c() => {
                        log_info!("SIGINT — shutting down");
                        break Ok(());
                    }
                    res = read_fut => match res {
                        Ok(0) => break Ok(()), // host closed stdin
                        Ok(n) => n,
                        Err(e) => break Err(anyhow::anyhow!("stdin read error: {e}")),
                    },
                }
            };
            fr.push(&buf[..n]);
            loop {
                match fr.next_frame() {
                    Ok(Some(frame)) => match serde_json::from_slice::<Message>(&frame) {
                        Ok(msg) => gateway.handle_message(msg).await,
                        Err(e) => {
                            log_warn!("dropping unparseable frame: {e}");
                            // JSON-RPC parse error, id null
                            let _ = gateway
                                .writer()
                                .send(jsonrpc::encode(&jsonrpc::err(
                                    crate::core::jsonrpc::Id::Null,
                                    PARSE_ERROR,
                                    format!("parse error: {e}"),
                                )))
                                .await;
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        log_warn!("framing error: {e}; re-syncing");
                        fr = FrameReader::new();
                        break;
                    }
                }
            }
        };

        log_info!(
            "stdin closed — tearing down {} server(s)",
            registry.server_names().await.len()
        );
        rescan_shutdown.notify_waiters();
        registry.shutdown_all().await;
        drop(writer);
        result
    })?;

    Ok(())
}

/// Cheap change detector for the config file: (mtime, len). `None` when
/// the file is absent (treated as "no servers configured").
fn config_signature(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok().map(|t| (t, m.len())))
}

async fn stdout_writer(mut rx: mpsc::Receiver<Vec<u8>>) {
    let mut stdout = tokio::io::stdout();
    while let Some(bytes) = rx.recv().await {
        if stdout.write_all(&bytes).await.is_err() || stdout.flush().await.is_err() {
            log_warn!("stdout closed — host disconnected");
            break;
        }
    }
}
