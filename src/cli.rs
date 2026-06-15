//! Command-line surface (clap derive) and dispatch. See specs/cli.md.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{self, Config};
use crate::error::{DsyncError, Result};
use crate::sync::{self, SyncDirection, SyncOptions};

#[derive(Parser)]
#[command(
    name = "dsync",
    version,
    about = "A fast, rsync-inspired directory sync tool (local and remote-over-SSH)"
)]
pub struct Cli {
    /// Remote-agent mode (internal; spawned over SSH).
    #[arg(long, hide = true)]
    pub server: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize the current directory for syncing (seeds the `default` remote)
    Init {
        /// Remote path for the `default` remote. Local (/path) or SSH (user@host:/path).
        path: String,
    },
    /// Sync changes from this directory to a remote
    Push(SyncArgs),
    /// Sync changes from a remote to this directory
    Pull(SyncArgs),
    /// Manage sync remotes (add / remove / list)
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// Manage ignore patterns (add / update / remove)
    Ignore {
        #[command(subcommand)]
        action: IgnoreAction,
    },
}

#[derive(Args)]
pub struct SyncArgs {
    /// Named remote to sync with (defaults to `default`)
    pub remote: Option<String>,
    /// List what would change without transferring anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Worker threads (overrides config; 0 = num CPUs)
    #[arg(short = 'j', long)]
    pub threads: Option<usize>,
    /// Disable zstd compression for this run
    #[arg(long)]
    pub no_compress: bool,
    /// Force full-content hashing for change detection
    #[arg(long)]
    pub checksum: bool,
    /// Delete extraneous files on the receiving side (default)
    #[arg(long, default_value_t = true)]
    pub delete: bool,
    /// Keep extraneous files on the receiving side
    #[arg(long, overrides_with = "delete")]
    pub no_delete: bool,
    /// Suppress progress bars; print only the final summary
    #[arg(short, long)]
    pub quiet: bool,
    /// Per-file logging (repeat for more detail)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum RemoteAction {
    /// Add a named remote
    Add { name: String, path: String },
    /// Remove a named remote
    Remove { name: String },
    /// List configured remotes
    List,
}

#[derive(Subcommand)]
pub enum IgnoreAction {
    /// Add gitignore-syntax patterns
    Add { patterns: Vec<String> },
    /// Import patterns from gitignore-syntax files
    Update { files: Vec<PathBuf> },
    /// Interactively remove patterns
    Remove,
}

/// Dispatch a parsed command. Returns a `DsyncError` mapped to an exit code by `main`.
pub async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init { path } => config::init(&path),
        Command::Push(args) => sync_command(SyncDirection::Push, args).await,
        Command::Pull(args) => sync_command(SyncDirection::Pull, args).await,
        Command::Remote { action } => match action {
            RemoteAction::Add { name, path } => config::remote_add(&name, &path),
            RemoteAction::Remove { name } => config::remote_remove(&name),
            RemoteAction::List => config::remote_list(),
        },
        Command::Ignore { action } => match action {
            IgnoreAction::Add { patterns } => {
                if patterns.is_empty() {
                    return Err(DsyncError::Other("usage: dsync ignore add <patterns…>".into()));
                }
                crate::ignore::add(&patterns)
            }
            IgnoreAction::Update { files } => {
                if files.is_empty() {
                    return Err(DsyncError::Other("usage: dsync ignore update <files…>".into()));
                }
                crate::ignore::update(&files)
            }
            IgnoreAction::Remove => crate::ignore::remove_interactive(),
        },
    }
}

async fn sync_command(direction: SyncDirection, args: SyncArgs) -> Result<()> {
    let (cfg, root) = Config::load()?;
    let remote = cfg.select_remote(args.remote.as_deref())?;

    // CLI flags override config without mutating it.
    let compress = if args.no_compress { false } else { cfg.compression };
    let delete = !args.no_delete; // --no-delete wins
    let threads = resolve_threads(args.threads.unwrap_or(cfg.threads));

    let opts = SyncOptions {
        direction,
        dry_run: args.dry_run,
        threads,
        compress,
        compression_level: cfg.compression_level,
        checksum: args.checksum,
        delete,
        quiet: args.quiet,
    };

    let summary = sync::run(&cfg, &root, &remote, &opts).await?;
    if summary.dry_run {
        // plan already printed by the engine
    } else if summary.files_transferred == 0 && summary.files_deleted == 0 {
        if !opts.quiet {
            println!("Already in sync (0 changes)");
        }
    }
    Ok(())
}

/// Resolve the worker count: 0 → num_cpus; clamp to cores×4 with a warning.
fn resolve_threads(requested: usize) -> usize {
    let cores = num_cpus::get().max(1);
    if requested == 0 {
        return cores;
    }
    let max = cores * 4;
    if requested > max {
        tracing::warn!("threads {requested} exceeds cores×4 ({max}); clamping");
        max
    } else {
        requested
    }
}

/// Map an error to its documented process exit code (see specs/cli.md).
pub fn exit_code(err: &DsyncError) -> i32 {
    match err {
        DsyncError::NotInitialized => 3,
        DsyncError::Config(_) | DsyncError::AlreadyInitialized => 4,
        DsyncError::Ssh(_) => 5,
        DsyncError::Integrity(_) => 6,
        DsyncError::Io { .. } | DsyncError::Protocol(_) | DsyncError::Other(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_push_flags() {
        let cli = Cli::try_parse_from(["dsync", "push", "staging", "-n", "-j", "4", "--checksum"])
            .unwrap();
        match cli.command.unwrap() {
            Command::Push(a) => {
                assert_eq!(a.remote.as_deref(), Some("staging"));
                assert!(a.dry_run);
                assert_eq!(a.threads, Some(4));
                assert!(a.checksum);
            }
            _ => panic!("expected push"),
        }
    }

    #[test]
    fn no_delete_overrides_delete() {
        let cli = Cli::try_parse_from(["dsync", "pull", "--no-delete"]).unwrap();
        match cli.command.unwrap() {
            Command::Pull(a) => assert!(a.no_delete),
            _ => panic!(),
        }
    }

    #[test]
    fn unknown_flag_errors() {
        let res = Cli::try_parse_from(["dsync", "push", "--bogus"]);
        assert!(res.is_err());
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&DsyncError::NotInitialized), 3);
        assert_eq!(exit_code(&DsyncError::AlreadyInitialized), 4);
        assert_eq!(exit_code(&DsyncError::Config("x".into())), 4);
        assert_eq!(exit_code(&DsyncError::Ssh("x".into())), 5);
        assert_eq!(exit_code(&DsyncError::Integrity("x".into())), 6);
        assert_eq!(exit_code(&DsyncError::Other("x".into())), 1);
    }

    #[test]
    fn server_flag_hidden_but_parses() {
        let cli = Cli::try_parse_from(["dsync", "--server"]).unwrap();
        assert!(cli.server);
    }
}
