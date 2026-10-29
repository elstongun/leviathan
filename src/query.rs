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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};
use serde::Serialize;

use crate::card::{Card, CardOptions};
use crate::config::{About, Config, Fields, Rank, plural};
use crate::fields::{FieldPath, KIND_FILTER, Mappings};
use crate::index::{check_schema, meta, open_ro, resolve_id, stored_config};
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
    /// Each kind of record, in an index that holds several.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<KindSummary>,
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

#[derive(Debug, Serialize)]
pub struct KindSummary {
    pub name: String,
    pub record: String,
    pub group: String,
    pub records: i64,
    pub fields: Fields,
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
    mappings: Mappings,
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
        let mappings = Mappings::new(&stored_config(&conn)?)?;
        Ok(Self { conn, path: path.to_path_buf(), stamp: file_stamp(path), mappings, card_opts })
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
        &self.mappings.config
    }

    pub fn mappings(&self) -> &Mappings {
        &self.mappings
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
        if query.is_empty() || !self.mappings.has_group() {
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

        let filter_fields = self.mappings.filter_fields();
        let filterable: Vec<&str> = filter_fields.iter().map(String::as_str).collect();
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
        if (out.since.is_some() || out.until.is_some()) && !self.mappings.has_date() {
            return bad(out, "this index has no date field, so --since/--until cannot apply".into());
        }

        if let Some(group) = &req.group {
            if !self.mappings.has_group() {
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
        Card::build(&id, &doc, self.mappings.for_id(&id), self.card_opts, highlight)
    }

    /// The stored id for `id`, accepting a bare id in an index with several kinds.
    fn stored_id(&self, id: &str) -> Result<Option<String>> {
        resolve_id(&self.conn, &self.mappings.kind_names(), id.trim())
    }

    /// The result card for one record id.
    pub fn card_by_id(&self, id: &str) -> Result<Option<Card>> {
        let Some(id) = self.stored_id(id)? else { return Ok(None) };
        let found: Option<(String, String)> = self
            .conn
            .prepare_cached("SELECT id, doc FROM records WHERE id = ?1")?
            .query_row([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        found
            .map(|(id, doc)| Card::build(&id, &doc, self.mappings.for_id(&id), self.card_opts, &[]))
            .transpose()
    }

    /// Full stored record, exactly as ingested.
    pub fn get(&self, id: &str) -> Result<Option<serde_json::Value>> {
        let Some(id) = self.stored_id(id)? else { return Ok(None) };
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT doc FROM records WHERE id = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        raw.map(|r| serde_json::from_str(&r).map_err(Into::into)).transpose()
    }

    // --------------------------------------------------------------- describe

    pub fn describe(&self, top: usize) -> Result<Description> {
        let cfg = self.config();
        let mut filters = Vec::new();
        for field in self.mappings.filter_fields() {
            let distinct: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM facets WHERE field = ?1", [&field], |r| r.get(0))?;
            let top_values = self
                .conn
                .prepare_cached(
                    "SELECT value, count FROM facets WHERE field = ?1 ORDER BY count DESC, value LIMIT ?2",
                )?
                .query_map(rusqlite::params![field, top as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            filters.push(FacetSummary { field, distinct, top: top_values });
        }
        let counts: std::collections::HashMap<String, i64> = self
            .conn
            .prepare_cached("SELECT key, count FROM facets WHERE field = ?1")?
            .query_map([KIND_FILTER], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let kinds = cfg
            .kinds
            .iter()
            .map(|k| {
                let about = cfg.for_kind(k).about;
                KindSummary {
                    name: k.name.clone(),
                    record: about.record,
                    group: about.group,
                    records: counts.get(&k.name.to_lowercase()).copied().unwrap_or(0),
                    fields: k.fields.clone(),
                }
            })
            .collect();
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
            kinds,
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

// -------------------------------------------------------------- aggregate

/// What to compute per bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Records per bucket.
    #[default]
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// Different values of `field` per bucket.
    Distinct,
}

impl Metric {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "count" => Self::Count,
            "sum" => Self::Sum,
            "avg" | "mean" | "average" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "distinct" | "count_distinct" | "unique" => Self::Distinct,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct AggregateRequest {
    /// Words the records must match (optional).
    pub query: String,
    /// One group, resolved like `search`.
    pub group: Option<String>,
    /// Every group whose key or name contains this text.
    pub group_contains: Option<String>,
    pub filters: Vec<(String, String)>,
    /// Conditions on number fields: `hours>4`, `total<=100`, `qty=0`.
    pub numeric: Vec<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    /// Up to two of `group`, `year`, `month`, `day`, a filter field or any
    /// field path, comma-separated (`group,month`); none for totals.
    pub by: Option<String>,
    pub metric: Metric,
    /// The field for sum/avg/min/max (numbers) or distinct (any values).
    pub field: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AggregateRow {
    /// The bucket; with two `by` dimensions, both keys joined by ` · `.
    pub key: String,
    /// Each dimension's key, with two `by` dimensions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// The group's name, when a dimension is the group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub records: i64,
    pub value: f64,
}

#[derive(Debug, Serialize)]
pub struct AggregateOutcome {
    pub status: Status,
    pub metric: Metric,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupMatch>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<GroupMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_contains: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub numeric: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Records that matched.
    pub records: i64,
    /// The metric over every matched record (the count, for `count`).
    pub value: f64,
    /// Distinct buckets, before `limit`.
    pub buckets: i64,
    /// Largest value first.
    pub rows: Vec<AggregateRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip)]
    pub about: About,
}

/// Running numbers for one bucket.
#[derive(Default)]
struct Acc {
    records: i64,
    n: i64,
    sum: f64,
    min: f64,
    max: f64,
    distinct: HashSet<String>,
}

impl Acc {
    fn add(&mut self, v: f64) {
        if self.n == 0 || v < self.min {
            self.min = v;
        }
        if self.n == 0 || v > self.max {
            self.max = v;
        }
        self.n += 1;
        self.sum += v;
    }

    fn value(&self, metric: Metric) -> f64 {
        let round = |v: f64| (v * 1e6).round() / 1e6;
        match metric {
            Metric::Count => self.records as f64,
            Metric::Distinct => self.distinct.len() as f64,
            Metric::Sum => round(self.sum),
            Metric::Avg if self.n > 0 => round(self.sum / self.n as f64),
            Metric::Min | Metric::Max if self.n == 0 => 0.0,
            Metric::Min => self.min,
            Metric::Max => self.max,
            Metric::Avg => 0.0,
        }
    }
}

fn number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().replace(',', "").parse().ok(),
        _ => None,
    }
}

/// A scalar as bucket text; objects, arrays and blanks give none.
fn scalar_text(v: &serde_json::Value) -> Option<String> {
    let text = match v {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then(|| text.chars().take(200).collect())
}

/// `hours>4`: true when any number at the path compares so.
struct Condition {
    path: FieldPath,
    op: &'static str,
    value: f64,
}

impl Condition {
    fn parse(raw: &str) -> Option<Condition> {
        for op in [">=", "<=", "!=", ">", "<", "="] {
            if let Some((field, value)) = raw.split_once(op) {
                let field = field.trim();
                let value: f64 = value.trim().replace(',', "").parse().ok()?;
                return (!field.is_empty() && value.is_finite()).then(|| Condition {
                    path: FieldPath::parse(field),
                    op,
                    value,
                });
            }
        }
        None
    }

    fn holds(&self, doc: &serde_json::Value) -> bool {
        self.path.values(doc).into_iter().filter_map(number).any(|v| match self.op {
            ">=" => v >= self.value,
            "<=" => v <= self.value,
            "!=" => v != self.value,
            ">" => v > self.value,
            "<" => v < self.value,
            _ => v == self.value,
        })
    }
}

/// Limits on one aggregate, so a caller of `serve` cannot hold a worker or
/// memory without end: seconds (`LEVIATHAN_AGGREGATE_SECONDS`, default 60)
/// and buckets or distinct values kept (`LEVIATHAN_AGGREGATE_BUCKETS`,
/// default 1,000,000).
fn aggregate_limits() -> (std::time::Duration, usize) {
    let env =
        |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    (
        std::time::Duration::from_secs(env("LEVIATHAN_AGGREGATE_SECONDS", 60)),
        env("LEVIATHAN_AGGREGATE_BUCKETS", 1_000_000) as usize,
    )
}

/// One way of splitting records into buckets.
enum Dim {
    Group,
    Date(usize),
    Facet(String),
    Field(FieldPath),
}

impl Store {
    /// Count records, count the different values of a field, or sum, average,
    /// min or max a numeric field, over the records a search would select
    /// (and that meet numeric conditions), in total or split by up to two of
    /// group, date unit, filter field or any field. Exact: every matching
    /// record is counted, none ranked.
    pub fn aggregate(&self, req: &AggregateRequest) -> Result<AggregateOutcome> {
        let about = self.config().about.clone();
        let limit = req.limit.clamp(1, 200);
        let mut out = AggregateOutcome {
            status: Status::Ok,
            metric: req.metric,
            field: req.field.clone(),
            by: req.by.clone(),
            query: req.query.clone(),
            group: None,
            candidates: Vec::new(),
            group_contains: req.group_contains.clone().filter(|s| !s.trim().is_empty()),
            filters: req.filters.iter().map(|(f, v)| Filter { field: f.clone(), value: v.clone() }).collect(),
            numeric: req.numeric.clone(),
            since: None,
            until: None,
            records: 0,
            value: 0.0,
            buckets: 0,
            rows: Vec::new(),
            notes: Vec::new(),
            about,
        };
        let bad = |mut out: AggregateOutcome, note: String| {
            out.status = Status::BadRequest;
            out.notes.push(note);
            Ok(out)
        };

        let filter_fields = self.mappings.filter_fields();
        let filterable: Vec<&str> = filter_fields.iter().map(String::as_str).collect();
        for (field, _) in &req.filters {
            if !filterable.contains(&field.as_str()) {
                return bad(
                    out,
                    format!("`{field}` is not a filter field; filterable: {}", filterable.join(", ")),
                );
            }
        }
        let mut conditions = Vec::new();
        for raw in req.numeric.iter().filter(|r| !r.trim().is_empty()) {
            match Condition::parse(raw) {
                Some(c) => conditions.push(c),
                None => {
                    return bad(
                        out,
                        format!(
                            "numeric condition {raw:?} must be <field><op><number>, op one of > >= < <= = !="
                        ),
                    );
                }
            }
        }
        for raw in [&req.since, &req.until].into_iter().flatten() {
            if normalize_bound(raw).is_none() {
                return bad(out, format!("unrecognized date {raw:?}; use YYYY, YYYY-MM or YYYY-MM-DD"));
            }
        }
        out.since = req.since.as_deref().and_then(normalize_bound);
        out.until = req.until.as_deref().and_then(normalize_bound);
        if (out.since.is_some() || out.until.is_some()) && !self.mappings.has_date() {
            return bad(out, "this index has no date field, so since/until cannot apply".into());
        }
        let field = match (req.metric, req.field.as_deref().map(str::trim).filter(|f| !f.is_empty())) {
            (Metric::Count, _) => None,
            (_, Some(f)) => Some(FieldPath::parse(f)),
            (_, None) => {
                return bad(
                    out,
                    "sum, avg, min, max and distinct need `field`, the field to aggregate".into(),
                );
            }
        };
        let mut dims = Vec::new();
        for raw in req.by.iter().flat_map(|b| b.split(',')).map(str::trim).filter(|b| !b.is_empty()) {
            dims.push(match raw {
                "group" if !self.mappings.has_group() => {
                    return bad(out, "this index has no group field, so it cannot aggregate by group".into());
                }
                "year" | "month" | "day" if !self.mappings.has_date() => {
                    return bad(out, "this index has no date field, so it cannot aggregate by date".into());
                }
                "group" => Dim::Group,
                "year" => Dim::Date(4),
                "month" => Dim::Date(7),
                "day" => Dim::Date(10),
                f if filterable.contains(&f) => Dim::Facet(f.to_string()),
                f => Dim::Field(FieldPath::parse(f)),
            });
        }
        if dims.len() > 2 {
            return bad(out, "`by` takes at most two dimensions, e.g. `group,month`".into());
        }

        if let Some(group) = &req.group {
            if !self.mappings.has_group() {
                return bad(out, "this index has no group field; aggregate without one".into());
            }
            match self.pick(group)? {
                Pick::One(m) => out.group = Some(m),
                Pick::Many(c) => {
                    out.status = Status::AmbiguousGroup;
                    out.notes.push(format!("{} groups match {group:?}; retry with one exact key", c.len()));
                    out.candidates = c;
                    return Ok(out);
                }
                Pick::None => {
                    out.status = Status::UnknownGroup;
                    out.notes
                        .push(format!("no group matches {group:?}; try resolve_group, or group_contains"));
                    return Ok(out);
                }
            }
        }

        // The records in scope: the same group, filters, words and dates as search.
        let plan = Plan {
            group: None,
            scope: Scope::All,
            filters: Vec::new(),
            since: out.since.clone(),
            until: out.until.clone(),
        };
        let (dates, mut args) = plan.date_sql(1);
        let mut clauses = vec!["1".to_string()];
        if !dates.is_empty() {
            clauses.push(dates.trim_start_matches("AND ").to_string());
        }
        if let Some(g) = &out.group {
            args.push(SqlValue::Text(g.key.clone()));
            clauses.push(format!("r.grp = ?{}", args.len()));
        }
        if let Some(text) = &out.group_contains {
            // Whole words of the name ("Hall A" is not "Hall AA"), or the
            // start of the key ("AHU" for every AHU-*).
            let escaped = normalize_name(text).replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            args.push(SqlValue::Text(format!("% {escaped} %")));
            args.push(SqlValue::Text(format!("{escaped}%")));
            let (words, prefix) = (args.len() - 1, args.len());
            clauses.push(format!(
                "r.grp IN (SELECT key FROM groups WHERE (' ' || name_normalized || ' ') LIKE ?{words} ESCAPE '\\' \
                 OR key_lower LIKE ?{prefix} ESCAPE '\\')"
            ));
        }
        let query = Query::parse(&req.query);
        let mut fts: Vec<String> = self.filter_clauses(&req.filters);
        if let Some(positive) = query.fts() {
            fts.push(format!("({positive})"));
        } else if !req.query.trim().is_empty() {
            out.notes.push("the query has no searchable words; it was ignored".into());
        }
        if !fts.is_empty() {
            let mut expr = fts.join(" AND ");
            if let (Some(_), Some(neg)) = (query.fts(), query.not_fts()) {
                expr = format!("({expr}) NOT ({neg})");
            }
            args.push(SqlValue::Text(expr));
            clauses.push(format!(
                "r.rowid IN (SELECT rowid FROM record_fts WHERE record_fts MATCH ?{})",
                args.len()
            ));
        }
        let where_sql = clauses.join(" AND ");

        let need_doc =
            field.is_some() || !conditions.is_empty() || dims.iter().any(|d| matches!(d, Dim::Field(_)));
        let need_facets = dims.iter().any(|d| matches!(d, Dim::Facet(_)));
        let sql = format!(
            "SELECT r.grp, r.date, {}, {} FROM records AS r WHERE {where_sql}",
            if need_facets { "r.facets" } else { "NULL" },
            if need_doc { "r.doc" } else { "NULL" }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args.iter()))?;
        let mut buckets: BTreeMap<Vec<String>, Acc> = BTreeMap::new();
        let mut overall = Acc::default();
        let mut seen_field_values = vec![false; dims.len()];
        let (max_time, max_buckets) = aggregate_limits();
        let started = std::time::Instant::now();
        let mut scanned = 0u64;
        while let Some(row) = rows.next()? {
            scanned += 1;
            if scanned.is_multiple_of(4096) && started.elapsed() > max_time {
                return bad(
                    out,
                    format!(
                        "stopped after {}s and {scanned} records; narrow it with a group, filters, words or dates",
                        max_time.as_secs()
                    ),
                );
            }
            if buckets.len() > max_buckets || overall.distinct.len() > max_buckets {
                return bad(
                    out,
                    format!(
                        "more than {max_buckets} buckets or distinct values; split by a field with fewer values, or narrow it"
                    ),
                );
            }
            let grp: Option<String> = row.get(0)?;
            let date: Option<String> = row.get(1)?;
            let facets: Option<String> = row.get(2)?;
            let doc: Option<serde_json::Value> =
                row.get::<_, Option<String>>(3)?.map(|d| serde_json::from_str(&d)).transpose()?;
            if !conditions.iter().all(|c| doc.as_ref().is_some_and(|d| c.holds(d))) {
                continue;
            }
            overall.records += 1;
            let facet_pairs: Vec<(String, String)> =
                facets.as_deref().and_then(|f| serde_json::from_str(f).ok()).unwrap_or_default();
            // Each dimension's keys for this record (several for a list field).
            let mut parts: Vec<Vec<String>> = Vec::with_capacity(dims.len());
            for (i, dim) in dims.iter().enumerate() {
                let mut keys: Vec<String> = match dim {
                    Dim::Group => vec![grp.clone().unwrap_or_default()],
                    Dim::Date(n) => {
                        vec![date.as_deref().map(|d| d.chars().take(*n).collect()).unwrap_or_default()]
                    }
                    Dim::Facet(f) => {
                        facet_pairs.iter().filter(|(k, _)| k == f).map(|(_, v)| v.clone()).collect()
                    }
                    Dim::Field(p) => doc
                        .as_ref()
                        .map(|d| p.values(d).into_iter().filter_map(scalar_text).collect())
                        .unwrap_or_default(),
                };
                keys.sort_unstable();
                keys.dedup();
                if keys.is_empty() {
                    keys.push(String::new());
                } else if keys.iter().any(|k| !k.is_empty()) {
                    seen_field_values[i] = true;
                }
                parts.push(keys);
            }
            let combos: Vec<Vec<String>> = match parts.as_slice() {
                [] => vec![Vec::new()],
                [a] => a.iter().map(|k| vec![k.clone()]).collect(),
                [a, b] => a.iter().flat_map(|x| b.iter().map(move |y| vec![x.clone(), y.clone()])).collect(),
                _ => unreachable!("at most two dimensions"),
            };
            let numbers: Vec<f64> = match (&field, req.metric) {
                (Some(p), Metric::Sum | Metric::Avg | Metric::Min | Metric::Max) => doc
                    .as_ref()
                    .map(|d| p.values(d).into_iter().filter_map(number).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let texts: Vec<String> = match (&field, req.metric) {
                (Some(p), Metric::Distinct) => doc
                    .as_ref()
                    .map(|d| p.values(d).into_iter().filter_map(scalar_text).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            numbers.iter().for_each(|v| overall.add(*v));
            overall.distinct.extend(texts.iter().cloned());
            for combo in combos {
                let acc = buckets.entry(combo).or_default();
                acc.records += 1;
                numbers.iter().for_each(|v| acc.add(*v));
                acc.distinct.extend(texts.iter().cloned());
            }
        }
        out.records = overall.records;
        out.value = overall.value(req.metric);
        if !dims.is_empty() {
            out.buckets = buckets.len() as i64;
            let mut rows: Vec<AggregateRow> = buckets
                .into_iter()
                .map(|(parts, acc)| AggregateRow {
                    value: acc.value(req.metric),
                    records: acc.records,
                    name: None,
                    key: parts.join(" · "),
                    keys: if parts.len() > 1 { parts } else { Vec::new() },
                })
                .collect();
            rows.sort_by(|a, b| b.value.total_cmp(&a.value).then_with(|| a.key.cmp(&b.key)));
            rows.truncate(limit);
            if let Some(g) = dims.iter().position(|d| matches!(d, Dim::Group)) {
                let mut name = self.conn.prepare_cached("SELECT name FROM groups WHERE key = ?1")?;
                for row in rows.iter_mut() {
                    let key = if row.keys.is_empty() { row.key.clone() } else { row.keys[g].clone() };
                    if !key.is_empty() {
                        row.name = name.query_row([&key], |r| r.get(0)).optional()?.flatten();
                    }
                }
            }
            out.rows = rows;
        }
        for (dim, seen) in dims.iter().zip(&seen_field_values) {
            if let Dim::Field(p) = dim
                && !seen
                && out.records > 0
            {
                out.notes.push(format!(
                    "no values at `{}` in the matched records; check the field name in describe",
                    p.raw
                ));
            }
        }
        if let Some(path) = &field
            && out.records > 0
            && overall.n == 0
            && overall.distinct.is_empty()
        {
            let kind = if req.metric == Metric::Distinct { "values" } else { "numeric values" };
            out.notes.push(format!("no {kind} at `{}` in the matched records", path.raw));
        }
        Ok(out)
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
