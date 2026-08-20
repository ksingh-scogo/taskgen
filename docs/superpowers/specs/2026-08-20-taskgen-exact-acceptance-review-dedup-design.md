# Taskgen Exact Acceptance, Quality Review, and Native Dedup Design

Date: 2026-08-20

Status: implemented and live-canary verified on 2026-08-20

Branch: `codex/netops-taxonomy-atif` from `origin/master`

## 1. Outcome

For `taskgen generate -c N`, `N` means **N newly accepted, unique prompt records**, not `N` generation attempts.

A successful command must publish exactly `N` new records that pass all of these gates:

1. completion and task-record validation;
2. deterministic exact and lexical deduplication;
3. local embedding-based semantic deduplication;
4. a separate model-based operational-quality review; and
5. a final atomic dedup check at acceptance time.

If any candidate fails a gate, Taskgen records the rejection and generates a replacement for the same pre-sampled task coordinate. If the configured attempt limit, budget, provider availability, local semantic model, output I/O, or shutdown signal prevents Taskgen from reaching `N`, the command exits non-zero and does not publish an incomplete final output as a success.

The quality contract is therefore:

```text
successful `taskgen generate -c N`
  => exactly N newly accepted records
  => every record passed the configured validation, review, and dedup gates
  => no accepted pair violates the configured dedup thresholds
```

This is a gate-defined guarantee. It does not claim that an LLM reviewer is an infallible source of networking or IT-operations truth. The reviewer prompt, model, provider, thresholds, reason codes, and run statistics are recorded so the acceptance policy is auditable.

## 2. Scope

Included:

- make operational-quality review part of generation rather than a manual post-generation suggestion;
- default the review model to the generation model while permitting a different OpenAI-compatible review model and provider;
- keep generation and review credentials separate when provider endpoints differ;
- port the useful behavior of `scripts/dedup_jsonl.py` into Rust;
- make deduplication part of acceptance so dropped duplicates are replaced;
- retain a standalone Rust dedup command for already-generated JSONL;
- preserve the requested taxonomy distribution by retrying the same sampled coordinate;
- publish the final output atomically only after the exact accepted count is reached;
- retain accepted-review, rejection, and run-summary audit artifacts;
- apply the same architecture to the unified compositional IT Ops and NetOps taxonomy schema.

Excluded in this increment:

- teacher trajectory generation, tool execution, or SFT projection;
- multiple reviewer-model voting or reviewer ensembles;
- a remote embedding API;
- vector databases or cross-run vector services;
- automatic calibration that changes thresholds during a run;
- treating reviewer approval as ground truth;
- backward compatibility for the old optional post-generation `--dedup` behavior.

## 3. Decisions

### 3.1 Review is mandatory and separately invoked

Every schema-valid candidate receives a separate quality-review call before acceptance. When `--review-model` is omitted, Taskgen uses the generation model. This is a separate request with a distinct system prompt and strict response contract, even when the underlying model and provider are the same.

The default optimizes ease of use and cost. Operators can select a stronger or independently hosted reviewer with `--review-model`, `--review-api-base`, and review-specific credentials.

For normal single-model generation:

```text
effective review model = --review-model, otherwise --model
```

For `--free-models`, if `--review-model` is omitted, the candidate is reviewed by the same concrete model that generated that candidate. Supplying an explicit stable `--review-model` is recommended for more consistent review behavior.

No malformed response, timeout, HTTP failure, or exhausted reviewer retry is converted into an acceptance. It is either retried as a transport/protocol failure or terminates the candidate attempt as a recorded review error.

### 3.2 Provider credentials are inherited only on the same endpoint

Generation and review each resolve to a `ProviderConfig`:

```rust
struct ProviderConfig {
    api_base: Url,
    models: ModelSelection,
    credentials: CredentialPool,
}
```

The review-provider contract is:

| Review configuration | Effective endpoint | Effective credentials |
|---|---|---|
| No review overrides | Generation endpoint | Generation credential pool |
| Only `--review-model` | Generation endpoint | Generation credential pool |
| Explicit review endpoint equal to the normalized generation endpoint | That endpoint | Review credentials when supplied, otherwise generation credentials |
| Explicit review endpoint different from generation endpoint | Review endpoint | Explicit review credentials are required |

