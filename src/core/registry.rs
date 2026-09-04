//! Registry: the subprocess pool + tool index.
//!
//! Holds one `Arc<McpServer>` per configured downstream server, exposes the
//! union of registered (minified) tools for discovery, and resolves `server:tool`
//! lookups for `call_action`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};

use crate::config::{Config, ServerDef, Settings};
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
            spawned.push(self.spawn_one(def, &cfg.settings).await);
        }
        spawned
    }

    /// Spawn (or re-spawn) a single server, replacing any live one.
    pub async fn spawn_one(&self, def: &ServerDef, settings: &Settings) -> String {
        let server = McpServer::new(def.name.clone(), def.clone(), Arc::new(settings.clone()));
        {
            let mut map = self.servers.write().await;
            map.insert(def.name.clone(), Arc::clone(&server));
        }
        let server_for_task = Arc::clone(&server);
        tokio::spawn(async move {
            server_for_task.run().await;
        });
        def.name.clone()
    }

    /// Stop and forget one server. In-flight calls on the old handle fail
    /// fast; the old lifecycle task tears itself down via `trigger_shutdown`.
    pub async fn remove_one(&self, name: &str) {
        let old = {
            let mut map = self.servers.write().await;
            map.remove(name)
        };
        if let Some(server) = old {
            server.trigger_shutdown();
        }
    }

    /// Reconcile live servers with the config: start added/enabled/changed
    /// definitions, stop removed/disabled ones. Returns human-readable events
    /// (empty = steady state). This is what makes `caby add/remove` take
    /// effect in a running `caby serve` with no restart.
    pub async fn reconcile(&self, cfg: &Config) -> Vec<String> {
        let mut events = Vec::new();
        let wanted: std::collections::HashMap<String, ServerDef> = cfg
            .enabled_servers()
            .into_iter()
            .map(|d| (d.name.clone(), d.clone()))
            .collect();

        // stop removed / disabled / redefined servers first
        let current: Vec<(String, ServerDef)> = {
            let map = self.servers.read().await;
            map.iter()
                .map(|(name, server)| (name.clone(), server.definition()))
                .collect()
        };
        for (name, def) in current {
            if wanted.get(&name) != Some(&def) {
                self.remove_one(&name).await;
                events.push(format!("stopped {name}"));
            }
        }

        // start added / enabled / redefined servers
        for def in cfg.enabled_servers() {
            let needs_start = match self.server(&def.name).await {
                Some(server) => server.definition() != *def,
                None => true,
            };
            if needs_start {
                self.spawn_one(def, &cfg.settings).await;
                events.push(format!("started {}", def.name));
            }
        }
        events
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
