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
    /// Embed texts and print timing and pairwise cosine (starts llama-server if needed)
    Embed { texts: Vec<String> },
}

fn main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::Init => init(),
        Cmd::Info => info(),
        Cmd::Embed { texts } => embed(&texts),
    }
}

fn embed(texts: &[String]) -> Result<ExitCode> {
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let vecs = engram::embed::embed(&refs)?;
    println!(
        "{} vectors x {} in {:?}",
        vecs.len(),
        engram::embed::DIM,
        started.elapsed()
    );
    for i in 1..vecs.len() {
        let cos = engram::embed::cosine(&vecs[0], &vecs[i]);
        println!("cos(0,{i}) = {cos:.4}");
    }
    Ok(ExitCode::SUCCESS)
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
