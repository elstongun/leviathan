//! Result cards: the compact, citable view of one record.
//!
//! A card carries the record's id, group, date and title, the configured
//! display fields (capped), and a snippet around the query words taken from
//! the searched text that is not already shown. Everything else stays in the
//! index; `get` returns the full record.

use std::collections::HashSet;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::fields::{FieldPath, Mapping};
use crate::text::truncate;

#[derive(Debug, Clone, Copy)]
pub struct CardOptions {
    /// Character cap per field value and snippet.
    pub max_chars: usize,
}

impl Default for CardOptions {
    fn default() -> Self {
        Self { max_chars: 300 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevance: Option<f64>,
    /// From a different group than the one asked about.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub other_group: bool,
}

impl Card {
    pub fn build(
        id: &str,
        doc: &str,
        mapping: &Mapping,
        opts: CardOptions,
        highlight: &[String],
    ) -> Result<Self> {
        let record: Value = serde_json::from_str(doc)?;
        let cap = opts.max_chars.max(40);
        let title =
            mapping.first(mapping.title_paths(), &record).map(|t| truncate(&one_line(&t), cap.min(240)));
        let (group, group_name) = mapping.group_of(&record);
        let date = mapping.date_of(&record).map(|d| display_date(&d));

        let mut fields = Map::new();
        let mut shown: Vec<String> = title.iter().cloned().collect();
        for path in &mapping.display {
            let mut seen = HashSet::new();
            let values: Vec<String> = mapping
                .strings(path, &record)
                .into_iter()
                .map(|v| one_line(&v))
                .filter(|v| seen.insert(v.clone()))
                .collect();
            if values.is_empty() {
                continue;
            }
            let joined = values.join("; ");
            if title.as_deref() == Some(joined.as_str()) {
                continue;
            }
            shown.push(joined.clone());
            fields.insert(label(path), Value::String(truncate(&joined, cap)));
        }

        let snippet =
            if highlight.is_empty() { None } else { snippet(&record, mapping, highlight, &shown, cap) };
        Ok(Card {
            id: id.to_string(),
            title,
            group,
            group_name,
            date,
            fields,
            snippet,
            rank: None,
            relevance: None,
            other_group: false,
        })
    }
}

/// `2024-03-11T14:20:00` as `2024-03-11 14:20`; midnight as the bare date.
fn display_date(d: &str) -> String {
    let d = d.strip_suffix(":00").filter(|s| s.len() == 16).unwrap_or(d);
    let d = d.strip_suffix("T00:00").unwrap_or(d);
    d.replacen('T', " ", 1)
}

fn label(path: &FieldPath) -> String {
    path.raw.replace("[]", "")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn stem(word: &str) -> &str {
    let n = word.chars().count();
    let keep = if n <= 4 { n } else { (n - 2).max(4) };
    let end = word.char_indices().nth(keep).map(|(i, _)| i).unwrap_or(word.len());
    &word[..end]
}

/// Alphanumeric runs with their starting char offset.
fn words(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<(usize, usize)> = None;
    for (ci, (bi, c)) in s.char_indices().enumerate() {
        if c.is_alphanumeric() {
            start.get_or_insert((ci, bi));
        } else if let Some((sc, sb)) = start.take() {
            out.push((sc, &s[sb..bi]));
        }
    }
    if let Some((sc, sb)) = start {
        out.push((sc, &s[sb..]));
    }
    out
}

/// Sentence-sized pieces, split after `.!?;` only when whitespace follows, so
/// versions, decimals and hostnames stay whole.
fn sentences(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?' | ';') && chars.peek().is_some_and(|(_, n)| n.is_whitespace()) {
            out.push(s[start..=i].trim());
            start = i + 1;
        }
    }
    out.push(s[start..].trim());
    out.retain(|p| !p.is_empty());
    out
}

/// Best sentence-sized segment of the searched text by distinct query words
/// matched, trimmed to `cap` around the first match.
fn snippet(
    record: &Value,
    mapping: &Mapping,
    highlight: &[String],
    shown: &[String],
    cap: usize,
) -> Option<String> {
    let stems: Vec<&str> = highlight.iter().map(|w| stem(w)).collect();
    let mut texts: Vec<String> = Vec::new();
    if mapping.text_paths().is_empty() {
        texts.extend(mapping.strings(&FieldPath::parse(""), record));
    } else {
        for p in mapping.text_paths() {
            texts.extend(mapping.strings(p, record));
        }
    }
    let mut best: Option<(usize, String, usize)> = None;
    for text in &texts {
        let flat = one_line(text);
        if shown.iter().any(|s| s.contains(&flat)) {
            continue;
        }
        for seg in sentences(&flat) {
            if seg.is_empty() || shown.iter().any(|s| s.contains(seg)) {
                continue;
            }
            let lower = seg.to_lowercase();
            let mut hits = HashSet::new();
            let mut first = None;
            for (pos, word) in words(&lower) {
                if let Some(k) = stems.iter().position(|s| word.starts_with(s)) {
                    hits.insert(k);
                    first.get_or_insert(pos);
                }
            }
            if !hits.is_empty() && best.as_ref().is_none_or(|(n, _, _)| hits.len() > *n) {
                best = Some((hits.len(), seg.to_string(), first.unwrap_or(0)));
            }
        }
    }
    let (_, seg, first) = best?;
    if seg.chars().count() <= cap {
        return Some(seg);
    }
    let lead = first.saturating_sub(40);
    let tail: String = seg.chars().skip(lead).collect();
    let cut = truncate(&tail, cap);
    Some(if lead > 0 { format!("\u{2026}{cut}") } else { cut })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn mapping() -> Mapping {
        let cfg: Config = toml::from_str(
            r#"
            [fields]
            id = "id"
            title = "subject"
            text = ["subject", "body", "resolution"]
            group = "customer"
            date = "closed"
            display = ["status", "resolution", "tags[]"]
            empty_values = ["n/a"]
            "#,
        )
        .unwrap();
        Mapping::new(&cfg).unwrap()
    }

    #[test]
    fn card_shows_display_fields_and_a_fresh_snippet() {
        let doc = r#"{"id":"9","subject":"Login fails","customer":"acme","closed":"2024-03-01T00:00:00Z",
            "status":"closed","resolution":"n/a","tags":["sso","sso","web"],
            "body":"User reports a blank page. After the SSO certificate rotated, login loops forever. Escalated."}"#;
        let c = Card::build(
            "9",
            doc,
            &mapping(),
            CardOptions::default(),
            &["certificate".into(), "loops".into()],
        )
        .unwrap();
        assert_eq!(c.title.as_deref(), Some("Login fails"));
        assert_eq!(c.date.as_deref(), Some("2024-03-01"));
        assert_eq!(c.fields.get("tags").and_then(Value::as_str), Some("sso; web"));
        assert!(!c.fields.contains_key("resolution"));
        assert_eq!(c.snippet.as_deref(), Some("After the SSO certificate rotated, login loops forever."));
    }

    #[test]
    fn no_snippet_when_the_match_is_already_shown() {
        let doc = r#"{"id":"1","subject":"Printer jam","body":"Printer jam"}"#;
        let c = Card::build("1", doc, &mapping(), CardOptions::default(), &["printer".into()]).unwrap();
        assert!(c.snippet.is_none());
    }
}
