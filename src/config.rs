//! The field mapping: which parts of a record are its id, title, searchable
//! text, group, date, filters and display fields.
//!
//! Resolution order: `leviathan.toml` (or inference from the data when there
//! is no config), then CLI flags on top. The effective mapping is stored in
//! the index, so later queries, upserts and the MCP server all use exactly
//! the mapping the index was built with.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

pub const DEFAULT_FILE: &str = "leviathan.toml";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub about: About,
    pub source: SourceConfig,
    pub fields: Fields,
    pub rank: Rank,
    #[serde(skip_serializing_if = "MemoryConfig::is_default")]
    pub memory: MemoryConfig,
}

/// The read/write memory store (`leviathan memory`, `--memory`). Separate
/// from the data index, which stays read-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    /// Memory database, relative to the config file
    /// [default: ~/.leviathan/memory.db].
    pub path: Option<String>,
    /// Namespace new memories are written to.
    pub namespace: String,
    /// Namespaces recall reads by default [default: just `namespace`].
    pub read: Vec<String>,
    /// Token budget for one recall.
    pub budget: usize,
    /// Token budget for the session-start briefing.
    pub briefing_budget: usize,
    /// Character cap for one memory.
    pub max_chars: usize,
    /// Recency half-life in days per kind; 0 means no decay.
    pub half_life: std::collections::BTreeMap<String, f64>,
    /// Subjects whose memories lead every briefing.
    pub pin: Vec<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            path: None,
            namespace: "default".into(),
            read: Vec::new(),
            budget: 800,
            briefing_budget: 600,
            max_chars: 400,
            half_life: Default::default(),
            pin: Vec::new(),
        }
    }
}

impl MemoryConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// How results talk about the data: "3 tickets for customer acme".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct About {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Singular noun for one record ("ticket", "log line", "incident").
    pub record: String,
    /// Singular noun for one group ("customer", "machine", "repo").
    pub group: String,
}

