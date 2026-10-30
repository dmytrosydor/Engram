//! Wiring engram into Claude Code and the system scheduler (launchd on macOS,
//! a systemd user timer on Linux), and taking it out again.
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

fn systemd_dir() -> PathBuf {
    home().join(".config/systemd/user")
}

/// Where the hourly `engram daily --if-due` job is registered on this system.
pub fn scheduler_name() -> String {
    if cfg!(target_os = "macos") {
        format!("launchd agent {LABEL}")
    } else {
        format!(
            "systemd user timer engram-daily ({})",
            systemd_dir().display()
        )
    }
}

/// Whether the daily job is registered.
pub fn scheduled() -> bool {
    if cfg!(target_os = "macos") {
        plist_path().exists()
    } else {
        systemd_dir().join("engram-daily.timer").exists()
    }
}

/// The binary as invoked; not canonicalised, so a stable symlink (e.g. in
/// ~/.cargo/bin or /opt/homebrew/bin) survives upgrades.
fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

/// Single-quoted for the shell Claude Code runs hook commands in.
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

pub fn install(apply: bool, daily: bool) -> Result<()> {
    let exe = exe()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: {} is a build output; `cargo install --path .` first so hooks survive `cargo clean`",
            exe.display()
        );
    }
    let hook = format!("{} {HOOK_ARGS}", shell_quote(&exe));
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
    if daily {
        println!(
            "4. {}: hourly `engram daily --if-due` (once a day; no model calls)",
            scheduler_name()
        );
    } else {
        println!("4. no daily pass (--no-daily); run `engram daily` by hand when you want one");
    }
    for warning in readiness() {
        println!("   warning: {warning}");
    }
    if !apply {
        return Ok(());
    }

    db::init(&db::db_path())?;
    let conn = db::connect(&db::db_path())?;
    spaces::ensure_builtin(&conn)?;
    if spaces_file.exists() {
        spaces::load(&conn, &spaces_file)?;
    }

    edit_settings(|s| set_hook(s, &hook))?;

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

    if daily {
        // schedulers start jobs with a bare PATH; claude and llama-server must resolve
        let path_env = format!(
            "{}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin",
            claude.parent().unwrap_or(Path::new("/usr/bin")).display()
        );
        schedule(&exe, &path_env)?;
    }
    println!(
        "done. Start a new Claude session to get the core; `engram` opens the memory map and review queue."
    );
    Ok(())
}

/// What would leave a fresh install half-working.
fn readiness() -> Vec<String> {
    let mut out = Vec::new();
    if !crate::embed::model_path().exists() {
        out.push(
            "embedding model missing; run `engram model pull` (search falls back to words only)"
                .into(),
        );
    }
    if !crate::embed::server_bin().is_file() {
        out.push(
            "llama-server not found; install llama.cpp (search falls back to words only)".into(),
        );
    }
    if !db::home().join("spaces.toml").exists() {
        out.push(
            "no spaces.toml; run `engram spaces --init` (until then every session is `unsorted`)"
                .into(),
        );
    }
    out
}

fn schedule(exe: &Path, path_env: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        std::fs::create_dir_all(plist_path().parent().context("no LaunchAgents dir")?)?;
        std::fs::write(plist_path(), plist(exe, path_env))?;
        let target = format!("gui/{}", uid()?);
        // a previous version may be loaded; replacing it is fine
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{target}/{LABEL}")])
            .output();
        run(Command::new("launchctl")
            .args(["bootstrap", &target])
            .arg(plist_path()))
    } else {
        let dir = systemd_dir();
        std::fs::create_dir_all(&dir)?;
        let (service, timer) = systemd_units(exe, path_env);
        std::fs::write(dir.join("engram-daily.service"), service)?;
        std::fs::write(dir.join("engram-daily.timer"), timer)?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", "engram-daily.timer"]))
    }
}

fn unschedule() -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid()?)])
            .output();
        let _ = std::fs::remove_file(plist_path());
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "engram-daily.timer"])
            .output();
        for unit in ["engram-daily.service", "engram-daily.timer"] {
            let _ = std::fs::remove_file(systemd_dir().join(unit));
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }
    Ok(())
}

fn systemd_units(exe: &Path, path_env: &str) -> (String, String) {
    let log = db::home().join("daily.log");
    let service = format!(
        "[Unit]\nDescription=engram daily pass\n\n[Service]\nType=oneshot\n\
         Environment=PATH={path_env}\nExecStart={exe} daily --if-due\n\
         StandardOutput=append:{log}\nStandardError=append:{log}\nNice=10\nIOSchedulingClass=idle\n",
        exe = exe.display(),
        log = log.display()
    );
    let timer = "[Unit]\nDescription=engram daily pass, checked hourly\n\n\
                 [Timer]\nOnCalendar=hourly\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
        .to_string();
    (service, timer)
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
    println!("3. remove the {}", scheduler_name());
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
    unschedule()?;
    println!("done.");
    Ok(())
}

