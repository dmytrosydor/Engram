//! Writing items: hashing, titles, the change log.

use anyhow::{Context, Result};
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
    /// Hash the body under this scope instead of `space_id` (see [`proposal_scope`]).
    pub hash_scope: Option<String>,
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

/// Whitespace-insensitive hash of the body within a space, so reformatted copies
/// still collide there while identical text in another space (another domain) is
/// stored on its own and never reveals that the other copy exists.
pub fn content_hash(space: &str, body: &str) -> String {
    let normalised = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = Sha256::digest(format!("{space}\n{normalised}").as_bytes());
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

/// Hash scope of a global record a session proposed and the user has not accepted yet.
///
/// Every session can write to `global`, so if hidden proposals shared global's hash
/// scope, a session writing the same text would collide with another domain's hidden
/// proposal and learn that it exists (and its id). Until it is accepted, a proposal
/// only collides with proposals from its own domain; accepting it rehashes it under
/// `global` ([`activate_global`]).
pub fn proposal_scope(domain: &str) -> String {
    format!("global@{domain}")
}

pub fn insert_item(conn: &Connection, item: &NewItem, actor: &str) -> Result<Inserted> {
    let hash = content_hash(
        item.hash_scope.as_deref().unwrap_or(&item.space_id),
        &item.body,
    );
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
/// Moves an item to another space and returns the space it came from.
pub fn move_item(conn: &Connection, id: i64, to: &str, actor: &str) -> Result<String> {
    crate::spaces::domain(conn, to)?;
    let before = item_json(conn, id)?;
    let from = before["space_id"].as_str().unwrap_or_default().to_string();
    if from == to {
        return Ok(from);
    }
    let body = before["body"].as_str().unwrap_or_default();
    conn.execute(
        "UPDATE item SET space_id = ?2, content_hash = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, to, content_hash(to, body), now(conn)?],
    )
    .with_context(|| format!("#{id}: the same text is already stored in {to}"))?;
    log(
        conn,
        actor,
        "move",
        Some(id),
        Some(&before),
        Some(&item_json(conn, id)?),
    )?;
    Ok(from)
}

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

/// Makes a proposed record live under its space's normal hash scope. If the space
/// already holds the same text, that record stays (restored if it was retired) and the
/// proposal is deleted, since the unique hash cannot hold both. Returns the live id.
pub fn activate(conn: &Connection, id: i64, actor: &str) -> Result<i64> {
    let before = item_json(conn, id)?;
    let space = before["space_id"].as_str().context("item without space")?;
    let body = before["body"].as_str().context("item without body")?;
    let hash = content_hash(space, body);
    let holder: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, status FROM item WHERE content_hash = ?1 AND id <> ?2",
            params![hash, id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((other, status)) = holder else {
        conn.execute(
            "UPDATE item SET content_hash = ?2 WHERE id = ?1",
            params![id, hash],
        )?;
        set_status(conn, id, "active", None, actor)?;
        return Ok(id);
    };
    conn.execute(
        "UPDATE item SET superseded_by = ?2 WHERE superseded_by = ?1",
        params![id, other],
    )?;
    conn.execute("DELETE FROM item_vec WHERE item_id = ?1", [id])?;
    conn.execute("DELETE FROM item WHERE id = ?1", [id])?;
    log(
        conn,
        actor,
        "activate:existing",
        Some(id),
        Some(&before),
        None,
    )?;
    if status != "active" {
        conn.execute(
            "UPDATE item SET superseded_by = NULL WHERE id = ?1",
            [other],
        )?;
        conn.execute(
            "DELETE FROM edge WHERE dst = ?1 AND type = 'supersedes'",
            [other],
        )?;
        set_status(conn, other, "active", None, actor)?;
    }
    Ok(other)
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
