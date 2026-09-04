//! Shared-daemon mode: many agent clients, one downstream set.
//!
//! Every `caby serve` invocation is a launcher. It reads the lockfile next to
//! its config file and:
//!   - attaches as a thin stdio↔TCP proxy when a daemon is reachable;
//!   - otherwise binds an ephemeral loopback port, claims the lockfile with an
//!     atomic exclusive create, and hosts the daemon itself (spawning the
//!     downstream set exactly once), serving its own client in-process.
//!
//! A dead daemon is detected by refused connections — its listener dies with
//! it — so the next launcher transparently takes over: no orphans, no PID
//! checks, no reaper, no `stop` command. The proxy tracks in-flight request
//! ids and replays unacknowledged requests after failover, so a daemon
//! handover costs at most one retry, never a lost call.
//!
//! `CABY_NO_DAEMON=1` forces classic single-process mode (the test harness
//! sets this so tests stay isolated).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpListener, TcpStream,
};
use tokio::sync::{mpsc, Notify};

use crate::config::Settings;
use crate::core::gateway::Gateway;
use crate::core::jsonrpc::{self, FrameReader, Message, PARSE_ERROR};
use crate::core::registry::Registry;
use crate::core::skillstore::SkillStore;
use crate::util::{log_debug, log_info, log_warn};

/// Set `CABY_NO_DAEMON=1` (any value except empty/`0`/`false`) to disable
/// sharing and run everything in-process.
pub fn sharing_disabled() -> bool {
    matches!(
        std::env::var("CABY_NO_DAEMON").as_deref(),
        Ok(v) if !v.is_empty() && v != "0" && v.to_lowercase() != "false"
    )
}

/// One shared core per daemon: downstream pool + skill index + settings.
/// Sessions are cheap per-connection `Gateway`s over the same core, each with
/// its own response channel, so replies always route back to the right client.
pub struct Shared {
    pub registry: Arc<Registry>,
    pub store: Arc<StdMutex<SkillStore>>,
    pub settings: Arc<Settings>,
    pub sessions: Sessions,
    pub rescan_shutdown: Arc<Notify>,
}

/// Fan-out list for server-initiated notifications (`tools/list_changed`).
pub type Sessions = Arc<tokio::sync::Mutex<Vec<mpsc::Sender<Vec<u8>>>>>;

pub fn new_sessions() -> Sessions {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

pub async fn broadcast(sessions: &Sessions, msg: Message) {
    let bytes = jsonrpc::encode(&msg);
    let mut guard = sessions.lock().await;
    guard.retain(|s| {
        let _ = s.try_send(bytes.clone());
        !s.is_closed()
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonInfo {
    port: u16,
    pid: u32,
    token: String,
}

/// The lockfile lives next to the config file, so different configs (e.g. a
/// project's `--config`) get isolated daemons: `config.json` →
/// `config.daemon.lock`.
pub fn lock_path_for_config(cfg_path: &Path) -> PathBuf {
    cfg_path.with_extension("daemon.lock")
}

fn gen_token() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), t, n)
}

fn read_lock(path: &Path) -> Option<DaemonInfo> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Atomic claim: succeeds for exactly one racing launcher (O_EXCL create).
/// The file is mode 0600 on unix — the token inside is defense-in-depth.
fn write_lock_exclusive(path: &Path, info: &DaemonInfo) -> bool {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let raw = match serde_json::to_vec(info) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    use std::io::Write;
    match opts.open(path) {
        Ok(mut f) => f.write_all(&raw).is_ok(),
        Err(_) => false, // someone else holds it
    }
}

/// Best-effort cleanup: remove the lockfile only if it is still ours.
pub fn release_if_ours(lock_path: &Path) {
    if read_lock(lock_path)
        .map(|info| info.pid == std::process::id())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(lock_path);
    }
}

/// An attached daemon connection: read half (frame-buffered) + write half.
pub struct Attached {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
}

/// Connect to the daemon in `info`, say hello, expect `ok`.
async fn connect_hello(info: &DaemonInfo) -> Option<Attached> {
    let addr = format!("127.0.0.1:{}", info.port);
    let stream = tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(&addr))
        .await
        .ok()
        .and_then(|r| r.ok())?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let hello = format!("{{\"token\":\"{}\"}}\n", info.token);
    if wr.write_all(hello.as_bytes()).await.is_err() {
        return None;
    }
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .ok()?
        .ok()?;
    if n == 0 || line.trim() != "ok" {
        return None;
    }
    Some(Attached { rd, wr })
}

pub enum Lane {
    /// Attach to a live daemon as a proxy.
    Proxy(Attached),
    /// No daemon: host it in this process (and serve our own client too).
    HostDaemon(HostDaemon),
    /// Sharing unavailable: classic single-process mode.
    Direct,
}

pub struct HostDaemon {
    pub listener: TcpListener,
    pub lock_path: PathBuf,
    pub token: String,
}

