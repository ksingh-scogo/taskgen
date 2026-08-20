# Taskgen Review v3 Staged Pipeline Design

**Date:** 2026-08-20  
**Status:** Approved for implementation  
**Scope:** Taskgen compositional ITOps and NetOps prompt generation

## Problem statement

The current generator performs generation, deduplication, and a monolithic binary LLM review inside each output slot. In the DeepSeek smoke run this produced 7 accepted tasks from 88 candidates: all 81 rejections occurred at `model_review`. The same endpoint produced 10 tasks in 99.4 seconds when review was disabled. Review latency was material, but the dominant failure was the reviewer's 92% rejection operating point, which multiplied generation and review calls.

The reviewer is also being asked to solve several different problems in one pass: verify taxonomy realization, internal logic, platform architecture, command syntax, evidence quality, action safety, and answerability. It has only the candidate prompt as evidence, yet its binary contract forces uncertainty into rejection. Meanwhile, broad domain-level taxonomy eligibility permits mechanically valid but technically implausible coordinates before the prompt reaches review.

Review remains mandatory by default. The redesign makes it an independent, inspectable quality pipeline instead of a regeneration gate embedded in every output slot.

## Goals

1. A successful `generate -c N` run publishes exactly `N` unique, accepted task records.
2. Reject technically invalid, internally contradictory, unsafe, unanswerable, or coordinate-mismatched prompts without treating unverifiable claims as proven defects.
3. Prevent known-invalid coordinate combinations before inference.
4. Preserve all generated candidates, decisions, and evidence for replay and calibration.
5. Separate generation and review concurrency so reviewer latency cannot serialize generation.
6. Support the generation model as the default reviewer and independent reviewer/adjudicator providers when supplied.
7. Make the same review engine available as a standalone command.

## Non-goals

- The reviewer is not a replacement for current vendor documentation, live device state, deterministic parsers, or execution sandboxes.
- A synthetic prompt fixture is not asserted to be observed production state.
- Schema validity or reviewer agreement alone is not a SOTA quality claim.
- This change does not preserve the v1 binary review schema or its internal orchestration.

## Evidence contract

Generated prompts may contain synthetic configuration excerpts, telemetry, command output, logs, tickets, topology facts, runbook text, or other fixtures. These fixtures are the scenario's supplied evidence. The generator must label them as supplied or simulated evidence and must not imply that Taskgen queried a live system.

The task itself must still require the downstream operator/model to:

- distinguish observations from hypotheses;
- request missing live state when needed;
- cite the supplied evidence used for each conclusion;
- abstain from unsupported claims;
- default to read-only investigation; and
- require explicit approval before changes.

The generator prompt will use this contract consistently; it will no longer say both “never invent tool output” and “include command output” without distinguishing scenario fixtures from live observations.

## Capability-aware coordinate compiler

Eligibility moves from broad domain inheritance toward explicit subdomain capabilities. Each subdomain may declare a `capabilities` mapping:

```yaml
capabilities:
  platform_groups: [security]
  platforms: [fortinet_fortios, palo_alto_panos]
  environments: [enterprise_campus, branch]
  task_families: [incident_diagnosis, safe_remediation]
  incident_mechanisms: [policy_shadowing, asymmetric_routing]
  evidence_bundles: [config_and_logs, packet_and_flow]
  action_risks: [read_only, approval_required]
  presentations: [vendor_cli, structured_evidence]
```

For v3 taxonomies, every subdomain must define capabilities for any axis whose domain-level set is broader than the subdomain. The compiler resolves capabilities before sampling and rejects empty or incompatible intersections during `taxonomy validate` and generator startup. Platform-neutral tasks remain valid when the presentation and wording are platform-neutral. Vendor-specific CLI or architecture requires a compatible explicit platform.

This is an intentional schema break: the taxonomy schema version becomes `scogo.taskgen.taxonomy.v3`, and bare subdomain IDs are invalid.

## Validation layers

### Layer 1: coordinate compilation

Before inference, validate that the sampled coordinate exists in the subdomain's resolved capability set. This layer catches impossible platform/subdomain/mechanism/presentation combinations deterministically.

### Layer 2: deterministic candidate checks

Before LLM review:

- task v2 schema validation;
- prompt length and planning-leak checks;
- coordinate materialization heuristics;
- fixture/live-state wording contract;
- deterministic safety phrases for approval-required changes;
- exact and semantic deduplication.

A hard deterministic failure is `reject` and never calls the reviewer.

### Layer 3: independent rubric review

The reviewer returns one status for each dimension:

- `coordinate_realization`
- `internal_consistency`
- `operational_quality`
- `safety`
- `technical_authenticity`

Each dimension contains `status` (`pass`, `fail`, or `unknown`), a concise rationale, and zero or more evidence paths pointing into the candidate record. The top-level outcome is one of:

- `accept`: all required dimensions pass; no hard failure;
- `revise`: the task is viable and has a bounded, repairable defect;
- `reject`: a proven hard defect makes the task unusable;
- `needs_verification`: technical authenticity depends on facts not established by the candidate or available references.

