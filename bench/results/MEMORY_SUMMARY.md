# Memory benchmark summary

Generated 2026-10-09T09:36:07Z · tokenizer `o200k_base` · leviathan 0.2.0 · AMD Ryzen 7 2700X Eight-Core Processor

Sessions: 300 · memories written: 2,400 · facts: 283 · subjects: 60 · memory tool schemas: 800 tokens per session.

| strategy | tokens at the last session start | answer in context | answer ranked first | stale value in context |
|---|---:|---:|---:|---:|
| Leviathan memory | 736 | 100.0% | 100.0% | 0.0% |
| MEMORY.md, ideal compaction | 13,182 | 100.0% | 100.0% | 0.0% |
| MEMORY.md, whole file | 33,313 | 100.0% | 100.0% | 73.8% |
| MEMORY.md, first 200 lines | 2,588 | 18.0% | 18.0% | 36.4% |

Leviathan recall adds 185 tokens per question on average (remember p50 0.38 ms, recall p50 0.89 ms).

| memories | store | import | recall p50 / p95 | briefing p50 | remember p50 / p95 | CLI recall p50 (process start) | markdown file tokens |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 10,000 | 12 MB | 1.3 s | 0.8 / 2.1 ms | 9.6 ms | 0.19 / 0.38 ms | 18 ms | 470,494 |
| 100,000 | 117 MB | 15.3 s | 5.4 / 8.2 ms | 9.6 ms | 0.18 / 0.39 ms | 28 ms | 4,704,940 |
| 1,000,000 | 1,164 MB | 174.7 s | 24.6 / 50.0 ms | 9.6 ms | 0.19 / 0.35 ms | 52 ms | 47,049,400 |
