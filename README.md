# timefork

[![CI](https://github.com/asjad3/timefork/actions/workflows/ci.yml/badge.svg)](https://github.com/asjad3/timefork/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A copy-on-write time machine for coding agent workspaces.** Every tool call your agent makes becomes a free filesystem checkpoint. Rewind to any moment, diff any two moments, and fork the entire workspace in about a second — powered by APFS `clonefile(2)` / Linux reflinks, with no daemon, no FUSE mount, no database.

![timefork demo: agent history, rm -rf recovery, and 4 parallel forks](docs/demo.gif)

## Why

Coding agents mutate state that git can't protect. A `git checkout` won't un-run a database migration in your local SQLite file, un-write `node_modules`, or recover the untracked scratch files an overnight agent deleted. Claude Code's built-in `/rewind` only tracks the agent's own `Edit`/`Write` calls — it is blind to everything a `Bash` tool call does, which is where the real damage happens.

And when you want to run four agents in parallel, `git worktree` gives you four checkouts of *tracked files only* — no build caches, no `node_modules`, no local databases — each paying a full `npm install` before it's usable.

The filesystem you already have solves both problems. APFS (and btrfs/XFS on Linux) can clone an entire directory hierarchy as copy-on-write references: no data copied, blocks shared until modified. `timefork` is a thin, careful CLI around that primitive, wired into your agent's tool-call loop.

## Benchmarks

Real workspace: an 18 GB Tauri project (Rust `target/` + `node_modules`, 78,000 files), Apple M4, APFS:

| operation | time |
|---|---|
| checkpoint entire 18 GB workspace | **0.9 s** |
| fork entire workspace to a sibling dir | **1.0 s** |
| full rewind (restore) | **3.0 s** |
| additional disk blocks after 4 checkpoints + 1 fork | **~0** |

Synthetic worst-case (50,000 files spread across 500 top-level directories): checkpoint 450 ms, fork 520 ms. Cost scales with directory structure, not bytes — `clonefile` clones whole subtrees in a single kernel call, which is why the 18 GB workspace snapshots *faster* than a 195 MB one with more top-level entries.

## Install

```sh
cargo install timefork          # or:
git clone https://github.com/asjad3/timefork && cd timefork && cargo install --path .
```

Requires macOS on APFS (any Mac from the last decade), or Linux on btrfs/XFS/bcachefs (reflink support; experimental). Everything happens on your real filesystem — if the fast path is ever unavailable, timefork degrades to plain copies rather than failing.

## Quickstart

```sh
cd your-repo
timefork init             # creates .timefork/, takes a baseline checkpoint
timefork install-hooks    # auto-checkpoint every mutating Claude Code tool call
```

That's it. From now on, every `Bash`, `Edit`, `Write`, `MultiEdit`, and `NotebookEdit` call gets a pre-execution checkpoint, labeled with what the agent was about to do:

```sh
timefork list                  # what happened, when
timefork show 45               # one checkpoint in detail + diffstat vs previous
timefork diff 45 live          # what changed since then (add/modify/delete)
timefork restore 45            # rewind (your current state is checkpointed first — restore is undoable)
timefork fork 45 -n 4          # four independent workspaces from that moment
timefork fork live             # fork the current state (parallel agents, zero setup)
timefork prune --keep 100      # reclaim old checkpoints + restore leftovers
```

Content-level diff composes with git:

```sh
git diff --no-index $(timefork path 45) $(timefork path live) -- src/
```

## How it works

- `timefork init` creates a `.timefork/` store inside the workspace (auto-added to `.git/info/exclude`).
- A checkpoint clones every top-level entry of the workspace (except `.timefork`) into `.timefork/trees/NNNNNN` via `clonefile(2)` — an atomic-per-entry, in-kernel CoW clone that preserves permissions, mtimes, and symlinks. A JSONL journal records id, timestamp, origin (which tool call, which agent session), and git HEAD.
- `restore` stages a clone of the target tree, then swaps it in with pure renames — the displaced live state goes to a trash area (reclaimed by `prune`), and a pre-restore checkpoint makes the whole operation undoable.
- `fork` clones a checkpoint (or the live tree) to a sibling directory. Forks are fully independent: writes in a fork never leak back, and the fork carries no `.timefork` store.
- Diffs compare `(type, size, mtime)` — sound here precisely because CoW clones preserve mtimes exactly.
- The Claude Code hook is **fail-open by contract**: it always exits 0. A checkpointing tool must never be the thing that breaks your agent. Uncloneable entries (sockets, fifos) are skipped with a warning; cross-device or non-reflink filesystems degrade to real copies.
- Concurrent snaps/restores are serialized with an advisory `flock`.

No daemon watches your filesystem. No FUSE layer sits in your I/O path. Your repo remains a completely normal directory; when you uninstall timefork, nothing changes except that `.timefork/` stops growing.

## vs. the alternatives

| | timefork | git worktree | git stash/checkout | Claude Code `/rewind` | FUSE agent filesystems |
|---|---|---|---|---|---|
| covers untracked files, build caches, local DBs | ✅ | ❌ | ❌ | ❌ | ✅ |
| catches `Bash` side effects | ✅ | ❌ | ❌ | ❌ | ✅ |
| fork cost for an 18 GB workspace | ~1 s | minutes (checkout + install) | n/a | n/a | varies |
| runs on your real filesystem, native I/O speed | ✅ | ✅ | ✅ | ✅ | ❌ (FUSE indirection) |
| zero daemons / mounts / kexts | ✅ | ✅ | ✅ | ✅ | ❌ |

## Caveats, honestly

- Checkpoints live on the same volume as the workspace (that's what makes them free). This is snapshot tooling, **not backup** — a dead disk takes the checkpoints with it.
- On filesystems without reflink support (ext4, network mounts), every "clone" is a real copy: correct, but no longer free. macOS/APFS is the first-class target; Linux reflink is implemented but lightly tested.
- Hook overhead is the checkpoint time: single-digit milliseconds on normal repos, ~1 s on 80k-file monsters. Per-checkpoint timings are recorded in the journal (`timefork show N`).
- Restores swap the *entire* workspace. Processes holding open file handles in it (dev servers, watchers) should be restarted after a restore.
- `du` against `.timefork/` wildly overstates real usage — clones share blocks. Trust `df`, not `du`.

## Roadmap

- Checkpoint ↔ transcript deep-linking (`timefork show 45 --transcript` → the exact conversation moment)
- Hook adapters for other agent harnesses (Codex CLI, opencode)
- `timefork fork --run "claude -p '…'"` — speculative execution: N forks, N attempts, keep the winner
- Scheduled auto-prune

## License

MIT
