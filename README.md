# Caby (CabyMCP)

> **Keep your agent calm & context lean.**

Caby 是一款面向 MCP（Model Context Protocol）生态的轻量级元网关与动态调度工具：它充当 AI 编程客户端（Claude Code / Cursor / Cline）与真实底层 MCP Servers 之间的透明多路复用代理。

- **Token 极致压制** — 对上游客户端恒定只暴露 **2 个元工具**，常驻上下文 ≈ **200 tokens**（cl100k 实测）。
- **目录约定自发现** — 自动扫描并毫秒级热监听本地技能目录，无需维护 Skill 配置清单。
- **开箱即用 CLI** — 一键挂载 MCP 服务、技能包安装、各大 Agent 客户端自动注册。
- **SOP 规则强绑定** — 不仅动态分发底层工具，更将业务操作准则与安全红线按需注入大模型。
- **零外部依赖** — 单静态二进制交付（musl static-pie，~3.4 MB），无需 Node / Python / 数据库。

```text
┌─────────────────────────────────────────────────────────────┐
│          Host Client (Claude Code / Cursor / Cline)         │
│  - 全局常驻仅见 2 个元工具 (~200 Tokens)                     │
└──────────────────────────────┬──────────────────────────────┘
                               │ stdio (JSON-RPC 2.0)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                       Caby Gateway Core                     │
│                                                             │
│  [1. 外层协议暴露]                                           │
│     ├── discover_skills(query)                             │
│     └── call_action(skill, action, parameters)              │
│                                                             │
│  [2. 核心调度与发现引擎]                                     │
│     ├── Skill Auto-Discovery & Watcher (扫描 + 热重载)       │
│     ├── In-Memory Matcher (TF-IDF 意图匹配，含中文大分词)    │
│     ├── Schema Minifier (递归裁剪 JSON Schema 冗余)          │
│     └── Security Sandbox (白名单校验，100% 越权拦截)         │
│                                                             │
│  [3. 下游子进程管理器 (Subprocess Pool)]                     │
│     ├── Server 1: GitHub MCP (常驻 stdio 管道)              │
│     ├── Server 2: Postgres MCP (常驻 stdio 管道)            │
│     └── Server N: 自定义可执行文件 / Docker 容器             │
└─────────────────────────────────────────────────────────────┘
```

---

## 快速开始

```bash
# 构建（开发）
cargo build

# 构建单静态二进制（零运行时依赖，musl）
./scripts/build-static.sh            # → target/x86_64-unknown-linux-musl/release/caby

# 挂载下游 MCP 服务（自动做 initialize 握手校验）
caby add github --command "github-mcp-server" --env GITHUB_TOKEN=ghp_xxx
caby add postgres --command "docker run -i --rm mcp/postgres postgresql://localhost/db"

# 让 AI 客户端接入（自动修改客户端配置，零手工 JSON）
caby install --target claude-code
caby install --target cursor
caby install --target cline

# 启动网关（客户端通过 stdio 连接）
caby serve
```

### 端到端演示

```bash
cargo build && ./scripts/demo.sh
```

演示会启动两个 Mock MCP Servers + 三个示例技能，逐步展示：`initialize` → `tools/list`（仅 2 个元工具）→ `discover_skills`（命中技能并下发 SOP + 裁剪后schema）→ 授权调用 → 越权调用被 **BLOCKED**。

---

## 核心机制

### 1. 双元工具（Dual Meta-Tools）

Caby 只向客户端声明 2 个工具，其余一切按需注入：

| 工具 | 触发时机 | 入参 | 返回 |
| --- | --- | --- | --- |
| `discover_skills` | 大模型接到具体垂直任务时首先调用 | `query`（任务意图/关键词） | 最佳匹配技能的 **SOP 正文** + 白名单动作的 **裁剪后 schema** |
| `call_action` | 拿到 SOP 后执行具体动作 | `skill`（激活技能名）、`action`（`server:tool`）、`parameters` | 下游 **无损结果透传**；越权立即返回标准错误 |

常驻开销实测（`tools/list` 完整载荷，cl100k tokenizer）：**200 tokens**。

### 2. Skills 目录自动发现 + 热重载

扫描优先级（同名时项目优先）：

1. 当前项目 `.caby/skills/`（项目独享）
2. `~/.config/caby/skills/`（全局通用）

单 Markdown 文件即一个技能，YAML Front-Matter 声明元数据，正文即 SOP：

```markdown
---
name: PR 代码审查与质量检查
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
keywords:
  - code review
allowed_tools:
  - github:get_pull_request
  - github:create_review_comment
---

# 执行准则与安全规范
1. 必须先通过 `get_pull_request` 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
```

文件系统监听（inotify，40ms 防抖）在**增删改后无需重启**即可重建内存索引——实测 `.md` 变更到 `discover_skills` 可见约 **68ms**（PRD 目标 ≤100ms）。

### 3. Schema Minifier

拉取下游工具元数据时递归剔除 `$schema`、`title`、`pattern`、`minLength`、`examples`、`default`、`const` 等，仅保留 `type` / `properties` / `required` / `description` / 小 `enum`。详见 `src/core/minifier.rs`。

### 4. Security Sandbox

