//! Text helpers shared by the indexer and the query layer.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}][\p{L}\p{N}_.-]*").unwrap());

/// Words that match nearly every record and only slow BM25 down.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "did", "do", "does", "for", "from",
    "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "last", "me", "my", "of", "on",
    "or", "our", "so", "that", "the", "their", "then", "there", "this", "time", "to", "was", "we", "were",
    "what", "when", "where", "which", "who", "why", "will", "with", "you",
];

/// Lowercase and collapse whitespace: the group-name comparison key.
pub fn normalize_name(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn words_in(text: &str) -> impl Iterator<Item = String> + '_ {
    TOKEN.find_iter(text).map(|m| m.as_str().trim_end_matches(['.', '-', '_']).to_lowercase())
}

/// Lowercased searchable words, stopwords removed.
pub fn query_words(text: &str) -> impl Iterator<Item = String> + '_ {
    words_in(text).filter(|t| !t.is_empty() && !STOPWORDS.contains(&t.as_str()))
}

/// A free-text query: words (ranked, any may match), `"quoted phrases"`, and
/// `-excluded` words or phrases. Everything is re-quoted for FTS5, so query
/// syntax in user input is inert.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub words: Vec<String>,
    pub phrases: Vec<String>,
    pub excluded: Vec<String>,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut q = Query::default();
        let mut seen = HashSet::new();
        let mut rest = text;
        while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
            rest = &rest[start..];
            let negate = rest.starts_with('-') && rest.len() > 1;
            let body = if negate { &rest[1..] } else { rest };
            let (chunk, phrase, next) = if let Some(inner) = body.strip_prefix('"') {
                let end = inner.find('"').unwrap_or(inner.len());
                (&inner[..end], true, &inner[(end + 1).min(inner.len())..])
            } else {
                let end = body.find(char::is_whitespace).unwrap_or(body.len());
                (&body[..end], false, &body[end..])
            };
            rest = next;
            let tokens: Vec<String> =
                if phrase { words_in(chunk).collect() } else { query_words(chunk).collect() };
            if tokens.is_empty() {
                continue;
            }
            if negate {
                q.excluded.push(tokens.join(" "));
            } else if phrase && tokens.len() > 1 {
                q.phrases.push(tokens.join(" "));
            } else {
                q.words.extend(tokens.into_iter().filter(|t| seen.insert(t.clone())));
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.phrases.is_empty()
    }

    /// Positive FTS5 expression: any word or phrase.
    pub fn fts(&self) -> Option<String> {
        let terms: Vec<String> =
            self.words.iter().chain(&self.phrases).take(64).map(|t| format!("\"{t}\"")).collect();
        (!terms.is_empty()).then(|| terms.join(" OR "))
    }

    pub fn not_fts(&self) -> Option<String> {
        let terms: Vec<String> = self.excluded.iter().take(32).map(|t| format!("\"{t}\"")).collect();
        (!terms.is_empty()).then(|| terms.join(" OR "))
    }

    /// Words to highlight in snippets.
    pub fn highlight(&self) -> Vec<String> {
        let mut out: Vec<String> = self.words.clone();
        out.extend(self.phrases.iter().flat_map(|p| p.split(' ').map(str::to_string)));
        out
    }
}

fn fnv1a(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            h ^= 0x1f;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        for b in part.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

/// Single FTS token that stands for one group inside the index.
pub fn group_token(key: &str) -> String {
    format!("g{:016x}", fnv1a(&[key]))
}

/// Single FTS token for `field = value` (case-insensitive).
pub fn facet_token(field: &str, value: &str) -> String {
    format!("f{:016x}", fnv1a(&[field, &value.trim().to_lowercase()]))
}

/// Truncate on a char boundary, marking the cut with an ellipsis.
pub fn truncate(value: &str, max_chars: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let cut: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}\u{2026}", cut.trim_end())
}

static ISO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4})[-/](\d{1,2})[-/](\d{1,2})(?:[T ](\d{1,2}):(\d{2})(?::(\d{2}))?(?:[.,]\d+)?\s*(?:Z|[+-]\d{2}:?\d{2}|UTC)?)?$").unwrap()
});
static US: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(\d{1,2})/(\d{1,2})/(\d{4}|\d{2})(?:[ T,]+(\d{1,2}):(\d{2})(?::(\d{2}))?\s*(AM|PM)?)?$")
        .unwrap()
});

