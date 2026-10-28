//! Wiring engram into Claude Code and launchd, and taking it out again.
//!
//! Without `--yes` nothing changes: the plan is printed. Settings are backed up
//! before they are edited; data in ~/.engram is never touched by uninstall.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{db, spaces};

const LABEL: &str = "dev.engram.daily";
const HOOK_ARGS: &str = "hook session-start";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn settings_path() -> PathBuf {
    home().join(".claude/settings.json")
}

fn plist_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

pub fn install(apply: bool) -> Result<()> {
    let exe = exe()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: {} is a build output; `cargo install --path .` first so hooks survive `cargo clean`",
            exe.display()
        );
    }
    let hook = format!("{} {HOOK_ARGS}", exe.display());
    let claude = which("claude").context("claude CLI not found on PATH")?;

    println!(
        "engram install{}",
        if apply {
            ""
        } else {
            " (dry run; add --yes to apply)"
        }
    );
    println!("1. database {}", db::db_path().display());
    let spaces_file = db::home().join("spaces.toml");
    println!(
        "   spaces: {}",
        if spaces_file.exists() {
            spaces_file.display().to_string()
        } else {
            "none yet (engram spaces <file>)".into()
        }
    );
    println!(
        "2. SessionStart hook in {}: {hook}",
        settings_path().display()
    );
    println!("3. MCP server `engram` (user scope): {} mcp", exe.display());
    println!("4. launchd agent {LABEL}: hourly `engram daily --if-due` (once a day, on AC power)");
    if !apply {
        return Ok(());
    }

    db::init(&db::db_path())?;
    let conn = db::connect(&db::db_path())?;
    spaces::ensure_builtin(&conn)?;
    if spaces_file.exists() {
        spaces::load(&conn, &spaces_file)?;
    }

    edit_settings(|s| add_hook(s, &hook))?;

    let listed = Command::new(&claude)
        .args(["mcp", "get", "engram"])
        .output()?;
    if listed.status.success() {
        println!("   MCP server already registered");
    } else {
        run(Command::new(&claude)
            .args(["mcp", "add", "--scope", "user", "engram", "--"])
            .arg(&exe)
            .arg("mcp"))?;
    }

    let path_env = format!(
        "{}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin",
        claude.parent().unwrap_or(Path::new("/usr/bin")).display()
    );
    std::fs::create_dir_all(plist_path().parent().context("no LaunchAgents dir")?)?;
    std::fs::write(plist_path(), plist(&exe, &path_env))?;
    let target = format!("gui/{}", uid()?);
    // a previous version may be loaded; replacing it is fine
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .output();
    run(Command::new("launchctl")
        .args(["bootstrap", &target])
        .arg(plist_path()))?;
    println!("done. Start a new Claude session to get the core; `engram ui` for the web UI.");
    Ok(())
}

pub fn uninstall(apply: bool) -> Result<()> {
    println!(
        "engram uninstall{}",
        if apply {
            ""
        } else {
            " (dry run; add --yes to apply)"
        }
    );
    println!(
        "1. remove the SessionStart hook from {}",
        settings_path().display()
    );
    println!("2. remove the MCP server `engram`");
    println!("3. unload and delete {}", plist_path().display());
    println!("   data in {} stays", db::home().display());
    if !apply {
        return Ok(());
    }
    edit_settings(remove_hook)?;
    if let Some(claude) = which("claude") {
        let _ = Command::new(claude)
            .args(["mcp", "remove", "engram", "--scope", "user"])
            .output();
    }
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{LABEL}", uid()?)])
        .output();
    let _ = std::fs::remove_file(plist_path());
    println!("done.");
    Ok(())
}

/// Applies `change` to settings.json after a timestamped backup; no-op if unchanged.
fn edit_settings(change: impl FnOnce(&mut Value) -> bool) -> Result<()> {
    let path = settings_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut settings: Value =
        serde_json::from_str(&text).context("settings.json is not valid JSON")?;
    if !change(&mut settings) {
        println!("   settings.json already up to date");
        return Ok(());
    }
    if path.exists() {
        let stamp = Command::new("date").arg("+%Y%m%d-%H%M%S").output()?;
        let backup = path.with_extension(format!(
            "json.engram-{}",
            String::from_utf8_lossy(&stamp.stdout).trim()
        ));
        std::fs::copy(&path, &backup)?;
        println!("   backup {}", backup.display());
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    Ok(())
}

/// Adds the engram SessionStart hook unless one is there. Returns whether it changed.
pub fn add_hook(settings: &mut Value, command: &str) -> bool {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .map(|o| o.entry("hooks").or_insert_with(|| json!({})))
        .expect("object");
    let Some(groups) = hooks
        .as_object_mut()
        .map(|o| o.entry("SessionStart").or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let present = groups.iter().any(|g| {
        g["hooks"].as_array().is_some_and(|hs| {
            hs.iter().any(|h| {
                h["command"]
                    .as_str()
                    .is_some_and(|c| c.ends_with(HOOK_ARGS))
            })
        })
    });
    if present {
        return false;
    }
    groups.push(json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]}));
    true
}

/// Removes engram's SessionStart hook, dropping groups and events left empty.
pub fn remove_hook(settings: &mut Value) -> bool {
    let Some(groups) = settings["hooks"]["SessionStart"].as_array_mut() else {
        return false;
    };
    let before = groups.len();
    for g in groups.iter_mut() {
        if let Some(hs) = g["hooks"].as_array_mut() {
            hs.retain(|h| {
                !h["command"]
                    .as_str()
                    .is_some_and(|c| c.ends_with(HOOK_ARGS))
            });
        }
    }
    groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    let changed = groups.len() != before;
    if groups.is_empty()
        && let Some(h) = settings["hooks"].as_object_mut()
    {
        h.remove("SessionStart");
    }
    changed
}

fn plist(exe: &Path, path_env: &str) -> String {
    let log = db::home().join("daily.log");
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>daily</string><string>--if-due</string></array>
  <key>StartInterval</key><integer>3600</integer>
  <key>RunAtLoad</key><false/>
  <key>LowPriorityIO</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = esc(&exe.to_string_lossy()),
        path = esc(path_env),
        log = esc(&log.to_string_lossy()),
    )
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/which").arg(name).output().ok()?;
    let p = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !p.is_empty()).then(|| PathBuf::from(p))
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_add_is_idempotent_and_remove_keeps_other_hooks() {
        let mut s = json!({
            "model": "x",
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "other.sh"}]}],
                      "Stop": [{"hooks": [{"type": "command", "command": "stop.sh"}]}]}
        });
        assert!(add_hook(&mut s, "/bin/engram hook session-start"));
        assert!(!add_hook(&mut s, "/bin/engram hook session-start"));
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        // key order of the user's file is preserved
        assert_eq!(s.as_object().unwrap().keys().next().unwrap(), "model");

        assert!(remove_hook(&mut s));
        assert_eq!(
            s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "other.sh"
        );
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "stop.sh");

        let mut only = json!({});
        add_hook(&mut only, "/bin/engram hook session-start");
        remove_hook(&mut only);
        assert!(only["hooks"].get("SessionStart").is_none());
    }
}
