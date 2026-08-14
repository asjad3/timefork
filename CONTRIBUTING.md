# Contributing to timefork

Contributions are very welcome — this project is young and the roadmap is wide open. Issues labeled [`good first issue`](https://github.com/asjad3/timefork/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22) and [`help wanted`](https://github.com/asjad3/timefork/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22) are the best entry points.

## Build & test

```sh
cargo build --release
TF=$PWD/target/release/timefork zsh tests/e2e.sh   # 12 end-to-end scenarios
cargo clippy --release -- -D warnings
cargo fmt --check
```

The e2e suite builds a synthetic repo in a temp dir and exercises the full surface: init, snap, restore (including untracked files and symlinks), undo-restore, diff, fork independence, live forks, hook behavior (mutating / non-mutating / no-store / garbage stdin), settings merging, and prune. If you add a feature, add a scenario there.

## Layout

| file | what lives there |
|---|---|
| `src/clone.rs` | the CoW engine: `clonefile(2)` on macOS, `FICLONE` on Linux, fallback walk |
| `src/store.rs` | `.timefork/` layout, JSONL journal, id state, `flock` locking |
| `src/ops.rs` | snap / restore / diff / fork / prune / list / show |
| `src/hook.rs` | Claude Code hook entrypoint + `install-hooks` settings merge |
| `src/main.rs` | clap CLI |

## Ground rules

- **The hook must never break an agent.** `timefork hook` always exits 0, no matter what. Anything that could block or fail a tool call is a bug.
- **Never touch user data outside `.timefork/`, forks, and explicit restores.** Restores must stay undoable.
- Correctness over speed, but keep the hot path (snap) allocation-light — it runs on every tool call.
- Platform-specific code stays in `clone.rs` behind `cfg`.

## Testing on Linux

macOS/APFS is the first-class target. On Linux, reflink filesystems (btrfs/XFS/bcachefs) get the CoW fast path and everything else degrades to real copies. CI runs the e2e suite on ext4 (fallback path); if you can test on real btrfs/XFS, that's especially valuable — see the open Linux issues.
