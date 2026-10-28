//! First-run helpers: fetching the model, drafting spaces.toml, and `doctor`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{db, embed, install, spaces};

const MODEL_URL: &str = "https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q8_0.gguf";
const MODEL_SHA256: &str = "950f4a8e5e19477a6d3c26d2f162233c20002c601f75e4b002e3239997821167";
const MODEL_BYTES: u64 = 634_553_760;

/// Downloads the embedding model (resumable, via curl) and checks its hash.
pub fn model_pull() -> Result<()> {
    let dest = embed::model_path();
    if dest.exists() && sha256(&dest)? == MODEL_SHA256 {
        println!("{} is already in place", dest.display());
        return Ok(());
    }
    let dir = dest.parent().context("model path has no folder")?;
    std::fs::create_dir_all(dir)?;
    let part = dest.with_extension("gguf.part");
    println!(
        "downloading {} MB from {MODEL_URL}",
        MODEL_BYTES / 1_000_000
    );
    let status = Command::new("curl")
        .args(["-L", "--fail", "--retry", "3", "-C", "-", "-o"])
        .arg(&part)
        .arg(MODEL_URL)
        .status()
        .context("curl is needed to download the model")?;
    if !status.success() {
        bail!("download failed; run `engram model pull` again to resume");
    }
    let got = sha256(&part)?;
    if got != MODEL_SHA256 {
        std::fs::remove_file(&part)?;
        bail!("checksum mismatch (got {got}); the partial file was removed, try again");
    }
    std::fs::rename(&part, &dest)?;
    println!("model ready: {}", dest.display());
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Folders that hold projects rather than being one.
const CONTAINERS: [&str; 14] = [
    "Desktop",
    "Documents",
    "Downloads",
    "code",
    "Code",
    "projects",
    "Projects",
    "src",
    "dev",
    "Developer",
    "repos",
    "git",
    "workspace",
    "Library",
];

/// A commented spaces.toml with one space per folder Claude Code has worked in.
/// The domain is a guess: the first folder under home that is not a generic
/// container (so ~/Desktop/acme/api lands in domain `acme`). The user decides.
pub fn draft_spaces(projects: &Path) -> Result<String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut folders: Vec<PathBuf> = Vec::new();
    for dir in std::fs::read_dir(projects)
        .with_context(|| format!("read {}", projects.display()))?
        .flatten()
    {
        if let Some(cwd) = project_cwd(&dir.path())
            && cwd.is_dir()
            && !folders.contains(&cwd)
        {
            folders.push(cwd);
        }
    }
    folders.sort();

    let mut by_domain: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for f in &folders {
        let Ok(rest) = f.strip_prefix(&home) else {
            continue; // /tmp and other scratch places stay unsorted
        };
        let parts: Vec<String> = rest
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        // dot folders (config, worktrees under .claude) belong to their parent project
        if parts.iter().any(|p| p.starts_with('.')) || parts.first().is_some_and(|p| p == "Library")
        {
            continue;
        }
        let Some(domain) = parts.iter().find(|p| !CONTAINERS.contains(&p.as_str())) else {
            continue; // home itself or a bare container like ~/Desktop
        };
        let domain = slug(domain);
        let name = slug(parts.last().map_or("", String::as_str));
        by_domain
            .entry(domain)
            .or_default()
            .push((name, format!("~/{}", rest.display())));
    }

    let mut out = String::from(
        "# Draft written by `engram spaces --init` from the folders Claude Code has worked in.\n\
         #\n\
         # A space is one project's memory; a domain groups spaces that may see each other.\n\
         # A session sees its own space, its domain's `<domain>/_shared` space and `global`,\n\
         # never another domain. The domains below are only guesses (the first folder\n\
         # under home that is not a container like Desktop or code):\n\
         # rename them, e.g. to \"work\" and \"personal\", so related projects share one.\n\
         # Folders without a rule land in `unsorted`, which sees only `global`.\n\
         #\n\
         # Then load it:  engram spaces ~/.engram/spaces.toml\n\n[spaces]\n",
    );
    for (domain, entries) in &by_domain {
        let mut names: Vec<&String> = entries.iter().map(|(n, _)| n).collect();
        names.dedup();
        for name in names {
            out.push_str(&format!(
                "\"{domain}/{name}\" = {{ domain = \"{domain}\" }}\n"
            ));
        }
    }
    out.push_str("\n[paths]\n");
    for (domain, entries) in &by_domain {
        for (name, path) in entries {
            out.push_str(&format!("\"{path}\" = \"{domain}/{name}\"\n"));
        }
    }
    Ok(out)
}

