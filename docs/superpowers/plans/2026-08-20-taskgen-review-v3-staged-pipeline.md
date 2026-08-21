# Taskgen Review v3 Staged Pipeline Implementation Plan

> **For Codex:** Execute this plan task-by-task with test-driven development. Observe every focused test fail before implementing the production change, then run the full verification gate before claiming completion.

**Goal:** Replace inline binary review with a capability-aware, staged four-outcome review pipeline that preserves exact-count publishing and succeeds on a live 10-task same-model smoke.

**Architecture:** Compile v3 subdomain capabilities before sampling, persist candidates, run deterministic validation and structured rubric review in separate bounded-concurrency waves, selectively adjudicate unverifiable claims, repair once, and top up the accepted pool to exactly `-c N`. Expose the review engine through both `generate` and a standalone `review` command.

**Tech stack:** Rust 2024, Tokio, reqwest, serde/serde_json/serde_yaml, clap, jsonschema, futures, sha2, existing Taskgen provider/dedup/artifact modules.

---

### Task 1: Define strict review-v3 contracts

**Files:**
- Replace: `schemas/prompt-review-v1.schema.json` with `schemas/prompt-review-v3.schema.json`
- Add: `schemas/prompt-adjudication-v1.schema.json`
- Modify: `src/schema.rs`
- Modify: `src/review.rs`
- Add: `tests/fixtures/canonical/valid-review-v3.json`
- Add: `tests/fixtures/canonical/valid-adjudication-v1.json`

**Test first:** Add schema/parser tests for all four outcomes, dimension statuses, evidence paths, claim requests, hard-failure invariants, and invalid contradictory decisions. Run `cargo test review::tests schema::tests -- --nocapture` and observe failure.

**Implementation:** Introduce `ReviewOutcome`, `CheckStatus`, `DimensionCheck`, `ReviewDecisionV3`, `VerificationClaim`, `AdjudicationDecision`, and code-level policy validation. Remove the binary v1 types. Clip only free-text fields after schema validation-safe normalization. Make invalid review output a retryable reviewer error.

**Verify:** Run the focused test command until green.

### Task 2: Add capability-aware taxonomy v3 compilation

**Files:**
- Modify: `src/taxonomy.rs`
- Modify: `docs/netops-taxonomy.yaml`
- Modify: `docs/it-ops-taxonomy.yaml`
- Modify: `docs/it-ops-taxonomy-v4-migration.json`
- Modify: `README.md`

**Test first:** Add tests proving that bare v3 subdomains are rejected, explicit capability overrides constrain platforms and mechanisms, invalid capability IDs fail validation, and repeated seeded sampling never emits the known `vrf_vdom_vsys + google_cloud` or `phase1_phase2_selectors + sonic` mismatches. Observe focused failures with `cargo test taxonomy::tests -- --nocapture`.

**Implementation:** Bump the taxonomy schema to `scogo.taskgen.taxonomy.v3`, require object-form subdomains with `capabilities`, resolve each axis by intersection with inherited eligibility, precompute valid coordinate capability sets, and update both production taxonomies with explicit subdomain capabilities. The existing distribution and difficulty weights remain unchanged unless a capability makes a combination impossible.

**Verify:** Run `cargo test taxonomy::tests -- --nocapture` and `cargo run -- taxonomy validate` for both taxonomy files.

### Task 3: Make scenario evidence semantics deterministic

**Files:**
- Modify: `prompts/netops-taskgen-system-v2.txt`
- Modify: `prompts/itops-taskgen-system-v2.txt`
- Modify: `src/main.rs`

**Test first:** Add prompt-loading and candidate-validation tests that reject claims of live access, require supplied/simulated fixture wording when evidence is embedded, and require approval language for approval-required changes. Observe focused failures.

**Implementation:** Replace contradictory generator instructions with the evidence contract. Add deterministic `CandidateChecks` containing schema, coordinate, fixture semantics, operational behavior, safety, and dedup findings. Hard failures bypass the LLM reviewer and are serialized into review artifacts.

**Verify:** Run the focused prompt/candidate checks and existing generation tests.

### Task 4: Implement rubric reviewer and selective adjudicator

**Files:**
- Replace: `prompts/netops-prompt-review-system-v2.txt` with `prompts/netops-prompt-review-system-v3.txt`
- Replace: `prompts/itops-prompt-review-system-v2.txt` with `prompts/itops-prompt-review-system-v3.txt`
- Add: `prompts/prompt-adjudication-system-v1.txt`
- Modify: `src/review.rs`
- Add: `src/references.rs`
- Modify: `src/main.rs`

