//! The review queue: proposals the daily pass makes and a person accepts or rejects.
//!
//! Ops:
//! - `new`: proposal `{kind, title, body, space, source}` becomes an item.
//! - `merge`: proposal `{keep, drop}`; `drop` is superseded by `keep`.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::memory::{self, Write, Written};
use crate::{embed, index, store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProposal {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub space: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    pub op: String,
    pub item_ids: Vec<i64>,
    pub proposal: Value,
    pub reason: Option<String>,
    pub status: String,
    pub created_at: String,
}

pub fn add(
    conn: &Connection,
    op: &str,
    item_ids: &[i64],
    proposal: &Value,
    reason: &str,
    status: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO review (op, item_ids, proposal, reason, status, created_at, decided_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, CASE WHEN ?5 = 'open' THEN NULL ELSE ?6 END)",
        params![
            op,
            serde_json::to_string(item_ids)?,
            proposal.to_string(),
            reason,
            status,
            store::now(conn)?
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// True if this pair was ever queued, whatever the outcome.
pub fn pair_known(conn: &Connection, a: i64, b: i64) -> Result<bool> {
    let ids = serde_json::to_string(&[a.min(b), a.max(b)])?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM review WHERE op = 'merge' AND item_ids = ?1",
            [ids],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn list(conn: &Connection, status: &str, limit: usize) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare(
        "SELECT id, op, item_ids, proposal, reason, status, created_at FROM review
         WHERE status = ?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![status, limit as i64], |r| {
        Ok(Entry {
            id: r.get(0)?,
            op: r.get(1)?,
            item_ids: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
            proposal: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
            reason: r.get(4)?,
            status: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn entry(conn: &Connection, id: i64) -> Result<Entry> {
    let mut stmt = conn.prepare(
        "SELECT id, op, item_ids, proposal, reason, status, created_at FROM review WHERE id = ?1",
    )?;
    stmt.query_row([id], |r| {
        Ok(Entry {
            id: r.get(0)?,
            op: r.get(1)?,
            item_ids: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
            proposal: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
            reason: r.get(4)?,
            status: r.get(5)?,
            created_at: r.get(6)?,
        })
    })
    .optional()?
    .with_context(|| format!("no review #{id}"))
}

/// Applies a proposal. `edited` replaces a `new` proposal's text before saving.
pub fn accept(
    conn: &Connection,
    id: i64,
    edited: Option<NewProposal>,
    actor: &str,
) -> Result<String> {
    let e = entry(conn, id)?;
    if e.status != "open" {
        bail!("review #{id} is already {}", e.status);
    }
    let outcome = match e.op.as_str() {
        "new" => {
            let p = match edited {
                Some(p) => p,
                None => serde_json::from_value::<NewProposal>(e.proposal.clone())?,
            };
            let vector = embed::embed_one(&index::item_text(&p.title, &p.body)).ok();
            let visible = vec![p.space.clone()];
            let written = memory::write(
                conn,
                &Write {
                    kind: &p.kind,
                    title: &p.title,
                    body: &p.body,
                    space: &p.space,
                    source: p.source.as_deref(),
                    valid_until: None,
                    mode: Some("add"),
                    vector: vector.as_deref(),
                    visible: &visible,
                },
                actor,
            )?;
            if let Written::Created(new) = written {
                crate::core::rebuild(conn, &p.space)?;
                conn.execute(
                    "UPDATE review SET item_ids = ?2 WHERE id = ?1",
                    params![id, format!("[{new}]")],
                )?;
            }
            written.message()
        }
        "merge" => {
            let keep = e.proposal["keep"].as_i64().context("merge without keep")?;
            let drop = e.proposal["drop"].as_i64().context("merge without drop")?;
            store::set_status(conn, drop, "superseded", Some(keep), actor)?;
            conn.execute(
                "INSERT OR IGNORE INTO edge (src, dst, type, confirmed) VALUES (?1, ?2, 'supersedes', 1)",
                params![keep, drop],
            )?;
            format!("#{drop} superseded by #{keep}")
        }
        other => bail!("unknown review op {other}"),
    };
    decide(conn, id, "accepted")?;
    Ok(outcome)
}

pub fn reject(conn: &Connection, id: i64) -> Result<()> {
    let e = entry(conn, id)?;
    if e.status != "open" {
        bail!("review #{id} is already {}", e.status);
    }
    decide(conn, id, "rejected")
}

fn decide(conn: &Connection, id: i64, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE review SET status = ?2, decided_at = ?3 WHERE id = ?1",
        params![id, status, store::now(conn)?],
    )?;
    Ok(())
}
