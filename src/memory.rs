//! The three operations Claude uses: search, get, write.
//!
//! Vectors are passed in by the caller, so this module never talks to the model
//! itself; that keeps it testable and lets a missing model degrade to keyword search.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::store::{self, BODY_MAX, Inserted, KINDS, NewItem, TITLE_MAX};
use crate::{embed, spaces};

const CANDIDATES: usize = 30;
/// Nearest neighbours fetched before the space filter; the larger pool is
/// sqlite-vec's maximum `k`.
const KNN_POOLS: [usize; 2] = [400, 4096];
const RRF_K: f64 = 60.0;
/// Cosine at or above which `write` asks before adding.
pub const SIMILAR: f32 = 0.88;
/// Cosine from which an older record is probably about the same topic
/// (bge-m3: same-topic pairs measured 0.59-0.73, unrelated ones up to about 0.55).
pub const RELATED: f32 = 0.58;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    /// Visible spaces, active and proposed items.
    Project,
    /// Every space of the session's domain plus `global`, active and proposed items.
    All,
    /// Visible spaces, archived and superseded items.
    Archive,
}

impl Scope {
    pub fn parse(s: Option<&str>) -> Result<Self> {
        Ok(match s.unwrap_or("project") {
            "project" => Scope::Project,
            "all" => Scope::All,
            "archive" => Scope::Archive,
            other => bail!("scope must be project, all or archive, not {other}"),
        })
    }

    fn statuses(self) -> &'static [&'static str] {
        match self {
            Scope::Project | Scope::All => &["active", "proposed"],
            Scope::Archive => &["archived", "superseded"],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub id: i64,
    pub kind: String,
    pub space: String,
    pub status: String,
    pub created: String,
    pub title: String,
    pub score: f64,
}

impl Hit {
    /// One dense line: `#4821 d work/acme 10-12 | title`. `?` = proposed, `~` = retired.
    pub fn line(&self) -> String {
        let mark = match self.status.as_str() {
            "proposed" => "?",
            "archived" | "superseded" => "~",
            _ => "",
        };
        let date = self.created.get(5..10).unwrap_or("");
        format!(
            "#{} {}{mark} {} {date} | {}",
            self.id,
            kind_letter(&self.kind),
            self.space,
            self.title
        )
    }
}

pub fn kind_letter(kind: &str) -> &'static str {
    match kind {
        "rule" => "r",
        "preference" => "p",
        "decision" => "d",
        "fact" => "f",
        "gotcha" => "g",
        "lesson" => "l",
        "state" => "s",
        "task" => "t",
        "person" => "P",
        "event" => "e",
        _ => "?",
    }
}

pub struct Query<'a> {
    pub text: &'a str,
    pub vector: Option<&'a [f32]>,
    /// Spaces to search, always enforced; see [`spaces_for`].
    pub spaces: &'a [String],
    pub scope: Scope,
    pub kind: Option<&'a str>,
    pub limit: usize,
    /// Search as of this moment (evaluation replays old queries): later items are
    /// hidden and ages count from here. None = now.
    pub as_of: Option<&'a str>,
}

/// Spaces a session in `space` may search with `scope`. `All` widens to the whole
/// domain, never across domains.
pub fn spaces_for(conn: &Connection, space: &str, scope: Scope) -> Result<Vec<String>> {
    match scope {
        Scope::Project | Scope::Archive => spaces::visible(conn, space),
        Scope::All => spaces::reachable(conn, space),
    }
}