Normalized endpoint equality removes trailing slashes and compares the parsed URL scheme, host, effective port, and path. Taskgen must never send a generation credential to a different endpoint merely because review credentials were omitted.

Review credential precedence is:

1. `--review-keyfile`;
2. `--review-api-key` or `TASKGEN_REVIEW_API_KEY`;
3. generation credential pool, only when the normalized endpoints are equal.

Generation credential precedence remains:

1. `--keyfile`;
2. `--api-key` or `OPENAI_API_KEY`.

The key-file form takes precedence over a single CLI or ambient key for the same provider. This permits an explicit keyfile even when `OPENAI_API_KEY` or `TASKGEN_REVIEW_API_KEY` exists in the shell. Credentials are never written to task records, review manifests, reports, logs, CLI help, or errors.

### 3.3 Dedup is native Rust and occurs before acceptance

The Python implementation established the intended behavior:

- global exact clones use lowercased, whitespace-collapsed text with spaces preserved;
- lexical near-duplicates are compared only within `(language, domain, subdomain)`;
- lexical similarity uses word 5-gram Jaccard with a default threshold of `0.80`;
- semantic paraphrases are compared within the same bucket with a default cosine threshold of `0.90`;
- dropped records carry an audit reason and, when available, the accepted record they resemble.

Rust will own that behavior in a shared `dedup` module used by both generation and the standalone `taskgen dedup` command. The existing global trigram pass and optional post-hoc `--dedup` flag are removed.

Exact and Jaccard behavior is a direct port. Semantic behavior is a native local-embedding implementation rather than a byte-for-byte port of Python SemHash. It uses FastEmbed's local ONNX inference and cosine similarity:

- English default: `sentence-transformers/all-MiniLM-L6-v2`;
- multilingual default: `intfloat/multilingual-e5-small`;
- embeddings stay in process and are never sent to an API;
- the model downloads on first use and is reused from a configurable local cache;
- an air-gapped run must pre-populate that cache;
- semantic-model initialization is a preflight step, so a missing model fails before generation requests begin.

FastEmbed documents local ONNX inference, both selected built-in models, cosine-similarity helpers, and first-use caching: <https://github.com/Anush008/fastembed-rs>.

The implementation is accepted only if release builds remain valid for the repository's Linux ARM64 and macOS ARM64 targets. A dependency or native-runtime failure on either target blocks completion; it is not silently downgraded to lexical-only dedup.

### 3.4 The sampled coordinate, not the failed text, owns a slot

Taskgen pre-samples exactly `N` coordinate slots using the selected taxonomy, distribution, difficulty distribution, language mode, and seed. Each slot remains pending until one candidate for that same coordinate is accepted.

```text
slot 17: sampled coordinates
  attempt 1 -> reviewer reject
  attempt 2 -> semantic duplicate
  attempt 3 -> accepted
```

This preserves the requested coordinate distribution. Top-up by sampling brand-new coordinates is rejected because it allows hard coordinates to disappear and distorts the original sample.

`--max-attempts-per-slot` defaults to `20`. Provider-level transient retries do not consume a new candidate attempt until the request either yields a candidate or reaches its transport retry limit. Every generated candidate that fails validation, review, or dedup consumes one slot attempt.

### 3.5 Final output is an atomic success artifact

Taskgen writes accepted records and audit events to one self-contained run directory. The user may supply `--run-dir`; otherwise Taskgen creates a timestamped directory under `taskgen-runs/`.

Successful publication creates:

```text
<run-dir>/tasks.jsonl
<run-dir>/reviews.jsonl
<run-dir>/rejected.jsonl
<run-dir>/run.json
```

`run.json` begins with `status: running`. On success, sidecars are flushed and the staged accepted dataset is atomically renamed to `tasks.jsonl`; the report is then atomically updated to `success`.

On failure, the same directory retains `accepted.partial.jsonl`, all audit events, and a terminal `status: failed` report. Partial data is never presented as `tasks.jsonl`.

