//! The daily pass. By default it only tidies, with no model: expired state goes to
//! the archive and every core is rebuilt.
//!
//! `--with-claude` adds the judgement steps: find near-duplicates, classify unreviewed
//! imports, and extract new knowledge from transcripts and the inbox. Model judgements
//! on imported items apply directly (logged, so reversible); everything new or
//! destructive goes to the review queue.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;

use crate::review::{self, NewProposal};
use crate::spaces;
use crate::store::{self, KINDS};
use crate::transcript;
use crate::{core, embed, index, memory};

pub const ACTOR: &str = "daily";
/// Cosine at or above which two items are worth a merge question.
const NEAR_DUP: f32 = 0.92;
const CLASSIFY_BATCH: usize = 40;
const PAIR_BATCH: usize = 20;
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(600);

const SYSTEM: &str = "You curate a developer's long-term memory for Claude Code sessions. \
Reply with JSON only, no prose, no code fences.";

/// The model behind the judgement steps.
pub trait Llm {
    fn ask(&self, prompt: &str) -> Result<String>;
}

/// `claude -p` in an isolated, tool-less, non-persisted session.
pub struct ClaudeCli {
    pub model: String,
    pub workdir: PathBuf,
}

impl Llm for ClaudeCli {
    fn ask(&self, prompt: &str) -> Result<String> {
        use std::io::Write as _;
        std::fs::create_dir_all(&self.workdir)?;
        let mut child = Command::new("claude")
            .args([
                "-p",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
            ])
            .args(["--setting-sources", "", "--output-format", "text"])
            .args(["--model", &self.model, "--system-prompt", SYSTEM])
            .current_dir(&self.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start claude")?;
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(prompt.as_bytes())?;
        // read the pipes on threads so a large reply cannot block the child
        let mut stdout = child.stdout.take().context("stdout")?;
        let mut stderr = child.stderr.take().context("stderr")?;
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut stdout, &mut s).map(|_| s)
        });
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
            s
        });
        let deadline = std::time::Instant::now() + CLAUDE_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("claude did not answer in {CLAUDE_TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let stdout = out
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
        if !status.success() {
            let stderr = err.join().unwrap_or_default();
            bail!("claude failed: {}", stderr.trim());
        }
        Ok(stdout)
    }
}

pub struct Options {
    pub classify: usize,
    pub transcripts: usize,
    pub pairs: usize,
    /// Folder with `*.jsonl` session transcripts per project.
    pub projects: PathBuf,
    /// Folder with dropped-in notes (`*.md`, `*.txt`).
    pub inbox_dir: PathBuf,
    /// Project folders whose transcripts are never read (the daily pass's own).
    pub exclude: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub expired: usize,
    pub pairs_found: usize,
    pub pairs_queued: usize,
    pub classified: usize,
    pub archived: usize,
    /// Moves to `global` queued for review.
    pub moved_global: usize,
    pub transcripts: usize,
    pub notes: usize,
    pub proposals: usize,
    pub skipped_known: usize,
    pub cores: usize,
    pub errors: Vec<String>,
}

/// Why the scheduled run should not start now, if it should not.
/// Only a run that calls the model waits for AC power; tidying costs nothing.
pub fn not_due(conn: &Connection, with_claude: bool) -> Result<Option<String>> {
    if meta(conn, "daily_last_run")?.as_deref() == Some(&store::today(conn)?) {
        return Ok(Some("already ran today".into()));
    }
    if with_claude && !on_ac_power() {
        return Ok(Some("on battery".into()));
    }
    Ok(None)
}

/// True unless the machine reports running on battery (desktops have no battery).
fn on_ac_power() -> bool {
    if cfg!(target_os = "macos") {
        return Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("AC Power"))
            .unwrap_or(true);
    }
    // Linux: an AC adapter ("Mains") that reports offline means battery
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
        return true;
    };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mains: Vec<_> = supplies
        .flatten()
        .map(|e| e.path())
        .filter(|p| read(p.join("type")).trim() == "Mains")
        .collect();
    mains.is_empty() || mains.iter().any(|p| read(p.join("online")).trim() == "1")
}

