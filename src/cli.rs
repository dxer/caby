//! CLI entry: command-line surface for Caby.
//!
//!   caby serve            run the MCP meta-gateway over stdio
//!   caby add <name>       attach a downstream MCP server (executable / docker)
//!   caby remove <name>    detach a downstream server
//!   caby list             show servers + skills status tree
//!   caby skill new <n>    scaffold a skill markdown template
//!   caby skill install    fetch a skill pack from github / URL
//!   caby install          register caby into an agent client config
//!   caby version          print version

use clap::{Parser, Subcommand};

// NOTE: modules (commands, config, core, installers, util) are declared at the
// crate root in main.rs — they are NOT re-declared here.

#[derive(Parser)]
#[command(
    name = "caby",
    version,
    about = "Keep your agent calm & context lean — MCP meta-gateway with auto-discovered skills",
    long_about = "Caby is a lightweight meta-gateway and dynamic dispatcher for the MCP ecosystem.\n\nIt sits between AI coding clients (Claude Code, Cursor, Cline...) and the real MCP servers,\nexposing only 2 meta tools (~150 tokens) while automatically discovering skill packs from\nproject and global directories."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the MCP gateway over stdio (what agent clients connect to)
    Serve(ServeArgs),
    /// Add a downstream MCP server (executable or docker command)
    Add(AddArgs),
    /// Remove a downstream MCP server
    Remove(RemoveArgs),
    /// Show the current load state: servers + skills
    List(ListArgs),
    /// Skill pack management (scaffold / install)
    Skill(SkillArgs),
    /// Register `caby serve` into an agent client's config
    Install(InstallArgs),
    /// Print version
    Version,
}

#[derive(clap::Args, Debug)]
pub struct ServeArgs {
    /// Path to the config file (default: ~/.config/caby/config.json)
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
    /// Log level: error|warn|info|debug|trace
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Never auto-restart crashed downstream servers
    #[arg(long)]
    pub no_restart: bool,
    /// Per downstream tool call timeout (seconds)
    #[arg(long, value_name = "SECS")]
    pub timeout_secs: Option<u64>,
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Unique server name, e.g. github
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Executable or full command line, e.g. "github-mcp-server" or
    /// "docker run -i --rm mcp/postgres postgresql://localhost/db"
    #[arg(long, value_name = "CMD", required = true)]
    pub command: String,
    /// Extra argv appended to the command
    #[arg(long = "args", value_name = "ARG", num_args = 1..)]
    pub extra_args: Vec<String>,
    /// Environment overrides, KEY=VALUE
    #[arg(long = "env", value_name = "K=V", num_args = 1..)]
    pub env: Vec<String>,
    /// Working directory for the server process
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<std::path::PathBuf>,
    /// Skip the connectivity (initialize handshake) check
    #[arg(long)]
    pub no_verify: bool,
    /// Config file path
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Skip live probing of downstream servers (show config only)
    #[arg(long)]
    pub offline: bool,
    /// Machine-readable JSON output
    #[arg(long)]
    pub json: bool,
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub cmd: SkillCmd,
}

#[derive(Subcommand, Debug)]
pub enum SkillCmd {
    /// Scaffold a skill markdown template
    New(SkillNewArgs),
    /// Install a skill pack: `github:user/repo[/path]` or https URL
    Install(SkillInstallArgs),
}

#[derive(clap::Args, Debug)]
pub struct SkillNewArgs {
    /// Skill name (used as file stem), e.g. deploy-pipeline
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Where to create the skill: project (.caby/skills) | global (~/.config/caby/skills)
    #[arg(long, value_enum, default_value_t = SkillDir::Project)]
    pub dir: SkillDir,
}

#[derive(clap::Args, Debug)]
pub struct SkillInstallArgs {
    /// Skill pack spec
    #[arg(value_name = "SPEC")]
    pub spec: String,
    /// Non-interactive: never prompt (missing servers reported instead)
    #[arg(long)]
    pub yes: bool,
    /// Where to install: project | global
    #[arg(long, value_enum, default_value_t = SkillDir::Project)]
    pub dir: SkillDir,
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum SkillDir {
    Project,
    Global,
}

#[derive(clap::Args, Debug)]
pub struct InstallArgs {
    /// Agent client to configure
    #[arg(long, value_enum)]
    pub target: AgentTarget,
    /// Use the project-level config instead of the user-global one
    #[arg(long)]
    pub project: bool,
    /// Non-interactive: create missing config files without asking
    #[arg(long)]
    pub yes: bool,
    /// Override the command recorded in the client config
    /// (default: absolute path of this caby binary)
    #[arg(long, value_name = "CMD")]
    pub command: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum AgentTarget {
    ClaudeCode,
    Cursor,
    Cline,
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Command::Serve(args) => crate::commands::serve::run(&args),
        Command::Add(args) => crate::commands::add::run(&args),
        Command::Remove(args) => crate::commands::remove::run(&args),
        Command::List(args) => crate::commands::list::run(&args),
        Command::Skill(args) => crate::commands::skill::run(&args),
        Command::Install(args) => crate::commands::installer::run(&args),
        Command::Version => {
            println!("caby {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}