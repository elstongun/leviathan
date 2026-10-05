//! Field paths into JSON records, and the compiled [`Mapping`] that turns a
//! record into what the index stores.

use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::text::{facet_token, group_token, normalize_date};

/// A dotted path such as `asset.id` or `steps[].text`. Arrays anywhere on the
/// path are flattened, so `[]` is optional. A key that itself contains dots
/// (common in CSV headers) is matched literally before the path is split.
#[derive(Debug, Clone)]
pub struct FieldPath {
    pub raw: String,
    segs: Vec<String>,
    rest: Vec<String>,
}

impl FieldPath {
    pub fn parse(raw: &str) -> Self {
        let segs: Vec<String> = raw
            .split('.')
            .map(|s| s.trim().trim_end_matches("[]").to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let rest = (0..segs.len()).map(|i| segs[i..].join(".")).collect();
        Self { raw: raw.trim().to_string(), segs, rest }
    }

    /// Every value at this path, arrays flattened.
    pub fn values<'a>(&self, record: &'a Value) -> Vec<&'a Value> {
        let mut out = Vec::new();
        self.walk(record, 0, &mut out);
        out
    }

    fn walk<'a>(&self, value: &'a Value, i: usize, out: &mut Vec<&'a Value>) {
        match value {
            Value::Array(items) => items.iter().for_each(|item| self.walk(item, i, out)),
            _ if i == self.segs.len() => out.push(value),
            Value::Object(map) => {
                if self.segs.len() - i > 1
                    && let Some(child) = map.get(&self.rest[i])
                {
                    return self.walk(child, self.segs.len(), out);
                }
                if let Some(child) = map.get(&self.segs[i]) {
                    self.walk(child, i + 1, out);
                }
            }
            _ => {}
        }
    }
}

/// Scalars as text. Objects contribute their nested strings.
fn push_text(value: &Value, out: &mut Vec<String>, top: bool) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Number(n) if top => out.push(n.to_string()),
        Value::Bool(b) if top => out.push(b.to_string()),
        Value::Array(items) => items.iter().for_each(|v| push_text(v, out, top)),
        Value::Object(map) => map.values().for_each(|v| push_text(v, out, false)),
        _ => {}
    }
}

/// The config compiled for fast per-record extraction.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub config: Config,
    id: Option<FieldPath>,
    title: Vec<FieldPath>,
    text: Vec<FieldPath>,
    group: Option<FieldPath>,
    group_name: Option<FieldPath>,
    date: Vec<FieldPath>,
    pub filters: Vec<FieldPath>,
    pub display: Vec<FieldPath>,
    empty: HashSet<String>,
    boosts: Vec<(Vec<FieldPath>, Option<String>, f64)>,
}

/// What the index stores for one record.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub id: Option<String>,
    pub title: Option<String>,
    pub body: String,
    /// Group key and name, searchable in their own column.
    pub names: String,
    pub group: Option<String>,
    pub group_name: Option<String>,
    pub date: Option<String>,
    pub boost: f64,
    /// `(field, value)` pairs, value as first seen (trimmed).
    pub facets: Vec<(String, String)>,
    /// Synthetic FTS tokens for the group and each facet value.
    pub tags: String,
}

/// `text` with the record's own group key and name removed (ASCII
/// case-insensitive). They are indexed once, in their own column; repeated in
/// a title they would outrank the words that tell records apart.
fn without_names(text: &str, names: &[&str]) -> String {
    let mut out = text.to_string();
    for name in names.iter().filter(|n| n.chars().count() >= 3) {
        if out.is_ascii() && name.is_ascii() {
            let lower = out.to_ascii_lowercase();
            let needle = name.to_ascii_lowercase();
            let mut kept = String::with_capacity(out.len());
            let mut last = 0;
            for (i, _) in lower.match_indices(&needle) {
                kept.push_str(&out[last..i]);
                kept.push(' ');
                last = i + needle.len();
            }
            kept.push_str(&out[last..]);
            out = kept;
        } else {
            out = out.replace(name, " ");
        }
    }
    out
}

fn placeholder_key(s: &str) -> String {
    s.trim().trim_end_matches(['.', '!']).trim().to_lowercase()
}