/// The model-free pass: archive expired state and rebuild every core.
pub fn tidy(conn: &Connection, mut say: impl FnMut(&str)) -> Result<Report> {
    let _lock = lock(conn)?;
    let mut r = Report {
        expired: expire(conn)?,
        ..Report::default()
    };
    say(&format!("expired {}", r.expired));
    r.cores = core::rebuild_all(conn)?.len();
    set_meta(conn, "daily_last_run", &store::today(conn)?)?;
    set_meta(conn, "daily_last_errors", "")?;
    store::log(
        conn,
        ACTOR,
        "daily",
        None,
        None,
        Some(&json!(format!("{r:?}"))),
    )?;
    Ok(r)
}

/// One run at a time per database (scheduled tick vs a manual run).
fn lock(conn: &Connection) -> Result<std::fs::File> {
    let lock_path = format!("{}.daily.lock", conn.path().unwrap_or("engram"));
    let lock = std::fs::File::create(&lock_path)?;
    if lock.try_lock().is_err() {
        bail!("another daily run is in progress");
    }
    Ok(lock)
}

pub fn run(
    conn: &Connection,
    llm: &dyn Llm,
    opts: &Options,
    mut say: impl FnMut(&str),
) -> Result<Report> {
    let _lock = lock(conn)?;
    let mut r = Report::default();
    let started = store::now(conn)?;
    let started_ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();

    r.expired = expire(conn)?;
    say(&format!("expired {}", r.expired));

    let pairs = near_duplicates(conn, meta(conn, "dup_since")?.as_deref())?;
    r.pairs_found = pairs.len();
    let mut judged = 0;
    for batch in pairs
        .chunks(PAIR_BATCH)
        .take(opts.pairs.div_ceil(PAIR_BATCH))
    {
        match judge_pairs(conn, llm, batch) {
            Ok(n) => {
                r.pairs_queued += n;
                judged += batch.len();
            }
            Err(e) => r.errors.push(format!("pairs: {e:#}")),
        }
    }
    // probe the same items again next time until every pair has an answer
    if judged == pairs.len() {
        set_meta(conn, "dup_since", &started)?;
    }
    say(&format!(
        "pairs {} found, {} queued",
        r.pairs_found, r.pairs_queued
    ));

    let proposed = proposed_items(conn, opts.classify)?;
    // one prompt never carries two domains' records
    let batches = proposed
        .chunk_by(|a, b| a.0 == b.0)
        .flat_map(|run| run.chunks(CLASSIFY_BATCH))
        .map(|chunk| chunk.iter().map(|(_, id)| *id).collect::<Vec<_>>());
    for batch in batches {
        let batch = batch.as_slice();
        match classify(conn, llm, batch) {
            Ok((n, archived, global)) => {
                r.classified += n;
                r.archived += archived;
                r.moved_global += global;
            }
            Err(e) => r.errors.push(format!("classify: {e:#}")),
        }
    }
    say(&format!(
        "classified {} ({} archived, {} to global)",
        r.classified, r.archived, r.moved_global
    ));

    // transcripts compare by file mtime, which needs full precision
    let since = meta(conn, "daily_last_ns")?
        .and_then(|ns| ns.parse::<u64>().ok())
        .map(|ns| SystemTime::UNIX_EPOCH + Duration::from_nanos(ns))
        .unwrap_or_else(|| SystemTime::now() - Duration::from_secs(24 * 3600));
    let changed = transcript::changed_since(&opts.projects, since, &opts.exclude)?;
    // oldest first; if some are left for tomorrow, resume right after the last one read
    let mut next_since = started_ns;
    let more = changed.len() > opts.transcripts;
    for (mtime, path) in changed.into_iter().take(opts.transcripts) {
        if more {
            next_since = mtime.duration_since(SystemTime::UNIX_EPOCH)?.as_nanos();
        }
        let t = match transcript::read(&path) {
            Ok(t) => t,
            Err(e) => {
                r.errors
                    .push(format!("transcript {}: {e:#}", path.display()));
                continue;
            }
        };
        if t.text.chars().count() < 400 {
            continue;
        }
        let space = match &t.cwd {
            Some(cwd) => spaces::resolve(conn, cwd)?,
            None => spaces::INBOX.to_string(),
        };
        match extract(
            conn,
            llm,
            &space,
            &t.text,
            &format!("session:{}", t.session),
        ) {
            Ok((added, known)) => {
                r.transcripts += 1;
                r.proposals += added;
                r.skipped_known += known;
            }
            Err(e) => r
                .errors
                .push(format!("transcript {}: {e:#}", path.display())),
        }
    }
    say(&format!(
        "transcripts {}, proposals {}",
        r.transcripts, r.proposals
    ));

    r.errors.extend(collect_inbox_dir(conn, &opts.inbox_dir));
    for (id, space, source, raw) in inbox(conn)? {
        match extract(conn, llm, &space, &raw, source.as_deref().unwrap_or("note")) {
            Ok((added, known)) => {
                r.notes += 1;
                r.proposals += added;
                r.skipped_known += known;
                conn.execute(
                    "UPDATE inbox SET processed_at = ?2 WHERE id = ?1",
                    params![id, store::now(conn)?],
                )?;
            }
            Err(e) => r.errors.push(format!("inbox #{id}: {e:#}")),
        }
    }

    r.cores = core::rebuild_all(conn)?.len();
    set_meta(conn, "daily_last_run", &store::today(conn)?)?;
    set_meta(conn, "daily_last_ns", &next_since.to_string())?;
    // shown on the health page and by `engram doctor`, since scheduled runs are silent
    set_meta(conn, "daily_last_errors", &r.errors.join("\n"))?;
    store::log(
        conn,
        ACTOR,
        "daily",
        None,
        None,
        Some(&json!(format!("{r:?}"))),
    )?;
    Ok(r)
}

