//! Read-only query layer over the index.
//!
//! Retrieval contract:
//! 1. Resolve the group first (exact key, case-insensitive key, normalized
//!    name, substring, then fuzzy). More than one candidate in the winning
//!    tier is reported as ambiguous; Leviathan never guesses.
//! 2. Rank matching records by BM25 (title weighted), times the configured
//!    boosts, then recency on ties. Group scope and filters are posting-list
//!    intersections on synthetic tokens, so they cost less as they narrow.
//! 3. With no query words, list records newest first.
//! 4. If a group has no match, fall back to clearly labeled matches from
//!    other groups.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};
use serde::Serialize;

use crate::card::{Card, CardOptions};
use crate::config::{About, Config, Fields, Rank, plural};
use crate::fields::Mapping;
use crate::index::{check_schema, meta, open_ro, stored_config};
use crate::text::{Query, facet_token, group_token, normalize_bound, normalize_name};

const FUZZY_CUTOFF: f64 = 0.6;
pub const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Only the named group.
    Group,
    /// Every group except the named one.
    Others,
    /// Everything.
    All,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Best match first (newest first when there are no query words).
    #[default]
    Relevance,
    Newest,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupMatch {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub records: i64,
    pub match_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    AmbiguousGroup,
    UnknownGroup,
    BadRequest,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub group: Option<String>,
    pub query: String,
    /// Defaults to `Group` when a group is given, else `All`.
    pub scope: Option<Scope>,
    /// `field = value` pairs; several values for one field mean any of them.
    pub filters: Vec<(String, String)>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub sort: Sort,
    pub limit: usize,
    pub offset: usize,
    /// When the group has no match, add labeled results from other groups.
    pub fallback: bool,
}

#[derive(Debug, Serialize)]
pub struct Filter {
    pub field: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct SearchOutcome {
    pub status: Status,
    pub query: String,
    pub scope: Scope,
    pub sort: Sort,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupMatch>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<GroupMatch>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Matches in scope before limit/offset. Zero means none found.
    pub total_matches: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub offset: usize,
    pub results: Vec<Card>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_groups: Vec<Card>,
    pub corpus_records: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip)]
    pub about: About,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct FacetSummary {
    pub field: String,
    pub distinct: i64,
    pub top: Vec<(String, i64)>,
}

/// Everything an agent needs to query this index well.
#[derive(Debug, Serialize)]
pub struct Description {
    pub index_path: PathBuf,
    pub index_bytes: u64,
    pub about: About,
    pub fields: Fields,
    pub rank: Rank,
    pub record_count: i64,
    pub group_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_min: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_max: Option<String>,
    pub filters: Vec<FacetSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub largest_groups: Vec<GroupMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example_id: Option<String>,
    pub mapping_inferred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub skipped_lines: i64,
    pub schema_version: String,
}

enum Pick {
    One(GroupMatch),
    Many(Vec<GroupMatch>),
    None,
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
    stamp: Option<(u64, SystemTime)>,
    mapping: Mapping,
    pub card_opts: CardOptions,
}

fn file_stamp(path: &Path) -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

impl Store {
    pub fn open(path: &Path, card_opts: CardOptions) -> Result<Self> {
        if !path.exists() {
            bail!(
                "no index at {}. Build one with `leviathan index <data>` \
                 or point --index / LEVIATHAN_INDEX at an existing one",
                path.display()
            );
        }
        let conn = open_ro(path).with_context(|| format!("open index {}", path.display()))?;
        check_schema(&conn, path)?;
        let mapping = Mapping::new(&stored_config(&conn)?)?;
        Ok(Self { conn, path: path.to_path_buf(), stamp: file_stamp(path), mapping, card_opts })
    }