`unknown` is not equivalent to `fail`. A `reject` decision must name at least one failed dimension and a hard-failure reason. `revise` must provide actionable repair guidance. `needs_verification` must list the exact claims to verify.

### Layer 4: selective adjudication

Only `needs_verification` decisions invoke adjudication. The adjudicator receives the candidate, the claims, the first decision, and locally retrieved references when configured with `--review-reference-dir`. It returns `accept` or `reject` per claim with evidence citations. With no matching reference, the claim remains unverified; this does not become a fabricated technical failure. The candidate is rejected from the published set and retained with `needs_verification` provenance for later expert/reference review.

The same model/provider is the default adjudicator. Separate `--adjudication-model`, `--adjudication-api-base`, `--adjudication-api-key`, and `--adjudication-keyfile` options override it.

## Staged orchestration

`taskgen generate` remains the one-command workflow but runs explicit stages in waves:

1. Compile and validate the taxonomy.
2. Generate a candidate wave for the current acceptance deficit.
3. Persist every candidate to `candidates.jsonl` before review.
4. Run deterministic checks and LLM rubric review with `--review-workers` concurrency.
5. Adjudicate only `needs_verification` candidates.
6. Queue at most one targeted repair for `revise`; `reject` and exhausted repairs are replaced by fresh compatible coordinates.
7. Deduplicate accepted candidates and place them in a reserve ordered by candidate sequence.
8. Publish exactly the first `N` unique accepted records.
9. Generate another wave equal to the remaining deficit until `N` is reached or the global candidate limit is exhausted.

Generation and review are never interleaved within an individual slot. They may use the same endpoint safely because the phases are separated. `--workers` controls generation concurrency and `--review-workers` controls review concurrency. Reviewer completions default to 1024 tokens.

The global safety bound is `--max-candidates`, defaulting to `max(100, 20 * count)`. Exhausting it fails the command without publishing `tasks.jsonl`; partial candidates and review evidence remain recoverable in the run directory.

## Standalone review

The new command:

```text
taskgen review --input candidates.jsonl --taxonomy docs/netops-taxonomy.yaml \
  --api-base URL --api-key KEY --model MODEL --run-dir DIR
```

runs layers 1–4 without generation. It writes `reviews.jsonl`, `rejected.jsonl`, `accepted.partial.jsonl`, and `run.json`. This makes review replayable with a new model, prompt, reference corpus, or calibration release without regenerating candidates.

## Artifacts and schemas

Each run directory contains:

- `candidates.jsonl`: immutable candidate envelope, sequence, coordinate, model, and prompt hash;
- `reviews.jsonl`: deterministic checks, rubric decision, optional adjudication, model/token/latency metadata, and final disposition;
- `rejected.jsonl`: hard failures, exhausted revisions, duplicates, unverified candidates, and infrastructure errors;
- `accepted.partial.jsonl`: crash-recoverable staged accepted records;
- `tasks.jsonl`: atomically published exact output, only on success;
- `run.json`: stage timings, generation/review/adjudication request counts, outcome counts, acceptance yield, top-up waves, workers, model/provider identities, token use, and cost.

The review schema becomes `scogo.taskgen.prompt-review.v3`. It is strict (`additionalProperties: false`) and contains dimension checks, claim verification requests, hard failures, summary, and repair guidance.

## Acceptance policy

The policy is deterministic and versioned:

- any deterministic hard failure -> reject;
- any reviewer dimension `fail` plus a supported hard failure -> reject;
- bounded correctable issue and no safety/technical hard failure -> revise once;
- any required fact marked `unknown` -> needs verification;
- all required dimensions pass and no hard failure -> accept;
- adjudication accepts only when every requested claim is supported by a cited local reference or is directly entailed by supplied candidate evidence.

The LLM proposes dimension findings; code computes and validates the final disposition. An invalid or self-contradictory review response is a review infrastructure error and is retried, not evidence that the candidate is bad.

## Calibration and release gate

A frozen expert-labelled JSONL set will contain candidate records and expected final dispositions. `taskgen review --gold-labels FILE` reports a confusion matrix, per-outcome precision/recall, false-reject rate, false-accept rate, invalid-response rate, and adjudication rate. Reviewer prompt/model changes must be evaluated on this set.

Initial release gates:

- zero false accepts for expert-labelled safety or internal-contradiction hard failures;
- false-reject rate at or below 15% on expert-labelled acceptable prompts;
- invalid reviewer response rate below 2%;
- exact `N` publish behavior under controlled reject/revise/verification fixtures;
- a live 10-task same-model reviewed smoke completes successfully with 10 schema-valid unique accepted records.

These are pipeline gates, not claims of overall model SOTA.

## Failure semantics

- Generation/review transport failures are retried and recorded separately from quality outcomes.
- Invalid reviewer JSON is `review_error`, never `reject`.
- Missing local references produce `needs_verification`, not a fabricated reason code.
- Cancellation or candidate-limit exhaustion leaves partial artifacts and a failed `run.json`; `tasks.jsonl` is not published.
- Credentials are never written to artifacts or logs.