/// state / task past `valid_until` go to the archive.
fn expire(conn: &Connection) -> Result<usize> {
    let ids = ids(
        conn,
        "SELECT id FROM item WHERE kind IN ('state', 'task') AND status IN ('active', 'proposed')
         AND valid_until IS NOT NULL AND valid_until < date('now')",
    )?;
    for &id in &ids {
        store::set_status(conn, id, "archived", None, ACTOR)?;
    }
    Ok(ids.len())
}

/// Pairs in the same domain with cosine >= NEAR_DUP, not asked about before.
/// Only items changed since `since` are probed (all of them on the first run).
fn near_duplicates(conn: &Connection, since: Option<&str>) -> Result<Vec<(i64, i64, f32)>> {
    let probes = match since {
        Some(t) => {
            let mut stmt = conn.prepare(
                "SELECT id FROM item WHERE status IN ('active', 'proposed')
                 AND COALESCE(updated_at, created_at) >= ?1
                 AND id IN (SELECT item_id FROM item_vec)",
            )?;
            stmt.query_map([t], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?
        }
        None => ids(
            conn,
            "SELECT id FROM item WHERE status IN ('active', 'proposed')
             AND id IN (SELECT item_id FROM item_vec)",
        )?,
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut knn = conn.prepare(
        "SELECT v.item_id, v.distance FROM item_vec v
         WHERE v.emb MATCH (SELECT emb FROM item_vec WHERE item_id = ?1) AND k = 6",
    )?;
    for id in probes {
        let rows = knn.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        for row in rows {
            let (other, dist) = row?;
            // vectors are unit length: L2² = 2 − 2·cos
            let cos = 1.0 - (dist * dist / 2.0) as f32;
            if other == id || cos < NEAR_DUP {
                continue;
            }
            let pair = (id.min(other), id.max(other));
            if !seen.insert(pair)
                || review::pair_known(conn, pair.0, pair.1)?
                || !same_domain_live(conn, pair)?
            {
                continue;
            }
            out.push((pair.0, pair.1, cos));
        }
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    Ok(out)
}

fn same_domain_live(conn: &Connection, (a, b): (i64, i64)) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT count(DISTINCT s.domain) = 1 AND sum(i.status IN ('active', 'proposed')) = 2
         FROM item i JOIN space s ON s.id = i.space_id WHERE i.id IN (?1, ?2)",
        params![a, b],
        |r| r.get(0),
    )?)
}

