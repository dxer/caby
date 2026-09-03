//! Downstream MCP server client: spawns and talks to real MCP servers over
//! persistent stdio pipes. Each server gets a long-lived lifecycle task that
//! performs the `initialize` handshake, lists + minifies its tools, and routes
//! `tools/call` requests. Requests are serialized per server (MCP's safe
//! default) and complete over a oneshot map keyed by JSON-RPC id.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use crate::config::{ServerDef, Settings};
use crate::core::jsonrpc::{self, Id, Message};
use crate::core::minifier::{minify_schema, MinifyStats};
use crate::util::{log_debug, log_error, log_info, log_warn};

pub const OUR_PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RegisteredTool {
    /// `server:tool`
    pub id: String,
    pub name: String,
    pub description: String,
    /// Minified JSON Schema.
    pub schema: Value,
    pub server: String,
    pub minify: MinifyStats,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerState {
    Starting,
    Ready,
    Failed(String),
    Stopped,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CallFailure {
    pub message: String,
    /// true when the downstream server itself replied (JSON-RPC error / isError)
    pub downstream_replied: bool,
}

type PendingMap = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, CallFailure>>>>>;

pub struct McpServer {
    pub name: String,
    cfg: ServerDef,
    settings: Arc<Settings>,
    state: RwLock<ServerState>,
    tools: RwLock<Vec<RegisteredTool>>,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    /// one in-flight request per server (MCP safe default)
    request_lock: Mutex<()>,
    shutdown: Arc<Notify>,
    child_exited: Arc<AtomicBool>,
    child_exit_notify: Arc<Notify>,
    minify_total: RwLock<MinifyStats>,
    /// bumped every time the tool list is (re)registered
    pub tools_revision: AtomicU64,
}

impl McpServer {
    pub fn new(name: String, cfg: ServerDef, settings: Arc<Settings>) -> Arc<McpServer> {
        Arc::new(McpServer {
            name,
            cfg,
            settings,
            state: RwLock::new(ServerState::Starting),
            tools: RwLock::new(Vec::new()),
            child: Mutex::new(None),
            stdin: Mutex::new(None),
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            request_lock: Mutex::new(()),
            shutdown: Arc::new(Notify::new()),
            child_exited: Arc::new(AtomicBool::new(false)),
            child_exit_notify: Arc::new(Notify::new()),
            minify_total: RwLock::new(MinifyStats::default()),
            tools_revision: AtomicU64::new(0),
        })
    }

    pub fn trigger_shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    pub async fn state(&self) -> ServerState {
        self.state.read().await.clone()
    }

    pub async fn tools(&self) -> Vec<RegisteredTool> {
        self.tools.read().await.clone()
    }

    #[allow(dead_code)]
    pub async fn minify_stats(&self) -> MinifyStats {
        *self.minify_total.read().await
    }

    pub async fn is_ready(&self) -> bool {
        *self.state.read().await == ServerState::Ready
    }

    /// Full lifecycle: spawn → handshake → serve → (restart with backoff).
    pub async fn run(self: &Arc<Self>) {
        // restart_max = number of automatic restarts after the initial attempt;
        // 0 disables restarting entirely.
        let max_attempts = self.settings.restart_max.saturating_add(1) as u64;
        let mut attempt: u64 = 0;
        loop {
            *self.state.write().await = ServerState::Starting;
            self.child_exited.store(false, Ordering::SeqCst);
            match self.connect_once().await {
                Ok(()) => {
                    log_info!("server '{}' entering serve loop", self.name);
                    // serve until the child dies or shutdown fires
                    tokio::select! {
                        _ = self.shutdown.notified() => {
                            self.teardown().await;
                            return;
                        }
                        _ = self.on_child_exit() => {
                            self.mark_all_pending_failed("server process exited");
                            if self.child_exited.load(Ordering::SeqCst) {
                                log_warn!("server '{}' process exited (will restart)", self.name);
                            }
                        }
                    }
                }
                Err(e) => {
                    log_warn!("server '{}' failed to connect: {e:#}", self.name);
                }
            }

            attempt += 1;
            if attempt >= max_attempts {
                let reason = format!(
                    "server '{}' failed after {max_attempts} attempt(s)",
                    self.name
                );
                *self.state.write().await = ServerState::Failed(reason.clone());
                log_error!("{reason}");
                return;
            }
            log_warn!(
                "server '{}' restarting (attempt {attempt}/{max_attempts})",
                self.name
            );
            tokio::time::sleep(Duration::from_millis((300 << attempt.min(6)).min(3000))).await;
        }
    }

    async fn on_child_exit(&self) {
        if !self.child_exited.load(Ordering::SeqCst) {
            self.child_exit_notify.notified().await;
        }
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let argv = self.cfg.argv();
        log_info!("starting server '{}': {}", self.name, argv.join(" "));

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false);
        if let Some(cwd) = &self.cfg.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &self.cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn '{}'", argv.join(" ")))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        *self.child.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);

        // stderr drain (never stdout — that is the JSON-RPC channel)
        let name = self.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let l = line.trim_end();
                        if !l.is_empty() {
                            log_debug!("[{}] {}", name, l);
                        }
                    }
                }
            }
        });

        // response reader
        self.spawn_reader(stdout);

        // --- handshake ---
        let handshake: anyhow::Result<()> = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let init_result = self
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": OUR_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "caby", "version": env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            let init = init_result.as_object().cloned().unwrap_or_default();
            let negotiated = init
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(OUR_PROTOCOL_VERSION);
            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated) {
                log_warn!(
                    "server '{}' negotiated unsupported protocol {negotiated}",
                    self.name
                );
            }
            log_debug!("server '{}' protocol {negotiated}", self.name);
            self.send_notification("notifications/initialized", None)
                .await?;

            // --- tools/list ---
            let list_result = self.request("tools/list", json!({})).await?;
            let tools_arr = list_result
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();

            let mut registered: Vec<RegisteredTool> = Vec::with_capacity(tools_arr.len());
            let mut minify_total = MinifyStats::default();
            for tool in tools_arr {
                let name = tool
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string();
                let raw_schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                let (schema, stats) = if self.settings.minify_schemas {
                    minify_schema(&raw_schema)
                } else {
                    (raw_schema, MinifyStats::default())
                };
                minify_total.original_chars += stats.original_chars;
                minify_total.minified_chars += stats.minified_chars;
                minify_total.fields_removed += stats.fields_removed;
                registered.push(RegisteredTool {
                    id: format!("{}:{name}", self.name),
                    name,
                    description,
                    schema,
                    server: self.name.clone(),
                    minify: stats,
                });
            }
            *self.tools.write().await = registered;
            *self.minify_total.write().await = minify_total;
            self.tools_revision.fetch_add(1, Ordering::SeqCst);
            *self.state.write().await = ServerState::Ready;
            log_info!(
                "server '{}' ready ({} tools registered, schema minified)",
                self.name,
                self.tools.read().await.len()
            );
            Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("server '{}' handshake timed out", self.name))?;
        handshake?;
        Ok(())
    }

    fn spawn_reader(&self, stdout: tokio::process::ChildStdout) {
        let name = self.name.clone();
        let pending = Arc::clone(&self.pending);
        let child_exited = Arc::clone(&self.child_exited);
        let child_exit_notify = Arc::clone(&self.child_exit_notify);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut fr = jsonrpc::FrameReader::new();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                let filled = reader.fill_buf().await;
                #[allow(clippy::redundant_guards)]
                match filled {
                    Ok(b) if b.is_empty() => break, // EOF
                    Ok(b) => {
                        let n = b.len();
                        fr.push(b);
                        reader.consume(n);
                    }
                    Err(_) => break,
                }
                loop {
                    match fr.next_frame() {
                        Ok(Some(frame)) => match serde_json::from_slice::<Message>(&frame) {
                            Ok(msg) => Self::dispatch(&pending, &name, msg),
                            Err(e) => log_warn!("[{}] unparseable frame dropped: {e}", name),
                        },
                        Ok(None) => break,
                        Err(e) => {
                            log_warn!("[{}] framing error: {e}", name);
                            fr = jsonrpc::FrameReader::new();
                            break;
                        }
                    }
                }
            }
            log_debug!("[{}] stdout closed", name);
            child_exited.store(true, Ordering::SeqCst);
            child_exit_notify.notify_waiters();
        });
    }

    fn dispatch(pending: &PendingMap, name: &str, msg: Message) {
        match msg {
            Message::Success(s) => {
                if let Id::Num(n) = s.id {
                    if let Some(tx) = pending.lock().expect("pending lock").remove(&n) {
                        let _ = tx.send(Ok(s.result));
                    } else {
                        log_debug!("[{}] response with unknown id {}", name, n);
                    }
                }
            }
            Message::RpcError(e) => {
                if let Id::Num(n) = e.id {
                    if let Some(tx) = pending.lock().expect("pending lock").remove(&n) {
                        let _ = tx.send(Err(CallFailure {
                            message: e.error.message.clone(),
                            downstream_replied: true,
                        }));
                    }
                }
            }
            Message::Notification(n) => {
                if n.method == "notifications/message" {
                    let text = n
                        .params
                        .as_ref()
                        .and_then(|p| p.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        log_debug!("[{}] server log: {}", name, text);
                    }
                }
            }
            Message::Request(r) => {
                // downstream asked for sampling/roots — unsupported
                log_debug!(
                    "[{}] unsupported downstream request '{}' (ignored)",
                    name,
                    r.method
                );
            }
        }
    }

    async fn send(&self, msg: &Message) -> anyhow::Result<()> {
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("pipe closed"))?;
        stdin.write_all(&jsonrpc::encode(msg)).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn send_notification(&self, method: &str, params: Option<Value>) -> anyhow::Result<()> {
        self.send(&jsonrpc::notif(method, params)).await
    }

    /// Serialized request: per-server lock, write, await response.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let _guard = self.request_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending lock");
            pending.insert(id, tx);
        }
        let msg = jsonrpc::req(id, method, params);
        if let Err(e) = self.send(&msg).await {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(e);
        }
        let timeout = Duration::from_secs(self.settings.call_timeout_secs.max(1));
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(failure))) => Err(anyhow::anyhow!(failure.message)),
            Ok(Err(_)) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(anyhow::anyhow!("request cancelled"))
            }
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(anyhow::anyhow!(
                    "call to server '{}' timed out after {}s",
                    self.name,
                    self.settings.call_timeout_secs
                ))
            }
        }
    }

    /// Public tool call: asserts Ready, routes, returns the server's result.
    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<Value, CallFailure> {
        match self.state().await {
            ServerState::Ready => {}
            ServerState::Starting => {
                return Err(CallFailure {
                    message: format!("server '{}' still starting", self.name),
                    downstream_replied: false,
                })
            }
            ServerState::Failed(m) => {
                return Err(CallFailure {
                    message: m,
                    downstream_replied: false,
                })
            }
            ServerState::Stopped => {
                return Err(CallFailure {
                    message: format!("server '{}' is stopped", self.name),
                    downstream_replied: false,
                })
            }
        }
        self.request("tools/call", json!({"name": tool, "arguments": args}))
            .await
            .map_err(|e| CallFailure {
                message: format!("server '{}:{}' failed: {e}", self.name, tool),
                downstream_replied: false,
            })
    }

    fn mark_all_pending_failed(&self, why: &str) {
        let mut pending = self.pending.lock().expect("pending lock");
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(CallFailure {
                message: why.to_string(),
                downstream_replied: false,
            }));
        }
    }

    async fn teardown(&self) {
        let mut child_guard = self.child.lock().await;
        if let Some(child) = child_guard.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        *child_guard = None;
        *self.stdin.lock().await = None;
        *self.state.write().await = ServerState::Stopped;
        self.mark_all_pending_failed("server stopped");
        self.child_exited.store(true, Ordering::SeqCst);
        self.child_exit_notify.notify_waiters();
    }
}
