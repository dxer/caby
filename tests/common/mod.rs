//! Shared harness for integration tests: spawn the real `caby serve` binary,
//! speak MCP over stdio as a host client, and provision mock downstream servers.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub fn caby_bin() -> &'static str {
    env!("CARGO_BIN_EXE_caby")
}

pub fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-mcp")
}

/// Create a temp project with `.caby/skills` populated from (name, content).
pub struct TestEnv {
    pub root: PathBuf,
    pub project_skills: PathBuf,
    pub config: PathBuf,
    pub mock_logs: Vec<PathBuf>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.keep(); // transfer ownership; TestEnv cleans it up
        let project_skills = root.join(".caby").join("skills");
        std::fs::create_dir_all(&project_skills).unwrap();
        TestEnv {
            root: root.clone(),
            project_skills: project_skills.clone(),
            config: root.join("config.json"),
            mock_logs: Vec::new(),
        }
    }

    pub fn write_skill(&self, name: &str, content: &str) -> PathBuf {
        let p = self.project_skills.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    pub fn remove_skill(&self, name: &str) {
        std::fs::remove_file(self.project_skills.join(name)).ok();
    }

    /// Write the gateway config referencing the mock servers.
    pub fn write_config(&mut self, servers: &[(&str, &str)]) {
        let mut defs: Vec<Value> = Vec::new();
        for (name, profile) in servers {
            let log = self.root.join(format!("{name}-calls.log"));
            self.mock_logs.push(log.clone());
            defs.push(json!({
                "name": name,
                "command": mock_bin(),
                "args": [profile],
                "env": { "MOCK_LOG": log.to_string_lossy() },
                "enabled": true,
            }));
        }
        let cfg = json!({
            "version": 1,
            "servers": defs,
            "settings": {
                "log_level": "warn",
                "discover_top_k": 3,
                "match_threshold": 0.0,
                "call_timeout_secs": 10,
                "minify_schemas": true,
                "restart_max": 0
            }
        });
        std::fs::write(&self.config, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    }

    pub fn mock_call_log(&self, name: &str) -> Vec<String> {
        let log = self.root.join(format!("{name}-calls.log"));
        std::fs::read_to_string(log)
            .map(|raw| {
                raw.lines()
                    .filter(|l| l.starts_with("CALL "))
                    .map(|l| l.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn xdg_config_home(&self) -> PathBuf {
        self.root.join("xdg")
    }
}

/// Host-side MCP client driving the gateway over stdio.
pub struct GatewayClient {
    pub child: Child,
    stdin: Option<ChildStdin>,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
    pub stderr_tail: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl GatewayClient {
    pub fn spawn(env: &TestEnv) -> GatewayClient {
        Self::spawn_with(env, &[])
    }

    pub fn spawn_with(env: &TestEnv, extra_args: &[&str]) -> GatewayClient {
        let mut cmd = Command::new(caby_bin());
        cmd.arg("serve")
            .arg("--config")
            .arg(&env.config)
            .arg("--log-level")
            .arg("warn")
            .args(extra_args)
            .current_dir(&env.root)
            .env("XDG_CONFIG_HOME", env.xdg_config_home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn caby serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        {
            let tail = std::sync::Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line) {
                    if n == 0 {
                        break;
                    }
                    let l = line.trim().to_string();
                    if let Ok(mut guard) = tail.lock() {
                        guard.push(l);
                        let excess = guard.len().saturating_sub(60);
                        if excess > 0 {
                            guard.drain(..excess);
                        }
                    }
                    line.clear();
                }
            });
        }

        GatewayClient {
            child,
            stdin: Some(stdin),
            reader: BufReader::new(stdout),
            next_id: 0,
            stderr_tail,
        }
    }

    pub fn stderr(&self) -> Vec<String> {
        self.stderr_tail.lock().unwrap().clone()
    }

    /// Send a request and block until the matching response arrives.
    /// Notifications are skipped.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut bytes = serde_json::to_vec(&msg).unwrap();
        bytes.push(b'\n');
        self.stdin.as_mut().unwrap().write_all(&bytes).unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {method} — stderr: {:?}",
                self.stderr()
            );
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .expect("read line from gateway");
            if n == 0 {
                panic!(
                    "gateway closed stdout while waiting for {method} — stderr: {:?}",
                    self.stderr()
                );
            }
            let resp: Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("gateway sent non-JSON '{}': {e}", line.trim()));
            if resp.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return resp;
            }
            // notification or unrelated message — skip
        }
    }

    pub fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "0.0.0"}
            }),
        )
    }

    pub fn tools_list(&mut self) -> Value {
        self.request("tools/list", json!({}))
    }

    pub fn tools_call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({ "name": name, "arguments": args }))
    }

    pub fn discover(&mut self, query: &str) -> Value {
        self.tools_call("discover_skills", json!({ "query": query }))
    }

    /// Retry discover until `predicate` passes on the response (used to wait
    /// for downstream servers to finish their handshake).
    pub fn discover_until(
        &mut self,
        query: &str,
        predicate: impl Fn(&Value) -> bool,
        timeout: Duration,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let resp = self.discover(query);
            if predicate(&resp) {
                return resp;
            }
            assert!(
                Instant::now() < deadline,
                "predicate never satisfied — resp: {resp} — stderr: {:?}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    pub fn shutdown(&mut self) {
        // dropping stdin closes the pipe -> gateway sees EOF and exits
        self.stdin.take();
        let _ = self.child.wait();
    }
}

impl Drop for GatewayClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Helper to extract the tool-call content text.
pub fn text_of(resp: &Value) -> String {
    let content: &Vec<Value> = resp
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("response has no content array: {resp}"));
    content
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_error(resp: &Value) -> bool {
    resp.pointer("/result/isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn parse_result(resp: &Value) -> Value {
    let text = text_of(resp);
    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text))
}

/// Approximate token estimator mirroring the crate's (words+punctuation+CJK).
pub fn approx_tokens(s: &str) -> usize {
    let mut count = 0usize;
    let mut word = String::new();
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !word.is_empty() {
                count += word.chars().count().div_ceil(4);
                word.clear();
            }
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '/' || ch == ':' {
            word.push(ch);
        } else {
            if !word.is_empty() {
                count += word.chars().count().div_ceil(4);
                word.clear();
            }
            // any non-ASCII (incl. CJK) counts as ~1 token
            count += 1;
        }
    }
    if !word.is_empty() {
        count += word.chars().count().div_ceil(4);
    }
    count
}

pub const GIT_REVIEW_SKILL: &str = r#"---
name: PR 代码审查与质量检查
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
keywords:
  - code review
  - pull request
allowed_tools:
  - github:get_pull_request
  - github:create_review_comment
---
# 执行准则与安全规范
1. 必须先通过 get_pull_request 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
"#;

pub const DB_SKILL: &str = r#"---
name: 数据库性能排查
description: 排查 postgres 慢查询、索引健康、表结构分析
allowed_tools:
  - postgres:query
  - postgres:list_tables
---
# 执行准则
1. 任何写操作必须显式确认。
2. 慢查询先看执行计划再下结论。
"#;

pub const FALLBACK_SKILL: &str = r#"---
name: General Helper
description: 兜底技能：未命中任何专项技能时使用，不授权任何工具
fallback: true
---
# 准则
小心行事，不要调用任何底层工具。
"#;