#[derive(Deserialize)]
struct PairVerdict {
    a: i64,
    b: i64,
    verdict: String,
    keep: Option<i64>,
}

fn judge_pairs(conn: &Connection, llm: &dyn Llm, pairs: &[(i64, i64, f32)]) -> Result<usize> {
    let mut prompt = String::from(
        "Each pair below is two memory items that look alike. For each pair decide:\n\
         - \"same\": they state the same thing; keep the clearer one\n\
         - \"replaces\": one is a newer or corrected version of the other; keep the newer\n\
         - \"different\": both carry distinct information\n\
         Reply: [{\"a\": 1, \"b\": 2, \"verdict\": \"same\", \"keep\": 1}, ...] covering every pair.\n\n",
    );
    for (a, b, cos) in pairs {
        prompt.push_str(&format!(
            "PAIR a={a} b={b} (cos {cos:.2})\n{}\n{}\n\n",
            brief(conn, *a, 600)?,
            brief(conn, *b, 600)?
        ));
    }
    let verdicts: Vec<PairVerdict> = ask_json(llm, &prompt)?;
    let wanted: HashSet<(i64, i64)> = pairs.iter().map(|(a, b, _)| (*a, *b)).collect();
    let mut queued = 0;
    for v in verdicts {
        let pair = (v.a.min(v.b), v.a.max(v.b));
        if !wanted.contains(&pair) {
            continue;
        }
        let ids = [pair.0, pair.1];
        match (v.verdict.as_str(), v.keep) {
            ("same" | "replaces", Some(keep)) if ids.contains(&keep) => {
                let drop = if keep == pair.0 { pair.1 } else { pair.0 };
                review::add(
                    conn,
                    "merge",
                    &ids,
                    &json!({"keep": keep, "drop": drop}),
                    &v.verdict,
                    "open",
                )?;
                queued += 1;
            }
            // remembered so the pair is not asked about again
            _ => {
                review::add(conn, "merge", &ids, &json!({}), "different", "rejected")?;
            }
        }
    }
    Ok(queued)
}

/// Oldest unreviewed items as (domain, id), grouped by domain.
fn proposed_items(conn: &Connection, limit: usize) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT domain, id FROM (
             SELECT s.domain, i.id, i.created_at FROM item i JOIN space s ON s.id = i.space_id
             WHERE i.status = 'proposed' ORDER BY i.created_at, i.id LIMIT ?1)
         ORDER BY domain, created_at, id",
    )?;
    let ids = stmt
        .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

#[derive(Deserialize)]
struct Classified {
    id: i64,
    verdict: String,
    kind: Option<String>,
    scope: Option<String>,
}