/// The folder a Claude Code project ran in, read from its newest transcript.
fn project_cwd(dir: &Path) -> Option<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let (_, newest) = files.pop()?;
    let f = std::fs::File::open(newest).ok()?;
    BufReader::new(f)
        .lines()
        .take(50)
        .flatten()
        .find_map(|line| {
            serde_json::from_str::<serde_json::Value>(&line)
                .ok()?
                .get("cwd")?
                .as_str()
                .map(PathBuf::from)
        })
}

fn slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Checks every piece a working setup needs. Returns whether all passed.
pub fn doctor() -> Result<bool> {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name:<12} {detail}", if pass { "ok  " } else { "FAIL" });
        ok &= pass;
    };

    let path = db::db_path();
    check("database", path.exists(), path.display().to_string());
    if path.exists() {
        db::init(&path)?;
        let conn = db::connect(&path)?;
        let rules: i64 = conn.query_row("SELECT count(*) FROM path_rule", [], |r| r.get(0))?;
        check(
            "spaces",
            rules > 0,
            if rules > 0 {
                format!("{} spaces, {rules} folder rules", spaces::all(&conn)?.len())
            } else {
                "no folder rules: every session is `unsorted` (engram spaces --init)".into()
            },
        );
        let missing: i64 = conn.query_row(
            "SELECT count(*) FROM item WHERE status IN ('active','proposed')
             AND id NOT IN (SELECT item_id FROM item_vec)",
            [],
            |r| r.get(0),
        )?;
        check(
            "vectors",
            missing == 0,
            if missing == 0 {
                "every live record has one".into()
            } else {
                format!("{missing} records without one (engram reindex)")
            },
        );
    }
    let model = embed::model_path();
    check(
        "model",
        model.exists(),
        if model.exists() {
            model.display().to_string()
        } else {
            "missing (engram model pull)".into()
        },
    );
    let server = embed::server_bin();
    check(
        "llama-server",
        server.is_file(),
        if server.is_file() {
            server.display().to_string()
        } else {
            "not found: install llama.cpp, or set ENGRAM_LLAMA_SERVER".into()
        },
    );
    if model.exists() && server.is_file() {
        let started = std::time::Instant::now();
        let probe = embed::embed_one("engram doctor");
        check(
            "embedding",
            probe.is_ok(),
            match probe {
                Ok(_) => format!("answered in {:?}", started.elapsed()),
                Err(e) => format!("{e:#}"),
            },
        );
    }
    let claude = Command::new("claude").arg("--version").output();
    check(
        "claude",
        claude.as_ref().is_ok_and(|o| o.status.success()),
        match &claude {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(_) => "claude CLI not on PATH".into(),
        },
    );
    let mcp = Command::new("claude")
        .args(["mcp", "get", "engram"])
        .output()
        .is_ok_and(|o| o.status.success());
    check(
        "mcp",
        mcp,
        if mcp {
            "registered".into()
        } else {
            "not registered (engram install --yes)".into()
        },
    );
    let hook = install::hook_installed();
    check(
        "hook",
        hook,
        if hook {
            "SessionStart hook in ~/.claude/settings.json".into()
        } else {
            "missing (engram install --yes)".into()
        },
    );
    if path.exists() {
        let conn = db::connect(&path)?;
        let last = crate::daily::meta(&conn, "daily_last_run")?;
        let errors = crate::daily::meta(&conn, "daily_last_errors")?.unwrap_or_default();
        println!(
            "{} {:<12} last run {}{}",
            if errors.is_empty() { "info" } else { "warn" },
            "daily pass",
            last.as_deref().unwrap_or("never"),
            if errors.is_empty() {
                String::new()
            } else {
                format!(", errors: {}", errors.replace('\n', "; "))
            }
        );
    }
    let scheduled = install::scheduled();
    println!(
        "{} {:<12} {}",
        if scheduled { "ok  " } else { "info" },
        "daily",
        if scheduled {
            install::scheduler_name()
        } else {
            "not scheduled (engram install --yes, or run `engram daily` by hand)".into()
        }
    );
    Ok(ok)
}