/// Hybrid search: two FTS lists and one vector list fused with RRF, then boosted.
pub fn search(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    let (filter, args) = filter(q);
    let mut lists: Vec<Vec<i64>> = Vec::new();

    let words = tokens(q.text);
    let long: Vec<&String> = words.iter().filter(|w| w.chars().count() >= 3).collect();
    for (table, toks) in [
        ("item_fts", long),
        ("item_fts_word", words.iter().collect()),
    ] {
        if toks.is_empty() {
            continue;
        }
        let expr = toks
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT i.id FROM {table} JOIN item i ON i.id = {table}.rowid
             WHERE {table} MATCH ? AND {filter}
             ORDER BY bm25({table}, 5.0, 1.0) LIMIT {CANDIDATES}"
        );
        let mut all = vec![Value::Text(expr)];
        all.extend(args.iter().cloned());
        lists.push(ids(conn, &sql, all)?);
    }
    if let Some(v) = q.vector {
        lists.push(nearest(conn, v, &filter, &args)?);
    }

    let mut fused: HashMap<i64, f64> = HashMap::new();
    for list in &lists {
        for (rank, id) in list.iter().enumerate() {
            *fused.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut hits = Vec::with_capacity(fused.len());
    for (id, base) in fused {
        let (kind, space, status, created, title, hits_n, pinned, age): (
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            f64,
        ) = conn.query_row(
            "SELECT kind, space_id, status, created_at, title, hits, pinned,
                        MAX(0, julianday(COALESCE(?2, 'now')) - julianday(created_at))
                 FROM item WHERE id = ?1",
            params![id, q.as_of],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                ))
            },
        )?;
        let mut score = base * (1.0 + 0.1 * (1.0 + hits_n as f64).ln());
        if pinned == 1 {
            score *= 1.3;
        }
        if matches!(kind.as_str(), "state" | "event") {
            score *= 0.5f64.powf(age / 90.0);
        }
        hits.push(Hit {
            id,
            kind,
            space,
            status,
            created,
            title,
            score,
        });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.id.cmp(&a.id)));
    hits.truncate(q.limit);
    Ok(hits)
}

/// Records what a search returned, so later opens can be attributed to it.
pub fn log_recall(
    conn: &Connection,
    session: &str,
    space: &str,
    query: &str,
    hits: &[Hit],
) -> Result<()> {
    let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
    conn.execute(
        "INSERT INTO recall (at, session, space_id, query, item_ids) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            store::now(conn)?,
            session,
            space,
            query,
            serde_json::to_string(&ids)?
        ],
    )?;
    Ok(())
}

fn filter(q: &Query) -> (String, Vec<Value>) {
    let mut sql = Vec::new();
    let mut args = Vec::new();
    let statuses = q.scope.statuses();
    sql.push(format!(
        "i.status IN ({})",
        vec!["?"; statuses.len()].join(", ")
    ));
    args.extend(statuses.iter().map(|s| Value::Text(s.to_string())));
    sql.push(format!(
        "i.space_id IN ({})",
        vec!["?"; q.spaces.len().max(1)].join(", ")
    ));
    if q.spaces.is_empty() {
        args.push(Value::Null);
    }
    args.extend(q.spaces.iter().map(|s| Value::Text(s.clone())));
    // every domain sees global, so a record nobody confirmed must not carry text across them
    sql.push("NOT (i.space_id = 'global' AND i.status = 'proposed')".into());
    if let Some(kind) = q.kind {
        sql.push("i.kind = ?".into());
        args.push(Value::Text(kind.into()));
    }
    if let Some(t) = q.as_of {
        sql.push("julianday(i.created_at) <= julianday(?)".into());
        args.push(Value::Text(t.into()));
    }
    (sql.join(" AND "), args)
}

/// Vector candidates. vec0's own KNN over everything is fast (a point lookup per row
/// is not), so take the nearest items overall and filter them. Small visible sets may
/// keep too few, so retry with a bigger pool; past that, scan the visible set exactly.
fn nearest(conn: &Connection, v: &[f32], filter: &str, args: &[Value]) -> Result<Vec<i64>> {
    // the vector is ?1; the filter's anonymous placeholders continue from 2
    let mut all = vec![Value::Blob(embed::to_blob(v))];
    all.extend(args.iter().cloned());
    for pool in KNN_POOLS {
        let knn = format!(
            "WITH knn AS (SELECT item_id, distance FROM item_vec WHERE emb MATCH ?1 AND k = {pool})
             SELECT i.id FROM knn JOIN item i ON i.id = knn.item_id
             WHERE {filter} ORDER BY knn.distance LIMIT {CANDIDATES}"
        );
        let found = ids(conn, &knn, all.clone())?;
        if found.len() == CANDIDATES {
            return Ok(found);
        }
    }
    // here the filter comes first in the text, so the vector is the last placeholder
    let exact = format!(
        "SELECT i.id FROM item i JOIN item_vec v ON v.item_id = i.id
         WHERE {filter} ORDER BY vec_distance_cosine(v.emb, ?) LIMIT {CANDIDATES}"
    );
    let mut last = args.to_vec();
    last.push(Value::Blob(embed::to_blob(v)));
    ids(conn, &exact, last)
}