    /// Reopen when the index file was atomically replaced by a rebuild.
    pub fn reload_if_replaced(&mut self) -> Result<bool> {
        let now = file_stamp(&self.path);
        if now.is_some() && now != self.stamp {
            *self = Self::open(&self.path, self.card_opts)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn config(&self) -> &Config {
        &self.mapping.config
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        meta(&self.conn, key)
    }

    fn meta_i64(&self, key: &str) -> Result<i64> {
        Ok(self.meta(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    // ----------------------------------------------------------------- groups

    fn groups_where(
        &self,
        clause: &str,
        arg: &str,
        match_type: &'static str,
        limit: usize,
    ) -> Result<Vec<GroupMatch>> {
        let sql = format!(
            "SELECT key, name, record_count FROM groups WHERE {clause} ORDER BY record_count DESC, key LIMIT ?2"
        );
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_map(rusqlite::params![arg, limit as i64], |r| group_row(r, match_type))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Group key or name to candidates, best tier first.
    pub fn resolve(&self, query: &str, limit: usize) -> Result<Vec<GroupMatch>> {
        let query = query.trim();
        if query.is_empty() || !self.mapping.has_group() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_LIMIT);
        let lower = query.to_lowercase();
        let normalized = normalize_name(query);
        let tiers: [(&str, &str, &'static str); 3] = [
            ("key = ?1", query, "exact"),
            ("key_lower = ?1", &lower, "case_insensitive"),
            ("name_normalized = ?1", &normalized, "name"),
        ];
        for (clause, arg, kind) in tiers {
            let rows = self.groups_where(clause, arg, kind, limit)?;
            if !rows.is_empty() {
                return Ok(rows);
            }
        }

        let escaped = normalized.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let contains = self.groups_where(
            "name_normalized LIKE '%' || ?1 || '%' ESCAPE '\\' OR key_lower LIKE '%' || ?1 || '%' ESCAPE '\\'",
            &escaped,
            "contains",
            limit,
        )?;
        if !contains.is_empty() {
            return Ok(contains);
        }

        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, name, record_count, key_lower, name_normalized FROM groups")?;
        let mut scored: Vec<GroupMatch> = stmt
            .query_map([], |r| {
                let key_lower: Option<String> = r.get(3)?;
                let name_norm: Option<String> = r.get(4)?;
                let mut m = group_row(r, "fuzzy")?;
                let score = [key_lower, name_norm]
                    .iter()
                    .flatten()
                    .map(|c| strsim::sorensen_dice(&normalized, c))
                    .fold(0.0, f64::max);
                m.similarity = Some((score * 100.0).round() / 100.0);
                Ok(m)
            })?
            .filter_map(|m| m.ok())
            .filter(|m| m.similarity.unwrap_or(0.0) >= FUZZY_CUTOFF)
            .collect();
        scored
            .sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap().then(b.records.cmp(&a.records)));
        scored.truncate(limit);
        Ok(scored)
    }

    fn pick(&self, group: &str) -> Result<Pick> {
        let mut found = self.resolve(group, 10)?;
        Ok(match found.len() {
            0 => Pick::None,
            1 => Pick::One(found.remove(0)),
            _ => Pick::Many(found),
        })
    }

    // ----------------------------------------------------------------- search

    pub fn search(&self, req: &SearchRequest) -> Result<SearchOutcome> {
        let about = self.config().about.clone();
        let group_noun = about.group.clone();
        let limit = req.limit.clamp(1, MAX_LIMIT);
        let scope = if req.group.is_some() { req.scope.unwrap_or(Scope::Group) } else { Scope::All };
        let mut out = SearchOutcome {
            status: Status::Ok,
            query: req.query.clone(),
            scope,
            sort: req.sort,
            group: None,
            candidates: Vec::new(),
            filters: req.filters.iter().map(|(f, v)| Filter { field: f.clone(), value: v.clone() }).collect(),
            since: None,
            until: None,
            total_matches: 0,
            offset: req.offset,
            results: Vec::new(),
            other_groups: Vec::new(),
            corpus_records: self.meta_i64("record_count")?,
            notes: Vec::new(),
            about,
        };
        let bad = |mut out: SearchOutcome, note: String| {
            out.status = Status::BadRequest;
            out.notes.push(note);
            Ok(out)
        };

        let filterable: Vec<&str> = self.mapping.filters.iter().map(|p| p.raw.as_str()).collect();
        for (field, _) in &req.filters {
            if !filterable.contains(&field.as_str()) {
                let available =
                    if filterable.is_empty() { "none".to_string() } else { filterable.join(", ") };
                return bad(out, format!("`{field}` is not a filter field; filterable: {available}"));
            }
        }
        for raw in [&req.since, &req.until].into_iter().flatten() {
            if normalize_bound(raw).is_none() {
                return bad(out, format!("unrecognized date {raw:?}; use YYYY, YYYY-MM or YYYY-MM-DD"));
            }
        }
        out.since = req.since.as_deref().and_then(normalize_bound);
        out.until = req.until.as_deref().and_then(normalize_bound);
        if (out.since.is_some() || out.until.is_some()) && !self.mapping.has_date() {
            return bad(out, "this index has no date field, so --since/--until cannot apply".into());
        }

        if let Some(group) = &req.group {
            if !self.mapping.has_group() {
                return bad(out, "this index has no group field; search without one".into());
            }
            match self.pick(group)? {
                Pick::One(m) => out.group = Some(m),
                Pick::Many(c) => {
                    out.status = Status::AmbiguousGroup;
                    out.notes.push(format!(
                        "{} {} match {group:?}; ask which one, then retry with its exact key",
                        c.len(),
                        plural(&group_noun)
                    ));
                    out.candidates = c;
                    return Ok(out);
                }
                Pick::None => {
                    out.status = Status::UnknownGroup;
                    out.notes.push(format!(
                        "no {group_noun} matches {group:?}; try `resolve` with part of the name, or search without one"
                    ));
                    return Ok(out);
                }
            }
        }

        let query = Query::parse(&req.query);
        if query.is_empty() && !req.query.trim().is_empty() && query.excluded.is_empty() {
            out.notes.push("the query has no searchable words; listing newest instead".into());
        }
        let plan = Plan {
            group: out.group.as_ref().map(|g| (g.key.clone(), group_token(&g.key))),
            scope,
            filters: self.filter_clauses(&req.filters),
            since: out.since.clone(),
            until: out.until.clone(),
        };
        let highlight = query.highlight();

        if query.is_empty() {
            (out.total_matches, out.results) = self.browse(&plan, limit, req.offset)?;
            if !query.excluded.is_empty() {
                out.notes.push("exclusions need at least one positive word; they were ignored".into());
            }
            return Ok(out);
        }

        let positive = query.fts().expect("non-empty query");
        let negative = query.not_fts();
        let expr = plan.expr(&positive, negative.as_deref(), scope);
        let sort = req.sort;
        (out.total_matches, out.results) = self.ranked(&expr, &plan, sort, limit, req.offset, &highlight)?;

        if scope == Scope::Group
            && out.results.is_empty()
            && req.offset == 0
            && req.fallback
            && plan.group.is_some()
        {
            let others = plan.expr(&positive, negative.as_deref(), Scope::Others);
            out.other_groups = self.ranked(&others, &plan, sort, limit, 0, &highlight)?.1;
            for c in &mut out.other_groups {
                c.other_group = true;
            }
            if !out.other_groups.is_empty() {
                out.notes.push(format!(
                    "nothing matched in this {group_noun}; other_groups results come from OTHER {} - say so and keep each card's {group_noun} visible",
                    plural(&group_noun)
                ));
            }
        }
        Ok(out)
    }

    /// One FTS clause per filter field; several values for a field are ORed.
    fn filter_clauses(&self, filters: &[(String, String)]) -> Vec<String> {
        let mut by_field: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (field, value) in filters {
            by_field
                .entry(field.as_str())
                .or_default()
                .push(format!("tags : \"{}\"", facet_token(field, value)));
        }
        by_field.into_values().map(|alts| format!("({})", alts.join(" OR "))).collect()
    }

    /// Total matches and the cards for one page.
    fn ranked(
        &self,
        expr: &str,
        plan: &Plan,
        sort: Sort,
        limit: usize,
        offset: usize,
        highlight: &[String],
    ) -> Result<(i64, Vec<Card>)> {
        struct Hit {
            score: f64,
            date: String,
            rowid: i64,
        }
        let want = offset + limit;
        let overfetch = if sort == Sort::Newest { want } else { (want * 10).max(100) } as i64;
        let (dates, mut args) = plan.date_sql(2);
        args.insert(0, SqlValue::Text(expr.to_string()));
        args.push(SqlValue::Integer(overfetch));
        let limit_param = args.len();
        let sql = if dates.is_empty() && sort == Sort::Relevance {
            format!(
                "SELECT r.rowid, r.boost, r.date, f.rank, f.total FROM \
                   (SELECT rowid, rank, count(*) OVER () AS total FROM record_fts WHERE record_fts MATCH ?1 \
                    ORDER BY rank LIMIT ?{limit_param}) AS f \
                 JOIN records AS r ON r.rowid = f.rowid"
            )
        } else {
            let order = if sort == Sort::Newest { "r.date DESC, r.rowid DESC" } else { "record_fts.rank" };
            format!(
                "SELECT r.rowid, r.boost, r.date, record_fts.rank, count(*) OVER () FROM record_fts \
                 JOIN records AS r ON r.rowid = record_fts.rowid \
                 WHERE record_fts MATCH ?1 {dates} ORDER BY {order} LIMIT ?{limit_param}"
            )
        };
        let mut total = 0i64;
        let mut hits: Vec<Hit> = self
            .conn
            .prepare_cached(&sql)?
            .query_map(params_from_iter(args), |r| {
                let boost: f64 = r.get(1)?;
                let bm25: f64 = r.get(3)?;
                total = r.get(4)?;
                Ok(Hit {
                    score: bm25 * boost,
                    date: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    rowid: r.get(0)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if sort == Sort::Relevance {
            hits.sort_by(|a, b| a.score.total_cmp(&b.score).then_with(|| b.date.cmp(&a.date)));
        }
        let cards = hits
            .into_iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, hit)| {
                let mut card = self.card(hit.rowid, highlight)?;
                card.rank = Some(offset + i + 1);
                card.relevance = Some((-hit.score * 100.0).round() / 100.0);
                Ok(card)
            })
            .collect::<Result<_>>()?;
        Ok((total, cards))
    }

    /// No query words: records in scope, newest first. The page and the
    /// count are separate queries so the page can walk the date index and
    /// stop early.
    fn browse(&self, plan: &Plan, limit: usize, offset: usize) -> Result<(i64, Vec<Card>)> {
        let (dates, mut args) = plan.date_sql(1);
        let mut clauses = vec!["1".to_string()];
        if !dates.is_empty() {
            clauses.push(dates.trim_start_matches("AND ").to_string());
        }
        if let Some((key, _)) = &plan.group
            && plan.scope != Scope::All
        {
            args.push(SqlValue::Text(key.clone()));
            let n = args.len();
            clauses.push(if plan.scope == Scope::Group {
                format!("r.grp = ?{n}")
            } else {
                format!("(r.grp IS NULL OR r.grp <> ?{n})")
            });
        }
        if !plan.filters.is_empty() {
            args.push(SqlValue::Text(plan.filters.join(" AND ")));
            clauses.push(format!(
                "r.rowid IN (SELECT rowid FROM record_fts WHERE record_fts MATCH ?{})",
                args.len()
            ));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM records AS r WHERE {where_sql}"),
            params_from_iter(args.clone()),
            |r| r.get(0),
        )?;
        let n = args.len();
        args.push(SqlValue::Integer(limit as i64));
        args.push(SqlValue::Integer(offset as i64));
        let rows: Vec<i64> = self
            .conn
            .prepare_cached(&format!(
                "SELECT r.rowid FROM records AS r WHERE {where_sql} \
                 ORDER BY r.date DESC, r.rowid DESC LIMIT ?{} OFFSET ?{}",
                n + 1,
                n + 2
            ))?
            .query_map(params_from_iter(args), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let cards = rows.iter().map(|rowid| self.card(*rowid, &[])).collect::<Result<_>>()?;
        Ok((total, cards))
    }

    fn card(&self, rowid: i64, highlight: &[String]) -> Result<Card> {
        let (id, doc): (String, String) = self
            .conn
            .prepare_cached("SELECT id, doc FROM records WHERE rowid = ?1")?
            .query_row([rowid], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Card::build(&id, &doc, &self.mapping, self.card_opts, highlight)
    }

    /// The result card for one record id.
    pub fn card_by_id(&self, id: &str) -> Result<Option<Card>> {
        let found: Option<(String, String)> = self
            .conn
            .prepare_cached("SELECT id, doc FROM records WHERE id = ?1")?
            .query_row([id.trim()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        found.map(|(id, doc)| Card::build(&id, &doc, &self.mapping, self.card_opts, &[])).transpose()
    }

    /// Full stored record, exactly as ingested.
    pub fn get(&self, id: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT doc FROM records WHERE id = ?1")?
            .query_row([id.trim()], |r| r.get(0))
            .optional()?;
        raw.map(|r| serde_json::from_str(&r).map_err(Into::into)).transpose()
    }

    // --------------------------------------------------------------- describe

    pub fn describe(&self, top: usize) -> Result<Description> {
        let cfg = self.config();
        let mut filters = Vec::new();
        for p in &self.mapping.filters {
            let distinct: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM facets WHERE field = ?1", [&p.raw], |r| r.get(0))?;
            let top_values = self
                .conn
                .prepare_cached(
                    "SELECT value, count FROM facets WHERE field = ?1 ORDER BY count DESC, value LIMIT ?2",
                )?
                .query_map(rusqlite::params![p.raw, top as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            filters.push(FacetSummary { field: p.raw.clone(), distinct, top: top_values });
        }
        let largest_groups = self
            .conn
            .prepare_cached(
                "SELECT key, name, record_count FROM groups ORDER BY record_count DESC, key LIMIT 5",
            )?
            .query_map([], |r| group_row(r, "listing"))?
            .collect::<rusqlite::Result<_>>()?;
        let nonempty = |v: Option<String>| v.filter(|s| !s.is_empty());
        Ok(Description {
            index_path: self.path.clone(),
            index_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            about: cfg.about.clone(),
            fields: cfg.fields.clone(),
            rank: cfg.rank.clone(),
            record_count: self.meta_i64("record_count")?,
            group_count: self.meta_i64("group_count")?,
            date_min: nonempty(self.meta("date_min")?),
            date_max: nonempty(self.meta("date_max")?),
            filters,
            largest_groups,
            example_id: self
                .conn
                .query_row("SELECT id FROM records ORDER BY date DESC, rowid DESC LIMIT 1", [], |r| r.get(0))
                .optional()?,
            mapping_inferred: self.meta("mapping_inferred")?.as_deref() == Some("true"),
            built_at: self.meta("built_at")?,
            updated_at: self.meta("updated_at")?,
            skipped_lines: self.meta_i64("skipped_lines")?,
            schema_version: self.meta("schema_version")?.unwrap_or_default(),
        })
    }
}

/// Scope, filters and date bounds shared by ranked search and browsing.
#[derive(Debug, Clone)]
struct Plan {
    /// Resolved group key and its token.
    group: Option<(String, String)>,
    scope: Scope,
    filters: Vec<String>,
    since: Option<String>,
    until: Option<String>,
}

impl Plan {
    fn expr(&self, positive: &str, negative: Option<&str>, scope: Scope) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let (Some((_, token)), Scope::Group) = (&self.group, scope) {
            parts.push(format!("tags : \"{token}\""));
        }
        parts.extend(self.filters.iter().cloned());
        parts.push(if scope == Scope::Group && self.group.is_some() {
            format!("{{title body}} : ({positive})")
        } else {
            format!("({positive})")
        });
        let mut expr = parts.join(" AND ");
        if let (Some((_, token)), Scope::Others) = (&self.group, scope) {
            expr = format!("({expr}) NOT tags : \"{token}\"");
        }
        if let Some(neg) = negative {
            expr = format!("({expr}) NOT ({neg})");
        }
        expr
    }

    /// `AND ...` conditions on `r.date`, parameters numbered from `first`.
    /// `until` is inclusive at its own precision (`--until 2024-03` keeps
    /// all of March).
    fn date_sql(&self, first: usize) -> (String, Vec<SqlValue>) {
        let mut sql = String::new();
        let mut args = Vec::new();
        if let Some(since) = &self.since {
            sql.push_str(&format!(" AND r.date >= ?{}", first + args.len()));
            args.push(SqlValue::Text(since.clone()));
        }
        if let Some(until) = &self.until {
            let n = first + args.len();
            sql.push_str(&format!(" AND substr(r.date, 1, length(?{n})) <= ?{n}"));
            args.push(SqlValue::Text(until.clone()));
        }
        (sql.trim_start().to_string(), args)
    }
}

fn group_row(r: &Row<'_>, match_type: &'static str) -> rusqlite::Result<GroupMatch> {
    Ok(GroupMatch { key: r.get(0)?, name: r.get(1)?, records: r.get(2)?, match_type, similarity: None })
}