For `--append-from <FILE>`, `-c N` means `N` additional accepted records. Taskgen loads the source file into the validation and dedup indexes and creates a new run containing the existing records plus the `N` new records. The source is never modified. Existing records do not count toward `N`; a new candidate that duplicates an existing record is rejected and replaced.

## 4. Command-line contract

### 4.1 Generation

Same-provider default:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --run-dir data/runs/netops-001
```

The reviewer uses `teacher/model`, the same endpoint, and the same credential pool.

Different model on the same provider:

```bash
taskgen generate \
  --api-base https://api.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-model strong/reviewer \
  --count 1000 \
  --run-dir data/runs/itops-001
```

Different review provider:

```bash
taskgen generate \
  --api-base https://generator.example/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-api-base https://reviewer.example/v1 \
  --review-api-key "$REVIEW_API_KEY" \
  --review-model strong/reviewer \
  --count 1000 \
  --run-dir data/runs/netops-split-review-001
```

New and changed generation flags:

| Flag | Default | Contract |
|---|---|---|
| `--review-model <MODEL>` | effective generation model | Model for the separate quality-review call |
| `--review-api-base <URL>` | generation API base | OpenAI-compatible reviewer endpoint |
| `--review-api-key <KEY>` | none | Single reviewer credential; also available as `TASKGEN_REVIEW_API_KEY` |
| `--review-keyfile <FILE>` | none | Reviewer credentials, one per line, round-robin |
| `--review-system-prompt <TEXT>` | taxonomy/built-in | Complete inline reviewer prompt |
| `--review-system-prompt-file <FILE>` | taxonomy/built-in | Complete UTF-8 reviewer prompt file |
| `--review-max-output-tokens <N>` | `1024`; `4096` for Qwen | Positive reviewer completion-token limit |
| `--request-timeout-seconds <N>` | `120` | Positive per-generation/review HTTP request timeout |
| `--max-attempts-per-slot <N>` | `20` | Positive candidate-attempt ceiling per coordinate slot |
| `--dedup-mode <MODE>` | `semantic` | `semantic` or `lexical`; dedup cannot be disabled |
| `--jaccard-threshold <F>` | `0.80` | Inclusive lexical duplicate threshold in `[0,1]` |
| `--semantic-threshold <F>` | `0.90` | Inclusive cosine duplicate threshold in `[0,1]` |
| `--dedup-ngram <N>` | `5` | Positive word n-gram size |
| `--semantic-model <MODEL>` | language-dependent | Supported local FastEmbed model |
| `--semantic-model-cache <DIR>` | FastEmbed default | Local embedding-model cache |
| `--review-input-price <F>` | none | Reviewer input price per million tokens |
| `--review-output-price <F>` | none | Reviewer output price per million tokens |
| `--run-dir <DIR>` | timestamped directory under `taskgen-runs/` | Self-contained run destination |
| `--append-from <FILE>` | none | Read an existing dataset into a new run and add exactly N accepted records |

Removed generation flags:

- `--dedup`;
- `--dedup-threshold`.

`--review-system-prompt` and `--review-system-prompt-file` conflict. Reviewer prompt precedence is:

1. `--review-system-prompt`;
2. `--review-system-prompt-file`;
3. `defaults.review_system_prompt_file` from the selected taxonomy;
4. the built-in general IT Ops reviewer prompt.

`--budget` covers the combined priced generation and review calls. If generation and review use different providers or models, the generation and review price flags calculate their respective contributions. Reaching the budget before all slots are accepted is a non-zero incomplete run, not a successful short file.

### 4.2 Standalone Rust dedup

```bash
taskgen dedup \
  --input data/raw.jsonl \
  --output data/raw.dedup.jsonl \
  --dropped data/raw.dropped.jsonl \
  --report data/raw.dedup-report.json
