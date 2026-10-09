//! Read/write memory for agents: small, typed, citable claims in their own
//! SQLite file, separate from the data index (which stays read-only).
//!
//! Each memory is one claim, optionally about a subject. A `key` names a slot
//! (`editor`, `deploy.target`): writing the same subject and key again
//! supersedes the old value instead of piling up beside it, and the old row
//! stays as history. Without a key, near-duplicates merge into the existing
//! memory. Every write is one transaction and one `log` entry.
//!
//! Recall ranks current memories by BM25, recency (a half-life per kind),
//! importance and pinning, then packs them into a token budget.

mod guard;
mod recall;
mod write;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::config::MemoryConfig;
use crate::text::{normalize_name, parse_unix};

pub use guard::{Refused, secret_kind};
pub use recall::{RecallOutcome, RecallRequest, RecallStatus, Recalled};
pub use write::{Forget, Remember, WriteOutcome, WriteStatus};

pub const SCHEMA_VERSION: &str = "m1";
const KIND_TAG: &str = "leviathan-memory";

const SCHEMA: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE memories (
    rowid            INTEGER PRIMARY KEY,
    id               TEXT NOT NULL UNIQUE,
    ns               TEXT NOT NULL,
    kind             TEXT NOT NULL,
    subject          TEXT,
    subject_norm     TEXT NOT NULL DEFAULT '',
    key              TEXT,
    text             TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '',
    importance       INTEGER NOT NULL DEFAULT 3,
    confidence       REAL NOT NULL DEFAULT 1.0,
    source           TEXT,
    refs             TEXT NOT NULL DEFAULT '[]',
    pinned           INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    valid_from       TEXT NOT NULL,
    valid_to         TEXT,
    superseded_by    TEXT,
    expires_at       TEXT,
    deleted_at       TEXT,
    recall_count     INTEGER NOT NULL DEFAULT 0,
    last_recalled_at TEXT
);
-- One current value per slot.
CREATE UNIQUE INDEX idx_memories_slot ON memories (ns, subject_norm, key)
    WHERE key IS NOT NULL AND valid_to IS NULL AND deleted_at IS NULL;
CREATE INDEX idx_memories_current ON memories (ns, valid_to, deleted_at, updated_at);
CREATE INDEX idx_memories_subject ON memories (ns, subject_norm);
-- Briefings walk current memories in this order and stop early.
CREATE INDEX idx_memories_briefing ON memories (ns, pinned DESC, importance DESC, updated_at DESC)
    WHERE valid_to IS NULL AND deleted_at IS NULL;

CREATE VIRTUAL TABLE memory_fts USING fts5(
    text, subject, key, tags,
    content = 'memories', content_rowid = 'rowid',
    tokenize = 'porter unicode61'
);
INSERT INTO memory_fts (memory_fts, rank) VALUES ('rank', 'bm25(1.0, 2.0, 2.0, 1.0)');
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memory_fts (rowid, text, subject, key, tags)
    VALUES (new.rowid, new.text, new.subject, new.key, new.tags);
END;
CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, text, subject, key, tags)
    VALUES ('delete', old.rowid, old.text, old.subject, old.key, old.tags);
END;
CREATE TRIGGER memories_au AFTER UPDATE OF text, subject, key, tags ON memories BEGIN
    INSERT INTO memory_fts (memory_fts, rowid, text, subject, key, tags)
    VALUES ('delete', old.rowid, old.text, old.subject, old.key, old.tags);
    INSERT INTO memory_fts (rowid, text, subject, key, tags)
    VALUES (new.rowid, new.text, new.subject, new.key, new.tags);
END;

CREATE TABLE subjects (
    ns         TEXT NOT NULL,
    norm       TEXT NOT NULL,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (ns, norm)
) WITHOUT ROWID;

CREATE TABLE subject_aliases (
    ns    TEXT NOT NULL,
    alias TEXT NOT NULL,
    norm  TEXT NOT NULL,
    PRIMARY KEY (ns, alias)
) WITHOUT ROWID;

