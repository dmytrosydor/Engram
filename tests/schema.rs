use engram::db;
use rusqlite::{Connection, params};
use tempfile::TempDir;

const TABLES: [&str; 19] = [
    "chunk",
    "chunk_fts",
    "chunk_fts_word",
    "chunk_vec",
    "doc",
    "edge",
    "inbox",
    "item",
    "item_fts",
    "item_fts_word",
    "item_vec",
    "log",
    "meta",
    "path_rule",
    "recall",
    "review",
    "space",
    "tag",
    "tag_vocab",
];

/// Fresh database in a temp folder with one space. Keep the TempDir alive.
fn open() -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("engram.db");
    db::init(&path).unwrap();
    let conn = db::connect(&path).unwrap();
    conn.execute(
        "INSERT INTO space (id, domain) VALUES ('personal/notes', 'personal')",
        [],
    )
    .unwrap();
    (dir, conn)
}

fn insert_item(
    conn: &Connection,
    space: &str,
    kind: &str,
    title: &str,
    hash: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO item (space_id, kind, title, body, created_at, content_hash)
         VALUES (?1, ?2, ?3, ?3, '2026-10-28', ?4)",
        params![space, kind, title, hash],
    )?;
    Ok(conn.last_insert_rowid())
}

fn add_item(conn: &Connection, text: &str) -> i64 {
    insert_item(conn, "personal/notes", "fact", text, None).unwrap()
}

fn fts(conn: &Connection, table: &str, query: &str) -> Vec<i64> {
    let sql = format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid");
    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map([query], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn vector(first: f32) -> Vec<u8> {
    let mut v = vec![0f32; 1024];
    v[0] = first;
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[test]
fn init_creates_all_tables_and_is_idempotent() {
    let (dir, conn) = open();
    db::init(&dir.path().join("engram.db")).unwrap();

    assert_eq!(db::tables(&conn).unwrap(), TABLES);
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, db::VERSION);
    let dim: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'embed_dim'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dim, "1024");
    let vec: String = conn
        .query_row("SELECT vec_version()", [], |r| r.get(0))
        .unwrap();
    assert!(vec.starts_with('v'));
}

#[test]
fn fts_follows_inserts_updates_deletes() {
    let (_dir, conn) = open();
    let id = add_item(&conn, "zsh не ділить $VAR на слова");
    assert_eq!(fts(&conn, "item_fts", "ділить"), [id]);
    // trigram cannot see two-letter words, the unicode61 table can
    assert_eq!(fts(&conn, "item_fts_word", "на"), [id]);

    conn.execute("UPDATE item SET body = 'інший текст' WHERE id = ?1", [id])
        .unwrap();
    assert_eq!(fts(&conn, "item_fts", "інший"), [id]);
    assert_eq!(fts(&conn, "item_fts", "слова"), [id]); // still in the title
    conn.execute("UPDATE item SET title = 'новий' WHERE id = ?1", [id])
        .unwrap();
    assert!(fts(&conn, "item_fts", "слова").is_empty());

    conn.execute("DELETE FROM item WHERE id = ?1", [id])
        .unwrap();
    assert!(fts(&conn, "item_fts", "інший").is_empty());
}

#[test]
fn vectors_knn_and_cleanup() {
    let (_dir, conn) = open();
    let near = add_item(&conn, "near");
    let far = add_item(&conn, "far");
    conn.execute(
        "INSERT INTO item_vec (item_id, emb) VALUES (?1, ?2)",
        params![near, vector(1.0)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO item_vec (item_id, emb) VALUES (?1, ?2)",
        params![far, vector(-1.0)],
    )
    .unwrap();

    let mut stmt = conn
        .prepare("SELECT item_id FROM item_vec WHERE emb MATCH ?1 AND k = 2 ORDER BY distance")
        .unwrap();
    let ids: Vec<i64> = stmt
        .query_map([vector(0.9)], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ids, [near, far]);

    conn.execute("DELETE FROM item WHERE id = ?1", [near])
        .unwrap();
    let left: Vec<i64> = conn
        .prepare("SELECT item_id FROM item_vec")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(left, [far]);
}

#[test]
fn constraints() {
    let (_dir, conn) = open();
    insert_item(&conn, "personal/notes", "fact", "x", Some("h1")).unwrap();
    assert!(
        insert_item(&conn, "personal/notes", "fact", "y", Some("h1")).is_err(),
        "duplicate hash"
    );
    assert!(
        insert_item(&conn, "personal/notes", "checkpoint", "z", None).is_err(),
        "unknown kind"
    );
    assert!(
        insert_item(&conn, "work/unknown", "fact", "w", None).is_err(),
        "unknown space"
    );
}
