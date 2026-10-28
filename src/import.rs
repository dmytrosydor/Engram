//! One-off import from Memora and Claude Code auto-memory.
//!
//! Both sources are only read. Classification comes from manifest CSVs prepared
//! during planning (one row per source record); rows missing from a manifest land in
//! the inbox as `proposed`. Re-running is safe: anything with a known `legacy_id`
//! or document URI is skipped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;

use crate::spaces::{self, INBOX, ImportRules};
use crate::store::{self, Inserted, NewItem};

const ACTOR: &str = "import";

#[derive(Debug, Default)]
pub struct Report {
    pub items: usize,
    pub duplicates: usize,
    pub already: usize,
    pub dropped: usize,
    pub docs: usize,
    pub chunks: usize,
    pub edges: usize,
    pub unmapped: usize,
}

// ---------- Memora ----------

#[derive(Deserialize)]
struct MemoraRow {
    id: i64,
    space: String,
    kind: String,
    action: String,
}

struct Memory {
    id: i64,
    content: String,
    metadata: Value,
    tags: Vec<String>,
    created_at: String,
    updated_at: Option<String>,
}

pub fn memora(conn: &Connection, source: &Path, manifest: &Path) -> Result<Report> {
    let plan: HashMap<i64, MemoraRow> = read_csv::<MemoraRow>(manifest)?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let src = Connection::open_with_flags(
        format!("file:{}?mode=ro", source.display()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open {}", source.display()))?;
    let memories = read_memories(&src)?;
    let today = store::today(conn)?;
    let mapper = Mapper::new(conn)?;
    let mut report = Report::default();
    let mut ids: HashMap<i64, i64> = HashMap::new(); // memora id -> item id
    let mut documents: BTreeMap<String, Vec<&Memory>> = BTreeMap::new();

    let tx = conn.unchecked_transaction()?;
    for m in &memories {
        let Some(row) = plan.get(&m.id) else {
            report.unmapped += 1;
            let item = NewItem {
                space_id: INBOX.into(),
                kind: "fact".into(),
                status: "proposed".into(),
                ..memora_item(m)
            };
            record(&tx, &item, &mut report, |id| {
                ids.insert(m.id, id);
            })?;
            continue;
        };
        if row.action == "documents" {
            let key = m.metadata["document_key"]
                .as_str()
                .map_or_else(|| format!("memora:{}", m.id), |k| format!("memora-doc:{k}"));
            documents.entry(key).or_default().push(m);
            continue;
        }
        let space = mapper.space(&row.space, &row.kind);
        let mut item = NewItem {
            space_id: space,
            ..memora_item(m)
        };
        match row.action.as_str() {
            "migrate-active" => {
                item.kind = kind_from(&row.kind).into();
                item.status = "active".into();
            }
            "review-old-import" | "classify" => {
                item.kind = kind_from(&row.kind).into();
                item.status = "proposed".into();
            }
            "archive-episodic" => {
                item.kind = "event".into();
                item.status = "archived".into();
            }
            "review-stale-state" => {
                item.kind = "state".into();
                item.status = "active".into();
                item.valid_until = Some(today.clone());
            }
            "tasks-review" => {
                item.kind = "task".into();
                let closed = m
                    .tags
                    .iter()
                    .any(|t| matches!(t.as_str(), "resolved" | "done" | "closed"));
                item.status = if closed { "archived" } else { "proposed" }.into();
            }
            other => anyhow::bail!("memora {}: unknown action {other}", m.id),
        }
        record(&tx, &item, &mut report, |id| {
            ids.insert(m.id, id);
        })?;
    }

    for (uri, parts) in &documents {
        import_document(&tx, uri, parts, &plan, &mapper, &mut report)?;
    }
    report.edges = import_edges(&tx, &src, &ids)?;
    import_tombstones(&tx, &src)?;
    tx.commit()?;
    Ok(report)
}

fn read_memories(src: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = src.prepare(
        "SELECT id, content, COALESCE(metadata, '{}'), COALESCE(tags, '[]'), created_at, updated_at
         FROM memories ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Memory {
            id: r.get(0)?,
            content: r.get(1)?,
            metadata: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
            tags: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn memora_item(m: &Memory) -> NewItem {
    NewItem {
        title: store::title_from(&m.content),
        body: m.content.trim().to_string(),
        source: Some(format!("memora:{}", m.id)),
        created_at: Some(m.created_at.clone()),
        updated_at: m.updated_at.clone(),
        legacy_id: Some(format!("memora:{}", m.id)),
        ..NewItem::default()
    }
}

/// Maps manifest spaces onto spaces that exist; anything unresolvable goes to the inbox.
struct Mapper {
    rules: ImportRules,
    known: HashSet<String>,
}

impl Mapper {
    fn new(conn: &Connection) -> Result<Self> {
        Ok(Self {
            rules: spaces::import_rules(conn)?,
            known: spaces::all(conn)?.into_iter().collect(),
        })
    }

    fn space(&self, manifest_space: &str, source_kind: &str) -> String {
        let r = &self.rules;
        let space = match manifest_space {
            "unknown" if matches!(source_kind, "todo" | "issue") => {
                r.unknown_tasks.as_deref().or(r.unknown.as_deref())
            }
            "unknown" => r.unknown.as_deref(),
            "" => None,
            s => Some(s),
        };
        let space = space.map(|s| r.aliases.get(s).map_or(s, String::as_str));
        match space {
            Some(s) if self.known.contains(s) => s.to_string(),
            _ => INBOX.to_string(),
        }
    }
}

fn kind_from(source_kind: &str) -> &'static str {
    match source_kind {
        "checkpoint" | "day-summary" | "incident" => "event",
        "lesson" => "lesson",
        "gotcha" => "gotcha",
        "decision" => "decision",
        "feedback" | "incident-to-rule" | "rule" => "rule",
        "todo" | "issue" | "ticket" => "task",
        "user" => "preference",
        _ => "fact",
    }
}

fn import_document(
    conn: &Connection,
    uri: &str,
    parts: &[&Memory],
    plan: &HashMap<i64, MemoraRow>,
    mapper: &Mapper,
    report: &mut Report,
) -> Result<()> {
    let exists: Option<i64> = conn
        .query_row("SELECT id FROM doc WHERE uri = ?1", [uri], |r| r.get(0))
        .optional()?;
    if exists.is_some() {
        report.already += 1;
        return Ok(());
    }
    let mut parts = parts.to_vec();
    parts.sort_by_key(|m| (m.metadata["ordinal"].as_i64().unwrap_or(i64::MAX), m.id));
    let first = parts[0];
    let space = mapper.space(&plan[&first.id].space, &plan[&first.id].kind);
    let title = parts
        .iter()
        .find(|m| plan[&m.id].kind == "document")
        .map(|m| store::title_from(&m.content))
        .or_else(|| first.metadata["document_key"].as_str().map(str::to_string))
        .unwrap_or_else(|| store::title_from(&first.content));
    conn.execute(
        "INSERT INTO doc (space_id, title, uri, added_at) VALUES (?1, ?2, ?3, ?4)",
        params![space, title, uri, first.created_at],
    )?;
    let doc_id = conn.last_insert_rowid();
    for (ord, m) in parts.iter().enumerate() {
        conn.execute(
            "INSERT INTO chunk (doc_id, ord, text) VALUES (?1, ?2, ?3)",
            params![doc_id, ord as i64, m.content.trim()],
        )?;
    }
    report.docs += 1;
    report.chunks += parts.len();
    Ok(())
}

/// Explicit links only; Memora's automatic `related_to` is noise.
fn import_edges(conn: &Connection, src: &Connection, ids: &HashMap<i64, i64>) -> Result<usize> {
    let mut stmt = src.prepare("SELECT memory_id, related FROM memories_crossrefs")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut n = 0;
    for row in rows {
        let (from, related) = row?;
        let links: Vec<Value> = related
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for link in links {
            let (Some(to), Some(kind)) = (link["id"].as_i64(), link["edge_type"].as_str()) else {
                continue;
            };
            let (Some(&a), Some(&b)) = (ids.get(&from), ids.get(&to)) else {
                continue;
            };
            let (src_id, dst_id, edge) = match kind {
                "supersedes" => (a, b, "supersedes"),
                "superseded_by" => (b, a, "supersedes"),
                "contradicts" => (a, b, "contradicts"),
                "extends" => (a, b, "derived_from"),
                "implements" | "references" => (a, b, "about"),
                _ => continue,
            };
            if src_id == dst_id {
                continue;
            }
            n += conn.execute(
                "INSERT OR IGNORE INTO edge (src, dst, type, confirmed) VALUES (?1, ?2, ?3, 1)",
                params![src_id, dst_id, edge],
            )?;
            if edge == "supersedes" {
                conn.execute(
                    "UPDATE item SET status = 'superseded', superseded_by = ?1
                     WHERE id = ?2 AND status IN ('active', 'proposed')",
                    params![src_id, dst_id],
                )?;
            }
        }
    }
    Ok(n)
}

/// Deleted Memora content is recorded in the log (hash, id, reason) for the record;
/// nothing checks it on insert yet.
fn import_tombstones(conn: &Connection, src: &Connection) -> Result<()> {
    let mut stmt =
        src.prepare("SELECT content_hash, memory_id, reason, created_at FROM tombstones")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (hash, id, reason, at) = row?;
        let after = serde_json::json!({ "memora_hash": hash, "memora_id": id, "reason": reason });
        let seen: Option<i64> = conn
            .query_row(
                "SELECT id FROM log WHERE op = 'tombstone' AND json_extract(after, '$.memora_hash') = ?1",
                [&hash],
                |r| r.get(0),
            )
            .optional()?;
        if seen.is_none() {
            conn.execute(
                "INSERT INTO log (at, actor, op, after) VALUES (?1, ?2, 'tombstone', ?3)",
                params![at, ACTOR, after.to_string()],
            )?;
        }
    }
    Ok(())
}

// ---------- auto-memory ----------

#[derive(Deserialize)]
struct AutoRow {
    claude_project_dir: String,
    file: String,
    #[serde(rename = "type")]
    kind: String,
    proposed_space: String,
    action: String,
}

pub fn auto_memory(conn: &Connection, projects: &Path, inventory: &Path) -> Result<Report> {
    let rows = read_csv::<AutoRow>(inventory)?;
    let mapper = Mapper::new(conn)?;
    let mut report = Report::default();
    let tx = conn.unchecked_transaction()?;
    for row in &rows {
        if row.action.starts_with("drop") || row.file == "MEMORY.md" {
            report.dropped += 1;
            continue;
        }
        // manifest names are folder and file names, never paths
        let plain = |n: &str| !n.is_empty() && !n.contains('/') && n != ".." && n != ".";
        if !plain(&row.claude_project_dir) || !plain(&row.file) {
            report.dropped += 1;
            continue;
        }
        let path = projects
            .join(&row.claude_project_dir)
            .join("memory")
            .join(&row.file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            report.dropped += 1; // removed since the inventory was taken
            continue;
        };
        let (front, body) = split_frontmatter(&text);
        if body.trim().is_empty() {
            report.dropped += 1;
            continue;
        }
        let title = front
            .get("description")
            .or_else(|| front.get("name"))
            .map(|s| store::clip(s, store::TITLE_MAX))
            .unwrap_or_else(|| store::title_from(body));
        let project = mapper.space(&row.proposed_space, &row.kind);
        let (space, kind, status) = match (row.action.as_str(), row.kind.as_str()) {
            ("global", _) | (_, "user") => ("global".to_string(), "preference", "active"),
            (a, _) if a.starts_with("archive") => (project, "event", "archived"),
            // global vs project is decided by the daily pass
            (a, _) if a.starts_with("classify") => (project, "rule", "proposed"),
            (_, "feedback") => (project, "rule", "active"),
            _ => (project, "fact", "active"),
        };
        let item = NewItem {
            space_id: space,
            kind: kind.into(),
            title,
            body: body.trim().to_string(),
            status: status.into(),
            source: Some(format!("auto:{}", row.file)),
            created_at: modified(&path),
            legacy_id: Some(format!("auto:{}/{}", row.claude_project_dir, row.file)),
            ..NewItem::default()
        };
        record(&tx, &item, &mut report, |_| {})?;
    }
    tx.commit()?;
    Ok(report)
}

/// `---`-delimited YAML-ish header; only flat `key: value` lines are needed.
fn split_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();
    let Some(rest) = text.strip_prefix("---\n") else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text);
    };
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                fields.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    let body = rest[end + 4..].trim_start_matches('-');
    (fields, body)
}