-- Append-only: every write, merge, supersede, forget, import and prune.
CREATE TABLE log (
    seq     INTEGER PRIMARY KEY,
    at      TEXT NOT NULL,
    op      TEXT NOT NULL,
    id      TEXT,
    payload TEXT NOT NULL
);
"#;

const COLUMNS: &str = "id, ns, kind, subject, key, text, tags, importance, confidence, source, refs, pinned, \
    created_at, updated_at, valid_from, valid_to, superseded_by, expires_at, deleted_at, recall_count, last_recalled_at";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Something true about the world, a project or a person.
    Fact,
    /// How someone wants things done.
    Preference,
    /// A choice that was made, ideally with the reason.
    Decision,
    /// What was learned from a failure or a surprise.
    Lesson,
    /// Something that happened at a point in time.
    Event,
    /// Open work.
    Task,
    /// Anything else.
    Note,
}

impl Kind {
    pub const ALL: [Kind; 7] =
        [Kind::Fact, Kind::Preference, Kind::Decision, Kind::Lesson, Kind::Event, Kind::Task, Kind::Note];
    pub const NAMES: &'static str = "fact, preference, decision, lesson, event, task, note";

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fact => "fact",
            Kind::Preference => "preference",
            Kind::Decision => "decision",
            Kind::Lesson => "lesson",
            Kind::Event => "event",
            Kind::Task => "task",
            Kind::Note => "note",
        }
    }

    pub fn parse(value: &str) -> Option<Kind> {
        let v = value.trim().to_lowercase();
        Kind::ALL.into_iter().find(|k| k.as_str() == v || format!("{}s", k.as_str()) == v)
    }

    /// Days until recency counts half; 0 means it does not decay. Facts,
    /// preferences, decisions and lessons stay true until superseded.
    fn default_half_life(self) -> f64 {
        match self {
            Kind::Event => 30.0,
            Kind::Task => 7.0,
            Kind::Note => 90.0,
            _ => 0.0,
        }
    }
}

/// Effective memory settings: `[memory]` from `leviathan.toml`, then flags.
#[derive(Debug, Clone)]
pub struct Settings {
    pub namespace: String,
    pub read: Vec<String>,
    pub budget: usize,
    pub briefing_budget: usize,
    pub max_chars: usize,
    pub pin: Vec<String>,
    half_life: BTreeMap<Kind, f64>,
}

impl Settings {
    pub fn from_config(cfg: &MemoryConfig) -> Self {
        let half_life = Kind::ALL
            .into_iter()
            .map(|k| {
                let days = cfg
                    .half_life
                    .iter()
                    .find(|(name, _)| Kind::parse(name) == Some(k))
                    .map_or(k.default_half_life(), |(_, d)| *d);
                (k, days)
            })
            .collect();
        let namespace = cfg.namespace.trim().to_lowercase();
        let read = if cfg.read.is_empty() {
            vec![namespace.clone()]
        } else {
            cfg.read.iter().map(|n| n.trim().to_lowercase()).collect()
        };
        Self {
            namespace,
            read,
            budget: cfg.budget,
            briefing_budget: cfg.briefing_budget,
            max_chars: cfg.max_chars,
            pin: cfg.pin.clone(),
            half_life,
        }
    }

    pub fn half_life(&self, kind: Kind) -> f64 {
        self.half_life.get(&kind).copied().unwrap_or(0.0)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_config(&MemoryConfig::default())
    }
}

/// `~/.leviathan/memory.db`, or under `LEVIATHAN_HOME` when set.
pub fn default_path() -> PathBuf {
    if let Some(home) = std::env::var_os("LEVIATHAN_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join("memory.db");
    }
    match home_dir() {
        Some(home) => home.join(".leviathan").join("memory.db"),
        None => PathBuf::from("leviathan-memory.db"),
    }
}

fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|v| std::env::var_os(v).filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// Expand a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")), home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => home_dir().unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

/// One memory, exactly as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    pub id: String,
    pub ns: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub importance: u8,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
    pub valid_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    #[serde(default)]
    pub recall_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recalled_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Current,
    Superseded,
    Forgotten,
    Expired,
}