/// Elect a lane: attach to a live daemon, win the hosting race, or fall back
/// to direct mode. The winner always binds its listener *before* writing the
/// lockfile, so any lockfile we can read implies a listening socket — a
/// refused connection therefore means a dead daemon (stale lock), never a
/// starting one.
pub async fn acquire(cfg_path: &Path) -> Lane {
    let lock = lock_path_for_config(cfg_path);

    // fast path: live daemon?
    if let Some(info) = read_lock(&lock) {
        if let Some(att) = connect_hello(&info).await {
            log_info!("attached to shared caby daemon (pid {})", info.pid);
            return Lane::Proxy(att);
        }
        let _ = std::fs::remove_file(&lock); // refused → stale
    }

    for _ in 0..4 {
        // try to host: bind first, then claim atomically
        if let Ok(listener) = TcpListener::bind("127.0.0.1:0").await {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            let token = gen_token();
            let info = DaemonInfo {
                port,
                pid: std::process::id(),
                token: token.clone(),
            };
            if write_lock_exclusive(&lock, &info) {
                log_info!("hosting shared caby daemon on 127.0.0.1:{port}");
                return Lane::HostDaemon(HostDaemon {
                    listener,
                    lock_path: lock,
                    token,
                });
            }
            drop(listener);
        }
        // lost the race → attach to the winner. It bound its listener before
        // writing the lock, so a refused connection means it died mid-handover
        // (stale lock) rather than a slow start — clear it and re-elect.
        // (No lockfile: the winner vanished → re-elect.)
        if let Some(info) = read_lock(&lock) {
            if let Some(att) = connect_hello(&info).await {
                log_info!("attached to shared caby daemon (pid {})", info.pid);
                return Lane::Proxy(att);
            }
            let _ = std::fs::remove_file(&lock);
        }
    }

    log_warn!("daemon election failed — running single-process");
    Lane::Direct
}

/// Accept loop for a hosted daemon: one session task per client connection.
/// Ends when the listener closes (daemon done).
pub async fn accept_loop(listener: TcpListener, core: Arc<Shared>, token: String) {
    while let Ok((stream, _)) = listener.accept().await {
        let core = Arc::clone(&core);
        let token = token.clone();
        tokio::spawn(async move {
            serve_session(stream, core, &token).await;
        });
    }
}

/// Serve one proxied client: hello-check, then frames ↔ per-session gateway.
/// Ends when the client disconnects; the session writer is pruned on exit.
pub async fn serve_session(stream: TcpStream, core: Arc<Shared>, token: &str) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    let mut line = String::new();
    let hello_ok = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|n| {
            n > 0
                && serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_string))
                    == Some(token.to_string())
        })
        .unwrap_or(false);
    if !hello_ok {
        return;
    }
    if wr.write_all(b"ok\n").await.is_err() {
        return;
    }

    let (gateway, mut rx) = Gateway::new(
        Arc::clone(&core.registry),
        Arc::clone(&core.store),
        Arc::clone(&core.settings),
    );
    core.sessions.lock().await.push(gateway.writer());

    // responses → socket
    let writer_done = Arc::new(Notify::new());
    let writer_done_rx = Arc::clone(&writer_done);
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        writer_done_rx.notify_one();
    });

    // socket → gateway
    let mut fr = FrameReader::new();
    let mut buf = [0u8; 65536];
    loop {
        tokio::select! {
            biased;
            _ = writer_done.notified() => break,
            res = rd.read(&mut buf) => match res {
                Ok(0) => break, // client disconnected
                Ok(n) => {
                    fr.push(&buf[..n]);
                    loop {
                        match fr.next_frame() {
                            Ok(Some(frame)) => match serde_json::from_slice::<Message>(&frame) {
                                Ok(msg) => gateway.handle_message(msg).await,
                                Err(e) => {
                                    log_debug!("dropping unparseable frame from proxy: {e}");
                                    let _ = gateway
                                        .writer()
                                        .send(jsonrpc::encode(&jsonrpc::err(
                                            crate::core::jsonrpc::Id::Null,
                                            PARSE_ERROR,
                                            format!("parse error: {e}"),
                                        )))
                                        .await;
                                }
                            },
                            Ok(None) => break,
                            Err(_) => {
                                fr = FrameReader::new();
                                break;
                            }
                        }
                    }
                }
                Err(_) => break,
            },
        }
    }

    writer_task.abort();
    // prune our (now closed) writer so broadcasts don't accumulate
    let mut sessions = core.sessions.lock().await;
    sessions.retain(|s| !s.is_closed());
    log_debug!("proxy session ended ({} remaining)", sessions.len());
}

/// Unacknowledged requests, resent after failover so a daemon handover never
/// loses a call. Keyed by serialized id; tiny in practice.
#[derive(Default)]
pub struct Replay {
    pending: BTreeMap<String, Vec<u8>>,
}

fn id_key(id: &crate::core::jsonrpc::Id) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

/// One stdin line, or client exit. Produced by the single process-wide pump.
pub enum StdioLine {
    Line(Vec<u8>),
    Eof,
}

