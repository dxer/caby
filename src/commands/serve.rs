//! `caby serve` — the MCP meta-gateway runtime over stdio.
//!
//! Every invocation is a launcher: it attaches to the shared daemon for its
//! config file when one is reachable, otherwise it hosts the daemon itself
//! (spawning the downstream set exactly once) while serving its own client.
//! A dead daemon is transparently replaced by the next launcher, so N agent
//! clients share N=1 downstream sets with no manual service management.
//! `CABY_NO_DAEMON=1` forces classic single-process mode.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use crate::cli::ServeArgs;
use crate::config::{load_config, resolve_config_path, Config};
use crate::core::daemon::{self, Shared};
use crate::core::gateway::Gateway;
use crate::core::jsonrpc::{self, Message, PARSE_ERROR};
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

    rt.block_on(run_async(cfg, cfg_path))?;
    Ok(())
}

async fn run_async(cfg: Config, cfg_path: PathBuf) -> anyhow::Result<()> {
    if daemon::sharing_disabled() {
        return run_direct(cfg, cfg_path).await;
    }
    let replay = Arc::new(std::sync::Mutex::new(daemon::Replay::default()));
    // One stdin reader for the process lifetime (see spawn_stdin_pump):
    // bridge and host session take turns draining its lines, so a daemon
    // handover strands no read and loses no request.
    let mut stdin_rx = daemon::spawn_stdin_pump();
    loop {
        match daemon::acquire(&cfg_path).await {
            daemon::Lane::Proxy(att) => {
                match daemon::bridge(att, &replay, &mut stdin_rx).await {
                    daemon::BridgeEnd::Eof => return Ok(()), // client gone, quit
                    daemon::BridgeEnd::DaemonDead => {
                        log_info!("daemon connection lost — re-electing")
                    }
                }
            }
            daemon::Lane::HostDaemon(host) => {
                let core = setup_core(&cfg, cfg_path.clone()).await?;
                // Failover case: requests the dead daemon never answered plus
                // pump lines the dead bridge never forwarded — replay locally.
                let initial = daemon::take_pending(&replay);
                let accept_core = Arc::clone(&core);
                tokio::spawn(daemon::accept_loop(host.listener, accept_core, host.token));
                let result = stdio_session(&core, initial, &mut stdin_rx).await;
                daemon::release_if_ours(&host.lock_path);
                teardown(&core).await;
                return result;
            }
            daemon::Lane::Direct => return run_direct(cfg, cfg_path).await,
        }
    }
}

/// Classic single-process mode: everything in this process, stdio client.
async fn run_direct(cfg: Config, cfg_path: PathBuf) -> anyhow::Result<()> {
    let core = setup_core(&cfg, cfg_path).await?;
    let mut stdin_rx = daemon::spawn_stdin_pump();
    let result = stdio_session(&core, Vec::new(), &mut stdin_rx).await;
    teardown(&core).await;
    result
}

/// Shared core: downstream pool + skill index + broadcast fan-out, plus the
/// rescan / reconcile / startup background tasks. Used by every mode.
async fn setup_core(cfg: &Config, cfg_path: PathBuf) -> anyhow::Result<Arc<Shared>> {
    let registry = Registry::new();
    let spawned = registry.spawn_all(cfg).await;
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
    let sessions = daemon::new_sessions();
    let core = Arc::new(Shared {
        registry: Arc::clone(&registry),
        store: Arc::clone(&store),
        settings,
        sessions: Arc::clone(&sessions),
        rescan_shutdown: Arc::clone(&rescan_shutdown),
    });

    // backend startup grace in the background; then tell all hosts the tool
    // list has settled
    {
        let core = Arc::clone(&core);
        tokio::spawn(async move {
            core.registry.await_startup(Duration::from_secs(8)).await;
            daemon::broadcast(
                &core.sessions,
                jsonrpc::notif("notifications/tools/list_changed", None),
            )
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
        let core = Arc::clone(&core);
        let cfg_path = cfg_path.clone();
        tokio::spawn(async move {
            let mut last: Option<(std::time::SystemTime, u64)> = None;
            loop {
                tokio::select! {
                    _ = core.rescan_shutdown.notified() => break,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        let cur = config_signature(&cfg_path);
                        if cur != last {
                            last = cur;
                            match load_config(&cfg_path) {
                                Ok(next) => {
                                    let events = core.registry.reconcile(&next).await;
                                    if !events.is_empty() {
                                        log_info!("config changed — {}", events.join(", "));
                                        daemon::broadcast(
                                            &core.sessions,
                                            jsonrpc::notif(
                                                "notifications/tools/list_changed",
                                                None,
                                            ),
                                        )
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

    Ok(core)
}

async fn teardown(core: &Shared) {
    core.rescan_shutdown.notify_waiters();
    core.registry.shutdown_all().await;
}

/// Serve our own stdio client as one session on the shared core.
/// `initial` replays unacknowledged requests from a dead daemon plus pump
/// lines the dead bridge never forwarded (failover): stdin never repeats
/// them, so without this a handover loses a call.
async fn stdio_session(
    core: &Arc<Shared>,
    initial: Vec<Vec<u8>>,
    stdin_rx: &mut tokio::sync::mpsc::UnboundedReceiver<daemon::StdioLine>,
) -> anyhow::Result<()> {
    let (gateway, rx) = Gateway::new(
        Arc::clone(&core.registry),
        Arc::clone(&core.store),
        Arc::clone(&core.settings),
    );
    core.sessions.lock().await.push(gateway.writer());

    // stdout writer task
    let writer = tokio::spawn(stdout_writer(rx));

    for raw in initial {
        if let Ok(msg) = serde_json::from_slice::<Message>(&raw) {
            gateway.handle_message(msg).await;
        }
    }

    // --- host stdio loop: lines arrive from the process-wide pump ---
    let result: anyhow::Result<()> = loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                log_info!("SIGINT — shutting down");
                break Ok(());
            }
            msg = stdin_rx.recv() => match msg {
                Some(daemon::StdioLine::Line(frame)) => {
                    match serde_json::from_slice::<Message>(&frame) {
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
                    }
                }
                // host closed stdin (or pump gone) — clean exit
                Some(daemon::StdioLine::Eof) | None => break Ok(()),
            },
        }
    };
    log_info!(
        "stdin closed — tearing down {} server(s)",
        core.registry.server_names().await.len()
    );
    drop(writer);
    result
}

/// Cheap change detector for the config file: (mtime, len). `None` when
/// the file is absent (treated as "no servers configured").
fn config_signature(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok().map(|t| (t, m.len())))
}

async fn stdout_writer(mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>) {
    let mut stdout = tokio::io::stdout();
    while let Some(bytes) = rx.recv().await {
        if stdout.write_all(&bytes).await.is_err() || stdout.flush().await.is_err() {
            log_warn!("stdout closed — host disconnected");
            break;
        }
    }
}