fn ids(conn: &Connection, sql: &str, args: Vec<Value>) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Words of a query, keeping identifiers like `ABC-123`, `$VAR`, `foo_bar.rs` whole.
fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '$' | '/')))
        .map(|t| {
            t.trim_matches(|c: char| matches!(c, '.' | '-' | '/'))
                .to_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect();
    out.dedup();
    out
}

// ---------- get ----------

/// Full items with confirmed links; counts as a hit and as an "open" of the last search.
/// `visible` limits which spaces may be read (sessions); None reads anything (CLI).
pub fn get(
    conn: &Connection,
    ids: &[i64],
    session: &str,
    visible: Option<&[String]>,
) -> Result<String> {
    let mut out = Vec::new();
    for &id in ids {
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT space_id, status, '#' || id || ' ' || kind || ' ' || space_id || ' ' || status || ' '
                        || substr(created_at, 1, 10)
                        || COALESCE(' until ' || valid_until, '') || COALESCE(' src ' || source, '')
                        || char(10) || title || char(10) || body
                 FROM item WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let text =
            row.and_then(|(space, status, text)| sees(visible, &space, &status).then_some(text));
        let Some(mut text) = text else {
            out.push(format!("#{id} not found"));
            continue;
        };
        let links = links(conn, id, visible)?;
        if !links.is_empty() {
            text.push_str(&format!("\nlinks: {}", links.join(", ")));
        }
        out.push(text);

        conn.execute(
            "UPDATE item SET hits = hits + 1, last_hit = ?2 WHERE id = ?1",
            params![id, store::now(conn)?],
        )?;
        conn.execute(
            "UPDATE recall SET opened = json_insert(COALESCE(opened, '[]'), '$[#]', ?2)
             WHERE id = (SELECT r.id FROM recall r, json_each(r.item_ids) j
                         WHERE r.session = ?1 AND j.value = ?2 ORDER BY r.id DESC LIMIT 1)",
            params![session, id],
        )?;
    }
    Ok(out.join("\n\n"))
}

/// Confirmed links of an item, as `type #id` / `#id type this`. With `visible`,
/// links to items outside those spaces are left out.
/// Whether a session limited to `visible` may read a record; `None` is the user.
fn sees(visible: Option<&[String]>, space: &str, status: &str) -> bool {
    visible.is_none_or(|v| {
        v.iter().any(|s| s == space) && !(space == "global" && status == "proposed")
    })
}

pub fn links(conn: &Connection, id: i64, visible: Option<&[String]>) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT e.type, e.dst, 1, i.space_id, i.status FROM edge e JOIN item i ON i.id = e.dst
         WHERE e.src = ?1 AND e.confirmed = 1
         UNION ALL
         SELECT e.type, e.src, 0, i.space_id, i.status FROM edge e JOIN item i ON i.id = e.src
         WHERE e.dst = ?1 AND e.confirmed = 1",
    )?;
    let rows = stmt.query_map([id], |r| {
        let (t, other, out, space, status): (String, i64, i64, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
        Ok((
            sees(visible, &space, &status),
            if out == 1 {
                format!("{t} #{other}")
            } else {
                format!("#{other} {t} this")
            },
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seen, link) = row?;
        if seen {
            out.push(link);
        }
    }
    Ok(out)
}

// ---------- write ----------

pub struct Write<'a> {
    pub kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub space: &'a str,
    pub source: Option<&'a str>,
    pub valid_until: Option<&'a str>,
    /// None (ask if similar exists), "add", "update:<id>", "supersede:<id>".
    pub mode: Option<&'a str>,
    /// Embedding of title + body, if the model is available.
    pub vector: Option<&'a [f32]>,
    /// Spaces checked for similar items, and the only ones update / supersede may touch.
    pub visible: &'a [String],
    /// Status of a new item: "active", or "proposed" when it needs a person's yes.
    pub status: &'a str,
}

#[derive(Debug)]
pub enum Written {
    Created(i64),
    Exists(i64),
    /// The same text was retired; the record is live again with this write's details.
    Restored(i64),
    Updated(i64),
    Superseded {
        new: i64,
        old: i64,
    },
    /// Nothing written: similar items exist, caller must choose a mode.
    Similar(Vec<(Hit, f32)>),
}

impl Written {
    pub fn message(&self) -> String {
        match self {
            Written::Created(id) => format!("saved #{id}"),
            Written::Exists(id) => format!("already stored as #{id}"),
            Written::Restored(id) => format!("restored #{id} from the archive"),
            Written::Updated(id) => format!("updated #{id}"),
            Written::Superseded { new, old } => format!("saved #{new}, #{old} marked superseded"),
            Written::Similar(similar) => {
                let lines: Vec<String> = similar
                    .iter()
                    .map(|(h, c)| format!("{} (cos {c:.2})", h.line()))
                    .collect();
                format!(
                    "not saved: similar items exist\n{}\nrepeat with mode=\"add\", \"update:<id>\" or \"supersede:<id>\"",
                    lines.join("\n")
                )
            }
        }
    }
}

pub fn write(conn: &Connection, w: &Write, actor: &str) -> Result<Written> {
    if !KINDS.contains(&w.kind) {
        bail!("kind must be one of {}", KINDS.join(", "));
    }
    let title = w.title.trim();
    let body = w.body.trim();
    if title.is_empty() || body.is_empty() {
        bail!("title and body are required");
    }
    if title.chars().count() > TITLE_MAX {
        bail!("title is over {TITLE_MAX} characters: make it the gist");
    }
    if body.chars().count() > BODY_MAX {
        bail!("body is over {BODY_MAX} characters: split it into separate facts");
    }
    if !w.visible.iter().any(|s| s == w.space) {
        bail!("space {} is not visible from this session", w.space);
    }
    let target = |prefix: &str| -> Result<Option<i64>> {
        w.mode
            .and_then(|m| m.strip_prefix(prefix))
            .map(|id| {
                id.trim_start_matches('#')
                    .parse::<i64>()
                    .context("bad id in mode")
            })
            .transpose()
    };
    let update = target("update:")?;
    let supersede = target("supersede:")?;
    if w.mode.is_some_and(|m| m != "add") && update.is_none() && supersede.is_none() {
        bail!("mode must be add, update:<id> or supersede:<id>");
    }
    // a global write from a session stays hidden until the user accepts it
    let hidden_global = w.space == "global" && w.status == "proposed";
    if hidden_global && supersede.is_some() {
        bail!(
            "supersede:<id> cannot go with space=\"global\": the global record waits for the \
             user, so the old one would be retired for nothing; save it without a mode and \
             tell the user which record it replaces"
        );
    }
    if let Some(d) = w.valid_until {
        let valid: bool = conn.query_row("SELECT date(?1) IS ?1", [d], |r| r.get(0))?;
        if !valid {
            bail!("valid_until must be a date as YYYY-MM-DD, got {d:?}");
        }
    }

    if w.mode.is_none()
        && let Some(v) = w.vector
    {
        let similar = similar(conn, v, w.visible)?;
        if !similar.is_empty() {
            return Ok(Written::Similar(similar));
        }
    }

    // BEGIN IMMEDIATE, and the checks inside it, so no other session changes the
    // targets between the check and the write
    let tx = crate::db::write_tx(conn)?;
    // a session may only change live items it can see
    for id in update.iter().chain(supersede.iter()) {
        let target: Option<(String, String)> = tx
            .query_row(
                "SELECT space_id, status FROM item WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match target {
            // a hidden global proposal answers like a missing id, so its existence stays hidden
            Some((space, status)) if space == "global" && status == "proposed" => {
                bail!("#{id} is not a live item visible from this session")
            }
            // every session sees global items, so one session must not rewrite or retire them
            Some((space, _)) if space == "global" => {
                bail!("#{id} is in global: a session cannot change it, ask the user")
            }
            Some((space, status))
                if w.visible.contains(&space) && (status == "active" || status == "proposed") => {}
            Some((_, status)) if w.visible.is_empty() => bail!("#{id} is {status}"),
            _ => bail!("#{id} is not a live item visible from this session"),
        }
    }

    let result = if let Some(id) = update {
        let before = store::item_json(&tx, id).context("no such item")?;
        let hash = store::content_hash(before["space_id"].as_str().unwrap_or_default(), body);
        let holder: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, status FROM item WHERE content_hash = ?1 AND id <> ?2",
                params![hash, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match holder {
            Some((other, status)) if status == "active" || status == "proposed" => {
                bail!(
                    "#{other} already holds this text; to retire #{id} in its favour, \
                     repeat with mode=\"supersede:{id}\""
                )
            }
            // a retired record holds the text: it comes back and takes over from the target
            Some((other, _)) => {
                let takes_over = Write {
                    status: before["status"].as_str().unwrap_or("active"),
                    ..*w
                };
                let valid_until = expiry(&tx, w)?;
                restore(
                    &tx,
                    other,
                    &takes_over,
                    title,
                    valid_until.as_deref(),
                    actor,
                )?;
                retire(&tx, id, other, actor)?;
                Written::Superseded {
                    new: other,
                    old: id,
                }
            }
            None => {
                tx.execute(
            "UPDATE item SET title = ?2, body = ?3, valid_until = COALESCE(?4, valid_until),
                             content_hash = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                id,
                title,
                body,
                w.valid_until,
                hash,
                store::now(&tx)?
            ],
        )?;
                store::log(
                    &tx,
                    actor,
                    "update",
                    Some(id),
                    Some(&before),
                    Some(&store::item_json(&tx, id)?),
                )?;
                set_vector(&tx, id, w.vector)?;
                Written::Updated(id)
            }
        }
    } else if let Some(live) = live_global_copy(&tx, hidden_global, body)? {
        // every session already sees a live global record: naming it reveals nothing
        Written::Exists(live)
    } else {
        let valid_until = expiry(&tx, w)?;
        let item = NewItem {
            space_id: w.space.into(),
            kind: w.kind.into(),
            title: title.into(),
            body: body.into(),
            status: w.status.into(),
            source: w.source.map(str::to_string),
            valid_until: valid_until.clone(),
            hash_scope: match w.visible.first() {
                Some(own) if hidden_global => {
                    Some(store::proposal_scope(&spaces::domain(&tx, own)?))
                }
                _ => None,
            },
            ..NewItem::default()
        };
        match store::insert_item(&tx, &item, actor)? {
            // the new text is already stored: a retired copy comes back, so the write
            // is never lost, and the old record can still retire in its favour
            Inserted::Duplicate(id) => {
                let restored = restore(&tx, id, w, title, valid_until.as_deref(), actor)?;
                match supersede.filter(|&old| old != id) {
                    Some(old) => {
                        retire(&tx, old, id, actor)?;
                        Written::Superseded { new: id, old }
                    }
                    None if restored => Written::Restored(id),
                    None => Written::Exists(id),
                }
            }
            Inserted::New(id) => {
                set_vector(&tx, id, w.vector)?;
                match supersede {
                    Some(old) => {
                        retire(&tx, old, id, actor)?;
                        Written::Superseded { new: id, old }
                    }
                    None => Written::Created(id),
                }
            }
        }
    };
    tx.commit()?;
    Ok(result)
}

/// A live global record holding `body`, for a session's global write.
fn live_global_copy(conn: &Connection, hidden_global: bool, body: &str) -> Result<Option<i64>> {
    if !hidden_global {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT id FROM item WHERE content_hash = ?1 AND status = 'active'",
            [store::content_hash("global", body)],
            |r| r.get(0),
        )
        .optional()?)
}

