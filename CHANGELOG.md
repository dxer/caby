# Changelog

## v0.2.0 — shared daemon for multi-agent hosts

One downstream set, any number of agent clients.

- **Shared single-daemon mode**: every `caby serve` is a launcher. The first
  one binds `127.0.0.1:0`, claims `<config>.daemon.lock` (`O_EXCL`, `0600`,
  token) and hosts the daemon (spawning the downstream set exactly once);
  later ones attach as thin stdio↔TCP proxies.
- **Transparent failover**: dead daemons are detected by refused connections
  (no PID checks, no reaper, no `stop` command). The next client re-elects,
  takes over hosting, and replays unacknowledged requests — record-before-write
  in the proxy plus `take_pending` on takeover, so a handover never loses a call.
- **Lossless handover**: a single process-wide stdin pump (`spawn_stdin_pump`)
  — bridge and host session take turns draining it, so no orphaned stdin read
  can swallow a request mid-failover.
- **MCP hot-attach/detach**: `serve` reconciles live downstream servers against
  the config file every 250ms (add/remove/enable/disable/redefine, no restart),
  re-notifying `tools/list_changed` on change.
- `CABY_NO_DAEMON=1` forces classic single-process mode (the test harness
  default, keeping tests hermetic).
- Covered by `shared_daemon_serves_two_clients_and_survives_first_host_exit`:
  two clients share one downstream set; killing the first host re-elects the
  second with no lost calls (at-least-once: a request the dead daemon
  already ran may run twice).

## v0.1.0 — initial OSS release

- MCP meta-gateway: 2 meta tools (`discover_skills` / `call_action`), skill
  auto-discovery + hot reload, TF-IDF CJK-bigram matcher, schema minifier,
  sandbox whitelist, stdio subprocess pool.
- CLI: `serve` / `add` / `remove` / `list` / `skill new` / `skill install` /
  `install --target <claude-code|cursor|cline>`.
- CI (fmt + clippy + tests + perf gate) and 5-platform releases (linux musl
  x86_64/aarch64, windows msvc, macOS x86_64/aarch64).
