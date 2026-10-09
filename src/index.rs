//! Build and update the on-disk index.
//!
//! A full build writes `<index>.building` and renames it over the live index
//! only on success, so a running server never sees a partial database.
//! Builds are skipped when the sources (paths, sizes, mtimes) and the mapping
//! are unchanged.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::config::{Config, Fields};
use crate::fields::Mapping;
use crate::source::{self, Item, Source};
use crate::text::normalize_name;

pub const SCHEMA_VERSION: &str = "2";

fn schema_sql(title_weight: f64) -> String {
    format!(
        r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE groups (
    key             TEXT PRIMARY KEY,
    key_lower       TEXT,
    name            TEXT,
    name_normalized TEXT,
    record_count    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_groups_key_lower ON groups (key_lower);
CREATE INDEX idx_groups_name_norm ON groups (name_normalized);

CREATE TABLE records (
    rowid    INTEGER PRIMARY KEY,
    id       TEXT NOT NULL UNIQUE,
    grp      TEXT,
    grp_name TEXT,
    date     TEXT,
    boost    REAL NOT NULL DEFAULT 1.0,
    facets   TEXT,
    doc      TEXT NOT NULL
);
CREATE INDEX idx_records_grp_date ON records (grp, date);
CREATE INDEX idx_records_date ON records (date);

CREATE TABLE facets (
    field TEXT NOT NULL,
    key   TEXT NOT NULL,
    value TEXT NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (field, key)
) WITHOUT ROWID;

-- Contentless: text is tokenized, never stored twice. `names` holds the
-- group key and name, searched only outside a group scope (inside one they
-- match every record). `tags` holds one synthetic token per group and per
-- filter value, so scoping and filtering are posting-list intersections,
-- not post-filters over every match.
CREATE VIRTUAL TABLE record_fts USING fts5(
    title, body, names, tags,
    content = '', contentless_delete = 1,
    tokenize = 'porter unicode61'
);
INSERT INTO record_fts (record_fts, rank) VALUES ('rank', 'bm25({title_weight}, 1.0, 0.5, 0.0)');
"#
    )
}

#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    pub built: bool,
    pub index_path: PathBuf,
    pub records_read: u64,
    pub inserted: u64,
    /// Existing ids replaced (in a full build: duplicate ids in the sources).
    pub updated: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub deleted: u64,
    pub skipped_lines: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_examples: Vec<String>,
    pub record_count: i64,
    pub group_count: i64,
    pub source_bytes: u64,
    pub index_bytes: u64,
    pub elapsed_seconds: f64,
    /// Set when no config or flags were given and the mapping was inferred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_mapping: Option<Fields>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions {
    pub force: bool,
    /// Fail on the first unusable record instead of counting and skipping it.
    pub strict: bool,
    pub quiet: bool,
}

/// Full rebuild into a fresh database, atomically swapped into place.
pub fn build(
    index_path: &Path,
    sources: &[Source],
    config: &Config,
    inferred: bool,
    opts: BuildOptions,
) -> Result<IngestReport> {
    let mapping = Mapping::new(config)?;
    let config = &Config { memory: Default::default(), ..config.clone() };
    let config_json = serde_json::to_string(config)?;
    let manifest = source::manifest(sources, &config_json)?;
    if !opts.force && !manifest.is_empty() && index_is_current(index_path, &manifest) {
        let conn = open_ro(index_path)?;
        return Ok(IngestReport {
            index_path: index_path.to_path_buf(),
            index_bytes: fs::metadata(index_path)?.len(),
            record_count: count(&conn, "records")?,
            group_count: count(&conn, "groups")?,
            ..Default::default()
        });
    }

    let started = Instant::now();
    if let Some(parent) = index_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let building = sibling(index_path, ".building");
    let _ = fs::remove_file(&building);

    let result = (|| -> Result<IngestReport> {
        let mut conn = Connection::open(&building)?;
        conn.execute_batch(
            "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; \
             PRAGMA temp_store = MEMORY; PRAGMA cache_size = -262144; PRAGMA page_size = 8192;",
        )?;
        conn.execute_batch(&schema_sql(config.rank.title_weight))?;
        let tx = conn.transaction()?;
        let mut ingest = Ingest::new(&tx, &mapping, opts, false)?;
        for s in sources {
            ingest.source(s, config.source.sql.as_deref())?;
        }
        let mut report = ingest.finish()?;
        rebuild_groups(&tx, false)?;
        log(opts, "optimizing full-text index");
        tx.execute("INSERT INTO record_fts (record_fts) VALUES ('optimize')", [])?;
        report.built = true;
        report.source_bytes = source::byte_size(sources);
        report.record_count = count(&tx, "records")?;
        report.group_count = count(&tx, "groups")?;
        for (key, value) in [
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("leviathan_version", env!("CARGO_PKG_VERSION").to_string()),
            ("built_at", crate::now_rfc3339()),
            ("config", config_json.clone()),
            ("mapping_inferred", inferred.to_string()),
            ("skipped_lines", report.skipped_lines.to_string()),
            ("source_bytes", report.source_bytes.to_string()),
            ("source_manifest", manifest.clone()),
        ] {
            set_meta(&tx, key, &value)?;
        }
        refresh_counts(&tx)?;
        tx.commit()?;
        conn.close().map_err(|(_, e)| e)?;
        Ok(report)
    })();

    let mut report = match result {
        Ok(report) => report,
        Err(err) => {
            let _ = fs::remove_file(&building);
            return Err(err);
        }
    };
    fs::rename(&building, index_path)
        .with_context(|| format!("install index at {}", index_path.display()))?;
    report.index_path = index_path.to_path_buf();
    report.index_bytes = fs::metadata(index_path)?.len();
    report.elapsed_seconds = round1(started.elapsed().as_secs_f64());
    if inferred {
        report.inferred_mapping = Some(config.fields.clone());
    }
    Ok(report)
}

