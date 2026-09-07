# Taskgen parallel-execution performance review

Baseline: `perf/parallel-execution` at `576e743`. Final implementation commit:
`8401857` (`perf: reuse compiled schema validators`). The earlier flush change
was explicitly reverted by `1eb5532` after the release-build comparison below.

## Accepted optimization

`schema::validate_instance` previously parsed and compiled the selected JSON
Schema for every row. The implementation now compiles one validator per schema
kind in `OnceLock` and reuses it. Validation errors, schema sources, and the
public `Result` contract remain unchanged.

The retained benchmark harness is an ignored unit test. Run it in release mode:

```text
cargo test --release --locked schema::tests::schema_validation_benchmark -- --ignored --nocapture
```

Workload: 20,000 valid Task v2 validations in one warmed process.

| Version | Samples (ms) | Median | Result |
|---|---:|---:|---|
| Baseline `576e743` | 1467, 1462, 1444 | 1462 | — |
| Compiled validator | 12, 27, 12 | 12 | 99.2% lower median |

This is a CPU-only operation benchmark that measures the repeated work removed
from every generation/review/ingestion validation; it does not claim provider
throughput or model-latency improvement.

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

Final branch results are recorded after the implementation commit:

- `cargo test --locked`: **206 passed, 3 ignored, 0 failed**.
- `cargo fmt --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `git diff --check`: passed.
