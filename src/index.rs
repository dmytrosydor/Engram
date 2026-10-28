//! Keeps `item_vec` / `chunk_vec` filled: embeds rows that have no vector yet.

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::embed;

const BATCH: usize = 16;

/// Text that represents an item in vector space.
pub fn item_text(title: &str, body: &str) -> String {
    format!("{title}\n\n{body}")
}

/// Returns (items, chunks) embedded.
pub fn embed_missing(
    conn: &Connection,
    mut progress: impl FnMut(usize, usize),
) -> Result<(usize, usize)> {
    let items = pending(
        conn,
        "SELECT id, title, body FROM item WHERE id NOT IN (SELECT item_id FROM item_vec) ORDER BY id",
        |r| {
            Ok((
                r.get(0)?,
                item_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?),
            ))
        },
    )?;
    let chunks = pending(
        conn,
        "SELECT id, text FROM chunk WHERE id NOT IN (SELECT chunk_id FROM chunk_vec) ORDER BY id",
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let total = items.len() + chunks.len();
    let mut done = 0;
    for (rows, table, key) in [
        (&items, "item_vec", "item_id"),
        (&chunks, "chunk_vec", "chunk_id"),
    ] {
        for batch in rows.chunks(BATCH) {
            let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
            let vectors = embed::embed(&texts)?;
            let tx = conn.unchecked_transaction()?;
            for ((id, _), v) in batch.iter().zip(vectors) {
                tx.execute(
                    &format!("INSERT INTO {table} ({key}, emb) VALUES (?1, ?2)"),
                    params![id, embed::to_blob(&v)],
                )?;
            }
            tx.commit()?;
            done += batch.len();
            progress(done, total);
        }
    }
    Ok((items.len(), chunks.len()))
}

/// Replaces an item's vector after its title or body changed.
pub fn reembed_item(conn: &Connection, id: i64) -> Result<()> {
    let text: String = conn.query_row("SELECT title, body FROM item WHERE id = ?1", [id], |r| {
        Ok(item_text(&r.get::<_, String>(0)?, &r.get::<_, String>(1)?))
    })?;
    let v = embed::embed_one(&text)?;
    conn.execute("DELETE FROM item_vec WHERE item_id = ?1", [id])?;
    conn.execute(
        "INSERT INTO item_vec (item_id, emb) VALUES (?1, ?2)",
        params![id, embed::to_blob(&v)],
    )?;
    Ok(())
}

fn pending(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&rusqlite::Row) -> rusqlite::Result<(i64, String)>,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], map)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
