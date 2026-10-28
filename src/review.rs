//! The review queue: proposals the daily pass makes and a person accepts or rejects.
//!
//! Ops:
//! - `new`: proposal `{kind, title, body, space, source}` becomes an item.
//! - `merge`: proposal `{keep, drop}`; `drop` is superseded by `keep`.
//! - `move`: proposal `{space}`; the item in `item_ids` moves there.
//! - `activate`: the proposed item in `item_ids` becomes active.

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

/// True if a `new` proposal with this title for this space was ever queued.
pub fn proposal_known(conn: &Connection, space: &str, title: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM review WHERE op = 'new'
             AND json_extract(proposal, '$.space') = ?1 AND json_extract(proposal, '$.title') = ?2",
            params![space, title],
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
    // claim it first, so two clicks (or a click and the CLI) cannot both apply it
    decide(conn, id, "accepted")?;
    let outcome = apply(conn, &e, edited, actor);
    if outcome.is_err() {
        conn.execute(
            "UPDATE review SET status = 'open', decided_at = NULL WHERE id = ?1",
            [id],
        )?;
    }
    outcome
}

fn apply(conn: &Connection, e: &Entry, edited: Option<NewProposal>, actor: &str) -> Result<String> {
    let id = e.id;
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
                    status: "active",
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
            for id in [keep, drop] {
                let status: String =
                    conn.query_row("SELECT status FROM item WHERE id = ?1", [id], |r| r.get(0))?;
                if status != "active" && status != "proposed" {
                    bail!("#{id} is {status} now; reject this proposal instead");
                }
            }
            store::set_status(conn, drop, "superseded", Some(keep), actor)?;
            conn.execute(
                "INSERT OR IGNORE INTO edge (src, dst, type, confirmed) VALUES (?1, ?2, 'supersedes', 1)",
                params![keep, drop],
            )?;
            format!("#{drop} superseded by #{keep}")
        }
        "move" => {
            let id = *e.item_ids.first().context("move without item")?;
            let to = e.proposal["space"].as_str().context("move without space")?;
            let before = store::item_json(conn, id)?;
            let from = before["space_id"].as_str().unwrap_or_default().to_string();
            conn.execute(
                "UPDATE item SET space_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, to, store::now(conn)?],
            )?;
            store::log(
                conn,
                actor,
                "move",
                Some(id),
                Some(&before),
                Some(&store::item_json(conn, id)?),
            )?;
            crate::core::rebuild_all(conn)?;
            format!("#{id} moved {from} -> {to}")
        }
        "activate" => {
            let id = *e.item_ids.first().context("activate without item")?;
            store::set_status(conn, id, "active", None, actor)?;
            crate::core::rebuild_all(conn)?;
            format!("#{id} is active")
        }
        other => bail!("unknown review op {other}"),
    };
    Ok(outcome)
}

pub fn reject(conn: &Connection, id: i64) -> Result<()> {
    let e = entry(conn, id)?;
    if e.status != "open" {
        bail!("review #{id} is already {}", e.status);
    }
    decide(conn, id, "rejected")
}

/// Closes an open review; fails if someone else decided it first.
fn decide(conn: &Connection, id: i64, status: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE review SET status = ?2, decided_at = ?3 WHERE id = ?1 AND status = 'open'",
        params![id, status, store::now(conn)?],
    )?;
    if changed == 0 {
        bail!("review #{id} was already decided");
    }
    Ok(())
}
