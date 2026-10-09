//! Validation for every write: one bounded claim, well-formed slots, and no
//! credentials. The secret check cannot be turned off; memories are meant
//! to be recalled into prompts, so a stored key would leak on every recall.

use std::sync::LazyLock;

use regex::Regex;

use super::{Kind, Remember, Settings};

/// A write refused before touching the database (exit status 2).
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refuse<T>(msg: impl Into<String>) -> Result<T, Refused> {
    Err(Refused(msg.into()))
}

static SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("private key", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY"),
        ("AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})"),
        ("API key", r"\bsk-(?:ant-|proj-|live-)?[A-Za-z0-9_-]{20,}"),
        ("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
        ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
        ("Stripe key", r"\b[rsp]k_(?:live|test)_[0-9A-Za-z]{16,}"),
        ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
        ("crates.io token", r"\bcio[A-Za-z0-9]{32}\b"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"),
        ("credentials in a URL", r"(?i)\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]{3,}@"),
        (
            "password or key assignment",
            r#"(?i)\b(?:password|passwd|pwd|passphrase|secret|api[_-]?key|access[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key|bearer)\b["']?\s*(?:[:=]|\bis\b)\s*["']?[^\s"']{6,}"#,
        ),
    ]
    .into_iter()
    .map(|(name, re)| (name, Regex::new(re).expect("secret pattern")))
    .collect()
});

static LONG_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/_=-]{32,}").unwrap());

/// What kind of credential `text` appears to contain, if any.
pub fn secret_kind(text: &str) -> Option<&'static str> {
    if let Some((name, _)) = SECRETS.iter().find(|(_, re)| re.is_match(text)) {
        return Some(name);
    }
    LONG_TOKEN.find_iter(text).any(|m| looks_random(m.as_str())).then_some("high-entropy token")
}

/// Mixed-case alphanumerics with more entropy than hex can have: hashes and
/// ids (hex, base32) pass, base64 keys do not.
fn looks_random(token: &str) -> bool {
    let has = |f: fn(&char) -> bool| token.chars().any(|c| f(&c));
    if !(has(char::is_ascii_lowercase) && has(char::is_ascii_uppercase) && has(char::is_ascii_digit)) {
        return false;
    }
    let mut counts = [0u32; 128];
    for b in token.bytes() {
        counts[usize::from(b & 127)] += 1;
    }
    let n = token.len() as f64;
    let entropy: f64 =
        counts.iter().filter(|c| **c > 0).map(|c| f64::from(*c) / n).map(|p| -p * p.log2()).sum();
    entropy > 4.2
}

static NAMESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9:_./@-]{0,63}$").unwrap());
static KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{Ll}\p{Lo}\p{N}][\p{Ll}\p{Lo}\p{N}_.:/@-]{0,79}$").unwrap());
static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{Ll}\p{Lo}\p{N}][\p{Ll}\p{Lo}\p{N}_.:/-]{0,39}$").unwrap());

pub fn namespace(raw: &str) -> Result<String, Refused> {
    let ns = raw.trim().to_lowercase();
    if !NAMESPACE.is_match(&ns) {
        return refuse(format!(
            "namespace {raw:?}: use 1-64 of a-z 0-9 : _ . / @ -, e.g. `user`, `project:billing`, `agent:reviewer`"
        ));
    }
    Ok(ns)
}

