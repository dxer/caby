//! Caby Gateway: the MCP *server* that host clients see.
//!
//! It answers with exactly two meta tools (≈150 tokens of resident context):
//!
//!   • `discover_skills(query)` — intent-matches a task against the skill
//!     corpus and returns the matched skill's SOP + its whitelisted actions
//!     with minified schemas.
//!   • `call_action(skill, action, parameters)` — sandbox-checked, then routed
//!     to the right downstream MCP server over its persistent stdio pipe.
//!
//! All host-facing traffic is JSON-RPC 2.0 over stdio (newline-delimited).

use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::core::jsonrpc::{self, Id, Message};
use crate::core::mcpserver::{
    CallFailure, RegisteredTool, OUR_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::core::registry::Registry;
use crate::core::sandbox;
use crate::core::skillstore::{Skill, SkillStore};
use crate::util::{
    approx_tokens, display_path, log_debug, log_info, log_warn, set_log_level, LogLevel,
};

const META_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result wrapper for tool responses sent back to the host.
fn tool_result_text(text: String, is_error: bool) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

pub struct Gateway {
    pub registry: Arc<Registry>,
    pub store: Arc<StdMutex<SkillStore>>,
    pub settings: Arc<Settings>,
    tx: mpsc::Sender<Vec<u8>>,
    client_info: StdMutex<Option<Value>>,
}

impl Gateway {
    pub fn new(
        registry: Arc<Registry>,
        store: Arc<StdMutex<SkillStore>>,
        settings: Arc<Settings>,
    ) -> (Arc<Gateway>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
        let gw = Arc::new(Gateway {
            registry,
            store,
            settings,
            tx,
            client_info: StdMutex::new(None),
        });
        (gw, rx)
    }

    pub fn writer(&self) -> mpsc::Sender<Vec<u8>> {
        self.tx.clone()
    }

    // --- message handling --------------------------------------------------

    pub async fn handle_message(&self, msg: Message) {
        match msg {
            Message::Request(r) => {
                let id = r.id.clone();
                let result = self.handle_request(&r.method, r.params.as_ref(), &id).await;
                let _ = self.tx.send(jsonrpc::encode(&result)).await;
            }
            Message::Notification(n) => {
                self.handle_notification(&n.method, n.params.as_ref()).await;
            }
            Message::Success(s) => {
                log_debug!("ignoring unexpected response from host (id {:?})", s.id);
            }
            Message::RpcError(e) => {
                log_debug!("ignoring unexpected error from host (id {:?})", e.id);
            }
        }
    }

    /// Send a server-initiated notification to the host (e.g. tools/list_changed).
    /// Multi-session mode uses broadcast instead; kept for direct embeds/tests.
    #[allow(dead_code)]
    pub async fn notify_host(&self, msg: Message) {
        let _ = self.tx.send(jsonrpc::encode(&msg)).await;
    }

    async fn handle_notification(&self, method: &str, params: Option<&Value>) {
        match method {
            "notifications/initialized" => {
                log_info!("host client initialized; gateway ready");
            }
            "notifications/cancelled"
            | "notifications/progress"
            | "notifications/roots/list_changed" => {}
            "logging/setLevel" => {
                if let Some(level) = params.and_then(|p| p.get("level")).and_then(|l| l.as_str()) {
                    if let Ok(l) = LogLevel::parse(level) {
                        set_log_level(l);
                        log_info!("host set log level to {level}");
                    }
                }
            }
            other => {
                log_debug!("ignoring notification '{other}'");
            }
        }
    }

    async fn handle_request(&self, method: &str, params: Option<&Value>, id: &Id) -> Message {
        match method {
            "initialize" => self.mcp_initialize(params, id),
            "ping" => jsonrpc::ok(id.clone(), json!({})),
            "tools/list" => jsonrpc::ok(id.clone(), self.tools_list()),
            "tools/call" => self.mcp_tools_call(params, id).await,
            "resources/list" => jsonrpc::ok(id.clone(), json!({"resources": []})),
            "resources/templates/list" => jsonrpc::ok(id.clone(), json!({"resourceTemplates": []})),
            "prompts/list" => jsonrpc::ok(id.clone(), json!({"prompts": []})),
            "completion/complete" => jsonrpc::err(
                id.clone(),
                jsonrpc::INVALID_PARAMS,
                "completion is not supported by caby",
            ),
            "logging/setLevel" => {
                if let Some(level) = params.and_then(|p| p.get("level")).and_then(|l| l.as_str()) {
                    if let Ok(l) = LogLevel::parse(level) {
                        set_log_level(l);
                    }
                }
                jsonrpc::ok(id.clone(), json!({}))
            }
            unknown => {
                log_debug!("host called unknown method '{unknown}'");
                jsonrpc::err(
                    id.clone(),
                    jsonrpc::METHOD_NOT_FOUND,
                    format!("method '{unknown}' not found"),
                )
            }
        }
    }

    fn mcp_initialize(&self, params: Option<&Value>, id: &Id) -> Message {
        let client_version = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&client_version) {
            client_version.to_string()
        } else {
            OUR_PROTOCOL_VERSION.to_string()
        };
        if let Some(ci) = params.and_then(|p| p.get("clientInfo")).cloned() {
            if let Ok(mut slot) = self.client_info.lock() {
                *slot = Some(ci);
            }
        }
        log_info!("host connected (protocol {negotiated}) — exposing 2 meta tools");
        jsonrpc::ok(
            id.clone(),
            json!({
                "protocolVersion": negotiated,
                "capabilities": {
                    "tools": { "listChanged": true },
                    "logging": {}
                },
                "serverInfo": { "name": "caby", "version": META_VERSION }
            }),
        )
    }

    /// Meta tools list — the ONLY two tools the host ever sees (~150 tokens).
    fn tools_list(&self) -> Value {
        meta_tools_json()
    }

    async fn mcp_tools_call(&self, params: Option<&Value>, id: &Id) -> Message {
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or_default();
        let args = params
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({}));

        match name {
            "discover_skills" => {
                let query = args
                    .get("query")
                    .and_then(|q| q.as_str())
                    .unwrap_or_default()
                    .to_string();
                let payload = self.discover_skills(&query).await;
                let text = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| "{}".to_string());
                jsonrpc::ok(id.clone(), tool_result_text(text, false))
            }
            "call_action" => {
                let result = self.call_action(&args).await;
                jsonrpc::ok(id.clone(), result)
            }
            other => jsonrpc::ok(
                id.clone(),
                tool_result_text(
                    format!("unknown tool '{other}' — caby exposes only discover_skills and call_action"),
                    true,
                ),
            ),
        }
    }

    // --- discover_skills ----------------------------------------------------

    async fn discover_skills(&self, query: &str) -> Value {
        let top_k = self.settings.discover_top_k.max(1);
        let threshold = self.settings.match_threshold;

        let chosen: Vec<(Skill, f64)> = {
            let store = self.store.lock().expect("skill store lock");
            if query.trim().is_empty() {
                // no query: surface the first top_k regular skills + fallbacks
                let mut out: Vec<(Skill, f64)> = store
                    .all_skills()
                    .iter()
                    .filter(|s| !s.is_fallback())
                    .take(top_k)
                    .cloned()
                    .map(|s| (s, 0.0))
                    .collect();
                for fb in store.fallback_skills().into_iter().take(top_k) {
                    out.push((fb.clone(), 0.0));
                }
                out
            } else {
                let mut ranked: Vec<(Skill, f64)> = store
                    .rank_regular(query, top_k, threshold)
                    .into_iter()
                    .map(|(sk, sc)| (sk.clone(), sc))
                    .collect();
                if ranked.is_empty() {
                    ranked = store
                        .fallback_skills()
                        .into_iter()
                        .take(top_k)
                        .map(|s| (s.clone(), 0.0))
                        .collect();
                }
                ranked
            }
        };

        let all_tools = self.registry.all_tools().await;
        let skills_out: Vec<Value> = chosen
            .iter()
            .map(|(s, score)| self.skill_payload(s, *score, &all_tools))
            .collect();

        json!({
            "query": query,
            "skills": skills_out
        })
    }

    fn skill_payload(&self, skill: &Skill, score: f64, all_tools: &[RegisteredTool]) -> Value {
        let mut actions: Vec<Value> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for entry in &skill.meta.allowed_tools {
            match all_tools
                .iter()
                .find(|t| t.id == *entry || t.name == *entry)
            {
                Some(tool) => actions.push(json!({
                    "action": tool.id,
                    "description": tool.description,
                    "schema": tool.schema
                })),
                None => {
                    if let Some((server, _)) = entry.split_once(':') {
                        if !missing.iter().any(|m| m == server) {
                            missing.push(server.to_string());
                        }
                    }
                }
            }
        }

        json!({
            "name": skill.name(),
            "score": score,
            "fallback": skill.is_fallback(),
            "path": display_path(&skill.path),
            "sop": skill.body,
            "actions": actions,
            "missing_servers": missing
        })
    }

    // --- call_action --------------------------------------------------------

    async fn call_action(&self, args: &Value) -> Value {
        let skill_name = args
            .get("skill")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or_default();
        let parameters = args.get("parameters").cloned().unwrap_or_else(|| json!({}));

        if skill_name.is_empty() || action.is_empty() {
            return tool_result_text(
                "invalid call_action: 'skill' (str) and 'action' (str, server_name:tool_name) are required; 'parameters' (object) is optional"
                    .to_string(),
                true,
            );
        }

        // --- 1. skill must exist (discover first) ---
        let skill = {
            let store = self.store.lock().expect("skill store lock");
            match store.get(skill_name) {
                Some(s) => s.clone(),
                None => {
                    return tool_result_text(
                        format!("skill '{skill_name}' not found — call discover_skills first"),
                        true,
                    )
                }
            }
        };

        // --- 2. sandbox whitelist check (100% interception) ---
        match sandbox::authorize(&skill, action) {
            sandbox::Verdict::Deny { reason } => {
                log_warn!("sandbox: {reason}");
                return tool_result_text(reason, true);
            }
            sandbox::Verdict::Allow { .. } => {
                log_debug!("sandbox: '{action}' authorized by skill '{skill_name}'");
            }
        }

        // --- 3. resolve downstream server ---
        let (server_handle, registered) = match self.registry.resolve_tool(action).await {
            Some(pair) => pair,
            None => {
                return tool_result_text(
                    format!(
                        "server for action '{action}' is not available (not configured, offline, or the tool is not registered)"
                    ),
                    true,
                )
            }
        };

        // --- 4. route over the persistent pipe ---
        let started = std::time::Instant::now();
        let call_name = registered.name.clone();
        match server_handle.call_tool(&call_name, parameters).await {
            Ok(result) => {
                let elapsed = format!("{:.1}ms", started.elapsed().as_secs_f64() * 1000.0);
                log_debug!("call '{action}' ok in {elapsed}");
                passthrough_result(result)
            }
            Err(CallFailure { message, .. }) => {
                let elapsed = format!("{:.1}ms", started.elapsed().as_secs_f64() * 1000.0);
                log_warn!("call '{action}' failed in {elapsed}: {message}");
                tool_result_text(message, true)
            }
        }
    }
}

