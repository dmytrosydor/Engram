//! Claude Code hooks. They must never break a session: any failure prints nothing.

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

use crate::{core, db, spaces};

#[derive(Deserialize)]
struct Input {
    cwd: Option<PathBuf>,
}

/// SessionStart: print the core of the session's space (stdout becomes context).
pub fn session_start() {
    match session_start_text() {
        Ok(Some(text)) => println!("{text}"),
        Ok(None) => {}
        Err(e) => eprintln!("engram: {e:#}"),
    }
}

fn session_start_text() -> Result<Option<String>> {
    let path = db::db_path();
    if !path.exists() {
        return Ok(None);
    }
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let cwd = serde_json::from_str::<Input>(&raw)
        .ok()
        .and_then(|i| i.cwd)
        .map_or_else(std::env::current_dir, Ok)?;
    let conn = db::connect(&path)?;
    let space = spaces::resolve(&conn, &cwd)?;
    let text = core::get(&conn, &space)?;
    Ok((!text.is_empty()).then_some(text))
}
