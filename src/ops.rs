//! High-level operations. Callers are expected to hold the store lock for
//! anything mutating (snap/restore/prune); helpers here don't lock.

use crate::clone::{clone_tree, CloneStats};
use crate::store::{humanize_age, now_epoch, Snap, Store, STORE_DIR};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct SnapMeta {
    pub origin: String,
    pub label: String,
    pub tool: Option<String>,
    pub session: Option<String>,
}

/// Take a checkpoint of the live workspace. Caller holds the lock.
pub fn snap(store: &Store, meta: SnapMeta) -> Result<Snap> {
    let started = Instant::now();
    let id = store.peek_next_id()?;
    let staging = store
        .tmp_dir()
        .join(format!("snap-{id}-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    let stats =
        clone_tree(&store.root, &staging, &[OsStr::new(STORE_DIR)]).context("cloning workspace")?;
    let final_path = store.tree(id);
    fs::rename(&staging, &final_path).context("finalizing checkpoint")?;
    let snap = Snap {
        id,
        ts: now_epoch(),
        origin: meta.origin,
        label: meta.label,
        tool: meta.tool,
        session: meta.session,
        git_head: store.git_head(),
        entries: Some(stats.entries),
        took_ms: Some(started.elapsed().as_millis() as u64),
    };
    store.append_journal(&snap)?;
    store.bump_id(id)?;
    report_clone_stats(&stats);
    Ok(snap)
}

fn report_clone_stats(stats: &CloneStats) {
    if !stats.skipped.is_empty() {
        eprintln!(
            "timefork: skipped {} uncloneable item(s) (sockets/fifos), e.g. {}",
            stats.skipped.len(),
            stats.skipped[0].display()
        );
    }
}

/// Rewind the workspace to checkpoint `id`. Takes a pre-restore checkpoint
/// first so the operation is itself undoable, then swaps top-level entries
/// via renames (old state goes to the trash, reclaimed by `prune`).
pub fn restore(store: &Store, id: u64) -> Result<(Snap, Snap)> {
    let target_snap = store.find_snap(id)?;
    let tree = store.tree(id);
    if !tree.is_dir() {
        bail!("checkpoint #{id} has been pruned — its tree is gone");
    }

    let pre = snap(
        store,
        SnapMeta {
            origin: "pre-restore".into(),
            label: format!("live state before restore to #{id}"),
            tool: None,
            session: None,
        },
    )?;

    // Stage a clone of the target tree inside the store (same volume, so the
    // final swap is pure renames).
    let staging = store
        .tmp_dir()
        .join(format!("restore-{id}-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).ok();
    }
    clone_tree(&tree, &staging, &[]).context("staging checkpoint tree")?;

    let trash = store
        .trash_dir()
        .join(format!("{}-pre{}", now_epoch(), pre.id));
    fs::create_dir_all(&trash)?;

    // Move live entries out…
    for entry in fs::read_dir(&store.root)? {
        let entry = entry?;
        if entry.file_name() == STORE_DIR {
            continue;
        }
        fs::rename(entry.path(), trash.join(entry.file_name()))
            .with_context(|| format!("moving {} to trash", entry.path().display()))?;
    }
    // …and the checkpoint's entries in.
    for entry in fs::read_dir(&staging)? {
        let entry = entry?;
        fs::rename(entry.path(), store.root.join(entry.file_name()))
            .with_context(|| format!("restoring {}", entry.file_name().to_string_lossy()))?;
    }
    fs::remove_dir(&staging).ok();

    Ok((pre, target_snap))
}

/// A tree reference on the command line: a checkpoint id or "live".
pub enum TreeRef {
    Live,
    Id(u64),
}

impl TreeRef {
    pub fn parse(s: &str) -> Result<TreeRef> {
        if s.eq_ignore_ascii_case("live") {
            return Ok(TreeRef::Live);
        }
        s.trim_start_matches('#')
            .parse::<u64>()
            .map(TreeRef::Id)
            .map_err(|_| anyhow!("expected a checkpoint id or 'live', got '{s}'"))
    }

    pub fn resolve(&self, store: &Store) -> Result<(PathBuf, String)> {
        match self {
            TreeRef::Live => Ok((store.root.clone(), "live".into())),
            TreeRef::Id(id) => {
                let tree = store.tree(*id);
                if !tree.is_dir() {
                    bail!("checkpoint #{id} not found (pruned or never existed)");
                }
                Ok((tree, format!("#{id}")))
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

struct Meta {
    kind: Kind,
    len: u64,
    mtime_ns: i128,
}

fn walk_into(
    base: &Path,
    rel: &Path,
    skip_root_store: bool,
    out: &mut BTreeMap<PathBuf, Meta>,
) -> Result<()> {
    let abs = base.join(rel);
    for entry in fs::read_dir(&abs).with_context(|| format!("reading {}", abs.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_root_store && rel.as_os_str().is_empty() && name == STORE_DIR {
            continue;
        }
        let rel_child = rel.join(&name);
        let md = entry
            .metadata()
            .or_else(|_| fs::symlink_metadata(entry.path()))?;
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(-1);
        if kind == Kind::Dir {
            walk_into(base, &rel_child, skip_root_store, out)?;
            out.insert(
                rel_child,
                Meta {
                    kind,
                    len: 0,
                    mtime_ns: -1,
                },
            );
        } else {
            out.insert(
                rel_child,
                Meta {
                    kind,
                    len: md.len(),
                    mtime_ns,
                },
            );
        }
    }
    Ok(())
}

pub struct DiffResult {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub modified: Vec<PathBuf>,
}

/// File-level diff between two trees, going from `a` to `b`. Unchanged files
/// are detected by (type, size, mtime) — CoW clones preserve all three.
pub fn diff_trees(a: &Path, a_is_live: bool, b: &Path, b_is_live: bool) -> Result<DiffResult> {
    let mut ma = BTreeMap::new();
    let mut mb = BTreeMap::new();
    walk_into(a, Path::new(""), a_is_live, &mut ma)?;
    walk_into(b, Path::new(""), b_is_live, &mut mb)?;

    let mut result = DiffResult {
        added: vec![],
        removed: vec![],
        modified: vec![],
    };
    for (path, meta_b) in &mb {
        match ma.get(path) {
            None => result.added.push(path.clone()),
            Some(meta_a) => {
                if meta_a.kind == Kind::Dir && meta_b.kind == Kind::Dir {
                    continue;
                }
                if meta_a.kind != meta_b.kind
                    || meta_a.len != meta_b.len
                    || meta_a.mtime_ns != meta_b.mtime_ns
                {
                    result.modified.push(path.clone());
                }
            }
        }
    }
    for path in ma.keys() {
        if !mb.contains_key(path) {
            result.removed.push(path.clone());
        }
    }
    Ok(result)
}

/// Clone a checkpoint (or the live workspace) into an independent sibling
/// directory. The fork gets no `.timefork` store of its own.
pub fn fork(
    store: &Store,
    source: &TreeRef,
    count: u32,
    dest: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let (src, name) = source.resolve(store)?;
    let live = matches!(source, TreeRef::Live);
    let repo_name = store
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".into());
    let parent = store
        .root
        .parent()
        .ok_or_else(|| anyhow!("workspace has no parent directory to fork into"))?
        .to_path_buf();

    let mut created = Vec::new();
    for k in 1..=count {
        let dest_k: PathBuf = match (dest, count) {
            (Some(d), 1) => d.to_path_buf(),
            (Some(d), _) => d.with_file_name(format!(
                "{}-{k}",
                d.file_name().unwrap_or_default().to_string_lossy()
            )),
            (None, _) => {
                let suffix = name.trim_start_matches('#');
                let base = if count > 1 {
                    format!("{repo_name}-fork-{suffix}-{k}")
                } else {
                    format!("{repo_name}-fork-{suffix}")
                };
                parent.join(base)
            }
        };
        if dest_k.exists() {
            bail!(
                "{} already exists — refusing to overwrite",
                dest_k.display()
            );
        }
        let exclude: &[&OsStr] = if live { &[OsStr::new(STORE_DIR)] } else { &[] };
        clone_tree(&src, &dest_k, exclude)
            .with_context(|| format!("forking into {}", dest_k.display()))?;
        created.push(dest_k);
    }
    Ok(created)
}

pub struct PruneReport {
    pub removed: Vec<u64>,
    pub trash_cleared: usize,
}

/// Delete old checkpoint trees and clear trash/tmp. Caller holds the lock.
pub fn prune(
    store: &Store,
    keep: Option<usize>,
    older_than_secs: Option<u64>,
) -> Result<PruneReport> {
    let snaps = store.read_journal()?;
    let now = now_epoch();
    let mut keep_ids: Vec<u64> = snaps.iter().map(|s| s.id).collect();

    if let Some(max_age) = older_than_secs {
        keep_ids.retain(|id| {
            snaps
                .iter()
                .find(|s| s.id == *id)
                .map(|s| now.saturating_sub(s.ts) <= max_age)
                .unwrap_or(false)
        });
    }
    if let Some(n) = keep {
        let cutoff = keep_ids.len().saturating_sub(n);
        keep_ids = keep_ids.split_off(cutoff);
    }

    let mut removed = Vec::new();
    for s in &snaps {
        if !keep_ids.contains(&s.id) {
            let tree = store.tree(s.id);
            if tree.exists() {
                fs::remove_dir_all(&tree)
                    .with_context(|| format!("removing tree for #{}", s.id))?;
            }
            removed.push(s.id);
        }
    }
    let kept: Vec<Snap> = snaps
        .into_iter()
        .filter(|s| keep_ids.contains(&s.id))
        .collect();
    store.rewrite_journal(&kept)?;

    let mut trash_cleared = 0;
    for dir in [store.trash_dir(), store.tmp_dir()] {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if fs::remove_dir_all(entry.path())
                    .or_else(|_| fs::remove_file(entry.path()))
                    .is_ok()
                {
                    trash_cleared += 1;
                }
            }
        }
    }
    Ok(PruneReport {
        removed,
        trash_cleared,
    })
}

pub fn print_list(store: &Store, limit: usize) -> Result<()> {
    let snaps = store.read_journal()?;
    if snaps.is_empty() {
        println!("no checkpoints yet — run `timefork snap` or `timefork install-hooks`");
        return Ok(());
    }
    println!(
        "{:>5}  {:>4}  {:<13} {:<9} LABEL",
        "ID", "AGE", "ORIGIN", "GIT"
    );
    for s in snaps.iter().rev().take(limit) {
        let pruned = !store.tree(s.id).is_dir();
        let origin = s.tool.as_deref().unwrap_or(&s.origin);
        println!(
            "{:>5}  {:>4}  {:<13} {:<9} {}{}",
            s.id,
            humanize_age(s.ts),
            origin,
            s.git_head.as_deref().unwrap_or("-"),
            truncate(&s.label, 70),
            if pruned { "  [pruned]" } else { "" },
        );
    }
    Ok(())
}

pub fn print_show(store: &Store, id: u64) -> Result<()> {
    let s = store.find_snap(id)?;
    println!("checkpoint #{}", s.id);
    println!("  when     {} ago", humanize_age(s.ts));
    println!("  origin   {}", s.origin);
    if let Some(t) = &s.tool {
        println!("  tool     {t}");
    }
    println!("  label    {}", s.label);
    if let Some(g) = &s.git_head {
        println!("  git      {g}");
    }
    if let Some(sess) = &s.session {
        println!("  session  {sess}");
    }
    if let Some(ms) = s.took_ms {
        println!("  snap took {ms}ms");
    }
    let tree = store.tree(s.id);
    if !tree.is_dir() {
        println!("  tree     [pruned]");
        return Ok(());
    }
    println!("  tree     {}", tree.display());

    // Diff against the nearest earlier checkpoint that still has a tree.
    let snaps = store.read_journal()?;
    let prev = snaps
        .iter()
        .rev()
        .find(|p| p.id < s.id && store.tree(p.id).is_dir());
    if let Some(prev) = prev {
        let d = diff_trees(&store.tree(prev.id), false, &tree, false)?;
        println!(
            "  vs #{}   +{} added  ~{} modified  -{} removed",
            prev.id,
            d.added.len(),
            d.modified.len(),
            d.removed.len()
        );
    }
    Ok(())
}

pub fn print_diff(store: &Store, a: &TreeRef, b: &TreeRef) -> Result<()> {
    let (pa, na) = a.resolve(store)?;
    let (pb, nb) = b.resolve(store)?;
    let d = diff_trees(
        &pa,
        matches!(a, TreeRef::Live),
        &pb,
        matches!(b, TreeRef::Live),
    )?;
    if d.added.is_empty() && d.removed.is_empty() && d.modified.is_empty() {
        println!("{na} and {nb} are identical");
        return Ok(());
    }
    for p in &d.added {
        println!("A {}", p.display());
    }
    for p in &d.modified {
        println!("M {}", p.display());
    }
    for p in &d.removed {
        println!("D {}", p.display());
    }
    println!(
        "\n{na} → {nb}: {} added, {} modified, {} removed",
        d.added.len(),
        d.modified.len(),
        d.removed.len()
    );
    println!("content diff: git diff --no-index $(timefork path {na}) $(timefork path {nb})");
    Ok(())
}

pub fn print_status(store: &Store) -> Result<()> {
    let snaps = store.read_journal()?;
    let alive = snaps.iter().filter(|s| store.tree(s.id).is_dir()).count();
    println!("workspace  {}", store.root.display());
    println!(
        "checkpoints {} in journal, {} with trees",
        snaps.len(),
        alive
    );
    if let Some(last) = snaps.last() {
        println!(
            "latest     #{} ({} ago) {}",
            last.id,
            humanize_age(last.ts),
            truncate(&last.label, 60)
        );
    }
    let trash_entries = fs::read_dir(store.trash_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    if trash_entries > 0 {
        println!("trash      {trash_entries} restore leftovers (reclaim with `timefork prune`)");
    }
    println!("note: checkpoint trees are CoW clones — they share disk blocks with the workspace, so `du` wildly overstates real usage");
    Ok(())
}

pub fn truncate(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let cut: String = collapsed.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