/// Insert or replace records (by id) in an existing index, using the mapping
/// it was built with.
pub fn upsert(index_path: &Path, sources: &[Source], opts: BuildOptions) -> Result<IngestReport> {
    let started = Instant::now();
    let mut conn = open_rw(index_path)?;
    let config = stored_config(&conn)?;
    let mapping = Mapping::new(&config)?;
    if !mapping.has_id() {
        bail!(
            "this index has no `id` field in its mapping, so records cannot be matched for replacement; rebuild instead"
        );
    }
    let tx = conn.transaction()?;
    let mut ingest = Ingest::new(&tx, &mapping, opts, true)?;
    for s in sources {
        ingest.source(s, config.source.sql.as_deref())?;
    }
    let mut report = ingest.finish()?;
    finish_incremental(&tx, &mut report)?;
    tx.commit()?;
    report.built = true;
    report.index_path = index_path.to_path_buf();
    report.source_bytes = source::byte_size(sources);
    report.index_bytes = fs::metadata(index_path)?.len();
    report.elapsed_seconds = round1(started.elapsed().as_secs_f64());
    Ok(report)
}

/// Remove records by id.
pub fn delete(index_path: &Path, ids: &[String]) -> Result<IngestReport> {
    let started = Instant::now();
    let mut conn = open_rw(index_path)?;
    let tx = conn.transaction()?;
    tx.execute_batch("CREATE TEMP TABLE touched_groups (key TEXT PRIMARY KEY)")?;
    let mut deltas = FacetDeltas::default();
    let mut report = IngestReport::default();
    for id in ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Some((rowid, grp, facets)) = existing(&tx, id)? {
            tx.prepare_cached("DELETE FROM records WHERE rowid = ?1")?.execute([rowid])?;
            tx.prepare_cached("DELETE FROM record_fts WHERE rowid = ?1")?.execute([rowid])?;
            deltas.remove_json(facets.as_deref());
            if let Some(g) = grp {
                touch(&tx, &g)?;
            }
            report.deleted += 1;
        }
    }
    deltas.apply(&tx)?;
    finish_incremental(&tx, &mut report)?;
    tx.commit()?;
    report.built = true;
    report.index_path = index_path.to_path_buf();
    report.index_bytes = fs::metadata(index_path)?.len();
    report.elapsed_seconds = round1(started.elapsed().as_secs_f64());
    Ok(report)
}

fn finish_incremental(tx: &Connection, report: &mut IngestReport) -> Result<()> {
    rebuild_groups(tx, true)?;
    report.record_count = count(tx, "records")?;
    report.group_count = count(tx, "groups")?;
    set_meta(tx, "updated_at", &crate::now_rfc3339())?;
    refresh_counts(tx)
}