/// Returns (classified, archived, moved to global).
fn classify(conn: &Connection, llm: &dyn Llm, batch: &[i64]) -> Result<(usize, usize, usize)> {
    let mut prompt = String::from(
        "Each item below was imported from an old memory store and nobody reviewed it. For each decide:\n\
         - verdict: \"keep\" if it is durable knowledge that would help a future session (a rule, decision, \
         gotcha, preference, non-obvious fact); \"archive\" if it is a progress log, one-off status, stale, \
         or too vague to act on\n\
         - kind: one of rule, preference, decision, fact, gotcha, lesson, state, task, person, event\n\
         - scope: \"global\" only if it is about how the user works or about tools, true in every project; \
         otherwise \"project\"\n\
         Reply: [{\"id\": 1, \"verdict\": \"keep\", \"kind\": \"gotcha\", \"scope\": \"project\"}, ...] covering every id.\n\n",
    );
    for &id in batch {
        prompt.push_str(&format!("{}\n\n", brief(conn, id, 700)?));
    }
    let verdicts: Vec<Classified> = ask_json(llm, &prompt)?;
    let wanted: HashSet<i64> = batch.iter().copied().collect();
    let (mut n, mut archived, mut global) = (0, 0, 0);
    for v in verdicts {
        if !wanted.contains(&v.id) {
            continue;
        }
        let before = store::item_json(conn, v.id)?;
        if before["status"] != "proposed" {
            continue;
        }
        let kind = v.kind.filter(|k| KINDS.contains(&k.as_str()));
        let to_global = v.scope.as_deref() == Some("global") && v.verdict == "keep";
        let status = if v.verdict == "archive" {
            "archived"
        } else {
            "active"
        };
        conn.execute(
            "UPDATE item SET status = ?2, kind = COALESCE(?3, kind), updated_at = ?4 WHERE id = ?1",
            params![v.id, status, kind, store::now(conn)?],
        )?;
        // global items reach every session's core, so that move waits for a person
        if to_global {
            review::add(
                conn,
                "move",
                &[v.id],
                &json!({"space": "global"}),
                "classified as true in every project",
                "open",
            )?;
        }
        store::log(
            conn,
            ACTOR,
            "classify",
            Some(v.id),
            Some(&before),
            Some(&store::item_json(conn, v.id)?),
        )?;
        n += 1;
        archived += usize::from(status == "archived");
        global += usize::from(to_global);
    }
    Ok((n, archived, global))
}

#[derive(Deserialize)]
struct Extracted {
    kind: String,
    title: String,
    body: String,
    scope: Option<String>,
}

/// Asks for durable knowledge in `text`; new items go to the review queue.
/// Returns (queued, already known).
fn extract(
    conn: &Connection,
    llm: &dyn Llm,
    space: &str,
    text: &str,
    source: &str,
) -> Result<(usize, usize)> {
    let prompt = format!(
        "Below is material from the project space \"{space}\". Extract only durable knowledge worth \
         remembering in future sessions: decisions with their reason, gotchas and pitfalls found, rules or \
         preferences the user stated, non-obvious facts about the code or environment. Skip progress \
         updates, plans, temporary state and anything readable from the code or git history. One fact per \
         item; title at most 100 characters, body at most 800, in the language of the material.\n\
         Reply: [{{\"kind\": \"decision\", \"title\": \"...\", \"body\": \"...\", \"scope\": \"project\"}}] with \
         kind one of {kinds} and scope \"global\" only for things true in every project. Reply [] if nothing \
         qualifies.\n\n---\n{text}",
        kinds = KINDS.join(", ")
    );
    let found: Vec<Extracted> = ask_json(llm, &prompt)?;
    let visible = spaces::visible(conn, space)?;
    let (mut queued, mut known) = (0, 0);
    for e in found {
        if !KINDS.contains(&e.kind.as_str())
            || e.title.trim().is_empty()
            || e.body.trim().is_empty()
        {
            continue;
        }
        let target = if e.scope.as_deref() == Some("global") {
            "global"
        } else {
            space
        };
        let title = store::clip(e.title.trim(), store::TITLE_MAX);
        let body = store::clip(e.body.trim(), store::BODY_MAX - 1);
        if let Ok(v) = embed::embed_one(&index::item_text(&title, &body))
            && !memory::similar(conn, &v, &visible)?.is_empty()
        {
            known += 1;
            continue;
        }
        if review::proposal_known(conn, target, &title)? {
            known += 1;
            continue;
        }
        let p = NewProposal {
            kind: e.kind,
            title,
            body,
            space: target.into(),
            source: Some(source.into()),
        };
        review::add(conn, "new", &[], &serde_json::to_value(&p)?, source, "open")?;
        queued += 1;
    }
    Ok((queued, known))
}