`call_action` 四步拦截链：**技能必须存在** → **action ∈ 白名单（100% 拦截，不触达下游）** → 服务可路由 → 派发。空白名单技能拒绝一切调用；`fallback` 技能只提供上下文、不授权任何工具。

### 5. 下游进程池

每个 Server 一条常驻 stdio 管道，启动即完成 `initialize` 握手并预注册工具，消除冷启动；请求按服务串行（MCP 安全默认）；崩溃自动重启（指数退避，`restart_max` 可配，`--no-restart` 关闭）。

---

## CLI

```
caby serve [--config PATH] [--log-level LEVEL] [--no-restart] [--timeout-secs N]
caby add <name> --command <CMD> [--args ARG...] [--env K=V...] [--cwd DIR] [--no-verify]
caby remove <name>
caby list [--offline] [--json]
caby skill new <name> [--dir project|global]
caby skill install <spec> [--yes] [--dir project|global]   # github:user/repo[/path] | https://… | 本地 .md
caby install --target <claude-code|cursor|cline> [--project] [--yes]
caby version
```

### `caby list` 示例

```text
Servers (2 running)
├── github (7 tools registered, schema minified -22%)
└── postgres (4 tools registered, schema minified -3%)

Skills (3 active)
├── .caby/skills/git_review.md (authorized: 2 tools)
├── .caby/skills/db_analytics.md (authorized: 2 tools)
└── ~/.config/caby/skills/general_helper.md (authorized: 0 tools, fallback)
```

---

## 配置

`~/.config/caby/config.json`（可用 `--config` / `$CABY_CONFIG` 覆盖）：

```json
{
  "version": 1,
  "servers": [
    {
      "name": "github",
      "command": "github-mcp-server",
      "args": [],
      "env": { "GITHUB_TOKEN": "ghp_xxx" },
      "cwd": null,
      "enabled": true
    }
  ],
  "settings": {
    "log_level": "info",
    "discover_top_k": 3,
    "match_threshold": 0.001,
    "call_timeout_secs": 30,
    "minify_schemas": true,
    "restart_max": 5
  }
}
```

---

## 验收指标（实测）

| 维度 | PRD 指标 | 实测 | 验证方式 |
| --- | --- | --- | --- |
| 环境依赖 | 零外部依赖、单静态二进制 | musl static-pie，3.4 MB，`ldd` 显示 statically linked | `scripts/build-static.sh` |
| Token 消耗 | 常驻工具数 = 2，基准 150-200 Tokens | 2 个元工具；`tools/list` 载荷 **200 tokens**（cl100k 实测） | `exactly_two_meta_tools_and_token_budget` |
| 自动化响应 | 技能 `.md` 增删改 100ms 内索引重载 | **~68 ms**（create 实测；modify/remove 亦热更新） | `hot_reload_picks_up_new_skill_under_100ms` |
| 安全与隔离 | 白名单外调用 100% 拦截 | 拦截并返回 `BLOCKED…` 错误，下游 Mock 日志证实零泄漏 | `unauthorized_call_is_blocked_before_downstream` |
| 执行延迟 | 网关内部路由/裁剪/匹配 ≤ 5ms | release 实测：minify **0.014ms**、matcher(100 技能) **0.333ms**、完整发现管线 **0.52ms** | `perf.rs`（`cargo test --release --bin caby perf`） |

## 测试

```bash
cargo test                      # 单元 + 集成（35 + 9）
cargo test --release --bin caby perf   # 5ms 性能门禁
```

集成测试为黑盒方式：真实启动 `caby serve` 进程，通过 stdio 与 Mock MCP Servers 对话。

## 项目结构

```text
src/
├── main.rs / cli.rs          # 入口 + clap CLI
├── config.rs                 # 配置持久化
├── util.rs                   # 日志、路径、shell 切分、token 估算
├── core/
│   ├── jsonrpc.rs            # JSON-RPC 2.0 + stdio framing（含 Content-Length 兼容）
│   ├── yaml_fm.rs            # Front-Matter 解析（零 YAML 依赖）
│   ├── matcher.rs            # TF-IDF 意图匹配（CJK 二元分词）
│   ├── minifier.rs           # Schema 剪枝
│   ├── skillstore.rs         # 扫描 + notify 热重载
│   ├── mcpserver.rs          # 下游 MCP 客户端（常驻管道、串行请求）
│   ├── registry.rs           # 进程池 + 工具索引
│   ├── sandbox.rs            # 白名单沙箱
│   └── gateway.rs            # 面向宿主的两元工具网关
├── commands/                 # serve / add / remove / list / skill / install
├── installers/               # claude-code / cursor / cline 配置注入
└── bin/mock-mcp.rs           # 测试用 Mock MCP Server
tests/                        # 黑盒集成测试
scripts/                      # 构建静态二进制 / 演示
```

## 设计取舍

- **stateless 沙箱**：`call_action` 只做白名单成员校验（技能存在 + action 属于其 `allowed_tools`），不依赖「激活窗口」状态，模型在会话任意时刻都可复用已发现的技能。
- **每服务串行请求**：遵循 MCP 客户端默认安全语义，LLM 调用频率下无感知。
- **schemas 在注册期裁剪**：minify 在 `tools/list` 返回时一次性完成，发现路径零开销。
- **README 之外的命令行为均可由 `--help` 查询**。

## License

MIT