impl MemoryRow {
    pub fn state(&self, now: &str) -> State {
        if self.deleted_at.is_some() {
            State::Forgotten
        } else if self.valid_to.is_some() {
            State::Superseded
        } else if self.expires_at.as_deref().is_some_and(|e| e <= now) {
            State::Expired
        } else {
            State::Current
        }
    }

    /// One compact line: what agents read.
    pub fn line(&self, now: &str, show_ns: bool) -> String {
        let mut head = format!("[{}", self.kind.as_str());
        match self.state(now) {
            State::Current => {}
            State::Superseded => {
                head.push_str(&format!(
                    " · SUPERSEDED {}",
                    day(self.valid_to.as_deref().unwrap_or_default())
                ));
            }
            State::Forgotten => {
                head.push_str(&format!(
                    " · FORGOTTEN {}",
                    day(self.deleted_at.as_deref().unwrap_or_default())
                ));
            }
            State::Expired => head.push_str(" · EXPIRED"),
        }
        head.push(']');
        let mut out = head;
        if show_ns {
            out.push_str(&format!(" {}:", self.ns));
        }
        match (&self.subject, &self.key) {
            (Some(s), Some(k)) => out.push_str(&format!(" {s} · {k}:")),
            (Some(s), None) => out.push_str(&format!(" {s}:")),
            (None, Some(k)) => out.push_str(&format!(" {k}:")),
            (None, None) => {}
        }
        out.push(' ');
        out.push_str(&self.text);
        let mut meta = vec![day(&self.valid_from).to_string()];
        if let Some(s) = &self.source {
            meta.push(s.clone());
        }
        if self.pinned {
            meta.push("pinned".into());
        }
        if !self.refs.is_empty() {
            meta.push(format!("refs {}", self.refs.join(", ")));
        }
        meta.push(self.id.clone());
        out.push_str(&format!(" ({})", meta.join(" · ")));
        out
    }
}

fn day(ts: &str) -> &str {
    &ts[..ts.len().min(10)]
}

fn row(r: &Row<'_>) -> rusqlite::Result<MemoryRow> {
    let kind: String = r.get(2)?;
    let tags: String = r.get(6)?;
    let refs: String = r.get(10)?;
    Ok(MemoryRow {
        id: r.get(0)?,
        ns: r.get(1)?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Note),
        subject: r.get(3)?,
        key: r.get(4)?,
        text: r.get(5)?,
        tags: tags.split_whitespace().map(str::to_string).collect(),
        importance: r.get::<_, i64>(7)?.clamp(1, 5) as u8,
        confidence: r.get(8)?,
        source: r.get(9)?,
        refs: serde_json::from_str(&refs).unwrap_or_default(),
        pinned: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
        valid_from: r.get(14)?,
        valid_to: r.get(15)?,
        superseded_by: r.get(16)?,
        expires_at: r.get(17)?,
        deleted_at: r.get(18)?,
        recall_count: r.get(19)?,
        last_recalled_at: r.get(20)?,
    })
}

/// Sortable id: `m`, 48 bits of milliseconds and 30 random bits in
/// lowercase Crockford base32 (17 characters).
fn new_id() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        & ((1 << 48) - 1);
    let mut rnd = [0u8; 4];
    let _ = getrandom::fill(&mut rnd);
    let v = (u128::from(ms) << 30) | u128::from(u32::from_le_bytes(rnd) & ((1 << 30) - 1));
    let mut out = String::with_capacity(17);
    out.push('m');
    for i in (0..16).rev() {
        out.push(ALPHABET[((v >> (i * 5)) & 31) as usize] as char);
    }
    out
}

