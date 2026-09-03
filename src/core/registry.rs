//! Registry: the subprocess pool + tool index.
//!
//! Holds one `Arc<McpServer>` per configured downstream server, exposes the
//! union of registered (minified) tools for discovery, and resolves `server:tool`
//! lookups for `call_action`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};

use crate::config::Config;
use crate::core::mcpserver::{McpServer, RegisteredTool, ServerState};

pub struct Registry {
    servers: RwLock<HashMap<String, Arc<McpServer>>>,
    startup: Arc<Notify>,
}

impl Registry {
    pub fn new() -> Arc<Registry> {
        Arc::new(Registry {
            servers: RwLock::new(HashMap::new()),
            startup: Arc::new(Notify::new()),
        })
    }

    /// Spawn lifecycle tasks for every enabled server in the config.
    pub async fn spawn_all(&self, cfg: &Config) -> Vec<String> {
        let mut spawned = Vec::new();
        for def in cfg.enabled_servers() {
            let server = McpServer::new(
                def.name.clone(),
                def.clone(),
                Arc::new(cfg.settings.clone()),
            );
            spawned.push(def.name.clone());
            {
                let mut map = self.servers.write().await;
                map.insert(def.name.clone(), Arc::clone(&server));
            }
            let server_for_task = Arc::clone(&server);
            tokio::spawn(async move {
                server_for_task.run().await;
            });
        }
        spawned
    }

    pub async fn server(&self, name: &str) -> Option<Arc<McpServer>> {
        self.servers.read().await.get(name).cloned()
    }

    pub async fn server_names(&self) -> Vec<String> {
        self.servers.read().await.keys().cloned().collect()
    }

    pub async fn server_state(&self, name: &str) -> Option<ServerState> {
        match self.server(name).await {
            Some(s) => Some(s.state().await),
            None => None,
        }
    }

    /// All tools from ready servers.
    pub async fn all_tools(&self) -> Vec<RegisteredTool> {
        let mut out = Vec::new();
        for server in self.servers.read().await.values() {
            if !server.is_ready().await {
                continue;
            }
            out.extend(server.tools().await);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Resolve `server:tool` to (server handle, tool meta).
    pub async fn resolve_tool(&self, id: &str) -> Option<(Arc<McpServer>, RegisteredTool)> {
        let (server_name, tool_name) = split_action(id)?;
        let server = self.server(&server_name).await?;
        if !server.is_ready().await {
            return None;
        }
        let tools = server.tools().await;
        tools
            .into_iter()
            .find(|t| t.name == tool_name)
            .map(|t| (server, t))
    }

    /// Wait (bounded) for every server to leave the Starting state, then
    /// signal readiness. Run once at gateway startup.
    pub async fn await_startup(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let names = self.server_names().await;
            if names.is_empty() {
                break;
            }
            let mut all_settled = true;
            for name in &names {
                if let Some(st) = self.server_state(name).await {
                    match st {
                        ServerState::Starting => all_settled = false,
                        ServerState::Ready => {}
                        ServerState::Failed(_) | ServerState::Stopped => {}
                    }
                }
            }
            if all_settled || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.startup.notify_waiters();
    }

    #[allow(dead_code)]
    pub fn startup_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.startup)
    }

    /// Graceful shutdown: stop all servers.
    pub async fn shutdown_all(&self) {
        for server in self.servers.read().await.values() {
            server.trigger_shutdown();
        }
        // give lifecycle tasks a moment to teardown
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Split `server:tool` at the first colon.
pub fn split_action(action: &str) -> Option<(String, String)> {
    let (s, t) = action.split_once(':')?;
    if s.is_empty() || t.is_empty() {
        return None;
    }
    Some((s.to_string(), t.to_string()))
}
