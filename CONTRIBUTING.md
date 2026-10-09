# Contributing to Leviathan

Thanks for helping. Bug reports, data-source recipes, ranking improvements
and docs are all welcome.

## Development setup

```bash
git clone https://github.com/elstongun/leviathan && cd leviathan
cargo build --workspace
cargo test --workspace
```

Requires Rust 1.88+ and a C compiler (SQLite is compiled in via `rusqlite`'s
`bundled` feature). No system SQLite is needed.

## Before you open a pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p leviathan-index --no-default-features   # the build without network code
```

- Keep pull requests focused; one behavior change per PR.
- Add or update tests for behavior changes.
- Keep the engine domain-neutral. Anything specific to one kind of data
  belongs in a config or an example, not in `src/`.
- **Ranking or output-format changes:** run the benchmark and paste the
  `bench/results/SUMMARY.md` table from before and after into the PR. A change
  that saves tokens but loses answers (hit@5) will not be merged.
- **Memory recall or briefing changes:** do the same with
  `bench/memory/run_memory_bench.py` and `bench/results/MEMORY_SUMMARY.md`.
  Answer rate and stale exposure matter more than tokens.
- **New agent targets for `wrap`:** cite the agent's own documentation for
  the config path and shape, and add the target to [docs/AGENTS.md](docs/AGENTS.md).
- Update `CHANGELOG.md` under *Unreleased*.

## Running the benchmark

```bash
python3 -m venv bench/.venv && bench/.venv/bin/pip install -r bench/requirements.txt
bench/.venv/bin/python bench/run_bench.py --scales 10000,100000 --queries 200
bench/.venv/bin/python bench/report.py
```

With no `--scales`, the runner reproduces the published numbers (10K to 1M
records, about 25 minutes). That needs about 6 GB of free disk, and the data
lands in `bench/data/` (git-ignored). Results are cached per scale in
`bench/results/`. After a ranking or output change,
`run_bench.py --leviathan-only` re-measures Leviathan in a few minutes and
reuses the cached grep baselines. See [docs/BENCHMARKS.md](docs/BENCHMARKS.md)
for the methodology.

## Adding an example or a data-source recipe

High-leverage contributions are tested configs for common exports (an issue
tracker's CSV, a log format, a database dump) under `examples/`, with a small
**synthetic** sample and a short section in [docs/CONFIG.md](docs/CONFIG.md).
Never include real or sensitive data.

## Commit messages

Use the imperative mood ("Add Jira CSV example"). Reference issues with
`Fixes #123`.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0, the license of this project.
