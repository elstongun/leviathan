//! `leviathan init`: profile a sample of the data and propose a mapping.
//!
//! The heuristics are deliberately conservative: a group or filter is only
//! proposed when the evidence is clear, and the generated file lists every
//! other field with its statistics so a person (or an agent) can adjust it.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::config::{About, Config, Fields, SourceConfig};
use crate::source::{self, Item, Source};
use crate::text::normalize_date;

const DISTINCT_CAP: usize = 5_000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct FieldStats {
    pub path: String,
    /// Records where the field has at least one non-empty value.
    pub present: usize,
    pub values: usize,
    pub strings: usize,
    pub numbers: usize,
    pub bools: usize,
    pub in_array: bool,
    pub distinct: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub distinct_capped: bool,
    pub avg_chars: f64,
    pub avg_words: f64,
    pub max_chars: usize,
    /// Share of values that parse as dates.
    pub date_share: f64,
    #[serde(skip)]
    seen: HashSet<String>,
    #[serde(skip)]
    chars: usize,
    #[serde(skip)]
    words: usize,
    #[serde(skip)]
    dates: usize,
    #[serde(skip)]
    order: usize,
}

#[derive(Debug, Serialize)]
pub struct Profile {
    pub sampled: usize,
    pub skipped: usize,
    pub fields: Vec<FieldStats>,
}

impl Profile {
    fn get(&self, path: &str) -> Option<&FieldStats> {
        self.fields.iter().find(|f| f.path == path)
    }
}

/// Read up to `limit` records across `sources` and profile every path.
pub fn profile(sources: &[Source], sql: Option<&str>, limit: usize) -> Result<Profile> {
    let mut stats: HashMap<String, FieldStats> = HashMap::new();
    let (mut sampled, mut skipped) = (0usize, 0usize);
    for s in sources {
        if sampled >= limit {
            break;
        }
        source::read(s, sql, &mut |item| {
            match item {
                Item::Record { value, .. } => {
                    let mut present = HashSet::new();
                    walk(&value, String::new(), false, &mut stats, &mut present);
                    for p in present {
                        if let Some(f) = stats.get_mut(&p) {
                            f.present += 1;
                        }
                    }
                    sampled += 1;
                }
                Item::Bad { .. } => skipped += 1,
            }
            Ok(sampled < limit)
        })?;
    }
    if sampled == 0 {
        bail!("no readable records in the sources ({skipped} unusable)");
    }
    let mut fields: Vec<FieldStats> = stats
        .into_values()
        .map(|mut f| {
            f.distinct = f.seen.len();
            let n = f.values.max(1) as f64;
            f.avg_chars = (f.chars as f64 / n * 10.0).round() / 10.0;
            f.avg_words = (f.words as f64 / n * 10.0).round() / 10.0;
            f.date_share = (f.dates as f64 / n * 100.0).round() / 100.0;
            f
        })
        .collect();
    fields.sort_by_key(|f| f.order);
    Ok(Profile { sampled, skipped, fields })
}

fn walk(
    value: &Value,
    path: String,
    in_array: bool,
    stats: &mut HashMap<String, FieldStats>,
    present: &mut HashSet<String>,
) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let child = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                walk(v, child, in_array, stats, present);
            }
        }
        Value::Array(items) => {
            let child = if path.is_empty() { "[]".to_string() } else { format!("{path}[]") };
            for item in items {
                walk(item, child.clone(), true, stats, present);
            }
        }
        Value::Null => {}
        scalar => {
            let text = match scalar {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            };
            if text.is_empty() {
                return;
            }
            let order = stats.len();
            let f = stats.entry(path.clone()).or_insert_with(|| FieldStats {
                path: path.clone(),
                order,
                ..Default::default()
            });
            f.values += 1;
            f.in_array |= in_array;
            match scalar {
                Value::String(_) => f.strings += 1,
                Value::Number(_) => f.numbers += 1,
                _ => f.bools += 1,
            }
            let chars = text.chars().count();
            f.chars += chars;
            f.max_chars = f.max_chars.max(chars);
            f.words += text.split_whitespace().count();
            if (matches!(scalar, Value::String(_)) || text.len() >= 9) && normalize_date(&text).is_some() {
                f.dates += 1;
            }
            if f.seen.len() < DISTINCT_CAP {
                f.seen.insert(text);
            } else if !f.seen.contains(&text) {
                f.distinct_capped = true;
            }
            present.insert(path);
        }
    }
}