/// The only stdin reader in the process. `tokio::io::stdin()` reads run on
/// the blocking pool: dropping a bridge/host loop while its read is
/// outstanding orphans that read, and the orphan can win the race for the
/// next client bytes and swallow them (failover loses a call). One pump for
/// the process lifetime makes handover lossless: lines the old loop never
/// forwarded wait in the channel for the next loop.
pub fn spawn_stdin_pump() -> mpsc::UnboundedReceiver<StdioLine> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut fr = FrameReader::new();
        let mut buf = [0u8; 65536];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => {
                    let _ = tx.send(StdioLine::Eof);
                    break;
                }
                Ok(n) => {
                    fr.push(&buf[..n]);
                    let mut alive = true;
                    loop {
                        match fr.next_frame() {
                            Ok(Some(frame)) => {
                                if tx.send(StdioLine::Line(frame)).is_err() {
                                    alive = false;
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => {
                                fr = FrameReader::new();
                                break;
                            }
                        }
                    }
                    if !alive {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(StdioLine::Eof);
                    break;
                }
            }
        }
    });
    rx
}

/// How a proxy bridge ended: client gone (quit) or daemon gone (re-elect).
pub enum BridgeEnd {
    Eof,
    DaemonDead,
}

/// Bridge our stdin lines to an attached daemon, frame-aware. Lines arrive
/// from the process-wide pump; socket frames go to stdout with acknowledged
/// ids cleared. Unacknowledged requests are replayed on the next bridge —
/// or, after a takeover, fed to the freshly hosted session.
pub async fn bridge(
    att: Attached,
    replay: &Arc<StdMutex<Replay>>,
    stdin_rx: &mut mpsc::UnboundedReceiver<StdioLine>,
) -> BridgeEnd {
    let Attached { rd, wr } = att;
    let wr = Arc::new(tokio::sync::Mutex::new(wr));

    // First: replay anything the previous daemon never answered.
    {
        let pending: Vec<Vec<u8>> = replay
            .lock()
            .expect("replay lock")
            .pending
            .values()
            .cloned()
            .collect();
        let mut w = wr.lock().await;
        for raw in pending {
            let mut framed = raw.clone();
            framed.push(b'\n');
            if w.write_all(&framed).await.is_err() {
                return BridgeEnd::DaemonDead;
            }
        }
    }

    let sock_dead = Arc::new(Notify::new());
    let sock_dead_rx = Arc::clone(&sock_dead);
    let replay_down = Arc::clone(replay);
    let down_task = tokio::spawn(async move {
        let mut rd = rd;
        let mut fr = FrameReader::new();
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 65536];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    fr.push(&buf[..n]);
                    let mut clean = true;
                    loop {
                        match fr.next_frame() {
                            Ok(Some(frame)) => {
                                ack_frame(&mut replay_down.lock().expect("replay lock"), &frame);
                                if stdout.write_all(&frame).await.is_err()
                                    || stdout.write_all(b"\n").await.is_err()
                                    || stdout.flush().await.is_err()
                                {
                                    clean = false;
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => {
                                fr = FrameReader::new();
                                break;
                            }
                        }
                    }
                    if !clean {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        sock_dead_rx.notify_one();
    });

    // pump lines → socket, tracking unacked requests. The pump outlives
    // every bridge, so a handover strands no read and loses no line.
    let end = loop {
        tokio::select! {
            biased;
            _ = sock_dead.notified() => break BridgeEnd::DaemonDead,
            msg = stdin_rx.recv() => match msg {
                Some(StdioLine::Line(frame)) => {
                    // Record BEFORE writing: a dead socket keeps the request
                    // pending so the next bridge (or hosted session) replays
                    // it. (Acks remove it once a daemon answers.)
                    if let Ok(Message::Request(r)) =
                        serde_json::from_slice::<Message>(&frame)
                    {
                        replay
                            .lock()
                            .expect("replay lock")
                            .pending
                            .insert(id_key(&r.id), frame.clone());
                    }
                    let mut w = wr.lock().await;
                    let mut framed = frame.clone();
                    framed.push(b'\n');
                    if w.write_all(&framed).await.is_err() {
                        break BridgeEnd::DaemonDead;
                    }
                }
                Some(StdioLine::Eof) | None => break BridgeEnd::Eof,
            },
        }
    };

    down_task.abort();
    end
}

/// Drain unacknowledged requests. Used when this launcher takes over hosting
/// after a failover: the old proxy loop is gone, so nothing else will replay
/// them — feed them to the freshly hosted session instead.
pub fn take_pending(replay: &Arc<StdMutex<Replay>>) -> Vec<Vec<u8>> {
    std::mem::take(&mut replay.lock().expect("replay lock").pending)
        .into_values()
        .collect()
}

/// Remove acked ids from the replay set. Called with each daemon→client frame.
/// (Split out so the socket task stays small; takes the frames it forwarded.)
pub fn ack_frame(replay: &mut Replay, frame: &[u8]) {
    if let Ok(Message::Success(s)) = serde_json::from_slice::<Message>(frame) {
        replay.pending.remove(&id_key(&s.id));
    } else if let Ok(Message::RpcError(e)) = serde_json::from_slice::<Message>(frame) {
        replay.pending.remove(&id_key(&e.id));
    }
}
