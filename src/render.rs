//! Compact text rendering. This is what agents read by default, so every
//! line is spent deliberately: scope and counts in the header, one short
//! block per record, and an explicit statement when nothing was found.

use std::fmt::Write;

use crate::card::Card;
use crate::config::plural;
use crate::index::IngestReport;
use crate::query::{Description, GroupMatch, Scope, SearchOutcome, Sort, Status};

pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn count(n: i64, noun: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { noun.to_string() } else { plural(noun) })
}

fn group_label(key: &str, name: Option<&str>) -> String {
    match name {
        Some(n) if n != key => format!("{key} \"{n}\""),
        _ => key.to_string(),
    }
}

pub fn card(out: &mut String, n: usize, c: &Card, show_group: bool, group_noun: &str) {
    let _ = write!(out, "[{n}] {}", c.id);
    if c.other_group {
        let _ = write!(out, " · OTHER {}", group_noun.to_uppercase());
    }
    if (show_group || c.other_group)
        && let Some(g) = &c.group
    {
        let _ = write!(out, " {}", group_label(g, c.group_name.as_deref()));
    }
    if let Some(d) = &c.date {
        let _ = write!(out, " · {d}");
    }
    if let Some(r) = c.relevance {
        let digits = if r.abs() < 1.0 { 2 } else { 1 };
        let _ = write!(out, " · rel {r:.digits$}");
    }
    out.push('\n');
    if let Some(t) = &c.title {
        let _ = writeln!(out, "  {t}");
    }
    let (short, long): (Vec<_>, Vec<_>) = c
        .fields
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|v| (k, v)))
        .partition(|(_, v)| v.chars().count() <= 40);
    if !short.is_empty() {
        let line: Vec<String> = short.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        let _ = writeln!(out, "  {}", line.join(" · "));
    }
    for (k, v) in long {
        let _ = writeln!(out, "  {k}: {v}");
    }
    if let Some(s) = &c.snippet {
        let _ = writeln!(out, "  match: {s}");
    }
}

fn candidates(out: &mut String, list: &[GroupMatch], record: &str) {
    for m in list {
        let _ = write!(
            out,
            "  {} · {} · {}",
            group_label(&m.key, m.name.as_deref()),
            count(m.records, record),
            m.match_type
        );
        if let Some(s) = m.similarity {
            let _ = write!(out, " {s:.2}");
        }
        out.push('\n');
    }
}

pub fn search(o: &SearchOutcome) -> String {
    let (record, group) = (o.about.record.as_str(), o.about.group.as_str());
    let mut out = String::from("leviathan search");
    match o.status {
        Status::AmbiguousGroup | Status::UnknownGroup | Status::BadRequest => {
            for note in &o.notes {
                let _ = writeln!(out, " · {note}");
            }
            if o.notes.is_empty() {
                out.push('\n');
            }
            candidates(&mut out, &o.candidates, record);
            return out;
        }
        Status::Ok => {}
    }
    match (&o.group, o.scope) {
        (Some(g), Scope::Group) => {
            let _ = write!(
                out,
                " · {group} {} ({})",
                group_label(&g.key, g.name.as_deref()),
                count(g.records, record)
            );
        }
        (Some(g), Scope::Others) => {
            let _ = write!(out, " · {} other than {}", plural(group), g.key);
        }
        _ => {}
    }
    if !o.query.trim().is_empty() {
        let _ = write!(out, " · query {:?}", o.query.trim());
    }
    for f in &o.filters {
        let _ = write!(out, " · {}={}", f.field, f.value);
    }
    if let Some(s) = &o.since {
        let _ = write!(out, " · since {s}");
    }
    if let Some(u) = &o.until {
        let _ = write!(out, " · until {u}");
    }
    if o.sort == Sort::Newest || crate::text::Query::parse(&o.query).is_empty() {
        out.push_str(" · newest first");
    }
    let first = if o.results.is_empty() { 0 } else { o.offset + 1 };
    let last = o.offset + o.results.len();
    let shown = if o.offset > 0 { format!("{first}-{last}") } else { o.results.len().to_string() };
    let _ = writeln!(
        out,
        " · shown {shown} of {} · {} indexed",
        thousands(o.total_matches),
        count(o.corpus_records, record)
    );

    let show_group = o.scope != Scope::Group;
    for (i, c) in o.results.iter().enumerate() {
        card(&mut out, o.offset + i + 1, c, show_group, group);
    }
    if o.results.is_empty() {
        let _ = writeln!(
            out,
            "no matching {} (none found, not none exist: try fewer or different words)",
            plural(record)
        );
    }
    for (i, c) in o.other_groups.iter().enumerate() {
        card(&mut out, i + 1, c, true, group);
    }
    for note in &o.notes {
        let _ = writeln!(out, "note: {note}");
    }
    if !o.results.is_empty() || !o.other_groups.is_empty() {
        let more = o.total_matches as usize > last;
        let page = if more { format!(" · --offset {last} for more") } else { String::new() };
        let _ = writeln!(out, "next: `leviathan get <id>` for a full {record}{page}");
    }
    out
}

