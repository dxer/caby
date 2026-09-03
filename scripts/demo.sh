#! /usr/bin/env bash
# One-shot demo: boots caby with two mock MCP servers and three example skills,
# then runs a scripted host conversation (initialize → tools/list →
# discover → authorized call → blocked call) and prints the transcript.
#
# Usage: cargo build && ./scripts/demo.sh

set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${BIN:-$(pwd)/target/debug/caby}"
MOCK="${MOCK:-$(pwd)/target/debug/mock-mcp}"
[ -x "$BIN" ] || { echo "build first: cargo build"; exit 1; }

DEMO="$(mktemp -d /tmp/caby-demo.XXXXXX)"
trap 'rm -rf "$DEMO"' EXIT
mkdir -p "$DEMO/.caby/skills"

cat > "$DEMO/.caby/skills/git_review.md" <<'MD'
---
name: PR 代码审查与质量检查
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
allowed_tools:
  - github:get_pull_request
  - github:create_review_comment
---
# 执行准则与安全规范
1. 必须先通过 get_pull_request 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
MD

cat > "$DEMO/.caby/skills/db_analytics.md" <<'MD'
---
name: 数据库性能排查
description: 排查 postgres 慢查询、索引健康、表结构分析
allowed_tools:
  - postgres:query
  - postgres:list_tables
---
# 执行准则
1. 任何写操作必须显式确认。
2. 慢查询先看执行计划再下结论。
MD

cat > "$DEMO/.caby/skills/general_helper.md" <<'MD'
---
name: General Helper
description: 兜底技能：未命中任何专项技能时使用
fallback: true
---
# 准则
小心行事，不要调用任何底层工具。
MD

cat > "$DEMO/config.json" <<JSON
{
  "version": 1,
  "servers": [
    { "name": "github",   "command": "$MOCK", "args": ["github"],   "env": {}, "enabled": true },
    { "name": "postgres", "command": "$MOCK", "args": ["postgres"], "env": {}, "enabled": true }
  ],
  "settings": { "log_level": "warn", "discover_top_k": 3, "match_threshold": 0.0, "call_timeout_secs": 10, "minify_schemas": true, "restart_max": 0 }
}
JSON

requests() {
  sleep 0.6  # let the backend servers complete their handshake
  cat <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"discover_skills","arguments":{"query":"帮我审查这个 pull request 的代码 diff"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"call_action","arguments":{"skill":"PR 代码审查与质量检查","action":"github:get_pull_request","parameters":{"pull_number":42,"repo":"acme/widgets"}}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"call_action","arguments":{"skill":"PR 代码审查与质量检查","action":"github:merge_pull_request","parameters":{"pull_number":42}}}}
EOF
}

RAW="$DEMO/out.txt"
requests | (cd "$DEMO" && XDG_CONFIG_HOME="$DEMO/xdg" "$BIN" serve --config "$DEMO/config.json" 2>"$DEMO/gateway.log") > "$RAW"

echo "────────────────────────────── caby demo transcript ──────────────────────────────"
for req in 1 2 3 4 5; do
  echo
  # gray hint: which line was the request
  case $req in
    1) echo "▶ initialize  (host -> caby)";;
    2) echo "▶ tools/list  (host asks what tools exist — answer: exactly 2 meta tools)";;
    3) echo "▶ discover_skills('帮我审查这个 pull request 的代码 diff')";;
    4) echo "▶ call_action(github:get_pull_request) — authorized → routed to github";;
    5) echo "▶ call_action(github:merge_pull_request) — NOT whitelisted → blocked";;
  esac
  if command -v /usr/bin/python3.11 >/dev/null 2>&1; then PY=/usr/bin/python3.11; else PY=python3; fi
  grep '"id":'$req'"\|"id":'$req',' "$RAW" | head -1 | "$PY" -m json.tool --no-ensure-ascii 2>/dev/null | sed 's/^/  /'
done
echo
echo "(gateway log: $DEMO/gateway.log)"
echo "───────────────────────────────────────────────────────────────────────────────────"