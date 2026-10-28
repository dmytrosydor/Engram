//! Spaces and the folder -> space rules.
//!
//! The mapping is personal, so it lives in a TOML file outside the repo:
//!
//! ```toml
//! [spaces]
//! "work/acme" = { domain = "work" }
//! [paths]
//! "~/code/acme" = "work/acme"
//! [import]                       # only for `engram import`
//! unknown = "global"             # records a manifest marks "unknown"
//! unknown_tasks = "work/acme"    # same, for todo / issue
//! aliases = { "work/old-name" = "work/acme" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Where sessions in folders without a rule land. It is its own domain, so such a
/// session sees only this space and `global`, never another domain's records.
pub const INBOX: &str = "unsorted";
/// Spaces every database has, whatever the config says.
const BUILTIN: [(&str, &str); 2] = [("global", "global"), (INBOX, INBOX)];

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    spaces: BTreeMap<String, SpaceDef>,
    #[serde(default)]
    paths: BTreeMap<String, String>,
    #[serde(default)]
    import: ImportRules,
}

/// How manifest spaces map onto real ones during import.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ImportRules {
    pub unknown: Option<String>,
    pub unknown_tasks: Option<String>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct SpaceDef {
    domain: String,
}

pub fn ensure_builtin(conn: &Connection) -> Result<()> {
    for (id, domain) in BUILTIN {
        conn.execute(
            "INSERT OR IGNORE INTO space (id, domain) VALUES (?1, ?2)",
            params![id, domain],
        )?;
    }
    Ok(())
}

/// Loads spaces and replaces all path rules. Returns (spaces, rules).
pub fn load(conn: &Connection, file: &Path) -> Result<(usize, usize)> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
    let tx = conn.unchecked_transaction()?;
    ensure_builtin(&tx)?;
    for (id, def) in &cfg.spaces {
        if def.domain == "global" || def.domain == INBOX {
            // the built-in spaces may be listed; nothing else may join their domains
            if *id == def.domain {
                continue;
            }
            bail!("space {id}: domain {} is reserved", def.domain);
        }
        tx.execute(
            "INSERT INTO space (id, domain) VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET domain = excluded.domain",
            params![id, def.domain],
        )
        .with_context(|| format!("space {id}"))?;
        // every domain gets a space its projects share
        tx.execute(
            "INSERT OR IGNORE INTO space (id, domain) VALUES (?1, ?2)",
            params![format!("{}/_shared", def.domain), def.domain],
        )?;
    }
    tx.execute("DELETE FROM path_rule", [])?;
    for (prefix, space) in &cfg.paths {
        let known: Option<String> = tx
            .query_row("SELECT id FROM space WHERE id = ?1", [space], |r| r.get(0))
            .optional()?;
        if known.is_none() {
            bail!("path {prefix} points to unknown space {space}");
        }
        tx.execute(
            "INSERT INTO path_rule (prefix, space_id) VALUES (?1, ?2)",
            params![canonical(&expand_home(prefix)), space],
        )?;
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('import_rules', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [serde_json::to_string(&cfg.import)?],
    )?;
    tx.commit()?;
    Ok((cfg.spaces.len(), cfg.paths.len()))
}

pub fn import_rules(conn: &Connection) -> Result<ImportRules> {
    let json: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'import_rules'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json
        .map(|j| serde_json::from_str(&j))
        .transpose()?
        .unwrap_or_default())
}

pub fn all(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM space ORDER BY id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Space of a session folder: the longest matching prefix wins, unknown -> inbox.
pub fn resolve(conn: &Connection, cwd: &Path) -> Result<String> {
    // symlinks and other spellings of the same folder resolve like the folder itself
    let cwd = canonical(&cwd.to_string_lossy());
    let mut stmt = conn.prepare("SELECT prefix, space_id FROM path_rule")?;
    let rules = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut best: Option<(String, String)> = None;
    for rule in rules {
        let (prefix, space) = rule?;
        let matches = cwd == prefix.as_str()
            || cwd.starts_with(&format!("{}/", prefix.trim_end_matches('/')));
        if matches && best.as_ref().is_none_or(|(p, _)| prefix.len() > p.len()) {
            best = Some((prefix, space));
        }
    }
    Ok(best.map_or_else(|| INBOX.to_string(), |(_, s)| s))
}

pub fn domain(conn: &Connection, space: &str) -> Result<String> {
    conn.query_row("SELECT domain FROM space WHERE id = ?1", [space], |r| {
        r.get(0)
    })
    .optional()?
    .with_context(|| format!("unknown space {space}"))
}

/// What a session in `space` sees by default: itself, its domain's `_shared`, and `global`.
pub fn visible(conn: &Connection, space: &str) -> Result<Vec<String>> {
    let domain = domain(conn, space)?;
    let mut out = vec![space.to_string()];
    if domain != "global" && domain != INBOX {
        out.push(format!("{domain}/_shared"));
    }
    if domain != "global" {
        out.push("global".into());
    }
    out.dedup();
    Ok(out)
}

/// Every space of `space`'s domain plus `global`: the widest a session may search.
pub fn reachable(conn: &Connection, space: &str) -> Result<Vec<String>> {
    let domain = domain(conn, space)?;
    let mut stmt =
        conn.prepare("SELECT id FROM space WHERE domain IN (?1, 'global') ORDER BY id")?;
    let ids = stmt
        .query_map([&domain], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The real path when it exists (symlinks resolved), otherwise the path as given.
fn canonical(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.trim_end_matches('/').to_string())
}

pub fn expand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}