pub fn resolve(query: &str, found: &[GroupMatch], record: &str, group: &str) -> String {
    let mut out = format!(
        "leviathan resolve · {query:?} · {}\n",
        count(found.len() as i64, &format!("{group} candidate"))
    );
    candidates(&mut out, found, record);
    if found.is_empty() {
        let _ = writeln!(out, "no {group} matches; try part of the name or ID");
    }
    out
}

pub fn ingest(r: &IngestReport) -> String {
    if !r.built {
        return format!(
            "leviathan index {} is current ({} records); sources and mapping unchanged, nothing to do (use --force to rebuild)\n",
            r.index_path.display(),
            thousands(r.record_count)
        );
    }
    let mut out = format!("leviathan index {}", r.index_path.display());
    if r.deleted > 0 {
        let _ = write!(out, " · deleted {}", thousands(r.deleted as i64));
    }
    if r.records_read > 0 || r.deleted == 0 {
        let _ = write!(
            out,
            " · read {} ({} new, {} replaced)",
            thousands(r.records_read as i64),
            thousands(r.inserted as i64),
            thousands(r.updated as i64)
        );
    }
    let _ = writeln!(
        out,
        " in {:.1}s\n  now {} records · {} groups · {:.1} MB source -> {:.1} MB index",
        r.elapsed_seconds,
        thousands(r.record_count),
        thousands(r.group_count),
        r.source_bytes as f64 / 1e6,
        r.index_bytes as f64 / 1e6,
    );
    if r.skipped_lines > 0 {
        let _ = writeln!(
            out,
            "  skipped {} unusable records (--strict to fail instead):",
            thousands(r.skipped_lines as i64)
        );
        for e in &r.skipped_examples {
            let _ = writeln!(out, "    {e}");
        }
    }
    if let Some(f) = &r.inferred_mapping {
        let _ = writeln!(out, "  mapping inferred from the data (no leviathan.toml or field flags):");
        let none = || "-".to_string();
        let _ = writeln!(
            out,
            "    id {} · title {} · group {} · date {}",
            f.id.clone().unwrap_or_else(none),
            f.title.first().cloned().unwrap_or_else(none),
            f.group.clone().unwrap_or_else(none),
            f.date.first().cloned().unwrap_or_else(none)
        );
        let text = if f.text.is_empty() { "every string".to_string() } else { f.text.join(", ") };
        let _ = writeln!(out, "    text {text}");
        if !f.filters.is_empty() {
            let _ = writeln!(out, "    filters {}", f.filters.join(", "));
        }
        let _ = writeln!(
            out,
            "  review it with `leviathan describe`; customize with `leviathan init` + `leviathan index --force`"
        );
    }
    out
}