/// The end date of a new or restored record. State written without one still
/// expires, so it cannot linger forever.
fn expiry(conn: &Connection, w: &Write) -> Result<Option<String>> {
    Ok(match (w.valid_until, w.kind) {
        (Some(d), _) => Some(d.to_string()),
        (None, "state") => {
            Some(conn.query_row("SELECT date('now', '+30 days')", [], |r| r.get(0))?)
        }
        (None, _) => None,
    })
}

/// Makes a retired record live again with the details of `w`; false if it is live already.
fn restore(
    conn: &Connection,
    id: i64,
    w: &Write,
    title: &str,
    valid_until: Option<&str>,
    actor: &str,
) -> Result<bool> {
    let before = store::item_json(conn, id)?;
    if before["status"] == "active" || before["status"] == "proposed" {
        return Ok(false);
    }
    conn.execute(
        "UPDATE item SET status = ?2, kind = ?3, title = ?4, source = COALESCE(?5, source),
                         valid_until = ?6, superseded_by = NULL, updated_at = ?7 WHERE id = ?1",
        params![
            id,
            w.status,
            w.kind,
            title,
            w.source,
            valid_until,
            store::now(conn)?
        ],
    )?;
    conn.execute(
        "DELETE FROM edge WHERE dst = ?1 AND type = 'supersedes'",
        [id],
    )?;
    store::log(
        conn,
        actor,
        "restore",
        Some(id),
        Some(&before),
        Some(&store::item_json(conn, id)?),
    )?;
    if w.vector.is_some() {
        set_vector(conn, id, w.vector)?;
    }
    Ok(true)
}