```

Flags:

| Flag | Default | Contract |
|---|---|---|
| `--input <FILE>` | required | Input JSONL |
| `--output <FILE>` | `<stem>.dedup.jsonl` | Atomically written kept records |
| `--dropped <FILE>` | `<stem>.dropped.jsonl` | Atomically written invalid and duplicate records |
| `--report <FILE>` | none | Optional JSON statistics report |
| `--prompt-field <FIELD>` | `prompt` | String field to compare |
| `--dedup-mode <MODE>` | `semantic` | `semantic` or `lexical` |
| `--jaccard-threshold <F>` | `0.80` | Inclusive bucketed Jaccard threshold |
| `--semantic-threshold <F>` | `0.90` | Inclusive bucketed cosine threshold |
| `--dedup-ngram <N>` | `5` | Positive word n-gram size |
| `--semantic-model <MODEL>` | language-dependent | Local FastEmbed model |
| `--semantic-model-cache <DIR>` | FastEmbed default | Local model cache |
| `--overwrite` | off | Permit atomic destination replacement |

The standalone command does not top up the file. Exact-count replacement belongs to `generate`, where Taskgen can create another candidate for the original coordinate.

## 5. Taxonomy contract

Both taxonomies move to `scogo.taskgen.taxonomy.v2` and `kind: compositional` under the separate [Unified Compositional IT Ops and NetOps Taxonomy Design](2026-08-20-unified-compositional-itops-netops-taxonomy-design.md).

IT Ops retains its 14-category, 129-domain, 884-subdomain subject hierarchy and adds cross-cutting operational coordinates. NetOps retains its 25 domains and 531 subdomains under one `enterprise_netops` category. Both are sampled and validated through one generalized compositional code path.

Both taxonomies configure taxonomy-specific generation and reviewer prompts:

```yaml
defaults:
  system_prompt_file: ../prompts/netops-taskgen-system-v2.txt
  review_system_prompt_file: ../prompts/netops-prompt-review-system-v2.txt
  difficulty_distribution: {...}
