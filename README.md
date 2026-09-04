# Caby — keep your agent calm & context lean

> **Caby** (a clipped form of "capability") is a lightweight **meta-gateway and dynamic dispatcher** for the [Model Context Protocol](https://modelcontextprotocol.io) (MCP) ecosystem. It sits between AI coding clients (Claude Code, Cursor, Cline, …) and the real MCP servers, exposing only **2 meta tools (~200 tokens)** while discovering skill packs automatically from local directories — and enforcing a per-skill tool whitelist.

[中文版说明 →](README.zh-CN.md) · MIT License · Rust 1.98+ · Zero runtime dependencies (single static binary)

---

## Why Caby?

Connecting an MCP server today is easy. Connecting **ten** is not:

- Every server's `tools/list` schemas live in your client's context **permanently** — thousands of tokens burned on tools you barely use.
- Keeping a skills/SOP list in sync means editing ever-growing config files.
- Once a model has *all* tools visible, it happily calls anything — including the ones it should never touch.

Caby inverts the model:

| Problem | Caby's answer |
| --- | --- |
| Token bloat | Host only ever sees **2 meta tools** (`discover_skills`, `call_action`) — ~**200 tokens** resident |
| Config maintenance | Skills are just `.md` files in a watched directory — **no skill list in config**, hot-reloaded in ~68 ms |
| Hallucinated / off-limits calls | **Sandbox whitelist**: every action must be listed in the active skill's `allowed_tools`, else it is blocked **before** reaching any backend (100% intercepted, tested) |
| Cold-start latency | Downstream servers are kept alive in a **persistent stdio subprocess pool**, initialized at boot |
| Schema token cost | Downstream schemas are **minified** once at registration (recursively stripped of `$schema`, `title`, `pattern`, `minLength`, `examples`, …) |

## Features

- 🪶 **2 meta tools** — `discover_skills(query)` and `call_action(skill, action, parameters)`
- 📁 **Directory convention discovery** — `.caby/skills/` (project) + `~/.config/caby/skills/` (global)
- ⚡ **Hot reload** — `inotify` watcher; create/modify/delete a `.md` and the index rebuilds in ~68 ms, zero restarts
- 🧠 **Intent matching** — TF-IDF cosine with CJK bigram tokenization (Chinese queries work out of the box)
- 🔒 **Security sandbox** — per-skill `allowed_tools` whitelist, 100% interception of out-of-whitelist calls
- ✂️ **Schema minifier** — keeps only `type` / `properties` / `required` / `description` / small `enum`s
- 🔌 **Subprocess pool** — persistent stdio pipes with `initialize` handshake, serialized requests, crash auto-restart (backoff)
- 🧩 **CLI** — `serve` / `add` / `remove` / `list` / `skill new` / `skill install` / `install --target <client>`
- 📦 **Single static binary** — musl static-pie, ~3.4 MB, zero runtime deps (no Node/Python/glibc)
- 🧪 **Tested** — 44 tests: unit + black-box integration (real processes over stdio) + perf gates

## How it works

```text
┌─────────────────────────────────────────────────────────────┐
│          Host Client (Claude Code / Cursor / Cline)         │
│  - resident context: exactly 2 meta tools (~200 tokens)     │
└──────────────────────────────┬──────────────────────────────┘
                               │ stdio (JSON-RPC 2.0)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                       Caby Gateway Core                     │
│                                                             │
│  [1. Host-facing protocol]                                   │
│     ├── discover_skills(query)                              │
│     └── call_action(skill, action, parameters)              │
│                                                             │
│  [2. Discovery & dispatch engine]                            │
│     ├── Skill Auto-Discovery & Watcher (scan + hot reload)  │
│     ├── In-Memory Matcher (TF-IDF, CJK bigrams)             │
│     ├── Schema Minifier (recursive schema pruning)          │
│     └── Security Sandbox (whitelist, 100% interception)     │
│                                                             │
│  [3. Downstream subprocess pool]                             │
│     ├── Server 1: GitHub MCP (persistent stdio pipe)        │
│     ├── Server 2: Postgres MCP (persistent stdio pipe)      │
│     └── Server N: custom executable / docker container      │
└─────────────────────────────────────────────────────────────┘
```

A typical agent conversation:

1. **`initialize`** → Caby negotiates protocol version, advertises `tools` capability.
2. **`tools/list`** → exactly two tools, ~200 cl100k tokens total.
3. The model gets a task → **`discover_skills("review PR #42")`** → Caby ranks skills, returns the winner's **SOP rules** (markdown body) + its whitelisted actions with **minified schemas**.
4. The model acts → **`call_action("github:get_pull_request", …)`** → sandbox check → routed over the persistent pipe → result passed through losslessly.
5. Anything else → **`BLOCKED: …`** before any downstream process ever sees it.

## Install

### From source

```bash
# debug build
cargo build

# fully static single binary (musl, zero runtime deps)
./scripts/build-static.sh        # → target/x86_64-unknown-linux-musl/release/caby

# run the whole demo (2 mock servers + 3 skills + scripted conversation)
cargo build && ./scripts/demo.sh
```

Requirements: **Rust 1.85+** (stable). For the static build: `musl-gcc` (`apt install musl-tools` on Debian/Ubuntu).

### Via package managers

Not published yet — `cargo install caby` will work once the crate is on crates.io. In the meantime: build from source or grab a release binary from the [Releases](https://github.com/caby-dev/caby/releases) page.

## Quick start

```bash
# 1. attach downstream MCP servers (handshake verified automatically)
caby add github --command "github-mcp-server" --env GITHUB_TOKEN=ghp_xxx
caby add postgres --command "docker run -i --rm mcp/postgres postgresql://localhost/db"

# 2. register into your AI client (zero manual JSON editing)
caby install --target claude-code     # writes ~/.claude.json
caby install --target cursor          # writes ~/.cursor/mcp.json
caby install --target cline           # writes Cline MCP settings

# 3. start the gateway (your client connects over stdio)
caby serve
```

That's it. Restart your client — you should see a single MCP server named `caby`, offering exactly two tools.

## Skills

### Authoring

A skill is **one markdown file**. Front-matter declares metadata; the body is the SOP handed to the model.

```markdown
---
name: PR 代码审查与质量检查          # display name (used by call_action)
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
keywords:                            # optional — extra search terms
  - code review
  - pull request
allowed_tools:                       # whitelist — the ONLY actions this skill may call
  - github:get_pull_request
  - github:create_review_comment
# fallback: true                     # optional — surfaces when nothing else matches
---

# 执行准则与安全规范
1. 必须先通过 `get_pull_request` 拉取完整 diff，进行上下文检查。
2. 重点排查死锁、空指针、并发安全与边界条件。
3. 评语必须提供修改建议并附带改进后的代码块。
```

Where they live (project wins on duplicate names):

| Priority | Directory | Scope |
| --- | --- | --- |
| 1 | `<project>/.caby/skills/` | project-only skills |
| 2 | `~/.config/caby/skills/` | cross-project skills |

Rules of thumb for good descriptions: say **when** the skill applies, use the words a model would naturally use for the task, and add English keywords for bilingual queries.

### Installation

```bash
caby skill new deploy-pipeline          # scaffold a template (front-matter included)
caby skill install github:user/repo/my-skill   # from a GitHub repo path
caby skill install github:user/repo             # scans the repo's skills/ directory
caby skill install https://…/my-skill.md        # directly from a URL
caby skill install ./local-skill.md             # from a local file
```

Missing servers are detected automatically (from `allowed_tools` prefixes) and reported — with an interactive offer to `caby add` them (`--yes` skips prompting).

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

`caby list` shows the live load state (probes each server for real tool counts):

```text
Servers (2 running)
├── github (7 tools registered, schema minified -22%)
└── postgres (4 tools registered, schema minified -3%)

Skills (3 active)
├── .caby/skills/git_review.md (authorized: 2 tools)
├── .caby/skills/db_analytics.md (authorized: 2 tools)
└── ~/.config/caby/skills/general_helper.md (authorized: 0 tools, fallback)
```

### Shared daemon (multi-agent hosts)

Every `caby serve` is a launcher: the first one hosts a shared daemon (spawning
the downstream set exactly once); later ones attach as thin proxies over
loopback TCP. If the host exits, the next client transparently takes over and
replays unanswered requests (at-least-once) — no orphans, no `stop` command. Daemons are
isolated per config file (`<config>.daemon.lock`). Set `CABY_NO_DAEMON=1` to
force classic single-process mode.

## Configuration

Location: **`~/.config/caby/config.json`** (override with `--config` or `$CABY_CONFIG`).

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

| Setting | Default | Meaning |
| --- | --- | --- |
| `log_level` | `info` | gateway logging to stderr (never stdout) |
| `discover_top_k` | 3 | max skills returned per `discover_skills` |
| `match_threshold` | 0.001 | minimum match score for a skill to be returned |
| `call_timeout_secs` | 30 | per downstream tool-call timeout |
| `minify_schemas` | true | prune schema noise at registration |
| `restart_max` | 5 | auto-restarts of a crashed downstream server (`--no-restart` sets 0) |

> Only **servers** live in the config file. Skills live in the skill directories — deliberately, see [Why Caby?](#why-caby).

## Security model

`call_action` runs a four-step interception chain, **in-order**:

1. The named skill must exist (discover first).
2. `action` must be strictly present in that skill's `allowed_tools` — **if not, return `BLOCKED` and stop. No downstream process is touched.**
3. The target server must be configured and Ready.
4. Only then is the call routed.

Additional hardening: skills with an empty whitelist deny **everything**; `fallback` skills provide context only; malformed actions (`missing colon`, empty skill) are rejected with standard `isError` results. Secrets stay in your config file; nothing is sent except framed JSON-RPC to the servers you configured.

## Performance (measured, not promised)

| Metric | Value |
| --- | --- |
| Resident tool count | **2** |
| Resident `tools/list` payload | **200 cl100k tokens** (802 chars, tiktoken-measured) |
| Skill hot reload (create) | **~68 ms** from file write to `discover_skills`-visible |
| Schema minification | **0.014 ms** / schema (release) |
| Intent match over 100 skills | **0.333 ms** (release) |
| Full discovery pipeline (match + 20 minified schemas) | **0.52 ms** (release) |
| Out-of-whitelist interception | **100%** (integration-tested: mock server log proves zero leakage) |
| Static binary | 3.4 MB, static-pie, zero shared libraries |

## Tests

```bash
cargo test                            # 35 unit + 9 black-box integration tests
cargo test --release --bin caby perf  # the ≤5 ms performance gate (strict in release)
cargo clippy --all-targets            # currently 0 warnings
```

Integration tests are true black-box: they spawn the real `caby serve` binary and speak MCP over stdio to mock downstream servers, covering ranking, minification, routing, **blocking**, lossless passthrough, hot reload, and error paths.

## Project layout

```text
src/
├── main.rs / cli.rs          # entry point + clap CLI
├── config.rs                 # config persistence (servers + settings)
├── util.rs                   # logging, paths, shell splitting, token estimate
├── core/
│   ├── jsonrpc.rs            # JSON-RPC 2.0 + stdio framing (newline + Content-Length)
│   ├── yaml_fm.rs            # front-matter parsing (zero YAML deps)
│   ├── matcher.rs            # TF-IDF intent matching (CJK bigrams)
│   ├── minifier.rs           # recursive schema pruning
│   ├── skillstore.rs         # directory scan + notify hot reload
│   ├── mcpserver.rs          # downstream MCP client (persistent pipes, serialized)
│   ├── registry.rs           # subprocess pool + tool index
│   ├── sandbox.rs            # whitelist enforcement
│   └── gateway.rs            # host-facing 2-tool gateway
├── commands/                 # serve / add / remove / list / skill / install
├── installers/               # claude-code / cursor / cline config writers
├── bin/mock-mcp.rs           # mock MCP servers for tests & demos
└── perf.rs                   # ≤5 ms performance gates
tests/                        # black-box integration suite
examples/skills/              # example skill packs
scripts/
├── build-static.sh           # musl static binary build
└── demo.sh                   # end-to-end demo transcript
```

## Roadmap

- [x] Core gateway (2 meta tools, discovery, dispatch, sandbox, minification)
- [x] Skill auto-discovery + hot reload, CLI, agent installers, static binary
- [ ] End-to-end verification against **real** servers (`github-mcp-server`, postgres-in-docker) — currently covered by mocks
- [ ] macOS / Windows support
- [ ] CI pipeline (tests + perf gates + artifacts) and release badges
- [ ] Skill pack registry / `caby skill search`
- [ ] `caby doctor` diagnostics subcommand
- [ ] Sampling / roots client capabilities (nice-to-have)

## Contributing

PRs welcome. Keep it small and testable:

```bash
cargo fmt && cargo clippy --all-targets && cargo test
# perf gates: cargo test --release --bin caby perf
```

Open an issue first for design changes. Be kind — the thread is the whole point of this project.

## FAQ

**Does Caby replace my per-tool MCP config?** Partially. Servers are still declared (in `config.json`, or just add them with `caby add`), but per-skill tool choices and SOPs live in skill files — which is exactly what removes the token and maintenance burden.

**Is my data sent anywhere?** No. Everything runs locally; there is no telemetry, no network except the servers you configured and the skill-pack fetches you trigger explicitly.

**What if a downstream server crashes?** Caby restarts it with exponential backoff (up to `restart_max`); in-flight calls fail cleanly with a `isError` result.

**Which protocol versions?** Supports `2025-06-18`, `2025-03-26`, `2024-11-05` (negotiated on `initialize`).

## License

MIT — see [LICENSE](LICENSE).