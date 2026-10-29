//! The space core: a short, prebuilt digest injected at session start.
//!
//! Built deterministically from the database (no model, no LLM), stored in
//! `space.core`, and served by the SessionStart hook in milliseconds.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params_from_iter};

use crate::memory::Hit;
use crate::spaces;
use crate::store;

/// Claude Code caps hook output at 10 000 characters; stay well under.
pub const MAX_CHARS: usize = 6000;

struct Section {
    title: &'static str,
    sql: &'static str,
    limit: usize,
}

/// Queries run over `v`, the items of the visible spaces; `:today` and `:space` are filled in.
const SECTIONS: [Section; 4] = [
    Section {
        title: "rules & preferences",
        // the project's own rules first, global ones fill the rest
        // unreviewed rules count only in their own project
        sql: "SELECT * FROM v WHERE kind IN ('rule', 'preference')
              AND (status = 'active' OR (status = 'proposed' AND space_id = :space))
              ORDER BY pinned DESC, space_id = :space DESC, status = 'active' DESC, hits DESC,
                       COALESCE(updated_at, created_at) DESC",
        limit: 15,
    },
    Section {
        title: "current state",
        sql: "SELECT * FROM v WHERE kind IN ('state', 'task') AND status = 'active'
              AND (valid_until IS NULL OR valid_until >= :today)
              AND (pinned = 1 OR julianday(:today) - julianday(created_at) <= 30)
              ORDER BY pinned DESC, created_at DESC",
        limit: 5,
    },
    Section {
        title: "recent decisions",
        sql: "SELECT * FROM v WHERE kind = 'decision' AND status = 'active'
              AND julianday(:today) - julianday(created_at) <= 14
              ORDER BY created_at DESC",
        limit: 10,
    },
    Section {
        title: "often opened",
        // popularity alone must not lift unreviewed imports into every session start, and
        // state and events go stale: a week-old checkpoint is noise however often it was read
        sql: "SELECT * FROM v WHERE hits > 0 AND status = 'active'
              AND (pinned = 1 OR kind NOT IN ('state', 'event')
                   OR (julianday(:today) - julianday(COALESCE(updated_at, created_at)) <= 7
                       AND (valid_until IS NULL OR valid_until >= :today)))
              ORDER BY hits DESC, last_hit DESC",
        limit: 10,
    },
];

/// Builds the core text for a session space (it sees its visible spaces).
pub fn build(conn: &Connection, space: &str) -> Result<String> {
    let visible = spaces::visible(conn, space)?;
    let today = store::today(conn)?;
    let mut text = format!(
        "engram memory · space {space} (sees {}) · tools: search / get / write\n\
         line = #id kind space date | title; kinds r rule p pref d decision f fact g gotcha l lesson s state t task; ? after the kind = not reviewed yet",
        visible[1..].join(", ")
    );
    let mut seen = std::collections::HashSet::new();
    let marks = vec!["?"; visible.len()].join(", ");
    for section in &SECTIONS {
        let sql = format!(
            "WITH v AS (SELECT * FROM item WHERE space_id IN ({marks})) {}",
            section
                .sql
                .replace(":today", &format!("'{today}'"))
                .replace(":space", &format!("'{}'", space.replace('\'', "''")))
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(&visible), |r| {
            Ok(Hit {
                id: r.get("id")?,
                kind: r.get("kind")?,
                space: r.get("space_id")?,
                status: r.get("status")?,
                created: r.get("created_at")?,
                title: r.get("title")?,
                score: 0.0,
            })
        })?;
        let mut lines = Vec::new();
        for hit in rows {
            let hit = hit?;
            if lines.len() == section.limit {
                break;
            }
            if seen.insert(hit.id) {
                lines.push(hit.line());
            }
        }
        if lines.is_empty() {
            continue;
        }
        let block = format!("\n\n{}:\n{}", section.title, lines.join("\n"));
        if text.chars().count() + block.chars().count() > MAX_CHARS {
            break;
        }
        text.push_str(&block);
    }
    Ok(text)
}

/// Rebuilds and stores the core of one space. Returns its size in characters.
pub fn rebuild(conn: &Connection, space: &str) -> Result<usize> {
    let text = build(conn, space)?;
    let chars = text.chars().count();
    conn.execute(
        "UPDATE space SET core = ?2, core_tokens = ?3, core_built_at = ?4 WHERE id = ?1",
        rusqlite::params![
            space,
            text,
            estimate_tokens(&text) as i64,
            store::now(conn)?
        ],
    )?;
    Ok(chars)
}

pub fn rebuild_all(conn: &Connection) -> Result<Vec<(String, usize)>> {
    spaces::all(conn)?
        .into_iter()
        .map(|s| rebuild(conn, &s).map(|n| (s, n)))
        .collect()
}

/// The stored core, built on first use.
/// The stored core of a space, built on first use; empty for an unknown space.
pub fn get(conn: &Connection, space: &str) -> Result<String> {
    let stored: Option<Option<String>> = conn
        .query_row("SELECT core FROM space WHERE id = ?1", [space], |r| {
            r.get(0)
        })
        .optional()?;
    match stored {
        None => Ok(String::new()),
        Some(Some(text)) => Ok(text),
        Some(None) => {
            rebuild(conn, space)?;
            Ok(
                conn.query_row("SELECT core FROM space WHERE id = ?1", [space], |r| {
                    r.get(0)
                })?,
            )
        }
    }
}

/// Rough token count for mixed natural-language and code text.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(3)
}