fn retire(conn: &Connection, old: i64, new: i64, actor: &str) -> Result<()> {
    store::set_status(conn, old, "superseded", Some(new), actor)?;
    conn.execute(
        "INSERT OR IGNORE INTO edge (src, dst, type, confirmed) VALUES (?1, ?2, 'supersedes', 1)",
        params![new, old],
    )?;
    Ok(())
}

fn set_vector(conn: &Connection, id: i64, v: Option<&[f32]>) -> Result<()> {
    conn.execute("DELETE FROM item_vec WHERE item_id = ?1", [id])?;
    if let Some(v) = v {
        conn.execute(
            "INSERT INTO item_vec (item_id, emb) VALUES (?1, ?2)",
            params![id, embed::to_blob(v)],
        )?;
    }
    Ok(())
}

/// Visible live items with cosine >= SIMILAR to `v`, best first.
pub fn similar(conn: &Connection, v: &[f32], spaces: &[String]) -> Result<Vec<(Hit, f32)>> {
    Ok(closest(conn, v, spaces)?
        .into_iter()
        .filter(|(_, c)| *c >= SIMILAR)
        .collect())
}

/// Older records on the same topic as `v` that a new one may have made outdated:
/// close, but not close enough for `write` to refuse.
pub fn related(conn: &Connection, v: &[f32], spaces: &[String]) -> Result<Vec<(Hit, f32)>> {
    Ok(closest(conn, v, spaces)?
        .into_iter()
        .filter(|(_, c)| (RELATED..SIMILAR).contains(c))
        .collect())
}