/// Pass through the downstream result losslessly: keep its `content` blocks and
/// any structured content; preserve the downstream `isError` flag.
fn passthrough_result(result: Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(content) = result.get("content") {
        out.insert("content".into(), content.clone());
    } else {
        out.insert(
            "content".into(),
            json!([{ "type": "text", "text": result.to_string() }]),
        );
    }
    let is_error = result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    out.insert("isError".into(), json!(is_error));
    if let Some(sc) = result.get("structuredContent") {
        out.insert("structuredContent".into(), sc.clone());
    }
    Value::Object(out)
}

/// The two meta tools, as JSON. Kept in one place so tests measure the exact
/// token baseline of what the host sees.
pub fn meta_tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "discover_skills",
                "description": "Intent matcher. Call first with your task. Returns skill SOP + authorized actions (minified schemas). Act only via call_action.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Task intent or keywords"
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "call_action",
                "description": "Run an authorized skill action: 'server:tool' id + params. Rejects non-whitelisted actions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name from discover_skills"
                        },
                        "action": {
                            "type": "string",
                            "description": "Authorized id, e.g. github:get_pull_request"
                        },
                        "parameters": {
                            "type": "object",
                            "description": "Tool arguments"
                        }
                    },
                    "required": ["skill", "action", "parameters"]
                }
            }
        ]
    })
}

/// Approximate token size of everything the host sees in its resident tool
/// list — the PRD baseline (150-200 tokens).
pub fn meta_tools_token_estimate() -> usize {
    approx_tokens(&meta_tools_json().to_string())
}
