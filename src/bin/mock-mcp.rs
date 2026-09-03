//! Mock MCP server used by integration tests and demos.
//!
//! Speaks MCP over stdio with verbose schemas (to exercise the minifier) and
//! optional latency/logging. Profiles: `github` (7 tools), `postgres` (4 tools).
//!
//! Environment:
//!   MOCK_LOG=<path>      append "CALL <tool> <args-json>" lines
//!   MOCK_DELAY_MS=<n>    delay tools/call responses

use std::io::{BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let profile = std::env::args().nth(1).unwrap_or_else(|| "github".into());
    let log_path = std::env::var("MOCK_LOG").ok().map(std::path::PathBuf::from);
    let delay_ms: u64 = std::env::var("MOCK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let tools = match profile.as_str() {
        "postgres" => postgres_tools(),
        _ => github_tools(),
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut counter: u64 = 0;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with("Content-Length:") {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        let reply: Option<Value> = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": format!("mock-{profile}"), "version": "1.0.0" }
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": { "tools": tools }
            })),
            "tools/call" => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));

                if let Some(lp) = &log_path {
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(lp)
                    {
                        let _ = writeln!(f, "CALL {name} {args}");
                    }
                }
                if delay_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                match name.as_str() {
                    "fail_always" => Some(json!({                        "jsonrpc": "2.0",
                        "id": id.unwrap_or(Value::Null),
                        "error": { "code": -32000, "message": "mock failure (as requested)" }
                    })),
                    "echo_error" => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id.unwrap_or(Value::Null),
                        "result": {
                            "content": [{"type": "text", "text": "downstream returned an error payload"}],
                            "isError": true
                        }
                    })),
                    other => {
                        // includes "echo" — generic echo tool
                        counter += 1;
                        let text = format!(
                            "mock-{profile}:{other} called #{} args={}",
                            counter,
                            serde_json::to_string(&args).unwrap_or_default()
                        );
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id.unwrap_or(Value::Null),
                            "result": {
                                "content": [{"type": "text", "text": text}],
                                "structuredContent": { "tool": other, "call": counter, "args": args }
                            }
                        }))
                    }
                }
            }
            "ping" => Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": {}
            })),
            _ => {
                if msg.get("method").is_some() && id.is_none() {
                    None // notification: ignore
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id.unwrap_or(Value::Null),
                        "error": { "code": -32601, "message": "method not found" }
                    }))
                }
            }
        };

        if let Some(r) = reply {
            let mut bytes = serde_json::to_vec(&r).unwrap();
            bytes.push(b'\n');
            let _ = stdout.write_all(&bytes);
            let _ = stdout.flush();
        }
    }
}

/// GitHub-style profile: 7 tools with verbose schemas.
fn github_tools() -> Vec<Value> {
    vec![
        verbose_tool(
            "get_pull_request",
            "获取 PR 详情与变更代码. Fetches the pull request, its diff and changed files.",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "title": "Get Pull Request",
                "additionalProperties": false,
                "required": ["pull_number", "repo"],
                "properties": {
                    "pull_number": {
                        "type": "integer",
                        "title": "Pull number",
                        "minimum": 1,
                        "examples": [42],
                        "description": "The pull request number"
                    },
                    "repo": {
                        "type": "string",
                        "pattern": "^[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+$",
                        "minLength": 3,
                        "default": "owner/repo",
                        "description": "Owner/repository slug"
                    },
                    "include_diff": {
                        "type": "boolean",
                        "default": true,
                        "description": "Include the full diff in the response"
                    }
                }
            }),
        ),
        verbose_tool(
            "create_review_comment",
            "提交审查评语. Submits a review comment on a pull request.",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "required": ["pull_number", "body"],
                "additionalProperties": false,
                "properties": {
                    "pull_number": {"type": "integer", "minimum": 1, "description": "PR number"},
                    "body": {"type": "string", "minLength": 1, "description": "Review comment body"},
                    "path": {"type": "string", "description": "File path the comment refers to (optional)"},
                    "line": {"type": "integer", "description": "Line number (optional)"}
                }
            }),
        ),
        verbose_tool(
            "list_issues",
            "List issues in a repository.",
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "repo slug"},
                    "state": {"type": "string", "enum": ["open", "closed", "all"], "default": "open", "description": "issue state"}
                }
            }),
        ),
        verbose_tool(
            "search_code",
            "Search code in a repository.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "description": "search query"},
                    "repo": {"type": "string", "description": "optional repo filter"}
                }
            }),
        ),
        verbose_tool(
            "create_issue",
            "Create a new issue.",
            json!({
                "type": "object",
                "required": ["repo", "title"],
                "properties": {
                    "repo": {"type": "string", "description": "repo slug"},
                    "title": {"type": "string", "description": "issue title"},
                    "body": {"type": "string", "description": "issue body"}
                }
            }),
        ),
        verbose_tool(
            "list_commits",
            "List commits of a branch.",
            json!({
                "type": "object",
                "required": ["repo"],
                "properties": {
                    "repo": {"type": "string", "description": "repo slug"},
                    "branch": {"type": "string", "description": "branch name"}
                }
            }),
        ),
        verbose_tool(
            "merge_pull_request",
            "Merge a pull request.",
            json!({
                "type": "object",
                "required": ["pull_number", "repo"],
                "properties": {
                    "pull_number": {"type": "integer", "description": "PR number"},
                    "repo": {"type": "string", "description": "repo slug"},
                    "method": {"type": "string", "enum": ["merge", "squash", "rebase"], "description": "merge method"}
                }
            }),
        ),
    ]
}

/// Postgres-style profile: 4 tools.
fn postgres_tools() -> Vec<Value> {
    vec![
        verbose_tool(
            "query",
            "Execute a read-only SQL query",
            json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": {"type": "string", "description": "SQL statement (read-only)"},
                    "params": {"type": "array", "items": {"type": "string"}, "description": "bind params"}
                }
            }),
        ),
        verbose_tool(
            "execute",
            "Execute a write SQL statement",
            json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": {"type": "string", "description": "SQL statement"}
                }
            }),
        ),
        verbose_tool(
            "list_tables",
            "List tables in the database",
            json!({
                "type": "object",
                "properties": {
                    "schema": {"type": "string", "default": "public", "description": "schema name"}
                }
            }),
        ),
        verbose_tool(
            "describe_table",
            "Describe a table's columns",
            json!({
                "type": "object",
                "required": ["table"],
                "properties": {
                    "table": {"type": "string", "description": "table name"},
                    "schema": {"type": "string", "description": "schema name"}
                }
            }),
        ),
    ]
}

fn verbose_tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}