```

IT Ops uses `../prompts/itops-taskgen-system-v2.txt` and `../prompts/itops-prompt-review-system-v2.txt`. NetOps uses `../prompts/netops-taskgen-system-v2.txt` and `../prompts/netops-prompt-review-system-v2.txt`.

Taxonomy validation must resolve and read both configured prompt files. A missing, unreadable, empty, or invalid UTF-8 prompt is a preflight error before any API call or output mutation.

## 6. Reviewer prompt and response contract

### 6.1 Reviewer input

The reviewer receives:

- the taxonomy ID and kind;
- domain, subdomain, difficulty, language, and all available compositional coordinates;
- the candidate prompt exactly as it would appear in the output;
- the allowed enterprise/IT Ops scope;
- a reminder that it is reviewing a prompt seed, not solving the incident;
- a strict instruction to reject unsupported assertions while accepting tasks that deliberately require live evidence, tool calls, or abstention before a diagnosis.

The reviewer does not receive API credentials, previous hidden reasoning, or the teacher system prompt.

### 6.2 Required checks

The reviewer must evaluate all of these dimensions:

1. vendor, product, command, configuration, API, and platform authenticity;
2. protocol, architecture, service, and dependency correctness;
3. numerical, temporal, state, and causal consistency;
4. fidelity to the sampled domain, subdomain, difficulty, and NetOps coordinates;
5. operational realism rather than certification trivia or documentation recall;
6. whether the declared evidence condition and evidence bundle make the task solvable, intentionally abstainable, or explicitly evidence-seeking;
7. read-only-by-default behavior and approval requirements for mutations;
8. absence of unsupported root-cause claims, hidden answers, fabricated live state, or contradictory evidence;
9. clear expected operator outcome without forcing a particular unverified diagnosis;
10. enterprise IT/NetOps scope, excluding telecom-provider operations for the NetOps taxonomy.

### 6.3 Model response schema

The model must return one JSON object:

```json
{
  "schema_version": "scogo.taskgen.prompt-review.v1",
  "verdict": "reject",
  "reason_codes": ["invalid_command_or_syntax", "coordinate_mismatch"],
  "summary": "The command belongs to a different platform and the prompt does not realize the sampled evidence condition.",
  "retry_guidance": "Use a valid command for the selected platform and make the contradictory routing evidence operationally relevant."
}
```

Exact JSON Schema rules:

- object with `additionalProperties: false`;
- required: `schema_version`, `verdict`, `reason_codes`, `summary`, `retry_guidance`;
- `schema_version`: constant `scogo.taskgen.prompt-review.v1`;
- `verdict`: `accept` or `reject`;
- `reason_codes`: unique array of known codes, maximum 8;
- accepted decisions require an empty `reason_codes` array and empty `retry_guidance`;
- rejected decisions require at least one reason code and non-empty retry guidance;
- `summary`: 1 to 800 characters;
- `retry_guidance`: 0 to 800 characters.

Reason codes:

```text
technical_inaccuracy
invented_platform_feature
invalid_command_or_syntax
protocol_or_architecture_error
unsupported_causality
numerical_or_temporal_inconsistency
internal_contradiction
coordinate_mismatch
insufficient_or_invalid_evidence
not_operational
unsafe_or_unapproved_change
hidden_answer_or_solution_leakage
scope_violation
ambiguous_or_unanswerable
```

The application adds trusted envelope metadata rather than asking the model to generate it:

```json
{
  "run_id": "...",
  "slot": 17,
  "attempt": 3,
  "candidate_sha256": "...",
  "generation_model": "...",
  "review_model": "...",
  "review_api_base_origin": "https://reviewer.example",
  "review": {"schema_version": "scogo.taskgen.prompt-review.v1", "verdict": "accept", "reason_codes": [], "summary": "...", "retry_guidance": ""},
  "usage": {"input_tokens": 0, "output_tokens": 0}
}
```

The manifest stores only the endpoint origin, never credentials or query parameters.

Reviewer retry guidance is supplied to the generator on the next attempt for that slot as untrusted critique. It is clearly delimited and cannot override the generation system prompt, taxonomy coordinate, safety rules, or output format.

## 7. Dedup contract

### 7.1 Normalization and exact identity

```text
normalized = Unicode text lowercased, split on Unicode whitespace, joined by one ASCII space
exact_key = SHA-256(UTF-8(normalized))
```

Exact identity is global across all languages, domains, and subdomains. Empty prompts are rejected by validation before dedup.

### 7.2 Bucket identity

```text
language = lowercase(trim(language or "en"))
domain = trim(domain or "_missing")
subdomain = trim(subdomain or "_missing")
bucket = language + "|" + domain + "|" + subdomain
```

Lexical and semantic comparisons occur only inside a bucket. This prevents generic firewall tickets from being collapsed into distinct product-specific or domain-specific tasks solely because both contain shared on-call boilerplate.

### 7.3 Lexical similarity

- lowercase and split on whitespace;
- use word n-grams when the prompt has two or more words;
- for one-word/no-space input, use non-whitespace character n-grams;
- if the token count is smaller than `n`, the complete token sequence is one gram;
- compare unique gram sets using Jaccard intersection divided by union;
- reject when similarity is greater than or equal to the configured threshold.

### 7.4 Semantic similarity

- embed the complete candidate prompt locally once;
- compare it with accepted embeddings in the same bucket using cosine similarity;
- reject when similarity is greater than or equal to the configured threshold;
- store the matched accepted prompt hash, score, model, and threshold in the rejection event;
- never compare embeddings generated by different model identifiers in the same index.

The implementation may add an in-memory approximate candidate shortlist for performance only if tests prove that it never omits a pair at or above the configured threshold. V1 therefore starts with exact bucket-local comparisons.

### 7.5 Concurrent check-and-commit

Workers may validate, normalize, embed, and review candidates concurrently. Acceptance is serialized by a commit coordinator:

1. run cheap exact, lexical, and semantic prechecks against an index snapshot;
2. invoke the quality reviewer;
3. acquire the acceptance coordinator;
4. repeat exact, lexical, and semantic checks against the latest accepted index;
5. insert the candidate and its embedding as one logical operation;
6. append the accepted record and accepted review event to the working journal;
7. mark the coordinate slot complete.

The final recheck prevents two concurrently reviewed paraphrases from both being accepted.

## 8. Acceptance-loop state machine

```text
PENDING_SLOT
  -> GENERATING
  -> VALIDATING
      invalid ---------------------------> REJECTED -> PENDING_SLOT
  -> DEDUP_PRECHECK
      duplicate ------------------------> REJECTED -> PENDING_SLOT
  -> REVIEWING
      semantic reject ------------------> REJECTED -> PENDING_SLOT
      malformed/transient -> retry call -> REVIEWING
      exhausted review error -----------> REJECTED -> PENDING_SLOT
  -> FINAL_DEDUP_AND_COMMIT
      concurrent duplicate -------------> REJECTED -> PENDING_SLOT
      unique ----------------------------> ACCEPTED_SLOT

