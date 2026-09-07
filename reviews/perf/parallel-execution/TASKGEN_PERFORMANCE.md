# Taskgen parallel-execution performance review

Branch baseline: `perf/parallel-execution` at `576e743`.

The generation pipeline already uses bounded generation/review semaphores and
`buffer_unordered`; Phase-B already drains a bounded `FuturesUnordered`. The
measured local hot path was artifact visibility: every live record write called
`flush_visible`, which attempted to flush all four `BufWriter`s even when three
had no pending bytes.

The change keeps immediate visibility for the stream that was written while
skipping empty-buffer flush calls. Final `flush()` still flushes and `sync_all`s
all four files, so publication durability, record ordering, error propagation,
resume artifacts, and provider quotas are unchanged.

## Controlled benchmark

Temporary benchmark workload: 10,000 iterations, each writing one candidate,
review, rejection, and accepted record, followed by `flush_visible()`. The test
was removed after measurement; raw output is not committed.

| Version | Runs (ms) | Median | Change |
|---|---:|---:|---:|
| Baseline `576e743` | 64, 78, 77 | 77 | — |
| Dirty-buffer flush | 76, 55, 56 | 56 | 27% lower median |

The result is an offline artifact-write measurement; provider latency and model
throughput are intentionally outside this benchmark. The existing controlled
overlap regression `tests::generation_publishes_candidates_immediately_and_overlaps_review`
remains the concurrency correctness gate.

## Validation

Final branch results:

- `cargo test --locked`: **206 passed, 1 ignored, 0 failed**.
- `cargo fmt --check`: **passed**.
- `cargo clippy --locked --all-targets -- -D warnings`: **passed**.
- `git diff --check`: **passed**.
- Controlled overlap regression: `tests::generation_publishes_candidates_immediately_and_overlaps_review` **passed**.