/// Slot names are lowercase; spaces become underscores.
pub fn key(raw: &str) -> Result<String, Refused> {
    let k = raw.split_whitespace().collect::<Vec<_>>().join("_").to_lowercase();
    if !KEY.is_match(&k) {
        return refuse(format!("key {raw:?}: use a short slot name like `editor` or `deploy.target`"));
    }
    Ok(k)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn check_secret(field: &str, value: &str) -> Result<(), Refused> {
    match secret_kind(value) {
        Some(kind) => refuse(format!(
            "refused: the {field} looks like a credential ({kind}). Never store secrets in memory; \
             remember where the secret lives instead (for example \"deploy key is in the team vault\")"
        )),
        None => Ok(()),
    }
}

/// A validated write.
pub(crate) struct Clean {
    pub ns: String,
    pub kind: Kind,
    pub subject: Option<String>,
    pub key: Option<String>,
    pub text: String,
    pub tags: Vec<String>,
    pub importance: u8,
    pub confidence: f64,
    pub source: Option<String>,
    pub refs: Vec<String>,
    pub pinned: bool,
    pub expires_at: Option<String>,
    pub aliases: Vec<String>,
}

pub(crate) fn check(req: &Remember, settings: &Settings, now_secs: i64) -> Result<Clean, Refused> {
    let text = one_line(&req.text);
    if text.is_empty() {
        return refuse("`text` is empty: say the one thing to remember");
    }
    let chars = text.chars().count();
    if chars > settings.max_chars {
        return refuse(format!(
            "memory is {chars} characters; the cap is {}. Store one claim per memory: split it, or keep the \
             long version in a file and remember where it is",
            settings.max_chars
        ));
    }
    check_secret("text", &text)?;

    let ns = namespace(req.ns.as_deref().unwrap_or(&settings.namespace))?;
    let subject = req.subject.as_deref().map(one_line).filter(|s| !s.is_empty());
    if let Some(s) = &subject {
        if s.chars().count() > 120 {
            return refuse(
                "subject is over 120 characters: name the person, project or thing, not a sentence",
            );
        }
        check_secret("subject", s)?;
    }
    let key = req.key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(key).transpose()?;
    if let Some(k) = &key {
        check_secret("key", k)?;
    }

    let mut tags = Vec::new();
    for raw in &req.tags {
        let t = raw.split_whitespace().collect::<Vec<_>>().join("-").to_lowercase();
        if t.is_empty() || tags.contains(&t) {
            continue;
        }
        if !TAG.is_match(&t) {
            return refuse(format!("tag {raw:?}: use short words like `infra` or `billing`"));
        }
        check_secret("tag", &t)?;
        tags.push(t);
    }
    if tags.len() > 12 {
        return refuse("at most 12 tags");
    }

    let mut refs: Vec<String> = Vec::new();
    for r in req.refs.iter().map(|r| one_line(r)).filter(|r| !r.is_empty()) {
        if r.chars().count() > 200 {
            return refuse("a ref is over 200 characters: refs are ids, paths or URLs");
        }
        check_secret("ref", &r)?;
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    if refs.len() > 20 {
        return refuse("at most 20 refs");
    }

    let source = req.source.as_deref().map(one_line).filter(|s| !s.is_empty());
    if let Some(s) = &source {
        if s.chars().count() > 120 {
            return refuse("source is over 120 characters: say who or what it came from, briefly");
        }
        check_secret("source", s)?;
    }

    let importance = req.importance.unwrap_or(3);
    if !(1..=5).contains(&importance) {
        return refuse("importance is 1 (trivia) to 5 (critical)");
    }
    let confidence = req.confidence.unwrap_or(1.0);
    if !(confidence.is_finite() && (0.0..=1.0).contains(&confidence)) {
        return refuse("confidence is between 0 and 1");
    }
    let expires_at = req
        .expires
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| expiry(e, now_secs))
        .transpose()?;

    let mut aliases = Vec::new();
    for a in req.aliases.iter().map(|a| one_line(a)).filter(|a| !a.is_empty()) {
        if subject.is_none() {
            return refuse("`aliases` need a `subject` to point at");
        }
        if a.chars().count() > 120 {
            return refuse("an alias is over 120 characters");
        }
        aliases.push(a);
    }

    Ok(Clean {
        ns,
        kind: req.kind.unwrap_or(if key.is_some() { Kind::Fact } else { Kind::Note }),
        subject,
        key,
        text,
        tags,
        importance,
        confidence,
        source,
        refs,
        pinned: req.pinned,
        expires_at,
        aliases,
    })
}

/// `2026-12-31`, an RFC 3339 time, or a duration from now: `12h`, `30d`, `6w`.
pub(crate) fn expiry(raw: &str, now_secs: i64) -> Result<String, Refused> {
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    if let (Ok(n), Some(secs)) = (
        num.parse::<i64>(),
        match unit.trim() {
            "h" => Some(3600),
            "d" => Some(86_400),
            "w" => Some(7 * 86_400),
            _ => None,
        },
    ) {
        return Ok(crate::text::format_unix(now_secs + n.min(100_000) * secs));
    }
    match crate::text::normalize_date(raw) {
        Some(d) if d.len() == 10 => Ok(format!("{d}T23:59:59Z")),
        Some(d) => Ok(format!("{d}Z")),
        None => refuse(format!("expires {raw:?}: use a date (2026-12-31) or a duration (12h, 30d, 6w)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_caught_and_ordinary_text_is_not() {
        for s in [
            "token ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5z",
            "key sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
            "AKIAIOSFODNN7EXAMPLE is the deploy key",
            "password: hunter2hunter2",
            "the db password is Tr0ub4dor&3",
            "postgres://admin:s3cretpw@db.internal:5432/app",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "x Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MEFCQ0RFRkdISUpL y",
        ] {
            assert!(secret_kind(s).is_some(), "missed: {s}");
        }
        for s in [
            "Prefers tabs over spaces and short commit messages",
            "deploys go through the staging cluster first",
            "commit 3f2a9c4e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a3f fixed the race",
            "memory m01j9zq4xk2m3n4p5 superseded the old value",
            "the password policy requires 12 characters",
            "API key rotation happens every 90 days",
        ] {
            assert_eq!(secret_kind(s), None, "false positive: {s}");
        }
    }

    #[test]
    fn expiry_accepts_dates_and_durations() {
        assert_eq!(expiry("2026-12-31", 0).unwrap(), "2026-12-31T23:59:59Z");
        assert_eq!(expiry("2d", 0).unwrap(), "1970-01-03T00:00:00Z");
        assert!(expiry("soonish", 0).is_err());
    }

    #[test]
    fn keys_and_namespaces_are_normalized() {
        assert_eq!(key(" Favorite Color ").unwrap(), "favorite_color");
        assert!(key("no;semicolons").is_err());
        assert_eq!(namespace("Project:Billing").unwrap(), "project:billing");
        assert!(namespace("*").is_err());
    }
}
