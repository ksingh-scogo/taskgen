# Taskgen gpt-6-astra review

Baseline: `gpt-6-astra` at `c156c0203d477c8e9d75f29af4d6a8841c1b81eb`, clean and equal to `origin/master` before this review. No Taskgen-local `AGENTS.md` was present. Open PRs [#17](https://github.com/ksingh-scogo/taskgen/pull/17), [#18](https://github.com/ksingh-scogo/taskgen/pull/18), [#19](https://github.com/ksingh-scogo/taskgen/pull/19), [#20](https://github.com/ksingh-scogo/taskgen/pull/20), and [#21](https://github.com/ksingh-scogo/taskgen/pull/21) were reviewed without merging them.

## Decisions

- Centralized review code-fence normalization in the live parser so `PromptOnly` fallback responses do not spend all structured retries on parse failures.
- Counted rejected structured-output probes as completed error requests in review/adjudication telemetry.
- Enforced both directions of the adjudication outcome contract in the JSON Schema and Rust validation.
- Rejected the contradictory `platform_neutral` + `cli_ssh_session` coordinate pair at ingestion.
- Included adjudication token spend in the generation budget gate and report. Explicit adjudication providers now require dedicated input/output prices when a budget is requested; non-finite and negative prices are rejected before provider setup.
- Rejected symlinked or multi-link reference files and credential-like reference content. The credential heuristic requires a long token-shaped suffix (`hf_` ≥30, `sk-` ≥20, or `Bearer ` ≥20), so public names such as `hf_hub_download` remain usable. Rejected symlinked or multi-link run append sources before artifact creation.
- Added the canonical fine-tuning platform portal link to the Taskgen README.

## RED/GREEN evidence

Each listed RED test was added against the baseline and run before its implementation fix. The baseline failures were deterministic assertion failures, not provider calls.

| Finding | RED test | Baseline failure | GREEN test |
|---|---|---|---|
| Fenced live review parsing | `review::tests::live_path_strips_json_code_fence_before_validation` | `invalid review JSON` | Passed after parser normalization |
| Format fallback telemetry | `review::tests::reviewer_falls_back_when_provider_rejects_strict_schema` | `requests: 1`, expected `2` | Passed with rejected probe counted |
| Adjudication reject contract | `review::tests::adjudication_reject_with_all_supported_cited_claims_is_rejected`; `schema::tests::adjudication_reject_with_all_supported_cited_claims_is_rejected` | Contradictory reject accepted by Rust and schema | Passed after bidirectional checks |
| Taxonomy cross-axis validation | `taxonomy::tests::coordinate_validator_rejects_platform_neutral_cli_ssh_session` | Contradictory coordinates returned `Ok` | Passed after validator guard |
| Adjudication budget accounting | `tests::budget_spend_includes_adjudication_tokens` | Calculated spend was `0` despite adjudication counters | Passed after budget/report accounting |
| Reference symlink safety | `references::tests::reference_store_rejects_symlinked_files` | Symlink contents were loaded | Passed after `symlink_metadata` checks |
| Reference credential safety | `references::tests::reference_store_rejects_credential_like_content` | Credential-like content was loaded | Passed after content scan |
| Artifact path safety | `artifacts::tests::symlinked_run_directory_is_never_followed`; `artifacts::tests::append_source_symlink_is_rejected` | Symlink target was used/copied | Passed after no-follow checks |
| Reference identifier false positive | `references::tests::reference_store_allows_public_huggingface_identifiers` | `hf_hub_download` was treated as a credential | Passed after token-shape threshold |
| Hard-link safety | `references::tests::reference_store_rejects_hardlinked_files`; `artifacts::tests::append_source_hardlink_is_rejected` | Multi-link regular files were accepted | Passed after Unix link-count checks |
| Ancestor symlink safety | `references::tests::reference_store_rejects_symlinked_ancestor`; `artifacts::tests::run_directory_rejects_symlinked_ancestor` | Existing symlinked ancestors were followed | Passed after ancestor walk |

The budget end-to-end regression `tests::budget_gate_accounts_for_inherited_adjudication_spend` and the explicit-provider fail-closed regression `tests::budget_gate_rejects_unpriced_explicit_adjudication_provider` both pass with a local wiremock server. No external provider was called.

## Changed files

- `README.md`
- `schemas/prompt-adjudication-v1.schema.json`
- `src/artifacts.rs`
- `src/main.rs`
- `src/phase_b.rs`
- `src/references.rs`
- `src/review.rs`
- `src/schema.rs`
- `src/taxonomy.rs`
- `reviews/gpt-6-astra/TASKGEN_REVIEW.md`

## Validation

Final branch results:

- `cargo test --locked`: **206 passed, 1 ignored, 0 failed**.
- `cargo fmt --check`: **passed**.
- `cargo clippy --locked --all-targets -- -D warnings`: **passed**.
- `git diff --check`: **passed**.

The required commands were:

```text
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
```

No live provider smoke, training, upload, merge, or push was performed by this review.

## Filesystem boundary

Standalone `review --review-reference-dir` and generation artifact paths are local trusted-input boundaries. Their checks walk existing ancestors, reject ordinary symlink/hard-link inputs, and reject Unix multi-link regular files before path reads. They do not hold no-follow directory descriptors across the entire operation and therefore do not claim protection against a concurrent ancestor replacement or in-place same-inode mutation. The stronger `HeldFile`/`HeldDirectory` snapshot path remains in bounded Phase-B source-plan/resume processing. Reusing those private Phase-B types here would create a cross-module filesystem framework; the standalone contract remains “do not mutate or concurrently replace supplied local paths while the command runs.”

## Provider smoke handoff

The retained 100-task evidence exposes 100 source IDs through its canonical candidates. The checked-in Data Factory NetOps fixture has 10 source rows; all 10 computed `task_<sha256>` IDs are disjoint from those 100 IDs. The first two fixture IDs are `task_fca35be8de6a00520742d40fe3cef27d9dc5f99c54315f76467e96c419d9a1af` and `task_2f38be64797a294892f3cf626412823fcebf22bf3ea1d94f59949c3e423a056b`. The 500-row Taskgen pilot source is also disjoint from the retained 100 IDs.

After the parent audit is complete, a bounded live Data Factory smoke can use a temporary copy of `examples/netops-live.yaml`, with `limit: 2`, new output/work roots, `cx/gpt-5.6-terra-medium` generation, and `cx/gpt-5.6-sol-high` judging through `https://omniroute.scogo.ai/v1`. Keep the Taskgen pilot source read-only and provide the credential through the existing environment variable name; do not reuse an existing run ID.

To exercise the Taskgen standalone review client itself, use two copied candidate rows and a new temporary run directory:

```bash
cd /path/to/taskgen
SMOKE_ROOT="$(mktemp -d /tmp/taskgen-review-smoke.XXXXXX)"
head -n 2 pilot-runs/netops-deepseek-500-20260821-retry-check/candidates.jsonl \
  > "$SMOKE_ROOT/candidates.jsonl"
cargo run --locked -- review \
  --input "$SMOKE_ROOT/candidates.jsonl" \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://omniroute.scogo.ai/v1 \
  --api-key "$OMNIROUTE_API_KEY" \
  --model cx/gpt-5.6-luna-max \
  --adjudication-model cx/gpt-5.6-sol-xhigh \
  --review-workers 1 \
  --review-requests-per-minute 10 \
  --run-dir "$SMOKE_ROOT/review-run"
```

The command prints the exact run directory and writes `run.json`, `candidates.jsonl`, `reviews.jsonl`, `rejected.jsonl`, `tasks.jsonl`, and `run.log` under `$SMOKE_ROOT/review-run`. It makes no change to the pilot source.