#[derive(Debug, Serialize)]
pub struct MemoryStats {
    pub path: PathBuf,
    pub bytes: u64,
    pub schema_version: String,
    pub namespace: String,
    pub read: Vec<String>,
    pub current: i64,
    pub superseded: i64,
    pub forgotten: i64,
    pub expired: i64,
    pub by_kind: Vec<(String, i64)>,
    pub by_namespace: Vec<(String, i64)>,
    pub top_subjects: Vec<(String, i64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub read: u64,
    pub created: u64,
    pub replaced: u64,
    pub merged: u64,
    pub skipped: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PruneOptions {
    /// Drop forgotten memories for good.
    pub forgotten: bool,
    /// Drop superseded history.
    pub superseded: bool,
    /// Forget current memories of importance 1-2 never recalled and older
    /// than this many days.
    pub unused_days: Option<u32>,
    pub dry_run: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct PruneReport {
    pub expired: u64,
    pub forgotten: u64,
    pub superseded: u64,
    pub unused: u64,
    pub dry_run: bool,
}

pub struct Memory {
    conn: Connection,
    path: PathBuf,
    pub settings: Settings,
}

pub(crate) fn now() -> String {
    crate::now_rfc3339()
}

impl Memory {
    /// Open a memory database, creating it (and its directory) on first use.
    pub fn open(path: &Path, settings: Settings) -> Result<Self> {
        let fresh = !path.exists();
        if fresh && let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("open memory {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let has_meta: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |r| r.get(0),
        )?;
        if has_meta {
            let get = |k: &str| -> Result<Option<String>> {
                Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0)).optional()?)
            };
            if get("kind")?.as_deref() != Some(KIND_TAG) {
                bail!(
                    "{} is not a Leviathan memory database (is it a data index? pass that as --index)",
                    path.display()
                );
            }
            let version = get("schema_version")?.unwrap_or_default();
            if version != SCHEMA_VERSION {
                bail!(
                    "memory {} has schema {version:?}; this build reads {SCHEMA_VERSION}. Export it with the version that wrote it",
                    path.display()
                );
            }
        } else {
            let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
            if tables > 0 {
                bail!("{} is an SQLite database but not a Leviathan memory database", path.display());
            }
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            let created = now();
            for (k, v) in
                [("kind", KIND_TAG), ("schema_version", SCHEMA_VERSION), ("created_at", created.as_str())]
            {
                tx.execute("INSERT INTO meta (key, value) VALUES (?1, ?2)", params![k, v])?;
            }
            tx.commit()?;
            restrict_permissions(path);
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { conn, path: path.to_path_buf(), settings })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_tx(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    /// Namespaces to read: an explicit comma list, `*` for every namespace,
    /// or the configured `read` list.
    pub fn read_namespaces(&self, ns: Option<&str>) -> Result<Vec<String>> {
        let Some(raw) = ns.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.settings.read.clone());
        };
        if raw == "*" {
            let mut stmt = self.conn.prepare_cached("SELECT DISTINCT ns FROM memories ORDER BY ns")?;
            let all = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            return Ok(if all.is_empty() { self.settings.read.clone() } else { all });
        }
        raw.split(',').map(|n| guard::namespace(n).map_err(Into::into)).collect()
    }

    pub fn get(&self, id: &str) -> Result<Option<MemoryRow>> {
        get_row(&self.conn, id)
    }