fn refresh_counts(tx: &Connection) -> Result<()> {
    let (min, max): (Option<String>, Option<String>) =
        tx.query_row("SELECT MIN(date), MAX(date) FROM records", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    set_meta(tx, "record_count", &count(tx, "records")?.to_string())?;
    set_meta(tx, "group_count", &count(tx, "groups")?.to_string())?;
    set_meta(tx, "date_min", &min.unwrap_or_default())?;
    set_meta(tx, "date_max", &max.unwrap_or_default())
}

#[derive(Default)]
struct FacetDeltas(HashMap<(String, String), (String, i64)>);

impl FacetDeltas {
    fn add(&mut self, field: &str, value: &str, delta: i64) {
        let entry =
            self.0.entry((field.to_string(), value.to_lowercase())).or_insert_with(|| (value.to_string(), 0));
        entry.1 += delta;
    }

    fn remove_json(&mut self, facets: Option<&str>) {
        let pairs: Vec<(String, String)> =
            facets.and_then(|f| serde_json::from_str(f).ok()).unwrap_or_default();
        for (f, v) in pairs {
            self.add(&f, &v, -1);
        }
    }

    fn apply(&mut self, conn: &Connection) -> Result<()> {
        let mut upsert = conn.prepare_cached(
            "INSERT INTO facets (field, key, value, count) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (field, key) DO UPDATE SET count = count + excluded.count",
        )?;
        for ((field, key), (value, delta)) in self.0.drain() {
            if delta != 0 {
                upsert.execute(params![field, key, value, delta])?;
            }
        }
        conn.execute("DELETE FROM facets WHERE count <= 0", [])?;
        Ok(())
    }
}

struct Ingest<'a> {
    conn: &'a Connection,
    mapping: &'a Mapping,
    opts: BuildOptions,
    report: IngestReport,
    deltas: FacetDeltas,
    incremental: bool,
}

impl<'a> Ingest<'a> {
    fn new(
        conn: &'a Connection,
        mapping: &'a Mapping,
        opts: BuildOptions,
        incremental: bool,
    ) -> Result<Self> {
        conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS touched_groups (key TEXT PRIMARY KEY)")?;
        Ok(Self {
            conn,
            mapping,
            opts,
            report: IngestReport::default(),
            deltas: FacetDeltas::default(),
            incremental,
        })
    }

    fn source(&mut self, source: &Source, sql: Option<&str>) -> Result<()> {
        log(self.opts, &format!("reading {}", source.path.display()));
        let label = source.label();
        source::read(source, sql, &mut |item| {
            match item {
                Item::Bad { line, error } => self.skip(&label, line, &error)?,
                Item::Record { line, value, raw } => self.record(&label, line, &value, raw)?,
            }
            Ok(true)
        })
    }

    fn skip(&mut self, label: &str, line: u64, why: &str) -> Result<()> {
        let message = format!("{label}:{line}: {why}");
        if self.opts.strict {
            bail!("unusable record (strict mode) {message}");
        }
        self.report.skipped_lines += 1;
        if self.report.skipped_examples.len() < 5 {
            self.report.skipped_examples.push(message);
        }
        Ok(())
    }