all N slots accepted -> VALIDATE_RUN -> PUBLISH -> SUCCESS
attempt/budget/provider/I/O/shutdown exhaustion -> RETAIN_WORKDIR -> NONZERO
```

The progress bar advances on accepted slots, not attempted candidates. Its message reports accepted, pending, rejected by gate, candidate attempts, and token/cost totals.

The generator may use the previous review `retry_guidance` for the same slot only. Dedup rejections provide a neutral instruction to produce a materially different incident while preserving every sampled coordinate; they do not paste a complete accepted prompt into the next generation request.

## 9. Audit artifacts

### 9.1 Rejection record

Every rejected candidate creates a JSONL event:

```json
{
  "schema_version": "scogo.taskgen.rejection.v1",
  "run_id": "...",
  "slot": 17,
  "attempt": 2,
  "gate": "semantic_dedup",
  "reason": "semantic_duplicate",
  "candidate_sha256": "...",
  "candidate": {"prompt": "...", "domain": "...", "subdomain": "..."},
  "match": {"accepted_sha256": "...", "score": 0.934, "threshold": 0.9},
  "review": null,
  "occurred_at": "..."
}
```

Allowed gates:

```text
generation
task_validation
exact_dedup
lexical_dedup
semantic_dedup
quality_review
final_dedup
```

The quality-review form includes the parsed reviewer decision. Invalid JSONL in the standalone dedup command uses `gate: input` and `reason: invalid_json`.

### 9.2 Run report

`<run-dir>/run.json` contains:

- run ID, start/end timestamps, duration seconds/minutes, command version, taxonomy ID and kind;
- acceptance-worker count, Tokio runtime-worker count, and logical CPU count;
- requested new count, existing count, accepted new count, and published total;
- sampled and accepted coordinate distributions;
- total candidate attempts and attempts per slot;
- rejection counts by gate and reason;
- generation and review model/endpoint-origin configuration;
- generation and review token usage and priced cost;
- generation/review request counts, retries, 429s, timeouts, errors, and aggregate request time;
- tasks-per-minute and candidates-per-minute throughput;
- dedup mode, n-gram size, thresholds, embedding model, and model cache identity;
- output and sidecar SHA-256 hashes;
- completion status and, for incomplete runs, the terminal reason.

No secret, full credential, authorization header, or proxy credential is permitted in the report.

## 10. Error and interruption behavior

- CLI and taxonomy preflight errors occur before the working directory or any API call is created when possible.
- Different review and generation endpoints without explicit review credentials are a preflight error.
- Semantic-model initialization failure is a preflight error in semantic mode.
- A rejected candidate is normal pipeline behavior, not a command error.
- Exhausting `--max-attempts-per-slot` is a command error naming the slot and coordinate in the run report.
- Exhausting the configured budget before `N` is a command error.
- SIGINT/SIGTERM stops scheduling new work, lets in-flight journal writes finish, records an incomplete run, retains the run directory, and exits non-zero.
- Output write, fsync, schema, or final-count validation failure prevents publication.
- A successful exit requires that the newly accepted count equals `N` and every final row reparses and revalidates.

## 11. Module layout

Implementation will move generation orchestration out of the current monolithic path:

```text
src/
  main.rs                 CLI and command dispatch
  provider.rs             endpoint and credential resolution
  review.rs               reviewer prompt, request, schema, and manifest
  dedup.rs                exact, bucketed Jaccard, embeddings, standalone command
  acceptance.rs           coordinate-slot scheduler and acceptance coordinator
  artifacts.rs            working journals and atomic publication
  taxonomy.rs             unified compositional taxonomy v2 loader
  schema.rs               task/review/rejection schema validation
schemas/
  prompt-review-v1.schema.json
  task-rejection-v1.schema.json
prompts/
  itops-prompt-review-system-v2.txt
  netops-prompt-review-system-v2.txt
