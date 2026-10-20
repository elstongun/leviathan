# GitHub issues exported with `gh`

This example indexes the JSON array produced by
[`gh issue list --json`](https://cli.github.com/manual/gh_issue_list).
The committed issues are entirely synthetic. The mapping uses the issue URL
as its stable ID, nested label names as facets, and milestone titles as groups.
Issues without a milestone remain available in unscoped searches.

From this directory, export issues from a repository you are authorized to read:

```sh
gh issue list --repo OWNER/REPO --state all --limit 1000 \
  --json url,number,title,body,state,createdAt,updatedAt,author,milestone,labels \
  > issues.json
```

`--limit 1000` is a bound, not a guarantee of a complete repository export;
raise it if needed. This export searches titles, bodies and label names; it
does not request issue comments or attachment contents. GitHub CLI handles
its own authentication; Leviathan only reads the saved JSON.

From the Leviathan repository root, try the synthetic fixture offline:

```sh
leviathan init examples/github-issues/issues.json -o -
leviathan --index /tmp/github-issues.db index --config examples/github-issues/leviathan.toml
leviathan --index /tmp/github-issues.db search -g "Release 1.0" "acknowledged chunk"
leviathan --index /tmp/github-issues.db search bug --where state=OPEN --where 'labels[].name=bug'
leviathan --index /tmp/github-issues.db search "offline cache"
leviathan --index /tmp/github-issues.db get https://github.com/example/fictional-app/issues/101
```

Review the inference from `init`; the supplied mapping deliberately uses
`url` rather than a repository-local issue number. To refresh an existing
index from the same export, run `leviathan --index /tmp/github-issues.db upsert examples/github-issues/issues.json`.
Upsert adds/replaces exported issues; it does not remove issues absent from
a bounded export. Use a full rebuild when you need the index to match exactly.

The nested shapes follow GitHub CLI's
[`Issue`, `Milestone`, and `Author` types](https://github.com/cli/cli/blob/ed40e856ffecd31286733876c613a2d3745e12bd/api/queries_issue.go)
and its [`ExportData` conversion](https://github.com/cli/cli/blob/ed40e856ffecd31286733876c613a2d3745e12bd/api/export_pr.go).
