# Taskgen parallel-execution performance review

Baseline: `perf/parallel-execution` at `576e743`. Final implementation commit:
`8401857` (`perf: reuse compiled schema validators`). The earlier flush change
was explicitly reverted by `1eb5532` after the release-build comparison below.

The refreshed parent `origin/gpt-6-astra` at `d3e9604` was merged without rebase
as `8d7a76a`. Current `origin/master` at release commit `e2a038b` (including
the `c108176` trusted-local-path canonicalization fix) was then merged without
rebase as `612d6d2`. The merges preserve that path hardening, the schema cache,
and the retained benchmark harness; their regression tests are included below.

## Accepted optimization

`schema::validate_instance` previously parsed and compiled the selected JSON
Schema for every row. The implementation now compiles one validator per schema
kind in `OnceLock` and reuses it. Validation errors, schema sources, and the
public `Result` contract remain unchanged.

The retained benchmark harness is an ignored unit test. It contains both the
original parse/compile/validate path and the cached path, so the baseline does
not need to contain the harness. Run both paths in one release binary:

```text
cargo test --release --locked schema::tests::schema_validation_benchmark -- --ignored --nocapture
```

Workload: five alternating, warmed pairs of 20,000 valid Task v2 validations.

| Path | Samples (ms) | Median | Result |
|---|---:|---:|---|
| Original parse/compile/validate | 1762, 1774, 1767, 1764, 1768 | 1767 | — |
| Cached validator | 20, 20, 20, 20, 20 | 20 | 98.87% lower median |

This is a CPU-only operation benchmark that measures the repeated work removed
from every generation/review/ingestion validation; it does not claim provider
throughput or model-latency improvement.

`schema::tests::cached_validation_matches_original_for_all_schema_kinds`
compares valid and invalid outcomes for Task, review, adjudication, audit, and
SFT schemas.

## Rejected flush experiment

The earlier change guarded `BufWriter::flush()` with `buffer().is_empty()`.
Because empty `BufWriter<File>::flush()` is already effectively a no-op, the
release result did not justify retaining the change. The artifact benchmark
harness remains in `artifacts.rs` for reproducibility:

```text
cargo test --release --locked artifacts::tests::artifact_visibility_benchmark -- --ignored --nocapture
```

Workload: 10,000 iterations writing candidate, review, rejection, and accepted
records, with `flush_visible()` after each iteration.

| Version | Samples (ms) | Median |
|---|---:|---:|
| Baseline/current flush behavior | 22, 28, 26, 24, 28 | 26 |
| Guarded flush experiment | 27, 24, 23, 21, 21 | 23 |

The samples overlap and the difference is within local noise, so the experiment
was rejected. The final code retains the original flush behavior and semantics.

The existing controlled overlap regression remains required:

```text
cargo test --locked generation_publishes_candidates_immediately_and_overlaps_review
```

## Validation

The final `perf/parallel-execution` branch head records results from the
unchanged code at merge commit `612d6d2`:

- `cargo test --locked`: **213 passed, 3 ignored, 0 failed**.
- `cargo test --release --locked schema::tests::schema_validation_benchmark -- --ignored --nocapture`:
  **1 passed**; measurements recorded above.
- `cargo fmt --check`: passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo build --release --locked`: passed.
- `git diff --check`: passed.
