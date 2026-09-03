//! Shared utilities: leveled stderr logger, path resolution, shell splitting,
//! and an approximate token estimator (for the 150-200 token baseline budget).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

// ---------------------------------------------------------------------------
// Logger — always to stderr so stdout stays a pure JSON-RPC channel.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl LogLevel {
    pub fn parse(s: &str) -> anyhow::Result<LogLevel> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => Err(anyhow::anyhow!(
                "invalid log level '{other}' (expected error|warn|info|debug|trace)"
            )),
        }
    }
}

static LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

pub fn set_log_level(l: LogLevel) {
    LOG_LEVEL.store(l as u8, Ordering::Relaxed);
}

pub fn log_level() -> LogLevel {
    match LOG_LEVEL.load(Ordering::Relaxed) {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        2 => LogLevel::Info,
        3 => LogLevel::Debug,
        _ => LogLevel::Trace,
    }
}

pub fn ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:09.3}", now.as_millis() as f64 / 1000.0)
}

/// Export the logging macros at the crate root so every module can use them.
#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $lv:ident, $($arg:tt)*) => {{
        if $crate::util::log_level() >= $lvl {
            eprintln!(
                "{} [{}] caby: {}",
                $crate::util::ts(),
                stringify!($lv).to_lowercase(),
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! log_error { ($($arg:tt)*) => { $crate::log_at!($crate::util::LogLevel::Error, error, $($arg)*) } }
#[macro_export]
macro_rules! log_warn  { ($($arg:tt)*) => { $crate::log_at!($crate::util::LogLevel::Warn, warn, $($arg)*) } }
#[macro_export]
macro_rules! log_info  { ($($arg:tt)*) => { $crate::log_at!($crate::util::LogLevel::Info, info, $($arg)*) } }
#[macro_export]
macro_rules! log_debug { ($($arg:tt)*) => { $crate::log_at!($crate::util::LogLevel::Debug, debug, $($arg)*) } }
#[macro_export]
macro_rules! log_trace { ($($arg:tt)*) => { $crate::log_at!($crate::util::LogLevel::Trace, trace, $($arg)*) } }

/// Re-export through `util` so `use crate::util::{log_debug, ...}` paths keep working.
pub use {log_debug, log_error, log_info, log_warn};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `$XDG_CONFIG_HOME/caby` or `~/.config/caby`.
pub fn caby_config_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("caby");
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("caby")
}

#[allow(dead_code)]
/// Default config file path.
pub fn default_config_path() -> PathBuf {
    caby_config_home().join("config.json")
}

/// Global skills directory: `~/.config/caby/skills`.
pub fn global_skills_dir() -> PathBuf {
    caby_config_home().join("skills")
}

/// Project skills directory: `<cwd>/.caby/skills`.
pub fn project_skills_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".caby")
        .join("skills")
}

// ---------------------------------------------------------------------------
// Shell-style tokenizer for `--command "docker run -i --rm ..."` strings
// ---------------------------------------------------------------------------

/// Split a command string the way a POSIX shell would for simple cases:
/// whitespace-separated tokens, single/double quotes, backslash escapes.
///
/// Backslash only escapes what a shell would escape (whitespace, quotes,
/// another backslash); everywhere else it is kept literally so Windows paths
/// like `C:\Users\runner\mock.exe` survive unmangled — this matters for
/// `caby add --command` and for server commands stored in config.json.
pub fn shell_split(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars();
    let mut in_tok = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_tok = true;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        break;
                    }
                    cur.push(c2);
                }
            }
            '"' => {
                in_tok = true;
                while let Some(c2) = chars.next() {
                    match c2 {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(n) if n == '"' || n == '\\' => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => cur.push('\\'),
                        },
                        other => cur.push(other),
                    }
                }
            }
            '\\' => {
                in_tok = true;
                match chars.next() {
                    // escapes whitespace, quotes or another backslash
                    Some(n) if n.is_whitespace() || n == '\'' || n == '"' || n == '\\' => {
                        cur.push(n);
                    }
                    // anything else: keep the backslash literally (Windows paths)
                    Some(n) => {
                        cur.push('\\');
                        cur.push(n);
                    }
                    None => cur.push('\\'),
                }
            }
            c if c.is_whitespace() => {
                if in_tok {
                    out.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            c => {
                in_tok = true;
                cur.push(c);
            }
        }
    }
    if in_tok {
        out.push(cur);
    }
    out
}

/// Build a full argv for a server definition. If `command` is a single token
/// it is the binary; otherwise it is shell-split into binary + args.
pub fn build_argv(command: &str, extra_args: &[String]) -> Vec<String> {
    let mut tokens = shell_split(command);
    if tokens.is_empty() {
        tokens.push(command.to_string());
    }
    tokens.extend(extra_args.iter().cloned());
    tokens
}

// ---------------------------------------------------------------------------
// Approximate token counter (used for baseline-budget assertions)
// ---------------------------------------------------------------------------
//
// This is a transparent, dependency-free estimator that behaves like a BPE
// tokenizer for English-opaque text: words and punctuation are split, whitespace
// is dropped, and CJK characters count individually. It overestimates slightly
// for real tokenizers, so passing it is a safe conservative bound.

/// Count approximate tokens in a string. A conservative upper-bound estimate
/// (tends to overcount vs a real BPE tokenizer like cl100k by ~1.3x) — used
/// only for budget assertions and startup logging.
pub fn approx_tokens(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut count = 0usize;
    let mut word = String::new();
    let mut i = 0usize;
    while i < s.len() {
        let ch = s[i..].chars().next().unwrap();
        let width = ch.len_utf8();
        if ch.is_whitespace() {
            flush_word(&mut word, &mut count);
        } else if is_cjk(ch) {
            flush_word(&mut word, &mut count);
            count += 1; // one CJK char ≈ one token
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            word.push(ch);
        } else {
            flush_word(&mut word, &mut count);
            // a run of punctuation typically encodes as a couple of tokens;
            // count each run once rather than per-character
            let mut j = i;
            while j < bytes.len() {
                let cj = s[j..].chars().next().unwrap();
                if cj.is_ascii_alphanumeric()
                    || cj == '_'
                    || cj == '-'
                    || cj.is_whitespace()
                    || is_cjk(cj)
                {
                    break;
                }
                j += cj.len_utf8();
            }
            if j > i {
                count += (j - i).div_ceil(3); // ~1 token per 3 punct chars
                i = j;
                continue;
            }
        }
        i += width;
    }
    flush_word(&mut word, &mut count);
    count
}

pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
        | 0x31F0..=0x31FF | 0x31C0..=0x31EF | 0xF900..=0xFAFF)
}

fn flush_word(word: &mut String, count: &mut usize) {
    if !word.is_empty() {
        // rough byte-length based split: ~4 chars per token
        let chars: usize = word.chars().count();
        *count += chars.div_ceil(4);
        word.clear();
    }
}

/// Format a byte size for the `caby list` output.
#[allow(dead_code)]
pub fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Path display: collapse `$HOME` and `$XDG_CONFIG_HOME` prefixes for nicer output.
pub fn display_path(p: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(rel) = p.strip_prefix(&home) {
            return format!("~/{rel}", rel = rel.display());
        }
    }
    p.display().to_string()
}

pub fn ensure_dir(p: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(p)
        .map_err(|e| anyhow::anyhow!("failed to create directory {}: {e}", p.display()))
}