/// Lowercase words of a field name: `Ticket ID`, `ticket_id` and `ticketId`
/// all give `["ticket", "id"]`.
fn name_words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in name.split(|c: char| !c.is_alphanumeric()).filter(|p| !p.is_empty()) {
        let mut word = String::new();
        let mut prev_lower = false;
        for c in part.chars() {
            if c.is_uppercase() && prev_lower {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = c.is_lowercase() || c.is_numeric();
            word.extend(c.to_lowercase());
        }
        out.push(word);
    }
    out
}

fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches("[]");
    match path.rsplit_once('.') {
        Some((parent, leaf)) => (parent, leaf.trim_end_matches("[]")),
        None => ("", path),
    }
}

fn name_score(path: &str, hints: &[&str]) -> usize {
    let (parent, leaf) = split_path(path);
    let name = leaf.to_lowercase();
    let words = name_words(leaf);
    let parent = name_words(split_path(parent).1);
    hints
        .iter()
        .position(|h| name == *h || words.iter().any(|w| w == h) || (parent.len() == 1 && parent[0] == *h))
        .map(|i| hints.len() - i)
        .unwrap_or(0)
}

/// Words of a group field's own name without a trailing id word.
fn base_words(leaf: &str) -> Vec<String> {
    let mut w = name_words(leaf);
    if w.len() > 1 && w.last().is_some_and(|l| ID_HINTS.contains(&l.as_str())) {
        w.pop();
    }
    w
}

const ID_HINTS: &[&str] = &["id", "uuid", "guid", "key", "pk", "number", "ref"];
const DATE_HINTS: &[&str] = &[
    "date",
    "timestamp",
    "time",
    "created",
    "opened",
    "reported",
    "started",
    "occurred",
    "updated",
    "modified",
    "closed",
    "resolved",
    "completed",
    "finished",
    "at",
];
const TITLE_HINTS: &[&str] = &[
    "title",
    "subject",
    "summary",
    "headline",
    "name",
    "problem",
    "issue",
    "reason",
    "message",
    "description",
];
const GROUP_HINTS: &[&str] = &[
    "asset",
    "machine",
    "equipment",
    "device",
    "host",
    "hostname",
    "server",
    "node",
    "service",
    "app",
    "application",
    "customer",
    "account",
    "client",
    "tenant",
    "user",
    "project",
    "repo",
    "repository",
    "team",
    "site",
    "store",
    "location",
    "product",
    "sku",
    "vehicle",
    "patient",
    "component",
];
const FILTER_HINTS: &[&str] = &[
    "status",
    "state",
    "type",
    "kind",
    "category",
    "priority",
    "severity",
    "level",
    "tags",
    "tag",
    "labels",
    "label",
    "stage",
    "channel",
    "source",
    "region",
    "env",
    "environment",
    "crew",
    "team",
    "shop",
];

/// A proposed mapping plus one line of reasoning per chosen field.
#[derive(Debug, Serialize)]
pub struct Proposal {
    pub config: Config,
    pub reasons: Vec<(String, String)>,
    pub profile: Profile,
}

