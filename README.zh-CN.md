# Caby — Keep your agent calm & context lean

> **Caby**（取 "capability" 之意的轻量昵称）是一款面向 [Model Context Protocol](https://modelcontextprotocol.io)（MCP）生态的**轻量级元网关与动态调度工具**。它充当 AI 编程客户端（Claude Code / Cursor / Cline…）与真实底层 MCP Servers 之间的透明多路复用代理：对外恒定只暴露 **2 个元工具（约 200 tokens）**，自动从本地目录发现技能包，并按「技能级白名单」强制执行工具调用安全。

[English README →](README.md) · MIT License · Rust 1.98+ · 零运行时依赖（单静态二进制）

---

## 为什么需要 Caby？

接入一个 MCP Server 很容易；接入十个很难：

- 每个 Server 的 `tools/list` schema 会**常驻**在客户端上下文里——数千 Token 花在几乎不用的工具上。
- 维护技能/SOP 清单意味着不断编辑越来越长的配置文件。
- 一旦模型同时看到**全部**工具，它就敢调用其中任何一个——包括不该碰的那些。

Caby 把问题整个掉转过来：

| 问题 | Caby 的解法 |
| --- | --- |
| Token 膨胀 | 宿主永远只看到 **2 个元工具**（`discover_skills` / `call_action`）——常驻约 **200 tokens** |
| 配置维护 | 技能就是被监听目录里的 `.md` 文件——**配置里没有技能清单**，增删改后 ~68ms 热重载 |
| 幻觉 / 越权调用 | **沙箱白名单**：每个 action 必须属于当前激活技能的 `allowed_tools`，否则在触达任何后端之前被拦截（100% 拦截，有测试背书） |
| 冷启动延迟 | 下游服务器在**常驻 stdio 子进程池**中保活，启动即完成握手 |
| Schema Token 开销 | 下游 schema 在注册期**一次性裁剪**（递归剔除 `$schema`、`title`、`pattern`、`minLength`、`examples`…） |

## 功能特性

- 🪶 **双元工具** — `discover_skills(query)` 与 `call_action(skill, action, parameters)`
- 📁 **目录约定自发现** — `.caby/skills/`（项目）+ `~/.config/caby/skills/`（全局）
- ⚡ **热重载** — inotify 监听；`.md` 增删改后 ~68ms 重建索引，零重启
- 🧠 **意图匹配** — TF-IDF 余弦相似度 + 中文二元分词，中文查询开箱即用
- 🔒 **安全沙箱** — 技能级 `allowed_tools` 白名单，越权调用 100% 拦截
- ✂️ **Schema 裁剪** — 只保留 `type` / `properties` / `required` / `description` / 小规模 `enum`
- 🔌 **子进程池** — 常驻 stdio 管道 + `initialize` 握手 + 请求串行 + 崩溃自动重启（指数退避）
- 🧩 **CLI** — `serve` / `add` / `remove` / `list` / `skill new` / `skill install` / `install --target <客户端>`
- 📦 **单静态二进制** — musl static-pie，约 3.4 MB，零运行时依赖（无需 Node/Python/glibc）
- 🧪 **有测试** — 44 项：单元 + 黑盒集成（真实进程走 stdio）+ 性能门禁

## 工作原理

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

一次典型的智能体对话：

1. **`initialize`** → Caby 协商协议版本，声明 `tools` 能力。
2. **`tools/list`** → 恰好两个工具，共约 200 cl100k tokens。
3. 模型接到任务 → **`discover_skills("review PR #42")`** → Caby 对技能排序，返回命中技能的 **SOP 规则**（正文）+ 其白名单动作的 **裁剪后 schema**。
4. 模型执行 → **`call_action("github:get_pull_request", …)`** → 沙箱校验 → 经常驻管道派发 → 结果无损透传。
5. 其它一律 → **`BLOCKED: …`**，任何下游进程都看不到这次调用。

## 安装

### 源码构建

```bash
# 开发构建
cargo build

# 单静态二进制（musl，零运行时依赖）
./scripts/build-static.sh        # → target/x86_64-unknown-linux-musl/release/caby

# 端到端演示（2 个 Mock Server + 3 个技能 + 脚本化对话）
cargo build && ./scripts/demo.sh
```

要求：**Rust 1.85+**（stable）；静态构建需要 `musl-gcc`（Debian/Ubuntu：`apt install musl-tools`）。

### 包管理器

尚未发布——crate 上架后即可 `cargo install caby`。当前可源码构建，或从 GitHub [Releases](https://github.com/caby-dev/caby/releases) 下载二进制。

## 快速开始

```bash
# 1. 挂载下游 MCP 服务（自动做 initialize 握手校验）
caby add github --command "github-mcp-server" --env GITHUB_TOKEN=ghp_xxx
caby add postgres --command "docker run -i --rm mcp/postgres postgresql://localhost/db"

# 2. 注册进 AI 客户端（零手工 JSON）
caby install --target claude-code     # 写入 ~/.claude.json
caby install --target cursor          # 写入 ~/.cursor/mcp.json
caby install --target cline           # 写入 Cline MCP 设置

# 3. 启动网关（客户端经 stdio 接入）
caby serve
```

重启客户端——你会看到一个名为 `caby` 的 MCP 服务，且只提供两个工具。

## 技能（Skills）

### 编写

一个技能 = 一个 Markdown 文件。Front-Matter 声明元数据，正文即下发给模型的 SOP。

```markdown
---
name: PR 代码审查与质量检查          # 显示名（call_action 用）
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
keywords:                            # 可选——额外检索词
  - code review
  - pull request
allowed_tools:                       # 白名单——本技能唯一可调用的动作
  - github:get_pull_request
  - github:create_review_comment
# fallback: true                     # 可选——未命中任何技能时兜底展示
---

# 执行准则与安全规范
1. 必须先通过 `get_pull_request` 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
```

存放位置（重名时项目优先）：

| 优先级 | 目录 | 范围 |
| --- | --- | --- |
| 1 | `<项目>/.caby/skills/` | 项目独享 |
| 2 | `~/.config/caby/skills/` | 跨项目通用 |

描述撰写建议：说明**何时**适用、使用模型自然会用到的措辞，并为双语查询补充英文关键词。

### 安装

```bash
caby skill new deploy-pipeline              # 生成标准模板（含 Front-Matter）
caby skill install github:user/repo/my-skill # 从 GitHub 仓库路径安装
caby skill install github:user/repo          # 扫描仓库 skills/ 目录
caby skill install https://…/my-skill.md     # 直接 URL
caby skill install ./local-skill.md          # 本地文件
```

若技能引用了未配置的下游服务器（从 `allowed_tools` 前缀自动嗅探），会提示并交互式引导 `caby add`（`--yes` 跳过交互）。

## CLI

```
caby serve [--config PATH] [--log-level error|warn|info|debug|trace]
           [--no-restart] [--timeout-secs N]
caby add <name> --command <CMD> [--args ARG...] [--env K=V...] [--cwd DIR] [--no-verify]
caby remove <name>
caby list [--offline] [--json]
caby skill new <name> [--dir project|global]
caby skill install <spec> [--yes] [--dir project|global]
caby install --target <claude-code|cursor|cline> [--project] [--yes] [--command <CMD>]
caby version
```

`caby list` 展示实时装载状态（对每个服务器现场探测真实工具数）：

```text
Servers (2 running)
├── github (7 tools registered, schema minified -22%)
└── postgres (4 tools registered, schema minified -3%)

Skills (3 active)
├── .caby/skills/git_review.md (authorized: 2 tools)
├── .caby/skills/db_analytics.md (authorized: 2 tools)
└── ~/.config/caby/skills/general_helper.md (authorized: 0 tools, fallback)
```

## 配置

位置：**`~/.config/caby/config.json`**（可用 `--config` / `$CABY_CONFIG` 覆盖）。

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

| 配置项 | 默认 | 含义 |
| --- | --- | --- |
| `log_level` | `info` | 网关日志级别（stderr，绝不污染 stdout 协议通道） |
| `discover_top_k` | 3 | 每次 `discover_skills` 最多返回的技能数 |
| `match_threshold` | 0.001 | 技能命中的最低匹配分 |
| `call_timeout_secs` | 30 | 下游工具调用超时 |
| `minify_schemas` | true | 注册期裁剪 schema 噪音 |
| `restart_max` | 5 | 下游崩溃自动重启次数（`--no-restart` 置 0） |

> **只有 servers 在配置文件里。** 技能存在于技能目录——这是刻意的设计，见[为什么需要 Caby](#为什么需要-caby)。

## 安全模型

`call_action` 四步拦截链，**严格按序**：

1. 技能必须存在（先 discover）。
2. `action` 必须严格出现在该技能 `allowed_tools` 中——**否则返回 `BLOCKED` 并停止，不触碰任何下游进程。**
3. 目标服务器必须已配置且处于 Ready。
4. 之后才派发。

其它加固：空白名单技能拒绝**一切**调用；`fallback` 技能只提供上下文；格式错误的 action（缺冒号、空技能名）以标准 `isError` 结果拒绝。密钥留在你的配置文件里；对外只向已配置的服务器发送标准 JSON-RPC 帧。

## 性能（实测数据）

| 指标 | 数值 |
| --- | --- |
| 常驻工具数 | **2** |
| 常驻 `tools/list` 载荷 | **200 cl100k tokens**（802 字符，tiktoken 实测） |
| 技能热重载（新增） | **~68 ms**（文件写入 → `discover_skills` 可见） |
| Schema 裁剪 | **0.014 ms** / 个（release） |
| 100 技能意图匹配 | **0.333 ms**（release） |
| 完整发现管线（匹配 + 20 个裁剪 schema） | **0.52 ms**（release） |
| 越权拦截率 | **100%**（集成测试：Mock 日志证实零泄漏） |
| 静态二进制 | 3.4 MB，static-pie，零共享库 |

## 测试

```bash
cargo test                            # 35 单元 + 9 黑盒集成
cargo test --release --bin caby perf  # ≤5ms 性能门禁（release 严格模式）
cargo clippy --all-targets            # 当前 0 警告
```

集成测试为纯黑盒：真实启动 `caby serve` 进程，经 stdio 与 Mock 下游服务器对话，覆盖排序、裁剪、路由、**拦截**、无损透传、热重载与错误路径。

## 项目结构

```text
src/
├── main.rs / cli.rs          # 入口 + clap CLI
├── config.rs                 # 配置持久化（servers + settings）
├── util.rs                   # 日志、路径、shell 切分、token 估算
├── core/
│   ├── jsonrpc.rs            # JSON-RPC 2.0 + stdio framing（newline + Content-Length 兼容）
│   ├── yaml_fm.rs            # Front-Matter 解析（零 YAML 依赖）
│   ├── matcher.rs            # TF-IDF 意图匹配（CJK 二元分词）
│   ├── minifier.rs           # 递归 Schema 剪枝
│   ├── skillstore.rs         # 目录扫描 + notify 热重载
│   ├── mcpserver.rs          # 下游 MCP 客户端（常驻管道、串行请求）
│   ├── registry.rs           # 进程池 + 工具索引
│   ├── sandbox.rs            # 白名单沙箱
│   └── gateway.rs            # 面向宿主的两元工具网关
├── commands/                 # serve / add / remove / list / skill / install
├── installers/               # claude-code / cursor / cline 配置写入
├── bin/mock-mcp.rs           # 测试/演示用 Mock MCP Server
└── perf.rs                   # ≤5ms 性能门禁
tests/                        # 黑盒集成测试套件
examples/skills/              # 示例技能包
scripts/
├── build-static.sh           # musl 静态二进制构建
└── demo.sh                   # 端到端演示
```

## Roadmap

- [x] 核心网关（双元工具、发现、派发、沙箱、裁剪）
- [x] 技能自发现 + 热重载、CLI、客户端注册器、静态二进制
- [ ] 与**真实**服务器（`github-mcp-server`、docker 内 postgres）的端到端验证——当前由 Mock 覆盖
- [ ] macOS / Windows 支持
- [ ] CI 流水线（测试 + 性能门禁 + 产物）与徽章
- [ ] 技能包注册表 / `caby skill search`
- [ ] `caby doctor` 诊断子命令
- [ ] sampling / roots 客户端能力（nice-to-have）

## 参与贡献

欢迎 PR。保持小而可测：

```bash
cargo fmt && cargo clippy --all-targets && cargo test
# 性能门禁：cargo test --release --bin caby perf
```

设计变更请先开 issue。友善交流——这个项目存在的全部意义就是把上下文变瘦。

## FAQ

**Caby 会取代我客户端的 MCP 配置吗？** 部分。服务器仍需声明（写进 `config.json`，或用 `caby add` 添加），但「哪个技能能用哪些工具 + 操作准则」全部在技能文件里——这正是省 Token、省维护的点。

**数据会外传吗？** 不会。全部本地运行，无遥测；唯一的网络行为是你配置的服务器和你显式触发的技能包下载。

**下游服务器崩溃了怎么办？** Caby 以指数退避自动重启（上限 `restart_max`）；在途调用以 `isError` 干净失败。

**支持哪些协议版本？** `2025-06-18`、`2025-03-26`、`2024-11-05`（`initialize` 时协商）。

## License

MIT — 见 [LICENSE](LICENSE)。