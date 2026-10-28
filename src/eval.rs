//! Search quality on real history.
//!
//! Past sessions searched Memora and then opened specific records; those
//! (query → opened ids) pairs are the ground truth. `extract` collects them from
//! transcripts, `run` measures how often engram puts an opened record in its top 5.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::memory::{self, Query, Scope};
use crate::{embed, spaces};

const SEARCH_TOOLS: [&str; 3] = [
    "mcp__memora__memory_semantic_search",
    "mcp__memora__memory_hybrid_search",
    "mcp__memora__memory_digest",
];
const GET_TOOL: &str = "mcp__memora__memory_get";
/// Opens further than this many tool calls after a search are not attributed to it.
const WINDOW: usize = 12;

#[derive(Debug, Serialize, Deserialize)]
pub struct Pair {
    pub query: String,
    pub cwd: Option<PathBuf>,
    pub memora_ids: Vec<i64>,
    /// When the search ran; replayed as of then.
    #[serde(default)]
    pub at: Option<String>,
}

/// Collects pairs from every top-level transcript under `projects`.
pub fn extract(projects: &Path, out: &Path) -> Result<usize> {
    let mut pairs: BTreeMap<String, Pair> = BTreeMap::new();
    for dir in std::fs::read_dir(projects)?.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            if f.path().extension().is_some_and(|e| e == "jsonl") {
                for p in from_transcript(&f.path())? {
                    pairs
                        .entry(p.query.clone())
                        .and_modify(|e| e.memora_ids.extend(&p.memora_ids))
                        .or_insert(p);
                }
            }
        }
    }
    let mut file = std::fs::File::create(out)?;
    for p in pairs.values_mut() {
        p.memora_ids.sort_unstable();
        p.memora_ids.dedup();
        writeln!(file, "{}", serde_json::to_string(p)?)?;
    }
    Ok(pairs.len())
}

fn from_transcript(path: &Path) -> Result<Vec<Pair>> {
    let mut out: Vec<Pair> = Vec::new();
    // index into `out` of the open search and how many tool calls ago it ran
    let mut open: Option<(usize, usize)> = None;
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if !line.contains("mcp__memora__") && open.is_none() {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if d["type"] != "assistant" {
            continue;
        }
        let Some(blocks) = d["message"]["content"].as_array() else {
            continue;
        };
        for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
            let name = b["name"].as_str().unwrap_or("");
            if SEARCH_TOOLS.contains(&name) {
                let q = b["input"]["query"]
                    .as_str()
                    .or(b["input"]["topic"].as_str())
                    .unwrap_or("");
                if !q.trim().is_empty() {
                    out.push(Pair {
                        query: q.trim().to_string(),
                        cwd: d["cwd"].as_str().map(PathBuf::from),
                        memora_ids: Vec::new(),
                        at: d["timestamp"].as_str().map(str::to_string),
                    });
                    open = Some((out.len() - 1, 0));
                }
                continue;
            }
            if let Some((i, age)) = open {
                if name == GET_TOOL
                    && let Some(id) = b["input"]["memory_id"].as_i64()
                {
                    out[i].memora_ids.push(id);
                }
                open = (age + 1 < WINDOW).then_some((i, age + 1));
            }
        }
    }
    Ok(out
        .into_iter()
        .filter(|p| !p.memora_ids.is_empty())
        .collect())
}

#[derive(Debug, Default, Serialize)]
pub struct Score {
    pub pairs: usize,
    pub hit5: usize,
    pub mrr: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub total: usize,
    /// Pairs whose opened records are not live items (archived, documents, merged away).
    pub unreachable: usize,
    pub by_space: BTreeMap<String, Score>,
    pub overall: Score,
    pub p50_ms: f64,
    pub p95_ms: f64,
    /// Per-pair outcome, for comparing with another system on the same pairs.
    pub rows: Vec<Row>,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub query: String,
    pub space: String,
    pub relevant: Vec<i64>,
    pub rank: Option<usize>,
}