**Test first:** Add mock-server tests covering accept, revise, reject, needs-verification, reviewer invalid JSON retry, selective adjudicator invocation, no adjudicator call for other outcomes, and missing-reference behavior. Observe failures.

**Implementation:** Send the v3 rubric with candidate evidence and deterministic findings. Default reviewer output to 1024 tokens. Add an optional local reference store that reads UTF-8 `.md`, `.txt`, `.json`, `.yaml`, and `.yml` files and retrieves bounded excerpts by deterministic token overlap. Adjudicate only requested claims and require citations to retrieved reference IDs or candidate JSON paths. Default adjudicator provider/model to the reviewer provider/model.

**Verify:** Run `cargo test review::tests references::tests -- --nocapture` plus mock integration tests.

### Task 5: Persist candidates and extend run reporting

**Files:**
- Modify: `src/artifacts.rs`
- Modify: `src/main.rs`
- Modify: `README.md`

**Test first:** Add artifact tests proving candidate durability before review, no final `tasks.jsonl` on failure, atomic exact-count publication, and run report fields for stages/outcomes/waves/concurrency. Observe failures.

**Implementation:** Add `candidates.jsonl`; stable candidate sequence/hash; review and adjudication envelopes; separate stage timers; outcome counters; candidate yield; top-up wave count; generation/review/adjudication concurrency and telemetry; and final disposition fields.

**Verify:** Run artifact and report tests.

### Task 6: Replace slot-coupled generation with staged waves

**Files:**
- Add: `src/pipeline.rs`
- Modify: `src/main.rs`
- Modify: `src/artifacts.rs`

**Test first:** Replace the old inline-review exact-count integration with scripted cases: mixed outcomes reach exact `N`; revise is repaired once; reject gets a fresh coordinate; needs-verification is selectively adjudicated; duplicates trigger top-up; candidate limit fails without final publish; generation and review concurrency are independent. Observe failures.

**Implementation:** Create generation, review, adjudication, repair, accepted-reserve, dedup, and top-up stages. Wave size equals the current accepted deficit. Preserve deterministic candidate sequence and publish the first `N` accepted unique candidates. Add `--review-workers`, `--max-candidates`, reference/adjudicator provider flags, and default inheritance rules. Keep `--skip-review` as a diagnostic path using deterministic checks only.

**Verify:** Run focused pipeline integrations until green.

### Task 7: Add standalone review and calibration reporting

**Files:**
- Modify: `src/main.rs`
- Add: `src/calibration.rs`
- Modify: `README.md`
- Add: `tests/fixtures/review-gold.jsonl`

**Test first:** Add CLI parsing, standalone artifact, exact accepted output, and gold-label metric tests. Observe failures.

**Implementation:** Add `taskgen review` using the shared review stages. Support `--gold-labels` and report confusion counts, per-outcome precision/recall, false-accept, false-reject, invalid-response, and adjudication rates in `run.json`.

**Verify:** Run standalone review/calibration tests and CLI help snapshot assertions.

### Task 8: Full local verification and checkpoint commit

Run:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
./target/release/taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
./target/release/taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
```

Inspect `git diff --check`, `git status --short`, and the relevant report/schema artifacts. Commit all implementation and documentation with Karan Singh's configured authorship and no AI attribution.

### Task 9: Live same-model reviewed smoke

Use the configured local OpenAI-compatible endpoint and `deepseek-v4-flash-0731` for both generation and review, one generation worker and one review worker, NetOps taxonomy, review enabled, and `-c 10`. Do not place credentials in the command or artifacts when the endpoint does not require them.

Validate:

- command exit code is zero;
- final `tasks.jsonl` contains exactly 10 lines;
- all 10 records pass task schema validation;
- exact/semantic dedup reports zero duplicates in final output;
- `candidates.jsonl`, `reviews.jsonl`, `rejected.jsonl`, and `run.json` are internally consistent;
- the reviewer model equals the generation model;
- outcome and request counts reconcile;
- no review was silently skipped;
- no task contains an invalid compiled coordinate.

If the smoke reveals a defect, add a failing regression test before applying the fix, rerun the focused test, then repeat the full gate and smoke. Commit verified fixes and record the final run directory and metrics.
