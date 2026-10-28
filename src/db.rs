//! SQLite connection and schema setup.

use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, ffi};

/// Every statement is idempotent, so `init` can be re-run safely.
const SCHEMA: &str = include_str!("schema.sql");

/// Data folder: `$ENGRAM_HOME`, or `~/.engram`.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("ENGRAM_HOME") {
        return dir.into();
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".engram")
}

pub fn db_path() -> PathBuf {
    home().join("engram.db")
}

/// Makes sqlite-vec available in every connection opened after this call.
fn register_vec() {
    type EntryPoint = unsafe extern "C" fn(
        *mut ffi::sqlite3,
        *mut *mut c_char,
        *const ffi::sqlite3_api_routines,
    ) -> c_int;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        // The crate declares the entry point without arguments; SQLite calls it
        // with the standard extension signature.
        let entry =
            std::mem::transmute::<*const (), EntryPoint>(sqlite_vec::sqlite3_vec_init as *const ());
        ffi::sqlite3_auto_extension(Some(entry));
    });
}

pub fn connect(path: &Path) -> Result<Connection> {
    register_vec();
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Creates the database or brings it up to the current schema.
pub fn init(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    connect(path)?.execute_batch(SCHEMA).context("apply schema")
}

/// Real and virtual tables, without FTS5 / vec0 internals.
pub fn tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM pragma_table_list
         WHERE schema = 'main' AND type IN ('table', 'virtual')
           AND name NOT LIKE 'sqlite_%' AND name NOT GLOB '*_vector_chunks[0-9]*'
         ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names)
}
