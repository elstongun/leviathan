//! `recall` and the session-start briefing.
//!
//! Candidates come from FTS5 (with words) or the current set (without),
//! scoped to namespaces, a subject, kinds and dates. Each is scored on
//! relevance, recency (a half-life per kind, from when it was last
//! reaffirmed), importance, pinning and confidence, then cards are packed
//! in score order until the token budget is spent. Nothing is dropped
//! silently: the header says how many matched and how many fit.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde::Serialize;

use super::{COLUMNS, Kind, Memory, MemoryRow, State, now, placeholders, row};
use crate::card::Card;
use crate::query::Store;
use crate::text::{Query, normalize_bound, normalize_name, parse_unix};

/// With words, matches under this share of the best match's relevance are
/// noise (one shared word); they are counted in a note, not shown.
const RELEVANCE_FLOOR: f64 = 0.5;

/// Lowercase alphanumeric words joined by single spaces.
fn plain_words(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Memory lines are dense (names, dates, ids): about three characters per
/// token with common tokenizers, so budgets hold in practice.
pub fn tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

#[derive(Debug, Clone, Default)]
pub struct RecallRequest {
    pub query: String,
    /// Comma list, `*` for all; default the configured `read` namespaces.
    pub ns: Option<String>,
    pub subject: Option<String>,
    pub kinds: Vec<Kind>,
    pub since: Option<String>,
    pub until: Option<String>,
    /// What was true at the end of this date.
    pub as_of: Option<String>,
    /// Include superseded, forgotten and expired memories.
    pub history: bool,
    /// Tokens; default the configured budget.
    pub budget: Option<usize>,
    pub limit: Option<usize>,
    /// Append the data-index records that memories reference.
    pub with_records: bool,
    /// Briefing mode: spread across subjects, pinned subjects first.
    pub briefing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallStatus {
    Ok,
    AmbiguousSubject,
    UnknownSubject,
}

#[derive(Debug, Serialize)]
pub struct Recalled {
    #[serde(flatten)]
    pub memory: MemoryRow,
    pub state: State,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct RecallOutcome {
    pub status: RecallStatus,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub query: String,
    pub namespaces: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// Matches before the budget and limit.
    pub total_matches: usize,
    pub budget: usize,
    pub tokens: usize,
    pub results: Vec<Recalled>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<Card>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip)]
    now: String,
    #[serde(skip)]
    briefing: bool,
}

impl RecallOutcome {
    pub fn render(&self) -> String {
        let show_ns = self.namespaces.len() > 1;
        let mut out = String::new();
        let scope = match &self.subject {
            Some(s) => format!(" about {s}"),
            None => String::new(),
        };
        let what = if self.query.is_empty() { String::new() } else { format!(" for \"{}\"", self.query) };
        match self.status {
            RecallStatus::AmbiguousSubject => {
                out.push_str(&format!(
                    "memory: subject is ambiguous; ask which one is meant: {}\n",
                    self.candidates.join(", ")
                ));
                return out;
            }
            RecallStatus::UnknownSubject => {
                out.push_str("memory: no memories about that subject");
                if !self.candidates.is_empty() {
                    out.push_str(&format!(" (known: {})", self.candidates.join(", ")));
                }
                out.push('\n');
                return out;
            }
            RecallStatus::Ok => {}
        }
        if self.briefing {
            out.push_str(&format!(
                "Leviathan memory ({}): {} of {} current memories, ~{} tokens.\n",
                self.namespaces.join(", "),
                self.results.len(),
                self.total_matches,
                self.tokens
            ));
        } else if self.total_matches == 0 {
            out.push_str(&format!(
                "memory: nothing{what}{scope} in {}. Not remembered does not mean untrue; try other words, or recall with no words to browse.\n",
                self.namespaces.join(", ")
            ));
        } else {
            out.push_str(&format!(
                "memory{what}{scope} · shown {} of {} · ~{}/{} tokens · {}\n",
                self.results.len(),
                self.total_matches,
                self.tokens,
                self.budget,
                self.namespaces.join(", ")
            ));
        }
        for r in &self.results {
            out.push_str(&r.memory.line(&self.now, show_ns));
            out.push('\n');
        }
        if !self.records.is_empty() {
            out.push_str("linked records:\n");
            for (i, c) in self.records.iter().enumerate() {
                crate::render::card(&mut out, i + 1, c, true, "group");
            }
        }
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        if self.briefing {
            out.push_str(
                "Use `recall` with words before acting on a subject. `remember` durable facts, preferences, \
                 decisions (with why) and lessons; give anything that can change a `key`. Never store secrets.\n",
            );
        }
        out
    }
}

struct Candidate {
    m: MemoryRow,
    relevance: f64,
}

impl Memory {
    pub fn recall(&mut self, req: &RecallRequest, data: Option<&Store>) -> Result<RecallOutcome> {
        let now = now();
        let now_secs = parse_unix(&now).unwrap_or(0);
        let namespaces = self.read_namespaces(req.ns.as_deref())?;
        let budget = req
            .budget
            .unwrap_or(if req.briefing { self.settings.briefing_budget } else { self.settings.budget })
            .clamp(50, 20_000);
        let limit = req.limit.unwrap_or(50).clamp(1, 200);
        let mut out = RecallOutcome {
            status: RecallStatus::Ok,
            query: req.query.trim().to_string(),
            namespaces: namespaces.clone(),
            subject: None,
            candidates: Vec::new(),
            total_matches: 0,
            budget,
            tokens: 0,
            results: Vec::new(),
            records: Vec::new(),
            notes: Vec::new(),
            now: now.clone(),
            briefing: req.briefing,
        };

        let mut clauses = vec![format!("m.ns IN ({})", placeholders(namespaces.len(), 2))];
        let mut args: Vec<SqlValue> = vec![SqlValue::Null];
        args.extend(namespaces.iter().cloned().map(SqlValue::Text));
        let arg = |args: &mut Vec<SqlValue>, v: String| {
            args.push(SqlValue::Text(v));
            format!("?{}", args.len())
        };

        if let Some(raw) = req.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            match self.resolve_subject(&namespaces, raw)? {
                SubjectMatch::Found(norm, name, note) => {
                    let p = arg(&mut args, norm);
                    clauses.push(format!("m.subject_norm = {p}"));
                    out.subject = Some(name);
                    out.notes.extend(note);
                }
                SubjectMatch::Ambiguous(names) => {
                    out.status = RecallStatus::AmbiguousSubject;
                    out.candidates = names;
                    return Ok(out);
                }
                SubjectMatch::Unknown(known) => {
                    out.status = RecallStatus::UnknownSubject;
                    out.candidates = known;
                    return Ok(out);
                }
            }
        }
        if !req.kinds.is_empty() {
            let list: Vec<String> =
                req.kinds.iter().map(|k| arg(&mut args, k.as_str().to_string())).collect();
            clauses.push(format!("m.kind IN ({})", list.join(", ")));
        }
        for (raw, op) in [(&req.since, ">="), (&req.until, "<=")] {
            if let Some(raw) = raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let b = normalize_bound(raw).ok_or_else(|| {
                    super::Refused(format!("date {raw:?}: use YYYY, YYYY-MM or YYYY-MM-DD"))
                })?;
                let len = b.len();
                let p = arg(&mut args, b);
                clauses.push(format!("substr(m.valid_from, 1, {len}) {op} {p}"));
            }
        }
        if let Some(raw) = req.as_of.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let b = normalize_bound(raw)
                .ok_or_else(|| super::Refused(format!("as_of {raw:?}: use YYYY, YYYY-MM or YYYY-MM-DD")))?;
            let len = b.len();
            let p = arg(&mut args, b);
            clauses.push(format!(
                "substr(m.valid_from, 1, {len}) <= {p} AND (m.valid_to IS NULL OR substr(m.valid_to, 1, {len}) > {p}) \
                 AND (m.deleted_at IS NULL OR substr(m.deleted_at, 1, {len}) > {p})"
            ));
        } else if !req.history {
            let p = arg(&mut args, now.clone());
            clauses.push(format!(
                "m.valid_to IS NULL AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > {p})"
            ));
        }

        let q = Query::parse(&req.query);
        let cols = COLUMNS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ");
        let where_sql = clauses.join(" AND ");
        let candidates: Vec<Candidate> = if let Some(expr) = q.fts() {
            let expr = match q.not_fts() {
                Some(not) => format!("({expr}) NOT ({not})"),
                None => expr,
            };
            args[0] = SqlValue::Text(expr);
            let sql = if out.subject.is_some() {
                // A subject has few memories: rank just those (an FTS5 rowid
                // lookup each) instead of every match of common words.
                format!(
                    "SELECT * FROM (SELECT {cols}, (SELECT rank FROM memory_fts WHERE memory_fts MATCH ?1 \
                     AND memory_fts.rowid = m.rowid) AS r FROM memories AS m WHERE {where_sql}) \
                     WHERE r IS NOT NULL ORDER BY r LIMIT 400"
                )
            } else {
                format!(
                    "SELECT {cols}, memory_fts.rank FROM memory_fts JOIN memories AS m ON m.rowid = memory_fts.rowid \
                     WHERE memory_fts MATCH ?1 AND {where_sql} ORDER BY memory_fts.rank LIMIT 400"
                )
            };
            let mut stmt = self.conn.prepare(&sql)?;
            let rows: Vec<(MemoryRow, f64)> = stmt
                .query_map(params_from_iter(args), |r| Ok((row(r)?, r.get::<_, f64>(21)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let best = rows.iter().map(|(_, rank)| -rank).fold(f64::MIN_POSITIVE, f64::max);
            rows.into_iter()
                .map(|(m, rank)| Candidate { m, relevance: (-rank / best).clamp(0.0, 1.0) })
                .collect()
        } else {
            let sql = format!(
                "SELECT {cols} FROM memories AS m WHERE ?1 IS NULL AND {where_sql} \
                 ORDER BY m.pinned DESC, m.importance DESC, m.updated_at DESC LIMIT 2000"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(args), row)?
                .map(|m| m.map(|m| Candidate { m, relevance: 0.0 }))
                .collect::<rusqlite::Result<_>>()?
        };

        let has_query = q.fts().is_some();
        let mut candidates = candidates;
        if has_query && req.subject.is_none() {
            // Words that name a subject make other subjects' memories noise.
            let words = format!(" {} ", plain_words(&req.query));
            let named: HashSet<String> = candidates
                .iter()
                .filter_map(|c| c.m.subject.as_deref().map(plain_words))
                .filter(|s| !s.is_empty() && words.contains(&format!(" {s} ")))
                .collect();
            if !named.is_empty() {
                for c in &mut candidates {
                    if !c.m.subject.as_deref().is_some_and(|s| named.contains(&plain_words(s))) {
                        c.relevance *= 0.5;
                    }
                }
            }
        }
        let pinned_subjects: HashSet<String> = self.settings.pin.iter().map(|s| normalize_name(s)).collect();
        let found = candidates.len();
        let mut scored: Vec<(f64, MemoryRow)> = candidates
            .into_iter()
            .filter(|c| !has_query || c.relevance >= RELEVANCE_FLOOR)
            .map(|c| {
                let age_days =
                    (now_secs - parse_unix(&c.m.updated_at).unwrap_or(now_secs)).max(0) as f64 / 86_400.0;
                let half_life = self.settings.half_life(c.m.kind);
                let recency = if half_life > 0.0 { 0.5f64.powf(age_days / half_life) } else { 1.0 };
                let importance = f64::from(c.m.importance - 1) / 4.0;
                let pinned = c.m.pinned
                    || c.m.subject.as_deref().is_some_and(|s| pinned_subjects.contains(&normalize_name(s)));
                let pinned = if pinned { 1.0 } else { 0.0 };
                let used = (c.m.recall_count.min(10) as f64) / 10.0;
                let score = if has_query {
                    0.6 * c.relevance + 0.15 * recency + 0.15 * importance + 0.07 * pinned + 0.03 * used
                } else {
                    0.4 * importance + 0.3 * recency + 0.25 * pinned + 0.05 * used
                };
                ((score * c.m.confidence.clamp(0.1, 1.0) * 1000.0).round() / 1000.0, c.m)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.updated_at.cmp(&a.1.updated_at)));
        out.total_matches = scored.len();
        if found > scored.len() {
            out.notes.push(format!(
                "{} weaker matches left out; use other words, or subject or kind, to see them",
                found - scored.len()
            ));
        }

        // Briefings spread over subjects: at most three per subject until
        // every subject has had a turn, then the rest by score.
        let order: Vec<usize> = if req.briefing {
            let mut per_subject: std::collections::HashMap<String, usize> = Default::default();
            let (first, rest): (Vec<usize>, Vec<usize>) = (0..scored.len()).partition(|i| {
                let n = per_subject
                    .entry(scored[*i].1.subject.as_deref().map(normalize_name).unwrap_or_default())
                    .or_default();
                *n += 1;
                *n <= 3
            });
            first.into_iter().chain(rest).collect()
        } else {
            (0..scored.len()).collect()
        };

        let mut used = 0usize;
        let mut picked = Vec::new();
        for i in order {
            if picked.len() >= limit {
                break;
            }
            let (score, m) = &scored[i];
            let cost = tokens(&m.line(&now, namespaces.len() > 1)) + 1;
            if used + cost > budget && !picked.is_empty() {
                continue;
            }
            used += cost;
            picked.push(Recalled { memory: m.clone(), state: m.state(&now), score: *score });
        }
        if picked.len() < out.total_matches {
            out.notes.push(format!(
                "{} more matched but did not fit; raise `budget`, narrow by subject or kind, or page with words",
                out.total_matches - picked.len()
            ));
        }

        if req.with_records {
            match data {
                Some(store) => {
                    let mut seen = HashSet::new();
                    'refs: for r in &picked {
                        for id in &r.memory.refs {
                            if !seen.insert(id.clone()) {
                                continue;
                            }
                            let Some(card) = store.card_by_id(id)? else { continue };
                            let mut text = String::new();
                            crate::render::card(&mut text, 1, &card, true, "group");
                            let cost = tokens(&text);
                            if used + cost > budget {
                                out.notes.push("linked records omitted: budget spent".into());
                                break 'refs;
                            }
                            used += cost;
                            out.records.push(card);
                        }
                    }
                }
                None => out.notes.push("with_records needs a data index (--index)".into()),
            }
        }
        out.tokens = used;

        if !req.history && req.as_of.is_none() && !picked.is_empty() {
            let ids: Vec<String> = picked.iter().map(|r| r.memory.id.clone()).collect();
            let sql = format!(
                "UPDATE memories SET recall_count = recall_count + 1, last_recalled_at = ?1 WHERE id IN ({})",
                placeholders(ids.len(), 2)
            );
            let mut args: Vec<SqlValue> = vec![SqlValue::Text(now.clone())];
            args.extend(ids.into_iter().map(SqlValue::Text));
            // Usage counts are a ranking hint; a read-only file must not fail a recall.
            let _ = self.conn.execute(&sql, params_from_iter(args));
        }
        out.results = picked;
        Ok(out)
    }

    /// The session-start view: pinned and important memories across
    /// subjects, within the briefing budget.
    pub fn briefing(
        &mut self,
        ns: Option<&str>,
        budget: Option<usize>,
        data: Option<&Store>,
    ) -> Result<RecallOutcome> {
        let req = RecallRequest { ns: ns.map(str::to_string), budget, briefing: true, ..Default::default() };
        self.recall(&req, data)
    }

    fn resolve_subject(&self, namespaces: &[String], raw: &str) -> Result<SubjectMatch> {
        let norm = normalize_name(raw);
        let ns_sql = placeholders(namespaces.len(), 2);
        let mut args: Vec<SqlValue> = vec![SqlValue::Text(norm.clone())];
        args.extend(namespaces.iter().cloned().map(SqlValue::Text));
        let names = |sql: &str, args: &[SqlValue]| -> Result<Vec<(String, String)>> {
            let mut stmt = self.conn.prepare(sql)?;
            Ok(stmt
                .query_map(params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        };
        // Two indexed lookups; an OR across them would scan every subject.
        let mut exact =
            names(&format!("SELECT norm, name FROM subjects WHERE ns IN ({ns_sql}) AND norm = ?1"), &args)?;
        if exact.is_empty() {
            exact = names(
                &format!(
                    "SELECT s.norm, s.name FROM subject_aliases AS a JOIN subjects AS s ON s.ns = a.ns AND s.norm = a.norm \
                     WHERE a.ns IN ({ns_sql}) AND a.alias = ?1"
                ),
                &args,
            )?;
        }
        if let Some((norm, name)) = exact.first() {
            return Ok(SubjectMatch::Found(norm.clone(), name.clone(), None));
        }
        let all =
            names(&format!("SELECT norm, name FROM subjects WHERE ns IN ({ns_sql}) AND ?1 = ?1"), &args)?;
        let mut contains: Vec<&(String, String)> =
            all.iter().filter(|(n, _)| n.contains(&norm) || norm.contains(n.as_str())).collect();
        if contains.is_empty() {
            contains = all.iter().filter(|(n, _)| strsim::sorensen_dice(n, &norm) >= 0.6).collect();
        }
        contains.dedup_by(|a, b| a.0 == b.0);
        Ok(match contains.as_slice() {
            [] => SubjectMatch::Unknown(all.iter().take(12).map(|(_, name)| name.clone()).collect()),
            [(n, name)] => SubjectMatch::Found(
                n.clone(),
                name.clone(),
                Some(format!("subject {raw:?} matched {name:?}")),
            ),
            many => SubjectMatch::Ambiguous(many.iter().map(|(_, name)| name.clone()).collect()),
        })
    }
}

enum SubjectMatch {
    Found(String, String, Option<String>),
    Ambiguous(Vec<String>),
    Unknown(Vec<String>),
}