```

The exact file split may be adjusted to keep modules cohesive, but quality review, dedup, acceptance scheduling, and atomic publication must not remain as one post-generation block in `main.rs`.

## 12. Verification contract

Implementation is incomplete until all of these pass.

### 12.1 Unit and property tests

- provider endpoint normalization and credential precedence;
- different endpoint never inherits the generation key;
- key values never appear in debug/error/report serialization;
- reviewer JSON accept/reject invariants and malformed-response retries;
- exact normalization preserves word boundaries;
- bucket identity and missing-field defaults;
- 5-gram and short-input behavior;
- Jaccard and cosine threshold boundaries are inclusive;
- deterministic dedup survivor selection;
- acceptance coordinator prevents a concurrent duplicate race;
- same sampled coordinate is reused after every rejection;
- accepted progress cannot exceed or stop below `N` on success.

### 12.2 Integration tests with fake providers

- generation reject, reviewer reject, exact duplicate, lexical duplicate, and semantic duplicate each trigger a replacement;
- a scripted run requesting `N` publishes exactly `N` accepted records;
- retry guidance is scoped to the same slot and cannot replace system instructions;
- exhausted attempts exits non-zero and leaves the requested final output untouched;
- budget exhaustion exits non-zero and leaves the requested final output untouched;
- append loads existing rows into dedup and adds exactly `N` new unique rows;
- final publication occurs only after sidecars and exact final-count validation;
- standalone Rust dedup matches the Python fixture for exact and Jaccard decisions;
- semantic fixtures contain both paraphrase rejects and operationally distinct survivors.

### 12.3 Repository and release checks

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo build --release --target aarch64-unknown-linux-gnu
cargo build --release --target aarch64-apple-darwin
```

Target builds may run on their native CI runners when a local cross toolchain is unavailable. Both release jobs must include semantic dedup; lexical-only compilation is not an acceptable substitute.

### 12.4 Live canaries

Using user-supplied credentials without persisting or printing them:

1. same generator/reviewer model and provider, `-c 10`;
2. different review model on the same provider, `-c 10`;
3. different review provider when credentials are available, `-c 5`;
4. an injected duplicate-heavy generation prompt that proves top-up behavior.

For each successful canary:

- final JSONL line count equals requested `-c` for a new file;
- every row validates;
- review manifest has one accepted review for each final new row;
- rejection sidecar explains every discarded candidate;
- an independent rerun of native dedup drops zero final rows under the same configuration;
- final secret scan finds no API keys in repository, outputs, sidecars, reports, or logs.

## 13. Documentation changes

README and data-contract documentation will explain:

- `-c` is accepted-count semantics;
- review defaults to the generator model and same provider;
- how to configure a different review model/provider safely;
- why a different endpoint requires explicit review credentials;
- native local semantic dedup and its first-use model cache;
- how to pre-populate the cache for an air-gapped machine;
- the accepted, reviews, rejected, and run-report artifacts;
- incomplete-run and append behavior;
- how the IT Ops hierarchy is preserved inside the shared compositional v2 schema.

The old advice to generate first and manually run the Python dedup script is removed.

## 14. Alternatives rejected

### Post-hoc dedup followed by arbitrary top-up

Rejected because it temporarily publishes short output, makes success ambiguous, and distorts taxonomy distribution when replacements use newly sampled coordinates.

### Keep Python as a required second runtime

Rejected because generation correctness would depend on an external Python environment and a separate command. A shared Rust module gives the integrated acceptance loop and standalone cleanup command one implementation.

### Remote embedding API

Rejected because prompts would be disclosed to another service and require another credential and failure surface. Local embeddings better fit the sovereign/on-prem product direction.

### Send generation credentials to a different reviewer endpoint by default

Rejected because endpoint inequality is a trust-boundary change. Explicit review credentials are required even when an operator intends to reuse the same literal key.

### Accept on reviewer failure

Rejected because it converts provider or parser errors into silent quality bypasses.

### One global semantic bucket

Rejected because operational templates across distinct domains and products share vocabulary. Domain/subdomain/language blocking reduces false merges while global exact matching still removes literal clones.