impl Mapping {
    pub fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let f = &config.fields;
        let paths = |v: &[String]| v.iter().map(|p| FieldPath::parse(p)).collect::<Vec<_>>();
        Ok(Self {
            config: config.clone(),
            id: f.id.as_deref().map(FieldPath::parse),
            title: paths(&f.title),
            text: paths(&f.text),
            group: f.group.as_deref().map(FieldPath::parse),
            group_name: f.group_name.as_deref().map(FieldPath::parse),
            date: paths(&f.date),
            filters: paths(&f.filters),
            display: paths(&f.display),
            empty: f.empty_values.iter().map(|s| placeholder_key(s)).collect(),
            boosts: config
                .rank
                .boost
                .iter()
                .map(|b| (paths(&b.field), b.equals.as_ref().map(|e| e.trim().to_lowercase()), b.weight))
                .collect(),
        })
    }

    pub fn has_id(&self) -> bool {
        self.id.is_some()
    }
    pub fn has_group(&self) -> bool {
        self.group.is_some()
    }
    pub fn has_date(&self) -> bool {
        !self.date.is_empty()
    }
    pub fn title_paths(&self) -> &[FieldPath] {
        &self.title
    }
    pub fn text_paths(&self) -> &[FieldPath] {
        &self.text
    }

    /// Missing, blank, or a configured placeholder.
    pub fn is_empty_value(&self, s: &str) -> bool {
        let key = placeholder_key(s);
        key.is_empty() || self.empty.contains(&key)
    }

    /// Present text values at `path`, placeholders dropped.
    pub fn strings(&self, path: &FieldPath, record: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for v in path.values(record) {
            push_text(v, &mut out, true);
        }
        out.retain(|s| !self.is_empty_value(s));
        out
    }

    pub fn first(&self, paths: &[FieldPath], record: &Value) -> Option<String> {
        paths.iter().find_map(|p| self.strings(p, record).into_iter().next()).map(|s| s.trim().to_string())
    }

    pub fn record_id(&self, record: &Value) -> Option<String> {
        self.id.as_ref().and_then(|p| self.first(std::slice::from_ref(p), record))
    }

    /// The record's date, normalized when recognizable.
    pub fn date_of(&self, record: &Value) -> Option<String> {
        self.first(&self.date, record).map(|d| normalize_date(&d).unwrap_or(d))
    }

    pub fn group_of(&self, record: &Value) -> (Option<String>, Option<String>) {
        let one =
            |p: &Option<FieldPath>| p.as_ref().and_then(|p| self.first(std::slice::from_ref(p), record));
        let group = one(&self.group);
        let name = if group.is_some() { one(&self.group_name) } else { None };
        (group, name)
    }

    pub fn prepare(&self, record: &Value) -> Prepared {
        let (group, group_name) = self.group_of(record);
        let own: Vec<&str> = group.iter().chain(&group_name).map(String::as_str).collect();
        let names = own.join("\n");
        let title = self.first(&self.title, record).map(|t| without_names(&t, &own));
        let mut body: Vec<String> = Vec::new();
        if self.text.is_empty() {
            push_text(record, &mut body, true);
            body.retain(|s| !self.is_empty_value(s));
        } else {
            for p in &self.text {
                body.extend(self.strings(p, record));
            }
        }
        for b in &mut body {
            *b = without_names(b, &own);
        }

        let date = self.date_of(record);

        let mut facets: Vec<(String, String)> = Vec::new();
        let mut seen = HashSet::new();
        for p in &self.filters {
            for v in self.strings(p, record) {
                let v: String = v.trim().chars().take(200).collect();
                if seen.insert((p.raw.clone(), v.to_lowercase())) {
                    facets.push((p.raw.clone(), v));
                }
            }
        }

        let mut boost = 1.0;
        for (paths, equals, weight) in &self.boosts {
            let hit = paths.iter().any(|p| {
                let values = self.strings(p, record);
                match equals {
                    Some(want) => values.iter().any(|v| v.trim().to_lowercase() == *want),
                    None => !values.is_empty(),
                }
            });
            if hit {
                boost += weight;
            }
        }

        let mut tags: Vec<String> = Vec::with_capacity(facets.len() + 1);
        tags.extend(group.as_deref().map(group_token));
        tags.extend(facets.iter().map(|(f, v)| facet_token(f, v)));

        Prepared {
            id: self.record_id(record),
            title,
            body: body.join("\n"),
            names,
            group,
            group_name,
            date,
            boost,
            facets,
            tags: tags.join(" "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_flatten_arrays_and_match_dotted_keys() {
        let doc = json!({"a": {"b": [{"c": 1}, {"c": 2}]}, "x.y": "flat", "tags": ["p", "q"]});
        let vals = |p: &str| FieldPath::parse(p).values(&doc).into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(vals("a.b[].c"), [json!(1), json!(2)]);
        assert_eq!(vals("a.b.c"), [json!(1), json!(2)]);
        assert_eq!(vals("x.y"), [json!("flat")]);
        assert_eq!(vals("tags[]"), [json!("p"), json!("q")]);
        assert!(vals("a.missing").is_empty());
    }

    #[test]
    fn prepare_applies_placeholders_facets_and_boosts() {
        let cfg: Config = toml::from_str(
            r#"
            [fields]
            id = "id"
            title = ["subject", "body"]
            text = ["subject", "resolution"]
            group = "customer.id"
            group_name = "customer.name"
            date = "closed"
            filters = ["status", "tags"]
            empty_values = ["n/a", "done"]
            [[rank.boost]]
            field = "resolution"
            weight = 0.2
            [[rank.boost]]
            field = "status"
            equals = "Closed"
            weight = 0.05
            "#,
        )
        .unwrap();
        let m = Mapping::new(&cfg).unwrap();
        let p = m.prepare(&json!({
            "id": 7, "subject": "Login fails", "resolution": "Done.",
            "customer": {"id": "ACME", "name": "Acme Corp"}, "closed": "2024-03-01 10:00",
            "status": "closed", "tags": ["sso", "SSO", "web"]
        }));
        assert_eq!(p.id.as_deref(), Some("7"));
        assert_eq!(p.title.as_deref(), Some("Login fails"));
        assert_eq!(p.date.as_deref(), Some("2024-03-01T10:00:00"));
        assert_eq!(p.group.as_deref(), Some("ACME"));
        assert!(!p.body.contains("Done"));
        assert_eq!(p.facets.len(), 3);
        assert!((p.boost - 1.05).abs() < 1e-9);
        assert_eq!(p.tags.split(' ').count(), 4);
    }
}
