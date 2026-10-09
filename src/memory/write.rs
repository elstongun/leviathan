//! `remember` and `forget`.
//!
//! With a key, a write replaces the slot's current value (the old row is
//! closed, not deleted). Without one, a near-duplicate of a current memory
//! merges into it; anything merely similar is stored and reported back as
//! `related`, so the agent can decide whether one replaces the other.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::guard::{self, Clean};
use super::{
    COLUMNS, Kind, Memory, MemoryRow, ensure_subject, get_row, insert_row, log, new_id, now, resolve_alias,
    row,
};
use crate::text::{Query, normalize_name, parse_unix, query_words};

/// Word overlap at which two memories about the same subject are the same
/// claim.
const DUPLICATE: f64 = 0.8;
/// Word overlap worth pointing out.
const RELATED: f64 = 0.3;

#[derive(Debug, Clone, Default)]
pub struct Remember {
    pub text: String,
    /// Default: `fact` with a key, `note` without.
    pub kind: Option<Kind>,
    pub subject: Option<String>,
    pub key: Option<String>,
    pub tags: Vec<String>,
    /// 1 (trivia) to 5 (critical); default 3.
    pub importance: Option<u8>,
    pub confidence: Option<f64>,
    pub source: Option<String>,
    /// Ids of related records in the data index, paths or URLs.
    pub refs: Vec<String>,
    pub pinned: bool,
    /// A date, or a duration from now (`12h`, `30d`, `6w`).
    pub expires: Option<String>,
    /// Other names for the subject.
    pub aliases: Vec<String>,
    /// Default: the configured namespace.
    pub ns: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStatus {
    Created,
    /// The slot's previous value was superseded.
    Replaced,
    /// Merged into an existing memory that says the same thing.
    Duplicate,
}

#[derive(Debug, Serialize)]
pub struct WriteOutcome {
    pub status: WriteStatus,
    pub memory: MemoryRow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced: Option<MemoryRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<MemoryRow>,
}

impl WriteOutcome {
    pub fn render(&self) -> String {
        let now = now();
        let mut out = match self.status {
            WriteStatus::Created => format!("remembered {}\n", self.memory.id),
            WriteStatus::Replaced => format!("remembered {} (replaced the previous value)\n", self.memory.id),
            WriteStatus::Duplicate => format!("already known: merged into {}\n", self.memory.id),
        };
        out.push_str(&format!("  {}\n", self.memory.line(&now, false)));
        if let Some(old) = &self.replaced {
            out.push_str(&format!("  was: {}\n", old.line(&now, false)));
        }
        if !self.related.is_empty() {
            out.push_str("related (if one of these is now wrong, forget it or give both the same key):\n");
            for m in &self.related {
                out.push_str(&format!("  {}\n", m.line(&now, false)));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct Forget {
    pub id: Option<String>,
    pub subject: Option<String>,
    pub key: Option<String>,
    pub ns: Option<String>,
    pub reason: Option<String>,
}

fn word_set(text: &str) -> HashSet<String> {
    query_words(text)
        .map(|w| {
            if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w
            }
        })
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

fn merge_lists(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| !a.contains(x)).cloned());
    out
}

impl Memory {
    pub fn remember(&mut self, req: &Remember) -> Result<WriteOutcome> {
        let now = now();
        let c: Clean = guard::check(req, &self.settings, parse_unix(&now).unwrap_or(0))?;
        let tx = self.write_tx()?;

        let (subject, subject_norm) = match &c.subject {
            Some(s) => {
                let norm = resolve_alias(&tx, &c.ns, &normalize_name(s))?;
                (Some(ensure_subject(&tx, &c.ns, s, &norm)?), norm)
            }
            None => (None, String::new()),
        };
        for alias in &c.aliases {
            let a = normalize_name(alias);
            if a != subject_norm {
                tx.execute(
                    "INSERT INTO subject_aliases (ns, alias, norm) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (ns, alias) DO UPDATE SET norm = excluded.norm",
                    params![c.ns, a, subject_norm],
                )?;
            }
        }

        let fresh = MemoryRow {
            id: new_id(),
            ns: c.ns.clone(),
            kind: c.kind,
            subject,
            key: c.key.clone(),
            text: c.text.clone(),
            tags: c.tags.clone(),
            importance: c.importance,
            confidence: c.confidence,
            source: c.source.clone(),
            refs: c.refs.clone(),
            pinned: c.pinned,
            created_at: now.clone(),
            updated_at: now.clone(),
            valid_from: now.clone(),
            valid_to: None,
            superseded_by: None,
            expires_at: c.expires_at.clone(),
            deleted_at: None,
            recall_count: 0,
            last_recalled_at: None,
        };
        let words = word_set(&c.text);

        let outcome = if let Some(key) = &c.key {
            let current: Option<MemoryRow> = tx
                .prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM memories WHERE ns = ?1 AND subject_norm = ?2 AND key = ?3 \
                     AND valid_to IS NULL AND deleted_at IS NULL"
                ))?
                .query_row(params![c.ns, subject_norm, key], row)
                .optional()?;
            match current {
                Some(old) if jaccard(&word_set(&old.text), &words) >= 1.0 && old.kind == c.kind => {
                    let merged = merge(&tx, &old, &c, req, &now)?;
                    WriteOutcome {
                        status: WriteStatus::Duplicate,
                        memory: merged,
                        replaced: None,
                        related: vec![],
                    }
                }
                Some(mut old) => {
                    tx.execute(
                        "UPDATE memories SET valid_to = ?2, superseded_by = ?3 WHERE id = ?1",
                        params![old.id, now, fresh.id],
                    )?;
                    old.valid_to = Some(now.clone());
                    old.superseded_by = Some(fresh.id.clone());
                    insert_row(&tx, &fresh)?;
                    log(
                        &tx,
                        "supersede",
                        Some(&fresh.id),
                        &serde_json::json!({"memory": fresh, "replaced": old.id}),
                    )?;
                    WriteOutcome {
                        status: WriteStatus::Replaced,
                        memory: fresh,
                        replaced: Some(old),
                        related: vec![],
                    }
                }
                None => {
                    insert_row(&tx, &fresh)?;
                    log(&tx, "remember", Some(&fresh.id), &serde_json::to_value(&fresh)?)?;
                    WriteOutcome {
                        status: WriteStatus::Created,
                        memory: fresh,
                        replaced: None,
                        related: vec![],
                    }
                }
            }
        } else {
            let mut similar: Vec<(f64, MemoryRow)> =
                similar(&tx, &c.ns, c.subject.as_ref().map(|_| subject_norm.as_str()), &c.text)?
                    .into_iter()
                    .map(|m| (jaccard(&word_set(&m.text), &words), m))
                    .collect();
            similar.sort_by(|a, b| b.0.total_cmp(&a.0));
            match similar.first() {
                Some((score, best)) if *score >= DUPLICATE && best.key.is_none() => {
                    let merged = merge(&tx, best, &c, req, &now)?;
                    WriteOutcome {
                        status: WriteStatus::Duplicate,
                        memory: merged,
                        replaced: None,
                        related: vec![],
                    }
                }
                _ => {
                    insert_row(&tx, &fresh)?;
                    log(&tx, "remember", Some(&fresh.id), &serde_json::to_value(&fresh)?)?;
                    let related =
                        similar.into_iter().filter(|(s, _)| *s >= RELATED).take(3).map(|(_, m)| m).collect();
                    WriteOutcome { status: WriteStatus::Created, memory: fresh, replaced: None, related }
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Soft delete: the row stays (marked forgotten) until `prune`.
    pub fn forget(&mut self, f: &Forget) -> Result<MemoryRow> {
        let now = now();
        let ns = guard::namespace(f.ns.as_deref().unwrap_or(&self.settings.namespace))?;
        let tx = self.write_tx()?;
        let target = match (&f.id, &f.key) {
            (Some(id), _) => get_row(&tx, id)?.with_context(|| format!("no memory with id {id:?}"))?,
            (None, Some(key)) => {
                let norm = match &f.subject {
                    Some(s) => resolve_alias(&tx, &ns, &normalize_name(s))?,
                    None => String::new(),
                };
                let key = guard::key(key)?;
                tx.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM memories WHERE ns = ?1 AND subject_norm = ?2 AND key = ?3 \
                     AND valid_to IS NULL AND deleted_at IS NULL"
                ))?
                .query_row(params![ns, norm, key], row)
                .optional()?
                .with_context(|| {
                    format!(
                        "nothing current for key {key:?}{} in {ns}",
                        f.subject.as_deref().map(|s| format!(" of {s:?}")).unwrap_or_default()
                    )
                })?
            }
            _ => bail!("pass a memory id, or a key (with its subject)"),
        };
        if target.deleted_at.is_some() {
            bail!("{} was already forgotten", target.id);
        }
        tx.execute(
            "UPDATE memories SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
            params![target.id, now],
        )?;
        log(&tx, "forget", Some(&target.id), &serde_json::json!({"reason": f.reason}))?;
        let forgotten = get_row(&tx, &target.id)?.context("forgotten row vanished")?;
        tx.commit()?;
        Ok(forgotten)
    }
}

/// Current memories that share words with `text`, best BM25 first.
fn similar(
    conn: &rusqlite::Connection,
    ns: &str,
    subject_norm: Option<&str>,
    text: &str,
) -> Result<Vec<MemoryRow>> {
    let mut seen = HashSet::new();
    let q =
        Query { words: query_words(text).filter(|w| seen.insert(w.clone())).collect(), ..Default::default() };
    let Some(expr) = q.fts() else { return Ok(Vec::new()) };
    let cols = COLUMNS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ");
    let subject_sql = if subject_norm.is_some() { "AND m.subject_norm = ?3" } else { "AND ?3 IS NULL" };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {cols} FROM memory_fts JOIN memories AS m ON m.rowid = memory_fts.rowid \
         WHERE memory_fts MATCH ?1 AND m.ns = ?2 {subject_sql} AND m.valid_to IS NULL AND m.deleted_at IS NULL \
         ORDER BY memory_fts.rank LIMIT 20"
    ))?;
    Ok(stmt.query_map(params![expr, ns, subject_norm], row)?.collect::<rusqlite::Result<_>>()?)
}

/// Fold a restatement into the existing memory: it is reaffirmed (fresh
/// `updated_at`), a little more important, and keeps every tag and ref.
fn merge(
    conn: &rusqlite::Connection,
    old: &MemoryRow,
    c: &Clean,
    req: &Remember,
    now: &str,
) -> Result<MemoryRow> {
    let importance = (old.importance + 1).min(5).max(req.importance.unwrap_or(0)).max(old.importance);
    let tags = merge_lists(&old.tags, &c.tags);
    let refs = merge_lists(&old.refs, &c.refs);
    conn.execute(
        "UPDATE memories SET importance = ?2, pinned = pinned OR ?3, tags = ?4, refs = ?5, updated_at = ?6, \
         confidence = MAX(confidence, ?7), expires_at = COALESCE(?8, expires_at) WHERE id = ?1",
        params![
            old.id,
            importance,
            c.pinned,
            tags.join(" "),
            serde_json::to_string(&refs)?,
            now,
            c.confidence,
            c.expires_at
        ],
    )?;
    let merged = get_row(conn, &old.id)?.context("merged row vanished")?;
    log(conn, "merge", Some(&old.id), &serde_json::json!({"text": c.text, "source": c.source}))?;
    Ok(merged)
}
