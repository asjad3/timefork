//! Claude Code hook integration.
//!
//! `timefork hook` is registered as a PreToolUse + SessionStart hook. It reads
//! the hook payload from stdin, takes a checkpoint if the tool about to run
//! can mutate the workspace, and ALWAYS exits 0 — a checkpointing tool must
//! never block or break the agent it is protecting.

use crate::ops::{snap, truncate, SnapMeta};
use crate::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Tools whose execution can change the workspace.
const MUTATING_TOOLS: &[&str] = &["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"];

pub fn run_hook() {
    if let Err(e) = run_hook_inner() {
        eprintln!("timefork hook: {e:#}");
    }
    // Fail-open by contract: exit code is always 0.
}

fn run_hook_inner() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .take(10 * 1024 * 1024)
        .read_to_string(&mut raw)?;
    let payload: Value = serde_json::from_str(&raw)?;

    let event = payload["hook_event_name"].as_str().unwrap_or("");
    let session = payload["session_id"].as_str().map(str::to_string);
    let cwd = payload["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    let Ok(store) = Store::discover_from(&cwd) else {
        // Not a timefork-managed workspace; nothing to do.
        return Ok(());
    };

    let meta = match event {
        "SessionStart" => SnapMeta {
            origin: "session-start".into(),
            label: "agent session started".into(),
            tool: None,
            session,
        },
        "PreToolUse" => {
            let tool = payload["tool_name"].as_str().unwrap_or("");
            if !MUTATING_TOOLS.contains(&tool) {
                return Ok(());
            }
            SnapMeta {
                origin: "hook".into(),
                label: describe_tool(tool, &payload["tool_input"], &store.root),
                tool: Some(tool.to_string()),
                session,
            }
        }
        _ => return Ok(()),
    };

    let _lock = store.lock()?;
    snap(&store, meta)?;
    Ok(())
}

fn describe_tool(tool: &str, input: &Value, root: &Path) -> String {
    match tool {
        "Bash" => {
            let cmd = input["command"].as_str().unwrap_or("");
            truncate(cmd, 80)
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = input["file_path"]
                .as_str()
                .or_else(|| input["notebook_path"].as_str())
                .unwrap_or("?");
            let shown = Path::new(path)
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.to_string());
            format!("{} {}", tool.to_lowercase(), truncate(&shown, 70))
        }
        _ => tool.to_string(),
    }
}

/// Wire `timefork hook` into the workspace's `.claude/settings.json`,
/// merging with whatever is already configured. Idempotent.
pub fn install_hooks(store: &Store) -> Result<()> {
    let settings_path = store.root.join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap())?;

    let mut settings: Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "timefork".to_string());
    let command = format!("{exe} hook");

    let hooks = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".claude/settings.json is not a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let mut installed = Vec::new();
    for (event, matcher) in [
        ("PreToolUse", Some("Bash|Edit|Write|MultiEdit|NotebookEdit")),
        ("SessionStart", None),
    ] {
        let arr = hooks
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("settings 'hooks' is not an object"))?
            .entry(event)
            .or_insert_with(|| serde_json::json!([]));
        let entries = arr
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("settings hooks.{event} is not an array"))?;

        let already = entries.iter().any(|entry| {
            entry["hooks"]
                .as_array()
                .map(|hs| {
                    hs.iter().any(|h| {
                        h["command"]
                            .as_str()
                            .map(|c| c.contains("timefork"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
        if already {
            continue;
        }
        let mut entry = serde_json::json!({
            "hooks": [{ "type": "command", "command": command }]
        });
        if let Some(m) = matcher {
            entry["matcher"] = Value::String(m.to_string());
        }
        entries.push(entry);
        installed.push(event);
    }

    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings)? + "\n",
    )?;

    if installed.is_empty() {
        println!("hooks already installed in {}", settings_path.display());
    } else {
        println!(
            "installed {} hook(s) in {}",
            installed.join(" + "),
            settings_path.display()
        );
        println!("every mutating tool call in Claude Code will now checkpoint this workspace");
    }
    Ok(())
}