    /// Every version of a slot (or of the memory with this id), oldest first.
    pub fn history(
        &self,
        id: Option<&str>,
        subject: Option<&str>,
        key: Option<&str>,
        ns: Option<&str>,
    ) -> Result<Vec<MemoryRow>> {
        let (ns, subject_norm, key) = match (id, key) {
            (Some(id), _) => {
                let m = self.get(id)?.with_context(|| format!("no memory with id {id:?}"))?;
                match m.key.clone() {
                    Some(k) => {
                        (m.ns.clone(), m.subject.as_deref().map(normalize_name).unwrap_or_default(), k)
                    }
                    None => return Ok(vec![m]),
                }
            }
            (None, Some(k)) => {
                let ns = guard::namespace(ns.unwrap_or(&self.settings.namespace))?;
                let norm = match subject {
                    Some(s) => resolve_alias(&self.conn, &ns, &normalize_name(s))?,
                    None => String::new(),
                };
                (ns, norm, guard::key(k)?)
            }
            _ => bail!("pass a memory id, or --key (with --subject when the slot has one)"),
        };
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories WHERE ns = ?1 AND subject_norm = ?2 AND key = ?3 ORDER BY valid_from, rowid"
        ))?;
        Ok(stmt.query_map(params![ns, subject_norm, key], row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Newest first. `all` includes superseded, forgotten and expired rows.
    pub fn list(
        &self,
        ns: &[String],
        kind: Option<Kind>,
        subject: Option<&str>,
        all: bool,
        limit: usize,
    ) -> Result<Vec<MemoryRow>> {
        let mut sql = format!("SELECT {COLUMNS} FROM memories WHERE ns IN ({})", placeholders(ns.len(), 1));
        let mut args: Vec<rusqlite::types::Value> = ns.iter().cloned().map(Into::into).collect();
        if !all {
            args.push(now().into());
            sql.push_str(&format!(
                " AND valid_to IS NULL AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > ?{})",
                args.len()
            ));
        }
        if let Some(k) = kind {
            args.push(k.as_str().to_string().into());
            sql.push_str(&format!(" AND kind = ?{}", args.len()));
        }
        if let Some(s) = subject {
            args.push(normalize_name(s).into());
            sql.push_str(&format!(" AND subject_norm = ?{}", args.len()));
        }
        args.push((limit.clamp(1, 10_000) as i64).into());
        sql.push_str(&format!(" ORDER BY updated_at DESC, rowid DESC LIMIT ?{}", args.len()));
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt.query_map(rusqlite::params_from_iter(args), row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn stats(&self) -> Result<MemoryStats> {
        let now = now();
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [&now], |r| r.get(0))?) };
        let current_sql =
            "valid_to IS NULL AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > ?1)";
        let pairs = |sql: &str| -> Result<Vec<(String, i64)>> {
            let mut stmt = self.conn.prepare(sql)?;
            Ok(stmt.query_map([&now], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
        };
        let version: String =
            self.conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))?;
        Ok(MemoryStats {
            path: self.path.clone(),
            bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            schema_version: version,
            namespace: self.settings.namespace.clone(),
            read: self.settings.read.clone(),
            current: one(&format!("SELECT COUNT(*) FROM memories WHERE {current_sql}"))?,
            superseded: one(
                "SELECT COUNT(*) FROM memories WHERE valid_to IS NOT NULL AND deleted_at IS NULL AND ?1 = ?1",
            )?,
            forgotten: one("SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL AND ?1 = ?1")?,
            expired: one(
                "SELECT COUNT(*) FROM memories WHERE valid_to IS NULL AND deleted_at IS NULL AND expires_at <= ?1",
            )?,
            by_kind: pairs(&format!(
                "SELECT kind, COUNT(*) FROM memories WHERE {current_sql} GROUP BY kind ORDER BY 2 DESC, 1"
            ))?,
            by_namespace: pairs(&format!(
                "SELECT ns, COUNT(*) FROM memories WHERE {current_sql} GROUP BY ns ORDER BY 2 DESC, 1"
            ))?,
            top_subjects: pairs(&format!(
                "SELECT subject, COUNT(*) FROM memories WHERE {current_sql} AND subject IS NOT NULL \
                 GROUP BY subject_norm ORDER BY 2 DESC, 1 LIMIT 10"
            ))?,
            newest: self
                .conn
                .query_row("SELECT MAX(updated_at) FROM memories", [], |r| r.get(0))
                .optional()?
                .flatten(),
        })
    }

    /// Every row, history included, as JSON lines. Returns the row count.
    pub fn export(&self, out: &mut impl Write) -> Result<u64> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM memories ORDER BY rowid"))?;
        let mut n = 0;
        for m in stmt.query_map([], row)? {
            serde_json::to_writer(&mut *out, &m?)?;
            out.write_all(b"\n")?;
            n += 1;
        }
        Ok(n)
    }

    /// Restore `export` output. Rows keep their ids and history; ids that
    /// already exist are skipped, so importing twice is harmless.
    pub fn import_jsonl(&mut self, input: impl BufRead) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let tx = self.write_tx()?;
        for (n, line) in input.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            report.read += 1;
            let m: MemoryRow = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(err) => {
                    report.skipped += 1;
                    if report.errors.len() < 5 {
                        report.errors.push(format!("line {}: {err}", n + 1));
                    }
                    continue;
                }
            };
            if let Some(kind) =
                [Some(m.text.as_str()), m.subject.as_deref(), m.key.as_deref(), m.source.as_deref()]
                    .into_iter()
                    .flatten()
                    .find_map(secret_kind)
            {
                report.skipped += 1;
                if report.errors.len() < 5 {
                    report.errors.push(format!("line {}: refused, looks like a secret ({kind})", n + 1));
                }
                continue;
            }
            let exists: bool =
                tx.query_row("SELECT EXISTS (SELECT 1 FROM memories WHERE id = ?1)", [&m.id], |r| r.get(0))?;
            if exists {
                report.skipped += 1;
                continue;
            }
            if m.valid_to.is_none() && m.deleted_at.is_none() && m.key.is_some() {
                // A restored current value supersedes whatever holds its slot.
                let norm = m.subject.as_deref().map(normalize_name).unwrap_or_default();
                let replaced = tx.execute(
                    "UPDATE memories SET valid_to = ?4, superseded_by = ?5 WHERE ns = ?1 AND subject_norm = ?2 \
                     AND key = ?3 AND valid_to IS NULL AND deleted_at IS NULL",
                    params![m.ns, norm, m.key, m.valid_from, m.id],
                )?;
                report.replaced += replaced as u64;
            }
            if let Some(s) = &m.subject {
                ensure_subject(&tx, &m.ns, s, &normalize_name(s))?;
            }
            insert_row(&tx, &m)?;
            log(&tx, "import", Some(&m.id), &serde_json::to_value(&m)?)?;
            report.created += 1;
        }
        tx.commit()?;
        Ok(report)
    }

    /// Bullets of a markdown memory file (`- ...`, `* ...`, `1. ...`) as
    /// notes, each about the nearest heading above it. Goes through the
    /// normal write path, so duplicates merge and secrets are refused.
    pub fn import_markdown(&mut self, text: &str, ns: Option<&str>, source: &str) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut heading: Option<String> = None;
        let mut fence: Option<&str> = None;
        let mut front_matter = text.trim_start().starts_with("---");
        for (n, line) in text.lines().enumerate() {
            let t = line.trim();
            if front_matter {
                front_matter = !(n > 0 && t == "---");
                continue;
            }
            if let Some(open) = fence {
                if t.starts_with(open) {
                    fence = None;
                }
                continue;
            }
            if let Some(open) = ["```", "~~~"].into_iter().find(|f| t.starts_with(f)) {
                fence = Some(open);
                continue;
            }
            if let Some(h) = t.strip_prefix('#') {
                let h = h.trim_start_matches('#').trim();
                heading = (!h.is_empty()).then(|| crate::text::truncate(h, 100));
                continue;
            }
            let item = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| {
                t.split_once(". ").filter(|(n, _)| n.parse::<u32>().is_ok()).map(|(_, rest)| rest)
            });
            let Some(item) = item.map(|i| i.trim_start_matches("[ ] ").trim_start_matches("[x] ").trim())
            else {
                continue;
            };
            if item.is_empty() {
                continue;
            }
            report.read += 1;
            let req = Remember {
                text: crate::text::truncate(item, self.settings.max_chars),
                subject: heading.clone(),
                source: Some(source.to_string()),
                ns: ns.map(str::to_string),
                ..Default::default()
            };
            match self.remember(&req) {
                Ok(out) => match out.status {
                    WriteStatus::Created => report.created += 1,
                    WriteStatus::Replaced => report.replaced += 1,
                    WriteStatus::Duplicate => report.merged += 1,
                },
                Err(err) => {
                    report.skipped += 1;
                    if report.errors.len() < 5 {
                        report.errors.push(format!("{}: {err}", crate::text::truncate(item, 60)));
                    }
                }
            }
        }
        Ok(report)
    }

    /// Expired rows are always dropped; the rest is opt-in.
    pub fn prune(&mut self, opts: PruneOptions) -> Result<PruneReport> {
        let now = now();
        let tx = self.write_tx()?;
        let mut report = PruneReport { dry_run: opts.dry_run, ..Default::default() };
        let run = |tx: &rusqlite::Transaction<'_>, sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<u64> {
            let count: i64 =
                tx.query_row(&format!("SELECT COUNT(*) FROM memories WHERE {sql}"), args, |r| r.get(0))?;
            if !opts.dry_run && count > 0 {
                tx.execute(&format!("DELETE FROM memories WHERE {sql}"), args)?;
            }
            Ok(count as u64)
        };
        report.expired = run(&tx, "deleted_at IS NULL AND valid_to IS NULL AND expires_at <= ?1", &[&now])?;
        if opts.forgotten {
            report.forgotten = run(&tx, "deleted_at IS NOT NULL", &[])?;
        }
        if opts.superseded {
            report.superseded = run(&tx, "valid_to IS NOT NULL AND deleted_at IS NULL", &[])?;
        }
        if let Some(days) = opts.unused_days {
            let cutoff = crate::text::format_unix(parse_unix(&now).unwrap_or(0) - i64::from(days) * 86_400);
            let sql = "deleted_at IS NULL AND valid_to IS NULL AND pinned = 0 AND importance <= 2 \
                       AND recall_count = 0 AND updated_at < ?1";
            let count: i64 =
                tx.query_row(&format!("SELECT COUNT(*) FROM memories WHERE {sql}"), [&cutoff], |r| r.get(0))?;
            if !opts.dry_run && count > 0 {
                tx.execute(
                    &format!("UPDATE memories SET deleted_at = ?2 WHERE {sql}"),
                    params![cutoff, now],
                )?;
            }
            report.unused = count as u64;
        }
        if !opts.dry_run {
            log(&tx, "prune", None, &serde_json::to_value(&report)?)?;
            tx.commit()?;
        }
        Ok(report)
    }
}

fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub(crate) fn placeholders(n: usize, start: usize) -> String {
    (start..start + n).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
}

pub(crate) fn get_row(conn: &Connection, id: &str) -> Result<Option<MemoryRow>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"))?
        .query_row([id.trim().to_lowercase()], row)
        .optional()?)
}

pub(crate) fn insert_row(conn: &Connection, m: &MemoryRow) -> Result<()> {
    conn.prepare_cached(&format!(
        "INSERT INTO memories (subject_norm, {COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
         ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)"
    ))?
    .execute(params![
        m.subject.as_deref().map(normalize_name).unwrap_or_default(),
        m.id,
        m.ns,
        m.kind.as_str(),
        m.subject,
        m.key,
        m.text,
        m.tags.join(" "),
        m.importance,
        m.confidence,
        m.source,
        serde_json::to_string(&m.refs)?,
        m.pinned,
        m.created_at,
        m.updated_at,
        m.valid_from,
        m.valid_to,
        m.superseded_by,
        m.expires_at,
        m.deleted_at,
        m.recall_count,
        m.last_recalled_at,
    ])?;
    Ok(())
}

pub(crate) fn log(conn: &Connection, op: &str, id: Option<&str>, payload: &serde_json::Value) -> Result<()> {
    conn.prepare_cached("INSERT INTO log (at, op, id, payload) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![now(), op, id, payload.to_string()])?;
    Ok(())
}

/// The canonical subject an alias points at (or the name itself).
pub(crate) fn resolve_alias(conn: &Connection, ns: &str, norm: &str) -> Result<String> {
    Ok(conn
        .prepare_cached("SELECT norm FROM subject_aliases WHERE ns = ?1 AND alias = ?2")?
        .query_row(params![ns, norm], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| norm.to_string()))
}

/// Register a subject; returns its display name (the first spelling seen).
pub(crate) fn ensure_subject(conn: &Connection, ns: &str, name: &str, norm: &str) -> Result<String> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO subjects (ns, norm, name, created_at) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![ns, norm, name, now()])?;
    Ok(conn
        .prepare_cached("SELECT name FROM subjects WHERE ns = ?1 AND norm = ?2")?
        .query_row(params![ns, norm], |r| r.get(0))?)
}