pub fn describe(d: &Description) -> String {
    let (record, group) = (d.about.record.as_str(), d.about.group.as_str());
    let mut out = format!("leviathan index {}", d.index_path.display());
    if let Some(name) = &d.about.name {
        let _ = write!(out, " · {name}");
    }
    out.push('\n');
    if let Some(desc) = &d.about.description {
        let _ = writeln!(out, "  {desc}");
    }
    let _ = write!(out, "  {}", count(d.record_count, record));
    if d.fields.group.is_some() {
        let _ = write!(out, " · {}", count(d.group_count, group));
    }
    if let (Some(a), Some(b)) = (&d.date_min, &d.date_max) {
        let _ = write!(out, " · dates {} .. {}", &a[..a.len().min(10)], &b[..b.len().min(10)]);
    }
    let _ = writeln!(
        out,
        " · {:.1} MB · built {}",
        d.index_bytes as f64 / 1e6,
        d.built_at.as_deref().unwrap_or("?")
    );
    if let Some(u) = &d.updated_at {
        let _ = writeln!(out, "  updated {u}");
    }

    let f = &d.fields;
    let _ = writeln!(out, "fields{}:", if d.mapping_inferred { " (inferred)" } else { "" });
    let mut line = Vec::new();
    line.push(format!("id {}", f.id.as_deref().unwrap_or("<file>:<line>")));
    if !f.title.is_empty() {
        line.push(format!("title {}", f.title.join(" | ")));
    }
    if let Some(g) = &f.group {
        line.push(match &f.group_name {
            Some(n) => format!("{group} {g} (name {n})"),
            None => format!("{group} {g}"),
        });
    }
    if !f.date.is_empty() {
        line.push(format!("date {}", f.date.join(" | ")));
    }
    let _ = writeln!(out, "  {}", line.join(" · "));
    let _ = writeln!(
        out,
        "  searched: {}",
        if f.text.is_empty() { "every string".into() } else { f.text.join(", ") }
    );
    if !f.display.is_empty() {
        let _ = writeln!(out, "  shown on cards: {}", f.display.join(", "));
    }
    if !d.filters.is_empty() {
        let _ = writeln!(out, "filters (--where field=value; repeat a field for any-of):");
        for s in &d.filters {
            let top: Vec<String> = s.top.iter().map(|(v, n)| format!("{v} {}", thousands(*n))).collect();
            let more = s.distinct - s.top.len() as i64;
            let tail = if more > 0 { format!(" (+{} more)", thousands(more)) } else { String::new() };
            let _ = writeln!(out, "  {}: {}{tail}", s.field, top.join(" · "));
        }
    }
    if !d.rank.boost.is_empty() {
        let boosts: Vec<String> = d
            .rank
            .boost
            .iter()
            .map(|b| {
                let cond = match &b.equals {
                    Some(v) => format!("{}={v}", b.field.join("|")),
                    None => format!("{} present", b.field.join("|")),
                };
                format!("{cond} {:+.0}%", b.weight * 100.0)
            })
            .collect();
        let _ = writeln!(out, "boosts: {}", boosts.join(" · "));
    }
    if !d.largest_groups.is_empty() {
        let top: Vec<String> = d
            .largest_groups
            .iter()
            .map(|g| format!("{} {}", group_label(&g.key, g.name.as_deref()), thousands(g.records)))
            .collect();
        let _ = writeln!(out, "largest {}: {}", plural(group), top.join(" · "));
    }
    let _ = writeln!(out, "try:");
    if f.group.is_some() {
        let _ = writeln!(out, "  leviathan search -g <{group}> \"<words>\"     # ranked, within one {group}");
        let _ = writeln!(
            out,
            "  leviathan recent -g <{group}>               # newest {} for a {group}",
            plural(record)
        );
    }
    let _ = writeln!(out, "  leviathan search \"<words>\"                  # ranked, everything");
    if let Some(first) =
        d.filters.first().and_then(|s| s.top.first().map(|(v, _)| (s.field.clone(), v.clone())))
    {
        let _ = writeln!(out, "  leviathan search \"<words>\" --where {}={}", first.0, first.1);
    }
    if !f.date.is_empty() {
        let _ = writeln!(out, "  leviathan search \"<words>\" --since 2024-01 --until 2024-06");
    }
    if let Some(id) = &d.example_id {
        let _ = writeln!(out, "  leviathan get {id}");
    }
    if d.skipped_lines > 0 {
        let _ = writeln!(
            out,
            "note: {} source records were unusable and skipped at build time",
            thousands(d.skipped_lines)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands_separators() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1_000_000), "1,000,000");
        assert_eq!(super::thousands(-12_345), "-12,345");
    }
}