    fn record(
        &mut self,
        label: &str,
        line: u64,
        value: &serde_json::Value,
        raw: Option<String>,
    ) -> Result<()> {
        let p = self.mapping.prepare(value);
        let id = match (&p.id, self.mapping.has_id()) {
            (Some(id), _) => id.clone(),
            (None, false) => format!("{label}:{line}"),
            (None, true) => {
                let field = self.mapping.config.fields.id.as_deref().unwrap_or_default();
                return self.skip(label, line, &format!("missing id field `{field}`"));
            }
        };
        self.report.records_read += 1;
        let doc = match raw {
            Some(raw) => raw,
            None => serde_json::to_string(value)?,
        };
        let facets_json = (!p.facets.is_empty()).then(|| serde_json::to_string(&p.facets)).transpose()?;
        let row = params![id, p.group, p.group_name, p.date, p.boost, facets_json, doc];
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO records (id, grp, grp_name, date, boost, facets, doc) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(row)?;
        let rowid = if inserted == 1 {
            self.report.inserted += 1;
            self.conn.last_insert_rowid()
        } else {
            let (rowid, old_group, old_facets) = existing(self.conn, &id)?.expect("conflicting row exists");
            self.conn
                .prepare_cached(
                    "UPDATE records SET grp = ?2, grp_name = ?3, date = ?4, boost = ?5, facets = ?6, doc = ?7 \
                     WHERE id = ?1",
                )?
                .execute(row)?;
            self.conn.prepare_cached("DELETE FROM record_fts WHERE rowid = ?1")?.execute([rowid])?;
            self.deltas.remove_json(old_facets.as_deref());
            if self.incremental
                && let Some(g) = old_group
            {
                touch(self.conn, &g)?;
            }
            self.report.updated += 1;
            rowid
        };
        self.conn
            .prepare_cached(
                "INSERT INTO record_fts (rowid, title, body, names, tags) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![rowid, p.title, p.body, p.names, p.tags])?;
        for (f, v) in &p.facets {
            self.deltas.add(f, v, 1);
        }
        if self.incremental
            && let Some(g) = &p.group
        {
            touch(self.conn, g)?;
        }
        if self.report.records_read.is_multiple_of(100_000) {
            log(self.opts, &format!("  ...{} records", self.report.records_read));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<IngestReport> {
        self.deltas.apply(self.conn)?;
        Ok(self.report)
    }
}

type Existing = (i64, Option<String>, Option<String>);

fn existing(conn: &Connection, id: &str) -> Result<Option<Existing>> {
    Ok(conn
        .prepare_cached("SELECT rowid, grp, facets FROM records WHERE id = ?1")?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?)
}

fn touch(conn: &Connection, key: &str) -> Result<()> {
    conn.prepare_cached("INSERT OR IGNORE INTO temp.touched_groups (key) VALUES (?1)")?.execute([key])?;
    Ok(())
}

/// Recompute group rows from their records. The name comes from each group's
/// most recent record (SQLite's bare-column-with-MAX rule).
fn rebuild_groups(conn: &Connection, only_touched: bool) -> Result<()> {
    let (filter, scope) = if only_touched {
        (
            "AND grp IN (SELECT key FROM temp.touched_groups)",
            "WHERE key IN (SELECT key FROM temp.touched_groups)",
        )
    } else {
        ("", "")
    };
    if only_touched {
        conn.execute(&format!("UPDATE groups SET record_count = 0 {scope}"), [])?;
    }
    conn.execute(
        &format!(
            "INSERT INTO groups (key, name, record_count) \
             SELECT grp, grp_name, n FROM ( \
                 SELECT grp, grp_name, MAX(COALESCE(date, '')), COUNT(*) AS n \
                 FROM records WHERE grp IS NOT NULL {filter} GROUP BY grp) WHERE true \
             ON CONFLICT (key) DO UPDATE SET \
               name = COALESCE(excluded.name, groups.name), \
               record_count = excluded.record_count"
        ),
        [],
    )?;
    conn.execute("DELETE FROM groups WHERE record_count = 0", [])?;
    let rows: Vec<(String, Option<String>)> = conn
        .prepare(&format!("SELECT key, name FROM groups {scope}"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut update = conn.prepare("UPDATE groups SET key_lower = ?2, name_normalized = ?3 WHERE key = ?1")?;
    for (key, name) in rows {
        update.execute(params![key, key.to_lowercase(), name.map(|n| normalize_name(&n))])?;
    }
    conn.execute("DELETE FROM temp.touched_groups", [])?;
    Ok(())
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.prepare_cached("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)")?
        .execute(params![key, value])?;
    Ok(())
}

pub(crate) fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |r| r.get(0))
        .optional()?)
}

/// The mapping an index was built with.
pub fn stored_config(conn: &Connection) -> Result<Config> {
    let raw = meta(conn, "config")?.context("index has no stored mapping")?;
    serde_json::from_str(&raw).context("stored mapping is unreadable")
}

fn index_is_current(index_path: &Path, manifest: &str) -> bool {
    let Ok(conn) = open_ro(index_path) else { return false };
    let get = |key: &str| meta(&conn, key).ok().flatten();
    get("schema_version").as_deref() == Some(SCHEMA_VERSION)
        && get("source_manifest").as_deref() == Some(manifest)
}

/// Open an existing index read-only, refusing other schema versions.
pub fn check_schema(conn: &Connection, path: &Path) -> Result<()> {
    let version = meta(conn, "schema_version")
        .with_context(|| format!("{} is not a Leviathan index", path.display()))?;
    if version.as_deref() != Some(SCHEMA_VERSION) {
        bail!(
            "index {} has schema {:?}; this build reads schema {SCHEMA_VERSION}. Rebuild it with `leviathan index --force`",
            path.display(),
            version.unwrap_or_default()
        );
    }
    Ok(())
}

fn open_rw(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("no index at {}; run `leviathan index` first", path.display());
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    check_schema(&conn, path)?;
    Ok(conn)
}

pub(crate) fn open_ro(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?)
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn log(opts: BuildOptions, message: &str) {
    if !opts.quiet {
        eprintln!("[leviathan] {message}");
    }
}