pub fn propose(profile: Profile, sources: &[Source]) -> Proposal {
    let n = profile.sampled;
    let share = |f: &FieldStats| f.present as f64 / n as f64;
    let mut reasons: Vec<(String, String)> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();

    let id = profile
        .fields
        .iter()
        .filter(|f| {
            !f.in_array && f.bools == 0 && share(f) >= 0.99 && f.distinct == f.values && !f.distinct_capped
        })
        .filter(|f| f.avg_chars <= 64.0 && f.date_share < 0.5)
        .max_by_key(|f| (name_score(&f.path, ID_HINTS), usize::MAX - f.path.len()))
        .filter(|f| name_score(&f.path, ID_HINTS) > 0 || f.path.split('.').count() == 1)
        .map(|f| {
            reasons.push(("id".into(), format!("unique in the sample ({} of {n})", f.values)));
            f.path.clone()
        });
    if let Some(p) = &id {
        taken.insert(p.clone());
    }

    let dates: Vec<String> = {
        let mut c: Vec<&FieldStats> = profile
            .fields
            .iter()
            .filter(|f| !f.in_array && f.date_share >= 0.9 && share(f) >= 0.3 && !taken.contains(&f.path))
            .collect();
        c.sort_by_key(|f| std::cmp::Reverse((share(f) >= 0.95, name_score(&f.path, DATE_HINTS), f.present)));
        c.iter().take(2).map(|f| f.path.clone()).collect()
    };
    if let Some(first) = dates.first() {
        let f = profile.get(first).expect("profiled");
        reasons.push(("date".into(), format!("{:.0}% of values parse as dates", f.date_share * 100.0)));
    }
    taken.extend(dates.iter().cloned());

    let group = profile
        .fields
        .iter()
        .filter(|f| !f.in_array && f.bools == 0 && share(f) >= 0.5 && !taken.contains(&f.path))
        .filter(|f| f.distinct >= 2 && !f.distinct_capped && (f.distinct as f64) <= (n as f64 / 4.0).max(2.0))
        .filter(|f| f.avg_chars <= 64.0 && f.date_share < 0.5)
        .filter(|f| name_score(&f.path, GROUP_HINTS) > 0 || group_name_for(&profile, &f.path).is_some())
        .max_by_key(|f| {
            (name_score(&f.path, GROUP_HINTS), group_name_for(&profile, &f.path).is_some(), f.present)
        })
        .map(|f| f.path.clone());
    let group_name = group.as_deref().and_then(|g| group_name_for(&profile, g));
    if let Some(g) = &group {
        let f = profile.get(g).expect("profiled");
        reasons.push(("group".into(), format!("{} distinct values in {n} records", f.distinct)));
        taken.insert(g.clone());
    }
    if let Some(gn) = &group_name {
        taken.insert(gn.clone());
    }

    let title = profile
        .fields
        .iter()
        .filter(|f| !f.in_array && f.strings > 0 && share(f) >= 0.5 && !taken.contains(&f.path))
        .filter(|f| (8.0..=240.0).contains(&f.avg_chars) && f.avg_words >= 2.0 && f.distinct * 3 >= f.values)
        .max_by_key(|f| {
            (name_score(&f.path, TITLE_HINTS), f.present, (1000.0 - (f.avg_chars - 60.0).abs()) as i64)
        })
        .map(|f| f.path.clone());
    if let Some(t) = &title {
        let f = profile.get(t).expect("profiled");
        reasons.push(("title".into(), format!("short text, avg {:.0} chars", f.avg_chars)));
        taken.insert(t.clone());
    }

    let mut filters: Vec<&FieldStats> = profile
        .fields
        .iter()
        .filter(|f| !taken.contains(&f.path) && share(f) >= 0.2)
        .filter(|f| f.distinct >= 2 && f.distinct <= 50 && f.distinct < f.values && !f.distinct_capped)
        .filter(|f| f.avg_chars <= 40.0)
        .filter(|f| f.date_share < 0.5 && (f.strings > 0 || f.bools > 0))
        .collect();
    filters.sort_by_key(|f| std::cmp::Reverse((name_score(&f.path, FILTER_HINTS), f.present)));
    let filters: Vec<String> = filters.iter().take(8).map(|f| f.path.clone()).collect();
    if !filters.is_empty() {
        reasons.push(("filters".into(), "few distinct values (2 to 50)".into()));
    }
    taken.extend(filters.iter().cloned());

    let text: Vec<String> = profile
        .fields
        .iter()
        .filter(|f| !taken.contains(&f.path) && f.strings > 0 && share(f) >= 0.02)
        .filter(|f| f.avg_chars >= 20.0 || f.avg_words >= 3.0)
        .map(|f| f.path.clone())
        .collect();
    if !text.is_empty() {
        reasons.push(("text".into(), "free text: avg 20+ chars or 3+ words".into()));
    }

    let record = sources
        .first()
        .and_then(|s| Path::new(&s.label()).file_stem().map(|n| n.to_string_lossy().into_owned()))
        .map(|stem| noun_from_stem(&stem))
        .unwrap_or_else(|| "record".into());
    let group_noun = group.as_deref().map(noun_from_path).unwrap_or_else(|| "group".into());

    let config = Config {
        about: About { name: None, description: None, record, group: group_noun },
        source: SourceConfig {
            paths: sources.iter().map(|s| s.path.display().to_string()).collect(),
            ..Default::default()
        },
        fields: Fields {
            id,
            title: title.into_iter().collect(),
            text,
            group,
            group_name,
            date: dates,
            display: filters.iter().take(3).cloned().collect(),
            filters,
            empty_values: Vec::new(),
        },
        rank: Default::default(),
        memory: Default::default(),
    };
    Proposal { config, reasons, profile }
}

