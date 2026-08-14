mod clone;
mod hook;
mod ops;
mod store;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use ops::{SnapMeta, TreeRef};
use store::Store;

#[derive(Parser)]
#[command(
    name = "timefork",
    version,
    about = "Copy-on-write time machine for coding agent workspaces.\nCheckpoint every tool call, rewind, diff, and fork — for free, via APFS/reflink clones.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a .timefork store in the current directory and take a baseline checkpoint
    Init,
    /// Take a checkpoint of the live workspace
    Snap {
        /// Label for this checkpoint
        #[arg(short, long, default_value = "manual checkpoint")]
        message: String,
    },
    /// List checkpoints, newest first
    #[command(alias = "ls")]
    List {
        /// Max rows to show
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Show one checkpoint in detail (including a diffstat vs the previous one)
    Show { id: u64 },
    /// Rewind the workspace to a checkpoint (takes an undo checkpoint first)
    Restore {
        id: u64,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// File-level diff between two checkpoints (or 'live')
    Diff {
        /// Base: checkpoint id or 'live'
        a: String,
        /// Target: checkpoint id or 'live' (default: live)
        #[arg(default_value = "live")]
        b: String,
    },
    /// Print the on-disk path of a checkpoint tree (composes with git diff --no-index)
    Path {
        /// Checkpoint id or 'live'
        id: String,
    },
    /// Clone a checkpoint (or the live workspace) into independent sibling directories
    Fork {
        /// Source: checkpoint id or 'live'
        #[arg(default_value = "live")]
        source: String,
        /// How many forks to create
        #[arg(short = 'n', long, default_value_t = 1)]
        count: u32,
        /// Destination directory (default: sibling of the workspace)
        #[arg(short, long)]
        dest: Option<std::path::PathBuf>,
    },
    /// Delete old checkpoint trees and clear restore leftovers
    Prune {
        /// Keep only the newest N checkpoints
        #[arg(short, long)]
        keep: Option<usize>,
        /// Drop checkpoints older than this many hours
        #[arg(long, value_name = "HOURS")]
        older_than: Option<u64>,
    },
    /// Store overview
    Status,
    /// Claude Code hook entrypoint (reads hook JSON on stdin; always exits 0)
    Hook,
    /// Register timefork as a Claude Code hook in this workspace's .claude/settings.json
    InstallHooks,
}

fn main() {
    // Die quietly on SIGPIPE (e.g. `timefork list | head`) like a normal
    // unix tool instead of panicking.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    if let Command::Hook = cli.command {
        hook::run_hook();
        return;
    }
    if let Err(e) = run(cli.command) {
        eprintln!("timefork: {e:#}");
        std::process::exit(1);
    }
}

fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Init => {
            let cwd = std::env::current_dir()?;
            let store = Store::init(&cwd)?;
            let _lock = store.lock()?;
            let snap = ops::snap(
                &store,
                SnapMeta {
                    origin: "baseline".into(),
                    label: "baseline (timefork init)".into(),
                    tool: None,
                    session: None,
                },
            )?;
            println!(
                "initialized {} — baseline checkpoint #{} ({} top-level entries, {}ms)",
                store.dir.display(),
                snap.id,
                snap.entries.unwrap_or(0),
                snap.took_ms.unwrap_or(0)
            );
            println!("next: `timefork install-hooks` to checkpoint every Claude Code tool call");
            Ok(())
        }
        Command::Snap { message } => {
            let store = Store::discover()?;
            let _lock = store.lock()?;
            let snap = ops::snap(
                &store,
                SnapMeta {
                    origin: "manual".into(),
                    label: message,
                    tool: None,
                    session: None,
                },
            )?;
            println!("checkpoint #{} ({}ms)", snap.id, snap.took_ms.unwrap_or(0));
            Ok(())
        }
        Command::List { limit } => ops::print_list(&Store::discover()?, limit),
        Command::Show { id } => ops::print_show(&Store::discover()?, id),
        Command::Restore { id, yes } => {
            let store = Store::discover()?;
            let target = store.find_snap(id)?;
            if !yes {
                eprintln!(
                    "restore workspace to #{id} ({} ago: {})? [y/N]",
                    store::humanize_age(target.ts),
                    target.label
                );
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    bail!("aborted");
                }
            }
            let _lock = store.lock()?;
            let (pre, _) = ops::restore(&store, id)?;
            println!(
                "restored to #{id}; previous live state saved as #{}",
                pre.id
            );
            println!("undo with: timefork restore {} --yes", pre.id);
            Ok(())
        }
        Command::Diff { a, b } => {
            let store = Store::discover()?;
            ops::print_diff(&store, &TreeRef::parse(&a)?, &TreeRef::parse(&b)?)
        }
        Command::Path { id } => {
            let store = Store::discover()?;
            let (p, _) = TreeRef::parse(&id)?.resolve(&store)?;
            println!("{}", p.display());
            Ok(())
        }
        Command::Fork {
            source,
            count,
            dest,
        } => {
            let store = Store::discover()?;
            let src = TreeRef::parse(&source)?;
            // Forking live state is only consistent if no snap/restore is mid-flight.
            let _lock = store.lock()?;
            let created = ops::fork(&store, &src, count, dest.as_deref())?;
            for p in &created {
                println!("forked → {}", p.display());
            }
            if let Some(first) = created.first() {
                println!(
                    "\nrun an agent in a fork:  cd {} && claude",
                    first.display()
                );
            }
            Ok(())
        }
        Command::Prune { keep, older_than } => {
            if keep.is_none() && older_than.is_none() {
                bail!("nothing to do: pass --keep N and/or --older-than HOURS");
            }
            let store = Store::discover()?;
            let _lock = store.lock()?;
            let report = ops::prune(&store, keep, older_than.map(|h| h * 3600))?;
            println!(
                "pruned {} checkpoint(s), cleared {} trash/tmp item(s)",
                report.removed.len(),
                report.trash_cleared
            );
            Ok(())
        }
        Command::Status => ops::print_status(&Store::discover()?),
        Command::InstallHooks => {
            let store = Store::discover()?;
            hook::install_hooks(&store)
        }
        Command::Hook => unreachable!("handled in main"),
    }
}