/// Applies `change` to settings.json after a timestamped backup; no-op if unchanged.
/// Refuses to touch a file it cannot read or parse as a JSON object.
fn edit_settings(change: impl FnOnce(&mut Value) -> Result<bool>) -> Result<()> {
    let path = settings_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut settings: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    if !settings.is_object() {
        bail!("{} is not a JSON object; not touching it", path.display());
    }
    if !change(&mut settings)? {
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
    // write aside and rename, so a crash never leaves a half-written settings file
    let tmp = path.with_extension("json.engram-tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all((serde_json::to_string_pretty(&settings)? + "\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Makes `command` the one engram SessionStart hook: replaces an entry pointing at
/// another engram binary, adds one if missing. Returns whether anything changed.
pub fn set_hook(settings: &mut Value, command: &str) -> Result<bool> {
    let root = settings
        .as_object_mut()
        .context("settings is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings.hooks is not an object")?;
    let groups = hooks
        .entry("SessionStart")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("settings.hooks.SessionStart is not an array")?;
    let mut found = false;
    let mut changed = false;
    for g in groups.iter_mut() {
        let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for h in hs.iter_mut() {
            let Some(current) = h.get("command").and_then(Value::as_str) else {
                continue;
            };
            if is_engram_hook(current) {
                found = true;
                if current != command {
                    h["command"] = json!(command);
                    changed = true;
                }
            }
        }
    }
    if !found {
        groups.push(json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]}));
        changed = true;
    }
    Ok(changed)
}

/// Whether ~/.claude/settings.json has engram's SessionStart hook.
pub fn hook_installed() -> bool {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|s| s["hooks"]["SessionStart"].as_array().cloned())
        .is_some_and(|groups| {
            groups.iter().any(|g| {
                g["hooks"].as_array().is_some_and(|hs| {
                    hs.iter()
                        .any(|h| h["command"].as_str().is_some_and(is_engram_hook))
                })
            })
        })
}

fn is_engram_hook(command: &str) -> bool {
    command.trim_end().ends_with(HOOK_ARGS)
}

/// Removes engram's SessionStart hook, dropping groups and events left empty.
pub fn remove_hook(settings: &mut Value) -> Result<bool> {
    let Some(groups) = settings
        .get_mut("hooks")
        .and_then(|h| h.get_mut("SessionStart"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    let before = groups.len();
    for g in groups.iter_mut() {
        // get_mut, not IndexMut: a hand-edited entry of another shape must not panic
        if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            hs.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_engram_hook)
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
    Ok(changed)
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
    fn systemd_units_run_the_daily_pass_hourly() {
        let (service, timer) = systemd_units(Path::new("/opt/engram"), "/usr/bin:/bin");
        assert!(service.contains("ExecStart=/opt/engram daily --if-due"));
        assert!(service.contains("Environment=PATH=/usr/bin:/bin"));
        assert!(timer.contains("OnCalendar=hourly") && timer.contains("Persistent=true"));
    }

    #[test]
    fn hook_add_is_idempotent_and_remove_keeps_other_hooks() {
        let mut s = json!({
            "model": "x",
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "other.sh"}]}],
                      "Stop": [{"hooks": [{"type": "command", "command": "stop.sh"}]}]}
        });
        assert!(set_hook(&mut s, "'/bin/engram' hook session-start").unwrap());
        assert!(!set_hook(&mut s, "'/bin/engram' hook session-start").unwrap());
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        // a stale binary path is replaced, not duplicated
        assert!(set_hook(&mut s, "'/new/engram' hook session-start").unwrap());
        assert_eq!(s["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(
            s["hooks"]["SessionStart"][1]["hooks"][0]["command"],
            "'/new/engram' hook session-start"
        );
        // key order of the user's file is preserved
        assert_eq!(s.as_object().unwrap().keys().next().unwrap(), "model");

        assert!(remove_hook(&mut s).unwrap());
        assert_eq!(
            s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "other.sh"
        );
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "stop.sh");

        let mut only = json!({});
        set_hook(&mut only, "/bin/engram hook session-start").unwrap();
        remove_hook(&mut only).unwrap();
        assert!(only["hooks"].get("SessionStart").is_none());

        assert!(
            set_hook(&mut json!({"hooks": []}), "x hook session-start").is_err(),
            "wrong type is an error"
        );
        assert_eq!(shell_quote(Path::new("/a b/it's")), r"'/a b/it'\''s'");
    }
}
