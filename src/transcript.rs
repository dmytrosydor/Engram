//! Reading Claude Code session transcripts (`~/.claude/projects/*/*.jsonl`).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;

/// Longest text handed to the model per transcript: the start sets context, the end
/// holds conclusions.
const HEAD: usize = 15_000;
const TAIL: usize = 35_000;

pub struct Transcript {
    pub path: PathBuf,
    pub session: String,
    pub cwd: Option<PathBuf>,
    /// User and assistant prose, without tool calls, tool output or thinking.
    pub text: String,
}

/// Top-level transcripts modified after `since`, newest first, skipping `exclude` folders.
pub fn changed_since(root: &Path, since: SystemTime, exclude: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    for dir in dirs.flatten() {
        let dir = dir.path();
        if !dir.is_dir() || exclude.iter().any(|e| e == &dir) {
            continue;
        }
        for f in std::fs::read_dir(&dir)?.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "jsonl")
                && let Ok(m) = f.metadata().and_then(|m| m.modified())
                && m > since
            {
                found.push((m, p));
            }
        }
    }
    found.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
    Ok(found.into_iter().map(|(_, p)| p).collect())
}

pub fn read(path: &Path) -> Result<Transcript> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut cwd = None;
    let mut parts: Vec<String> = Vec::new();
    for line in reader.lines() {
        let Ok(d) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let kind = d["type"].as_str().unwrap_or("");
        if !matches!(kind, "user" | "assistant") || d["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        if cwd.is_none() {
            cwd = d["cwd"].as_str().map(PathBuf::from);
        }
        let who = if kind == "user" { "U" } else { "A" };
        let content = &d["message"]["content"];
        let texts: Vec<&str> = match content {
            Value::String(s) => vec![s.as_str()],
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect(),
            _ => vec![],
        };
        for t in texts {
            let t = strip_tags(t);
            let t = t.trim();
            if !t.is_empty() {
                parts.push(format!("{who}: {t}"));
            }
        }
    }
    let session = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Transcript {
        path: path.to_path_buf(),
        session,
        cwd,
        text: clip(&parts.join("\n")),
    })
}

/// Drops `<system-reminder>…</system-reminder>` and similar injected blocks.
fn strip_tags(s: &str) -> String {
    const TAGS: [&str; 5] = [
        "system-reminder",
        "command-name",
        "command-message",
        "command-args",
        "local-command-stdout",
    ];
    let mut out = s.to_string();
    for tag in TAGS {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(i) = out.find(&open) {
            let end = out[i..]
                .find(&close)
                .map_or(out.len(), |j| i + j + close.len());
            out.replace_range(i..end, "");
        }
    }
    out
}

fn clip(text: &str) -> String {
    let n = text.chars().count();
    if n <= HEAD + TAIL {
        return text.to_string();
    }
    let head: String = text.chars().take(HEAD).collect();
    let tail: String = text.chars().skip(n - TAIL).collect();
    format!("{head}\n[… middle of the session omitted …]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_injected_blocks() {
        let s = "a<system-reminder>x</system-reminder>b<command-name>/x</command-name>c";
        assert_eq!(strip_tags(s), "abc");
    }
}
