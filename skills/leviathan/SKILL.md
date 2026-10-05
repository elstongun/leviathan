---
name: leviathan
description: Search a large indexed dataset (tickets, logs, incidents, maintenance records, orders, notes, any table or export) and get a few ranked, cited records instead of reading raw data. Use when the user asks about past records ("has this happened before", "what fixed it", "what's been going on with <customer/host/machine>", "find tickets about ..."), or when data is too large to read or grep. Also use to index a new dataset for search.
---

# Leviathan: search large datasets without reading them

The `leviathan` CLI answers questions from an indexed copy of a dataset. One
call returns a handful of ranked records, usually under 1,000 tokens,
however large the data is. Prefer it to grepping, `cat`, or paging through
exports: those cost tokens in proportion to the data.

## First: learn the dataset

```bash
leviathan describe
```

This shows what one record and one group are called (ticket/customer,
event/host, ...), the fields that are searched, the **filter fields with their
common values**, the date range, and example calls. Run it once per session
before searching, and use its exact field names in `--where`.

## Search

```bash
leviathan search "<words as the user said them>"                      # ranked, everything
leviathan search -g "<group as the user said it>" "<words>"           # within one customer/host/machine
leviathan search "<words>" --where status=open --where priority=high  # exact filters (same field twice = either)
leviathan search "<words>" --since 2024-01 --until 2024-06            # date range, inclusive
leviathan search '"exact phrase" -excluded'                           # quote the whole query for -exclusions
leviathan search -g acme "<words>" --scope others                     # same problem, every other group
leviathan recent -g "<group>" -n 10                                   # newest first, no words needed
leviathan resolve "<group name>"                                      # which groups match a name
leviathan get <id> [<id> ...]                                         # complete records
```

Add `--json` for structured output, `-n` for more results (max 50), and
`--offset` to page. The index comes from `--index` or `LEVIATHAN_INDEX`
(default `./leviathan.db`).

## Reading the output

- The header states the scope and `shown N of M`. Zero means *none found*,
  not *none exist*: retry with fewer or different words, or drop a filter.
- Each result has an id, its group, a date, the configured display fields,
  and a `match:` line from the text that matched.
- Results marked `OTHER <GROUP>` come from a different customer, host or
  machine than the one asked about. Say so, and keep each result's group
  visible.

## Exit status

`0` ok (including zero hits) · `1` error · `2` bad request (unknown filter
field, bad date) · `3` group unknown or **ambiguous**. On `3`, show the listed
candidates and ask which one the user means. **Never guess the group.**

## No index yet?

```bash
leviathan init <files|dir|db.sqlite> -o leviathan.toml   # proposes a field mapping, with reasons
# review and edit leviathan.toml (id, title, text, group, date, filters)
leviathan index                                          # reads source.paths from leviathan.toml
leviathan describe
```

If you already know the schema, write `leviathan.toml` directly; see
docs/CONFIG.md in the Leviathan repository. Databases are indexed from their
CLI exports through stdin, for example
`psql -At -c "SELECT row_to_json(t) FROM t" | leviathan index - -c leviathan.toml`.