/// A sibling that names the group: `customer.id` -> `customer.name`,
/// `customer_id` -> `customer_name`, `Customer` -> `Customer Name`.
fn group_name_for(profile: &Profile, group: &str) -> Option<String> {
    const NAME_WORDS: &[&str] = &["name", "title", "label", "display"];
    let (parent, leaf) = split_path(group);
    let base = base_words(leaf);
    let id_only = base.len() == 1 && ID_HINTS.contains(&base[0].as_str());
    profile
        .fields
        .iter()
        .filter(|f| f.path != group && !f.in_array && f.strings > 0 && split_path(&f.path).0 == parent)
        .filter_map(|f| {
            let w = name_words(split_path(&f.path).1);
            let rest = if id_only && !parent.is_empty() {
                w.as_slice()
            } else if w.len() > base.len() && w.starts_with(&base) {
                &w[base.len()..]
            } else {
                return None;
            };
            let rank = NAME_WORDS.iter().position(|n| rest.first().is_some_and(|r| r == n))?;
            (rest.len() <= 2).then_some((rank, f.path.clone()))
        })
        .min()
        .map(|(_, p)| p)
}

fn noun_from_stem(stem: &str) -> String {
    let words = name_words(stem).join(" ");
    if words.chars().count() < 3
        || ["data", "export", "dump", "records", "rows", "items", "stdin", "db", "database"]
            .contains(&words.as_str())
    {
        return "record".into();
    }
    match words.strip_suffix("ies") {
        Some(s) => format!("{s}y"),
        None => words.strip_suffix('s').unwrap_or(&words).to_string(),
    }
}

fn noun_from_path(path: &str) -> String {
    let (parent, leaf) = split_path(path);
    let mut words = base_words(leaf);
    if words.len() == 1 && ID_HINTS.contains(&words[0].as_str()) && !parent.is_empty() {
        words = base_words(split_path(parent).1);
    }
    if words.is_empty() { "group".into() } else { words.join(" ") }
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| q(s)).collect::<Vec<_>>().join(", "))
}

