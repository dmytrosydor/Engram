use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use engram::db;

#[derive(Parser)]
#[command(
    name = "engram",
    version,
    about = "Local long-term memory for Claude Code"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the database (safe to re-run)
    Init,
    /// Show versions and tables
    Info,
}

fn main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::Init => init(),
        Cmd::Info => info(),
    }
}

fn init() -> Result<ExitCode> {
    let path = db::db_path();
    db::init(&path)?;
    println!("{}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn info() -> Result<ExitCode> {
    let path = db::db_path();
    if !path.exists() {
        eprintln!("no database at {}; run `engram init`", path.display());
        return Ok(ExitCode::FAILURE);
    }
    let conn = db::connect(&path)?;
    let (sqlite, vec): (String, String) =
        conn.query_row("SELECT sqlite_version(), vec_version()", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;

    println!("db       {}", path.display());
    println!("sqlite   {sqlite}");
    println!("vec      {vec}");
    println!("schema   {schema} ({journal})");
    println!("tables   {}", db::tables(&conn)?.join(" "));
    Ok(ExitCode::SUCCESS)
}