/// The closest live items to `v` in `spaces`, best first.
fn closest(conn: &Connection, v: &[f32], spaces: &[String]) -> Result<Vec<(Hit, f32)>> {
    let q = Query {
        text: "",
        vector: Some(v),
        spaces,
        scope: Scope::Project,
        kind: None,
        limit: 3,
        as_of: None,
    };
    let (filter, filter_args) = filter(&q);
    // the vector is ?1; the filter's anonymous placeholders continue from 2
    let mut args = vec![Value::Blob(embed::to_blob(v))];
    args.extend(filter_args);
    let sql = format!(
        "SELECT i.id, i.kind, i.space_id, i.status, i.created_at, i.title, 1 - vec_distance_cosine(v.emb, ?1) AS cos
         FROM item i JOIN item_vec v ON v.item_id = i.id
         WHERE {filter} ORDER BY cos DESC LIMIT 3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok((
            Hit {
                id: r.get(0)?,
                kind: r.get(1)?,
                space: r.get(2)?,
                status: r.get(3)?,
                created: r.get(4)?,
                title: r.get(5)?,
                score: 0.0,
            },
            r.get::<_, f64>(6)? as f32,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_keep_identifiers() {
        assert_eq!(
            tokens("Чому ABC-123 і $VAR, foo_bar.rs?"),
            ["чому", "abc-123", "і", "$var", "foo_bar.rs"]
        );
    }
}