fn modified(path: &Path) -> Option<String> {
    let secs = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let conn = Connection::open_in_memory().ok()?;
    conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')",
        [secs as i64],
        |r| r.get(0),
    )
    .ok()
}

// ---------- shared ----------

fn record(
    conn: &Connection,
    item: &NewItem,
    report: &mut Report,
    on_new: impl FnOnce(i64),
) -> Result<()> {
    if let Some(legacy) = &item.legacy_id {
        let known: Option<i64> = conn
            .query_row("SELECT id FROM item WHERE legacy_id = ?1", [legacy], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(id) = known {
            report.already += 1;
            on_new(id);
            return Ok(());
        }
    }
    match store::insert_item(conn, item, ACTOR)? {
        Inserted::New(id) => {
            report.items += 1;
            on_new(id);
        }
        Inserted::Duplicate(_) => report.duplicates += 1,
    }
    Ok(())
}

fn read_csv<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let mut rdr =
        csv::Reader::from_path(path).with_context(|| format!("read {}", path.display()))?;
    rdr.deserialize()
        .collect::<std::result::Result<Vec<T>, _>>()
        .with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter() {
        let (f, body) = split_frontmatter(
            "---\nname: x\ndescription: \"Short\"\nmetadata:\n  type: feedback\n---\n\nBody\n",
        );
        assert_eq!(f["name"], "x");
        assert_eq!(f["description"], "Short");
        assert_eq!(f["type"], "feedback");
        assert_eq!(body.trim(), "Body");
        let (f, body) = split_frontmatter("no header");
        assert!(f.is_empty());
        assert_eq!(body, "no header");
    }
}
