//! The `.timefork` store: journal, id state, tree layout, locking.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STORE_DIR: &str = ".timefork";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snap {
    pub id: u64,
    /// Unix epoch seconds.
    pub ts: u64,
    /// "manual" | "hook" | "session-start" | "pre-restore" | "baseline"
    pub origin: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    /// Top-level entries cloned (cheap proxy for "did anything exist").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<u64>,
    /// Milliseconds the snapshot took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    next_id: u64,
}

pub struct Store {
    /// Workspace root (the directory containing `.timefork`).
    pub root: PathBuf,
    /// The `.timefork` directory itself.
    pub dir: PathBuf,
}

impl Store {
    /// Walk up from `start` looking for a `.timefork` directory.
    pub fn discover_from(start: &Path) -> Result<Store> {
        let mut cur = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        loop {
            let candidate = cur.join(STORE_DIR);
            if candidate.is_dir() {
                return Ok(Store {
                    root: cur,
                    dir: candidate,
                });
            }
            if !cur.pop() {
                bail!(
                    "no {} store found here or in any parent directory (run `timefork init` in your workspace root)",
                    STORE_DIR
                );
            }
        }
    }

    pub fn discover() -> Result<Store> {
        Store::discover_from(&std::env::current_dir()?)
    }

    pub fn init(root: &Path) -> Result<Store> {
        let dir = root.join(STORE_DIR);
        if dir.exists() {
            bail!(
                "{} already exists — this workspace is already initialized",
                dir.display()
            );
        }
        fs::create_dir_all(dir.join("trees"))?;
        fs::create_dir_all(dir.join("tmp"))?;
        fs::create_dir_all(dir.join("trash"))?;
        fs::write(
            dir.join("state.json"),
            serde_json::to_string(&State { next_id: 1 })?,
        )?;
        fs::write(dir.join("journal.jsonl"), "")?;
        let store = Store {
            root: root.to_path_buf(),
            dir,
        };
        store.git_exclude_self();
        Ok(store)
    }

    /// Add `.timefork/` to `.git/info/exclude` so the store never shows up in
    /// git status (without touching the user's .gitignore).
    fn git_exclude_self(&self) {
        let info = self.root.join(".git/info");
        if !self.root.join(".git").is_dir() {
            return;
        }
        let exclude = info.join("exclude");
        let existing = fs::read_to_string(&exclude).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == ".timefork/") {
            return;
        }
        let _ = fs::create_dir_all(&info);
        if let Ok(mut f) = fs::File::options().create(true).append(true).open(&exclude) {
            let _ = writeln!(f, ".timefork/");
        }
    }

    pub fn trees_dir(&self) -> PathBuf {
        self.dir.join("trees")
    }

    pub fn tree(&self, id: u64) -> PathBuf {
        self.trees_dir().join(format!("{id:06}"))
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.dir.join("tmp")
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.dir.join("trash")
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Exclusive advisory lock for mutating operations. Held for the life of
    /// the returned file handle.
    pub fn lock(&self) -> Result<fs::File> {
        let f = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(anyhow!(std::io::Error::last_os_error()).context("acquiring store lock"));
        }
        Ok(f)
    }

    pub fn read_journal(&self) -> Result<Vec<Snap>> {
        let raw = fs::read_to_string(self.journal_path()).unwrap_or_default();
        let mut snaps = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Snap>(line) {
                Ok(s) => snaps.push(s),
                Err(e) => eprintln!("timefork: skipping corrupt journal line: {e}"),
            }
        }
        Ok(snaps)
    }

    pub fn append_journal(&self, snap: &Snap) -> Result<()> {
        let mut f = fs::File::options()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        writeln!(f, "{}", serde_json::to_string(snap)?)?;
        Ok(())
    }

    pub fn rewrite_journal(&self, snaps: &[Snap]) -> Result<()> {
        let tmp = self.dir.join("journal.jsonl.tmp");
        let mut out = String::new();
        for s in snaps {
            out.push_str(&serde_json::to_string(s)?);
            out.push('\n');
        }
        fs::write(&tmp, out)?;
        fs::rename(&tmp, self.journal_path())?;
        Ok(())
    }

    pub fn find_snap(&self, id: u64) -> Result<Snap> {
        self.read_journal()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow!("no checkpoint #{id} (see `timefork list`)"))
    }

    pub fn peek_next_id(&self) -> Result<u64> {
        let raw = fs::read_to_string(self.state_path()).context("reading state.json")?;
        let state: State = serde_json::from_str(&raw).context("parsing state.json")?;
        Ok(state.next_id)
    }

    pub fn bump_id(&self, claimed: u64) -> Result<()> {
        fs::write(
            self.state_path(),
            serde_json::to_string(&State {
                next_id: claimed + 1,
            })?,
        )?;
        Ok(())
    }

    /// Best-effort current git HEAD, without spawning git.
    pub fn git_head(&self) -> Option<String> {
        let head = fs::read_to_string(self.root.join(".git/HEAD")).ok()?;
        let head = head.trim();
        if let Some(reference) = head.strip_prefix("ref: ") {
            let branch = reference
                .rsplit('/')
                .next()
                .unwrap_or(reference)
                .to_string();
            let sha = fs::read_to_string(self.root.join(".git").join(reference))
                .ok()
                .map(|s| s.trim().chars().take(7).collect::<String>());
            match sha {
                Some(sha) if !sha.is_empty() => Some(format!("{branch}@{sha}")),
                _ => Some(branch),
            }
        } else {
            Some(head.chars().take(7).collect())
        }
    }
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn humanize_age(epoch: u64) -> String {
    let now = now_epoch();
    let delta = now.saturating_sub(epoch);
    match delta {
        0..=59 => format!("{delta}s"),
        60..=3599 => format!("{}m", delta / 60),
        3600..=86399 => format!("{}h", delta / 3600),
        _ => format!("{}d", delta / 86400),
    }
}