/// Sortable `YYYY-MM-DD[THH:MM:SS]` from common date spellings: ISO 8601 /
/// RFC 3339 (offset ignored), `YYYY/MM/DD`, US `M/D/YYYY [h:mm[:ss] [AM|PM]]`,
/// and Unix epoch seconds or milliseconds. `None` when unrecognized.
pub fn normalize_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let num = |c: Option<regex::Match<'_>>| c.and_then(|m| m.as_str().parse::<u32>().ok());
    let fmt = |y: u32, mo: u32, d: u32, h: Option<u32>, mi: Option<u32>, sec: Option<u32>| {
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || y < 1000 {
            return None;
        }
        Some(match (h, mi) {
            (Some(h), Some(mi)) if h < 24 && mi < 60 => {
                format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{:02}", sec.unwrap_or(0).min(59))
            }
            _ => format!("{y:04}-{mo:02}-{d:02}"),
        })
    };
    if let Some(c) = ISO.captures(s) {
        return fmt(
            num(c.get(1))?,
            num(c.get(2))?,
            num(c.get(3))?,
            num(c.get(4)),
            num(c.get(5)),
            num(c.get(6)),
        );
    }
    if let Some(c) = US.captures(s) {
        let mut y = num(c.get(3))?;
        if y < 100 {
            y += if y < 70 { 2000 } else { 1900 };
        }
        let mut h = num(c.get(4));
        if let (Some(hour), Some(ampm)) = (h, c.get(7)) {
            let pm = ampm.as_str().eq_ignore_ascii_case("pm");
            h = Some(match (hour % 12, pm) {
                (x, true) => x + 12,
                (x, false) => x,
            });
        }
        return fmt(y, num(c.get(1))?, num(c.get(2))?, h, num(c.get(5)), num(c.get(6)));
    }
    if s.len() >= 9 && s.len() <= 13 && s.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = s.parse().ok()?;
        let secs = if s.len() == 13 { n / 1000 } else { n };
        return Some(format_unix(secs).trim_end_matches('Z').to_string());
    }
    None
}

/// A `--since/--until` bound: any [`normalize_date`] input, or a bare year
/// (`2024`) or month (`2024-03`).
pub fn normalize_bound(raw: &str) -> Option<String> {
    let s = raw.trim();
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    match b.len() {
        4 if digits(0..4) => Some(s.to_string()),
        7 if digits(0..4)
            && b[4] == b'-'
            && digits(5..7)
            && (1..=12).contains(&s[5..7].parse::<u32>().ok()?) =>
        {
            Some(s.to_string())
        }
        _ => normalize_date(s),
    }
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (Howard Hinnant's civil_from_days).
pub fn format_unix(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Unix seconds from `YYYY-MM-DD[THH:MM[:SS]][Z]`, the inverse of
/// [`format_unix`]. `None` when the string has another shape.
pub fn parse_unix(value: &str) -> Option<i64> {
    let s = value.trim().trim_end_matches('Z');
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if s.len() < 10 || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, sec) =
        if s.len() >= 16 { (num(11..13)?, num(14..16)?, num(17..19).unwrap_or(0)) } else { (0, 0, 0) };
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_round_trips() {
        for secs in [0, 951_782_400, 1_710_166_800, 1_791_000_000, 4_102_444_799] {
            assert_eq!(parse_unix(&format_unix(secs)), Some(secs));
        }
        assert_eq!(parse_unix("2024-03-11"), Some(1_710_115_200));
        assert_eq!(parse_unix("soon"), None);
    }

    #[test]
    fn query_parsing_handles_phrases_exclusions_and_syntax() {
        let q = Query::parse(r#"Login LOOP on app-01 "reset link" -sso -"single sign-on" OR* NEAR"#);
        assert_eq!(q.words, ["login", "loop", "app-01", "near"]);
        assert_eq!(q.phrases, ["reset link"]);
        assert_eq!(q.excluded, ["sso", "single sign-on"]);
        assert_eq!(q.fts().unwrap(), r#""login" OR "loop" OR "app-01" OR "near" OR "reset link""#);
        assert_eq!(q.not_fts().unwrap(), r#""sso" OR "single sign-on""#);
        assert!(Query::parse("the the").is_empty());
        assert!(Query::parse(r#""unterminated phrase"#).phrases.len() == 1);
    }

    #[test]
    fn tokens_are_single_alnum_and_case_insensitive_for_facets() {
        let g = group_token("web-01 east");
        assert!(g.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(facet_token("status", " Closed "), facet_token("status", "closed"));
        assert_ne!(facet_token("status", "closed"), facet_token("state", "closed"));
    }

    #[test]
    fn dates_normalize_to_sortable_strings() {
        let n = |s: &str| normalize_date(s);
        assert_eq!(n("2024-03-11T14:20:00Z").as_deref(), Some("2024-03-11T14:20:00"));
        assert_eq!(n("2024-03-11 14:20:05.123+02:00").as_deref(), Some("2024-03-11T14:20:05"));
        assert_eq!(n("2024/3/1").as_deref(), Some("2024-03-01"));
        assert_eq!(n("3/11/2024 2:05 PM").as_deref(), Some("2024-03-11T14:05:00"));
        assert_eq!(n("12/1/24 12:30 AM").as_deref(), Some("2024-12-01T00:30:00"));
        assert_eq!(n("1710166800").as_deref(), Some("2024-03-11T14:20:00"));
        assert_eq!(n("1710166800000").as_deref(), Some("2024-03-11T14:20:00"));
        assert_eq!(n("2024-13-01"), None);
        assert_eq!(n("soon"), None);
    }

    #[test]
    fn truncate_marks_cut() {
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate(" abc ", 4), "abc");
    }
}