pub fn run(conn: &Connection, pairs_file: &Path) -> Result<Report> {
    let mut r = Report::default();
    let mut times = Vec::new();
    for line in BufReader::new(std::fs::File::open(pairs_file)?).lines() {
        let p: Pair = serde_json::from_str(&line?)?;
        r.total += 1;
        let relevant: Vec<i64> = p
            .memora_ids
            .iter()
            .filter_map(|id| live_item(conn, *id).transpose())
            .collect::<Result<_>>()?;
        if relevant.is_empty() {
            r.unreachable += 1;
            continue;
        }
        let space = match &p.cwd {
            Some(cwd) => spaces::resolve(conn, cwd)?,
            None => "global".into(),
        };
        let visible = spaces::visible(conn, &space)?;
        let started = Instant::now();
        let vector = embed::embed_one(&p.query).ok();
        let hits = memory::search(
            conn,
            &Query {
                text: &p.query,
                vector: vector.as_deref(),
                spaces: &visible,
                scope: Scope::Project,
                kind: None,
                limit: 10,
                as_of: p.at.as_deref(),
            },
        )?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        let rank = hits
            .iter()
            .position(|h| relevant.contains(&h.id))
            .map(|i| i + 1);
        for score in [&mut r.overall, r.by_space.entry(space.clone()).or_default()] {
            score.pairs += 1;
            if rank.is_some_and(|k| k <= 5) {
                score.hit5 += 1;
            }
            score.mrr += rank.map_or(0.0, |k| 1.0 / k as f64);
        }
        r.rows.push(Row {
            query: p.query,
            space,
            relevant,
            rank,
        });
    }
    for s in std::iter::once(&mut r.overall).chain(r.by_space.values_mut()) {
        s.mrr /= s.pairs.max(1) as f64;
    }
    times.sort_by(f64::total_cmp);
    let pct = |q: f64| {
        times
            .get(((times.len() as f64 - 1.0) * q).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    r.p50_ms = pct(0.5);
    r.p95_ms = pct(0.95);
    Ok(r)
}

/// The engram item a Memora record became, if it is still live.
/// A record merged away counts as its replacement.
fn live_item(conn: &Connection, memora_id: i64) -> Result<Option<i64>> {
    let row: Option<(i64, String, Option<i64>)> = conn
        .query_row(
            "SELECT id, status, superseded_by FROM item WHERE legacy_id = ?1",
            [format!("memora:{memora_id}")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((id, s, _)) if s == "active" || s == "proposed" => Some(id),
        Some((_, s, Some(by))) if s == "superseded" => Some(by),
        _ => None,
    })
}

/// Work queries asked from a personal folder (and the reverse) must return nothing
/// from the other domain. Returns (queries, leaked results).
pub fn isolation(
    conn: &Connection,
    pairs_file: &Path,
    personal: &Path,
    work: &Path,
) -> Result<(usize, usize)> {
    let (mut queries, mut leaks) = (0, 0);
    for line in BufReader::new(std::fs::File::open(pairs_file)?)
        .lines()
        .take(40)
    {
        let p: Pair = serde_json::from_str(&line?)?;
        let vector = embed::embed_one(&p.query).ok();
        for dir in [personal, work] {
            let space = spaces::resolve(conn, dir)?;
            let domain = spaces::domain(conn, &space)?;
            let visible = spaces::visible(conn, &space)?;
            let hits = memory::search(
                conn,
                &Query {
                    text: &p.query,
                    vector: vector.as_deref(),
                    spaces: &visible,
                    scope: Scope::Project,
                    kind: None,
                    limit: 8,
                    as_of: None,
                },
            )?;
            queries += 1;
            for h in hits {
                let d = spaces::domain(conn, &h.space)?;
                if d != domain && d != "global" {
                    leaks += 1;
                }
            }
        }
    }
    Ok((queries, leaks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, input: Value) -> String {
        serde_json::json!({"type": "assistant", "cwd": "/code/acme", "timestamp": "2026-10-01T10:00:00Z",
            "message": {"content": [{"type": "tool_use", "name": name, "input": input}]}})
        .to_string()
    }

    #[test]
    fn pairs_link_searches_to_the_records_opened_after_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut lines = vec![
            tool(
                "mcp__memora__memory_digest",
                serde_json::json!({"topic": "deploy vpn"}),
            ),
            tool(
                "mcp__memora__memory_get",
                serde_json::json!({"memory_id": 7}),
            ),
            tool(
                "mcp__memora__memory_semantic_search",
                serde_json::json!({"query": "no opens"}),
            ),
            tool(
                "mcp__memora__memory_hybrid_search",
                serde_json::json!({"query": "late open"}),
            ),
        ];
        lines.extend((0..WINDOW).map(|_| tool("Bash", serde_json::json!({"command": "ls"}))));
        lines.push(tool(
            "mcp__memora__memory_get",
            serde_json::json!({"memory_id": 9}),
        ));
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();

        let pairs = from_transcript(&path).unwrap();
        assert_eq!(
            pairs.len(),
            1,
            "searches without opens in the window are dropped"
        );
        assert_eq!(
            (pairs[0].query.as_str(), pairs[0].memora_ids.as_slice()),
            ("deploy vpn", &[7][..])
        );
        assert_eq!(pairs[0].at.as_deref(), Some("2026-10-01T10:00:00Z"));
    }
}