/// The proposal as a commented `leviathan.toml`.
pub fn to_toml(p: &Proposal) -> String {
    let c = &p.config;
    let f = &c.fields;
    let why = |key: &str| {
        p.reasons.iter().find(|(k, _)| k == key).map(|(_, r)| format!("  # {r}")).unwrap_or_default()
    };
    let opt = |key: &str, v: &Option<String>| match v {
        Some(v) => format!("{key} = {}{}\n", q(v), why(key)),
        None => format!("# {key} = \"\"\n"),
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# leviathan.toml, proposed by `leviathan init` from {} sampled records.\n\
         # Review it, then run `leviathan index`. Every field is a path into a record:\n\
         # `a.b` for nesting, `items[].name` for every element of an array.\n",
        p.profile.sampled
    );
    let _ = writeln!(out, "[about]");
    let _ = writeln!(out, "# name = \"what this dataset is\"");
    let _ = writeln!(out, "record = {}   # noun for one record, used in output", q(&c.about.record));
    let _ = writeln!(out, "group = {}    # noun for one group\n", q(&c.about.group));
    let _ = writeln!(out, "[source]");
    let _ = writeln!(out, "paths = {}", list(&c.source.paths));
    let _ = writeln!(out, "# format = \"auto\"   # auto | jsonl | json | csv | tsv | sqlite");
    let _ = writeln!(out, "# sql = \"SELECT * FROM table\"   # SQLite sources\n");
    let _ = writeln!(out, "[fields]");
    out.push_str(&opt("id", &f.id));
    if f.id.is_none() {
        let _ = writeln!(
            out,
            "# (no unique field found: records are numbered <file>:<line>, and upsert is disabled)"
        );
    }
    out.push_str(&match f.title.first() {
        Some(t) => format!("title = {}{}\n", q(t), why("title")),
        None => "# title = \"\"\n".into(),
    });
    let _ = writeln!(
        out,
        "text = {}{}",
        list(&f.text),
        if f.text.is_empty() { "  # empty = every string".to_string() } else { why("text") }
    );
    out.push_str(&opt("group", &f.group));
    out.push_str(&opt("group_name", &f.group_name));
    let _ = writeln!(out, "date = {}{}", list(&f.date), why("date"));
    let _ = writeln!(out, "filters = {}{}", list(&f.filters), why("filters"));
    let _ = writeln!(out, "display = {}   # shown on every result card", list(&f.display));
    let _ = writeln!(out, "empty_values = []   # placeholders to ignore, e.g. [\"n/a\", \"-\", \"done\"]\n");
    let _ = writeln!(out, "[rank]");
    let _ = writeln!(out, "title_weight = 2.0");
    let _ = writeln!(out, "# Favor records that are filled in or in a given state:");
    let _ = writeln!(out, "# [[rank.boost]]\n# field = \"resolution\"\n# weight = 0.15");
    let _ = writeln!(out, "# [[rank.boost]]\n# field = \"status\"\n# equals = \"closed\"\n# weight = 0.03\n");
    let _ = writeln!(out, "# Fields seen in the sample (path: type, present, distinct, avg chars):");
    for s in p.profile.fields.iter().take(60) {
        let kind = match (s.strings > 0, s.numbers > 0, s.bools > 0) {
            (true, _, _) if s.date_share >= 0.9 => "date",
            (true, _, _) => "text",
            (_, true, _) => "number",
            _ => "bool",
        };
        let distinct = if s.distinct_capped { format!("{}+", s.distinct) } else { s.distinct.to_string() };
        let _ = writeln!(
            out,
            "#   {}: {kind}, {:.0}%, {distinct} distinct, {:.0}",
            s.path,
            s.present as f64 / p.profile.sampled as f64 * 100.0,
            s.avg_chars
        );
    }
    if p.profile.fields.len() > 60 {
        let _ = writeln!(out, "#   ... {} more", p.profile.fields.len() - 60);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn proposes_a_sensible_mapping_for_tickets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tickets.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let statuses = ["open", "closed", "pending"];
        for i in 0..300 {
            let customer = i % 12;
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "ticket_id": format!("T-{i}"),
                    "subject": format!("Cannot log in to portal number {i}"),
                    "body": format!("Customer reports that after the update the login page loops back, attempt {i}."),
                    "customer": {"id": format!("C{customer}"), "name": format!("Customer {customer}")},
                    "status": statuses[i % 3],
                    "created_at": format!("2024-03-{:02}T10:00:00Z", i % 28 + 1),
                    "tags": ["web", if i % 2 == 0 { "sso" } else { "billing" }],
                })
            )
            .unwrap();
        }
        drop(f);
        let sources = source::discover(&[path], crate::config::Format::Auto).unwrap();
        let p = propose(profile(&sources, None, 1000).unwrap(), &sources);
        let fields = &p.config.fields;
        assert_eq!(fields.id.as_deref(), Some("ticket_id"));
        assert_eq!(fields.title, ["subject"]);
        assert_eq!(fields.group.as_deref(), Some("customer.id"));
        assert_eq!(fields.group_name.as_deref(), Some("customer.name"));
        assert_eq!(fields.date, ["created_at"]);
        assert!(fields.filters.contains(&"status".to_string()));
        assert!(fields.filters.contains(&"tags[]".to_string()));
        assert_eq!(fields.text, ["body"]);
        assert_eq!(p.config.about.record, "ticket");
        assert_eq!(p.config.about.group, "customer");
        let toml_text = to_toml(&p);
        let parsed: Config = toml::from_str(&toml_text).unwrap();
        assert_eq!(parsed.fields, *fields);
    }

    #[test]
    fn spreadsheet_headers_are_matched_by_words() {
        assert_eq!(name_words("Ticket ID"), ["ticket", "id"]);
        assert_eq!(name_words("customerName"), ["customer", "name"]);
        assert_eq!(noun_from_path("Customer ID"), "customer");
        assert_eq!(noun_from_path("asset.id"), "asset");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orders.csv");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "Order ID,Customer ID,Customer Name,Status,Order Date,Notes").unwrap();
        for i in 0..200 {
            let c = i % 9;
            writeln!(
                f,
                "O{i},C{c},Customer {c},{},3/{}/2024,Package arrived late and the box was damaged {i}",
                ["new", "shipped"][i % 2],
                i % 28 + 1
            )
            .unwrap();
        }
        drop(f);
        let sources = source::discover(&[path], crate::config::Format::Auto).unwrap();
        let p = propose(profile(&sources, None, 1000).unwrap(), &sources);
        let fields = &p.config.fields;
        assert_eq!(fields.id.as_deref(), Some("Order ID"));
        assert_eq!(fields.group.as_deref(), Some("Customer ID"));
        assert_eq!(fields.group_name.as_deref(), Some("Customer Name"));
        assert_eq!(fields.date, ["Order Date"]);
        assert_eq!(fields.filters, ["Status"]);
        assert_eq!((p.config.about.record.as_str(), p.config.about.group.as_str()), ("order", "customer"));
    }
}