impl Default for About {
    fn default() -> Self {
        Self { name: None, description: None, record: "record".into(), group: "group".into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfig {
    /// Files or directories, relative to the config file.
    pub paths: Vec<String>,
    pub format: Format,
    /// Query for SQLite sources (default: the database's only table).
    pub sql: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// From the file extension; stdin is sniffed.
    #[default]
    Auto,
    Jsonl,
    Json,
    Csv,
    Tsv,
    Sqlite,
}

/// Every field is a path into a record: `a.b` for nesting, `items[].name`
/// (or just `items.name`) for every element of an array.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fields {
    /// Unique record id. Without one, records are numbered `<file>:<line>`
    /// and upserts cannot replace them.
    pub id: Option<String>,
    /// Short headline; the first path that is present wins.
    #[serde(deserialize_with = "one_or_many")]
    pub title: Vec<String>,
    /// Searched text. Empty means every string in the record.
    pub text: Vec<String>,
    /// Scope searches to one entity (customer, machine, repo, host, ...).
    pub group: Option<String>,
    /// Human name of the group, used to resolve "the filler on line 3".
    pub group_name: Option<String>,
    /// Recency for ranking ties, `recent`, and `--since/--until`. First
    /// present path wins.
    #[serde(deserialize_with = "one_or_many")]
    pub date: Vec<String>,
    /// Exact-match filters (`--where status=open`), counted in `describe`.
    pub filters: Vec<String>,
    /// Shown on every result card, in this order.
    pub display: Vec<String>,
    /// Placeholder values treated as missing ("n/a", "done", "-").
    pub empty_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rank {
    /// BM25 weight of title matches relative to body text.
    pub title_weight: f64,
    pub boost: Vec<Boost>,
}

impl Default for Rank {
    fn default() -> Self {
        Self { title_weight: 2.0, boost: Vec::new() }
    }
}

/// Multiply relevance by `1 + weight` for records where any of `field` is
/// present (non-empty, not a placeholder) or, with `equals`, has that value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boost {
    #[serde(deserialize_with = "one_or_many")]
    pub field: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    pub weight: f64,
}

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            for p in &mut cfg.source.paths {
                if p != "-" && Path::new(p).is_relative() {
                    *p = dir.join(&*p).display().to_string();
                }
            }
            if let Some(p) = &mut cfg.memory.path
                && Path::new(p).is_relative()
                && !p.starts_with('~')
            {
                *p = dir.join(&*p).display().to_string();
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let f = &self.fields;
        let all = f.id.iter().chain(&f.title).chain(&f.text).chain(&f.group).chain(&f.group_name);
        for path in all.chain(&f.date).chain(&f.filters).chain(&f.display) {
            if path.trim().is_empty() {
                bail!("empty field path in the mapping");
            }
        }
        if f.group_name.is_some() && f.group.is_none() {
            bail!("`group_name` needs `group`");
        }
        if !(self.rank.title_weight.is_finite() && self.rank.title_weight >= 0.0) {
            bail!("`rank.title_weight` must be a non-negative number");
        }
        for b in &self.rank.boost {
            if b.field.is_empty() || !(b.weight.is_finite() && b.weight > -1.0) {
                bail!("each [[rank.boost]] needs a `field` and a `weight` above -1");
            }
        }
        let m = &self.memory;
        if !(50..=20_000).contains(&m.budget) || !(50..=20_000).contains(&m.briefing_budget) {
            bail!("`memory.budget` and `memory.briefing_budget` must be between 50 and 20000 tokens");
        }
        if !(40..=4000).contains(&m.max_chars) {
            bail!("`memory.max_chars` must be between 40 and 4000");
        }
        for (kind, days) in &m.half_life {
            if crate::memory::Kind::parse(kind).is_none() || !(days.is_finite() && *days >= 0.0) {
                bail!(
                    "`memory.half_life.{kind}`: kind must be one of {} and days >= 0",
                    crate::memory::Kind::NAMES
                );
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }
}

/// Plural of a configured noun, for headers like "33 log lines".
pub fn plural(noun: &str) -> String {
    if noun.ends_with('s') || noun.ends_with("data") {
        noun.to_string()
    } else if let Some(stem) = noun.strip_suffix('y').filter(|s| !s.ends_with(['a', 'e', 'o', 'u'])) {
        format!("{stem}ies")
    } else {
        format!("{noun}s")
    }
}

/// Mapping overrides from the command line, applied on top of the config.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct FieldFlags {
    /// Config file [default: ./leviathan.toml when present]
    #[arg(long, short = 'c', env = "LEVIATHAN_CONFIG")]
    pub config: Option<std::path::PathBuf>,
    /// Unique id field
    #[arg(long, value_name = "PATH")]
    pub id: Option<String>,
    /// Title field(s); first present wins
    #[arg(long, value_name = "PATH", value_delimiter = ',')]
    pub title: Vec<String>,
    /// Searched text fields (default: every string)
    #[arg(long, value_name = "PATH", value_delimiter = ',')]
    pub text: Vec<String>,
    /// Group field for scoped search
    #[arg(long, value_name = "PATH")]
    pub group: Option<String>,
    /// Human-readable group name field
    #[arg(long, value_name = "PATH")]
    pub group_name: Option<String>,
    /// Date field(s); first present wins
    #[arg(long, value_name = "PATH", value_delimiter = ',')]
    pub date: Vec<String>,
    /// Exact-match filter fields
    #[arg(long = "filter", value_name = "PATH", value_delimiter = ',')]
    pub filters: Vec<String>,
    /// Fields shown on result cards
    #[arg(long, value_name = "PATH", value_delimiter = ',')]
    pub display: Vec<String>,
    /// Placeholder values treated as missing
    #[arg(long = "empty-value", value_name = "TEXT")]
    pub empty_values: Vec<String>,
    /// Input format
    #[arg(long, value_enum)]
    pub format: Option<Format>,
    /// SQL query for SQLite sources
    #[arg(long)]
    pub sql: Option<String>,
}

impl FieldFlags {
    /// The config file to use: explicit, else `./leviathan.toml` if it exists.
    pub fn config_path(&self) -> Option<std::path::PathBuf> {
        self.config.clone().or_else(|| {
            let default = Path::new(DEFAULT_FILE);
            default.exists().then(|| default.to_path_buf())
        })
    }

    pub fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.title.is_empty()
            && self.text.is_empty()
            && self.group.is_none()
            && self.group_name.is_none()
            && self.date.is_empty()
            && self.filters.is_empty()
            && self.display.is_empty()
            && self.empty_values.is_empty()
    }

    pub fn apply(&self, cfg: &mut Config) {
        let f = &mut cfg.fields;
        if self.id.is_some() {
            f.id.clone_from(&self.id);
        }
        if self.group.is_some() {
            f.group.clone_from(&self.group);
        }
        if self.group_name.is_some() {
            f.group_name.clone_from(&self.group_name);
        }
        for (dst, src) in [
            (&mut f.title, &self.title),
            (&mut f.text, &self.text),
            (&mut f.date, &self.date),
            (&mut f.filters, &self.filters),
            (&mut f.display, &self.display),
            (&mut f.empty_values, &self.empty_values),
        ] {
            if !src.is_empty() {
                dst.clone_from(src);
            }
        }
        if let Some(format) = self.format {
            cfg.source.format = format;
        }
        if self.sql.is_some() {
            cfg.source.sql.clone_from(&self.sql);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_or_many_and_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [fields]
            id = "id"
            title = "subject"
            date = ["closed", "opened"]
            [[rank.boost]]
            field = "resolution"
            weight = 0.15
            "#,
        )
        .unwrap();
        assert_eq!(cfg.fields.title, ["subject"]);
        assert_eq!(cfg.fields.date, ["closed", "opened"]);
        assert_eq!(cfg.about.record, "record");
        assert_eq!(cfg.rank.title_weight, 2.0);
        assert_eq!(cfg.rank.boost[0].field, ["resolution"]);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("[fields]\nidd = \"x\"").is_err());
    }

    #[test]
    fn plurals() {
        assert_eq!(plural("log line"), "log lines");
        assert_eq!(plural("entry"), "entries");
        assert_eq!(plural("day"), "days");
        assert_eq!(plural("logs"), "logs");
    }
}
