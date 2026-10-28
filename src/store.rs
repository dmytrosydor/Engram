//! Writing items: hashing, titles, the change log.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use sha2::{Digest, Sha256};

pub const TITLE_MAX: usize = 120;
pub const BODY_MAX: usize = 1500;

pub const KINDS: [&str; 10] = [
    "rule",
    "preference",
    "decision",
    "fact",
    "gotcha",
    "lesson",
    "state",
    "task",
    "person",
    "event",
];

#[derive(Debug, Default, Clone)]
pub struct NewItem {
    pub space_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub source: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub valid_until: Option<String>,
    pub legacy_id: Option<String>,
}

pub enum Inserted {
    New(i64),
    /// Same content already stored under this id.
    Duplicate(i64),
}

pub fn now(conn: &Connection) -> Result<String> {
    Ok(
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |r| {
            r.get(0)
        })?,
    )
}

pub fn today(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT date('now')", [], |r| r.get(0))?)
}

/// Whitespace-insensitive hash of the body, so reformatted copies still collide.
pub fn content_hash(body: &str) -> String {
    let normalised = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = Sha256::digest(normalised.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// First meaningful line without markdown decoration, at most TITLE_MAX chars.
pub fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches(['#', '-', '*', '>', ' '])
                .replace("**", "")
        })
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    clip(&line, TITLE_MAX)
}

pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

pub fn insert_item(conn: &Connection, item: &NewItem, actor: &str) -> Result<Inserted> {
    let hash = content_hash(&item.body);
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM item WHERE content_hash = ?1",
            [&hash],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(Inserted::Duplicate(id));
    }
    let now = now(conn)?;
    conn.execute(
        "INSERT INTO item (space_id, kind, title, body, status, source, created_at, updated_at,
                           valid_until, content_hash, legacy_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            item.space_id,
            item.kind,
            item.title,
            item.body,
            item.status,
            item.source,
            item.created_at.as_deref().unwrap_or(&now),
            item.updated_at,
            item.valid_until,
            hash,
            item.legacy_id,
        ],
    )?;
    let id = conn.last_insert_rowid();
    log(
        conn,
        actor,
        "create",
        Some(id),
        None,
        Some(&item_json(conn, id)?),
    )?;
    Ok(Inserted::New(id))
}

pub fn item_json(conn: &Connection, id: i64) -> Result<serde_json::Value> {
    Ok(conn.query_row(
        "SELECT space_id, kind, title, body, status, pinned, valid_until, superseded_by, source
         FROM item WHERE id = ?1",
        [id],
        |r| {
            Ok(json!({
                "space_id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                "title": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                "status": r.get::<_, String>(4)?, "pinned": r.get::<_, i64>(5)?,
                "valid_until": r.get::<_, Option<String>>(6)?,
                "superseded_by": r.get::<_, Option<i64>>(7)?, "source": r.get::<_, Option<String>>(8)?,
            }))
        },
    )?)
}

pub fn log(
    conn: &Connection,
    actor: &str,
    op: &str,
    item_id: Option<i64>,
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO log (at, actor, op, item_id, before, after) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            now(conn)?,
            actor,
            op,
            item_id,
            before.map(|v| v.to_string()),
            after.map(|v| v.to_string())
        ],
    )?;
    Ok(())
}

/// Changes status (and optionally the replacement) with a log entry.
pub fn set_status(
    conn: &Connection,
    id: i64,
    status: &str,
    superseded_by: Option<i64>,
    actor: &str,
) -> Result<()> {
    let before = item_json(conn, id)?;
    conn.execute(
        "UPDATE item SET status = ?2, superseded_by = COALESCE(?3, superseded_by), updated_at = ?4
         WHERE id = ?1",
        params![id, status, superseded_by, now(conn)?],
    )?;
    let after = item_json(conn, id)?;
    log(
        conn,
        actor,
        &format!("status:{status}"),
        Some(id),
        Some(&before),
        Some(&after),
    )
}

/// Edits an item by hand (UI). Changing title or body drops its vector until
/// the caller re-embeds it.
pub fn edit(
    conn: &Connection,
    id: i64,
    title: &str,
    body: &str,
    kind: &str,
    space: &str,
    actor: &str,
) -> Result<bool> {
    if !KINDS.contains(&kind) {
        anyhow::bail!("unknown kind {kind}");
    }
    let before = item_json(conn, id)?;
    let text_changed = before["title"] != title || before["body"] != body;
    conn.execute(
        "UPDATE item SET title = ?2, body = ?3, kind = ?4, space_id = ?5, content_hash = ?6, updated_at = ?7
         WHERE id = ?1",
        params![id, title, body, kind, space, content_hash(body), now(conn)?],
    )?;
    if text_changed {
        conn.execute("DELETE FROM item_vec WHERE item_id = ?1", [id])?;
    }
    log(
        conn,
        actor,
        "edit",
        Some(id),
        Some(&before),
        Some(&item_json(conn, id)?),
    )?;
    Ok(text_changed)
}

pub fn set_pinned(conn: &Connection, id: i64, pinned: bool, actor: &str) -> Result<()> {
    let before = item_json(conn, id)?;
    conn.execute(
        "UPDATE item SET pinned = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, pinned, now(conn)?],
    )?;
    log(
        conn,
        actor,
        if pinned { "pin" } else { "unpin" },
        Some(id),
        Some(&before),
        Some(&item_json(conn, id)?),
    )
}