/// Moves `*.md` / `*.txt` notes from the inbox folder into the inbox table.
/// A file is moved aside before it is stored, so a failure never repeats hourly.
fn collect_inbox_dir(conn: &Connection, dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let done = dir.join("processed");
    let mut errors = Vec::new();
    for f in entries.flatten() {
        let p = f.path();
        if !p.is_file() || !p.extension().is_some_and(|e| e == "md" || e == "txt") {
            continue;
        }
        let name = p.file_name().unwrap_or_default().to_owned();
        let moved = done.join(&name);
        let stored = std::fs::create_dir_all(&done)
            .and_then(|()| std::fs::rename(&p, &moved))
            .and_then(|()| std::fs::read(&moved))
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                let raw = String::from_utf8_lossy(&bytes);
                add_note(
                    conn,
                    spaces::INBOX,
                    &raw,
                    &format!("note:{}", name.to_string_lossy()),
                )
            });
        if let Err(e) = stored {
            errors.push(format!("inbox file {}: {e:#}", p.display()));
        }
    }
    errors
}

pub fn add_note(conn: &Connection, space: &str, raw: &str, source: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO inbox (space_id, source, raw, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![space, source, raw, store::now(conn)?],
    )?;
    Ok(conn.last_insert_rowid())
}

type Note = (i64, String, Option<String>, String);

fn inbox(conn: &Connection) -> Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(space_id, ?1), source, raw FROM inbox WHERE processed_at IS NULL ORDER BY id",
    )?;
    let rows = stmt.query_map([spaces::INBOX], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `#id [space, kind] title` + clipped body, for prompts.
fn brief(conn: &Connection, id: i64, body_chars: usize) -> Result<String> {
    let (space, kind, title, body): (String, String, String, String) = conn.query_row(
        "SELECT space_id, kind, title, body FROM item WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(format!(
        "#{id} [{space}, {kind}] {title}\n{}",
        store::clip(&body, body_chars)
    ))
}

/// Asks for a JSON array; one retry with the parse error if the reply is malformed.
fn ask_json<T: for<'de> Deserialize<'de>>(llm: &dyn Llm, prompt: &str) -> Result<Vec<T>> {
    match parse_json(&llm.ask(prompt)?) {
        Ok(v) => Ok(v),
        Err(e) => {
            let retry = format!(
                "{prompt}\n\nYour previous reply was not valid JSON ({e:#}). Reply again with a valid \
                 JSON array only; escape double quotes inside strings."
            );
            parse_json(&llm.ask(&retry)?)
        }
    }
}

/// The model sometimes wraps JSON in prose or fences; take the outermost array.
fn parse_json<T: for<'de> Deserialize<'de>>(reply: &str) -> Result<Vec<T>> {
    let start = reply.find('[').context("no JSON array in reply")?;
    let end = reply.rfind(']').context("no JSON array in reply")?;
    if end < start {
        bail!("no JSON array in reply");
    }
    // items the model got wrong are skipped rather than failing the batch
    let values: Vec<serde_json::Value> =
        serde_json::from_str(&reply[start..=end]).context("reply is not JSON")?;
    Ok(values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

fn ids(conn: &Connection, sql: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Flaky(Cell<usize>);

    impl Llm for Flaky {
        fn ask(&self, prompt: &str) -> Result<String> {
            self.0.set(self.0.get() + 1);
            Ok(if prompt.contains("not valid JSON") {
                r#"[{"a": 1}]"#.into()
            } else {
                r#"[{"a": "unescaped "quote"}]"#.into()
            })
        }
    }

    #[derive(Deserialize)]
    struct A {
        a: i64,
    }

    #[test]
    fn retries_malformed_json_once() {
        let llm = Flaky(Cell::new(0));
        let v: Vec<A> = ask_json(&llm, "give me JSON").unwrap();
        assert_eq!((v[0].a, llm.0.get()), (1, 2));
    }
}
