# taskgen

`taskgen` creates reviewed, unique prompt seeds for teacher-model dataset generation through OpenAI-compatible APIs. It ships Scogo's compositional IT Operations and Sovereign Enterprise NetOps taxonomies.

Taskgen owns prompt text, sampled coordinates, prompt-schema validation, local deduplication, and the configured model-review decision. It does not fabricate telemetry, tool results, live state, approvals, ground truth, remediation success, safety grades, or trainable teacher trajectories.

## Core contract

A successful command:

```text
taskgen generate -c N
  => exactly N newly accepted prompt records
  => every record is schema-valid
  => every record passed exact, lexical, and configured semantic dedup
  => every record passed a separate operational-quality review call
  => no partial final dataset is published on failure
```

`-c N` is not a request for N model attempts. Taskgen pre-samples N immutable taxonomy coordinate slots and retries each rejected slot until it receives an accepted replacement or `--max-attempts-per-slot` is exhausted. This preserves the sampled distribution instead of silently dropping hard subjects.

The model reviewer is an auditable gate, not an infallible source of truth. Its model, prompt, token usage, decisions, rejection reasons, and retry guidance are retained in sidecars.

## Build and commands

```bash
cargo build --release
./target/release/taskgen --version
```

```text
taskgen generate [OPTIONS]
taskgen dedup --input <FILE> [OPTIONS]
taskgen taxonomy validate --taxonomy <FILE>
taskgen atif export --input <FILE> --output <FILE>
taskgen atif import --input <FILE> --output <FILE>
```

## Compositional taxonomies

Both taxonomies use `schema_version: scogo.taskgen.taxonomy.v2`, `kind: compositional`, and the same sampling/validation implementation.

| Taxonomy | ID | Categories | Domains | Subdomains |
|---|---|---:|---:|---:|
| IT Operations | `scogo-itops-v4` | 14 | 129 | 884 |
| Enterprise NetOps | `scogo-enterprise-netops-v2` | 1 | 25 | 531 |

Every sampled task composes:

```text
category + domain + subdomain
+ task family + environment
+ platform scope + selected platforms
+ incident mechanism + evidence condition + evidence bundle
+ action risk + difficulty + presentation
```

IT Ops retains its complete category/domain/subdomain inventory but now gains operational coordinates such as task family, environment, platform scope, evidence state, and risk. NetOps includes enterprise, campus, branch, data-center, cloud, hybrid, multicloud, Kubernetes, remote-access, edge, OT/IoT, AI/HPC, and enterprise real-time networks. Telecom-provider RAN, packet core, IMS, OSS/BSS, and carrier-core operations remain out of scope.

Validate either file without an API key:

```bash
taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

Validation rejects v1/hierarchical taxonomies, duplicate or unknown IDs, unreachable coordinate combinations, invalid platform cardinality, empty eligible sets, and invalid weights.

## Generate prompts

IT Ops is embedded and is used when `--taxonomy` is omitted:

```bash
taskgen generate \
  --api-base https://api.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --run-dir data/runs/itops-001
```

Enterprise NetOps:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --seed 20260820 \
  --run-dir data/runs/netops-001
```

Generation-prompt precedence is `--system-prompt`, `--system-prompt-file`, `defaults.system_prompt_file`, then the built-in IT Ops prompt. The taxonomy defaults are `prompts/itops-taskgen-system-v2.txt` and `prompts/netops-taskgen-system-v2.txt`.

### Mandatory reviewer

With no review overrides, Taskgen makes a separate review call using the effective generation model, endpoint, and credential pool.

Different model on the same provider:

```bash
taskgen generate \
  --api-base https://api.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-model strong/reviewer \
  --count 1000 \
  --run-dir data/runs/same-provider-001
```

Different reviewer provider:

```bash
taskgen generate \
  --api-base https://generator.example/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-api-base https://reviewer.example/v1 \
  --review-api-key "$REVIEW_API_KEY" \
  --review-model strong/reviewer \
  --count 1000 \
  --run-dir data/runs/split-provider-001
```

A different normalized review endpoint requires explicit review credentials. Taskgen never sends generation credentials to another endpoint implicitly. `--keyfile` and `--review-keyfile` take precedence over single or ambient keys. CLI help hides environment-secret values.

Reviewer-prompt precedence is `--review-system-prompt`, `--review-system-prompt-file`, `defaults.review_system_prompt_file`, then the built-in IT Ops reviewer. Malformed output, reviewer errors, or review rejection never become implicit acceptance.

Using the generation model as its own reviewer is the convenience default, not the recommended production release gate. It can share the generator's blind spots. For benchmark/release datasets, configure a stronger independently hosted `--review-model`, calibrate it against expert-labeled accept/reject cases, and retain human sampling for platform syntax, architecture, capacity, pricing, and causal claims.

### Mandatory native dedup

Deduplication cannot be disabled. Acceptance applies:

- global lowercased, whitespace-collapsed exact matching;
- word 5-gram Jaccard within `(language, domain, subdomain)`, default threshold `0.80`;
- local embedding cosine similarity in the same bucket, default threshold `0.90`;
- a serialized final recheck immediately before insertion.

Semantic mode uses local FastEmbed ONNX inference. English defaults to `sentence-transformers/all-MiniLM-L6-v2`; multilingual generation defaults to `intfloat/multilingual-e5-small`. Embeddings and prompts are not sent to an embedding API. The model is downloaded on first use and cached; air-gapped deployments must pre-populate `--semantic-model-cache`.

Use `--dedup-mode lexical` only when semantic inference is intentionally excluded. Exact and Jaccard checks remain mandatory.

### Run directories, atomic output, and append

Every invocation owns one directory. Supply `--run-dir <DIR>`, or omit it and Taskgen creates `taskgen-runs/<UTC timestamp>-<taxonomy ID>-<run ID>/`.

A successful directory contains:

```text
data/runs/netops-001/
├── tasks.jsonl
├── reviews.jsonl
├── rejected.jsonl
└── run.json
```

`run.json` is created with `status: running` before generation and atomically updated to `success` or `failed`. `tasks.jsonl` exists only after the exact accepted count is reached. Incomplete runs retain `accepted.partial.jsonl`, reviews, rejections, and their terminal report in the same directory.

`--append-from <FILE> -c N` creates a new run containing the source records plus exactly N newly accepted records. The source dataset is never modified, and its records are loaded into the dedup index so new candidates cannot duplicate them.

The report records start/end timestamps, duration in seconds and minutes, acceptance-worker concurrency, Tokio runtime threads, logical CPUs, request timing, retries, 429s, timeouts, rejection reasons, throughput, token usage, sanitized endpoint origins, and artifact size/SHA-256 metadata. It never records credentials.

### Prompt record

Every final line validates against `schemas/task-v2.schema.json`:

```json
{
  "schema_version": "scogo.taskgen.task.v2",
  "prompt": "BGP paths changed after the maintenance window...",
  "category": "enterprise_netops",
  "domain": "layer3_routing",
  "subdomain": "bgp_route_leak",
  "difficulty": 8,
  "coordinates": {
    "taxonomy_id": "scogo-enterprise-netops-v2",
    "category_id": "enterprise_netops",
    "task_family": "troubleshooting_rca",
    "environment": "hybrid",
    "platform_scope": "multi_platform",
    "platforms": ["cisco_ios_xe", "juniper_junos"],
    "incident_mechanism": "misconfiguration",
    "evidence_condition": "contradictory",
    "evidence_bundle": "routing_tables",
    "action_risk": "read_only_investigation",
    "presentation": "war_room"
  },
  "taskgen_model": "teacher/model",
  "temperature": 0.9
}
```

`language` appears only for multilingual generation. A seed makes coordinate/language sampling reproducible; it cannot make a remote completion deterministic.

## Generation options

| Flag | Default | Purpose |
|---|---:|---|
| `--taxonomy <FILE>` | embedded IT Ops | Runtime taxonomy |
| `-c, --count <N>` | `250` | Newly accepted records required for success |
| `-w, --workers <N>` | `5` | Concurrent coordinate slots |
| `--request-timeout-seconds <N>` | `120` | Per generation/review HTTP request timeout |
| `--max-attempts-per-slot <N>` | `20` | Candidate ceiling for each slot |
| `--review-model <MODEL>` | generation model | Separate review-call model |
| `--review-api-base <URL>` | generation endpoint | Reviewer provider endpoint |
| `--review-api-key <KEY>` | inherited on same endpoint | Reviewer credential |
| `--review-keyfile <FILE>` | none | Reviewer keys, round-robin |
| `--review-max-output-tokens <N>` | `1024` (`4096` for Qwen) | Reviewer completion limit |
| `--dedup-mode <MODE>` | `semantic` | `semantic` or `lexical` |
| `--jaccard-threshold <F>` | `0.80` | Inclusive lexical threshold |
| `--semantic-threshold <F>` | `0.90` | Inclusive cosine threshold |
| `--dedup-ngram <N>` | `5` | Lexical word n-gram size |
| `--semantic-model <MODEL>` | language-dependent | Local FastEmbed model |
| `--semantic-model-cache <DIR>` | FastEmbed default | Local model cache |
| `--run-dir <DIR>` | generated under `taskgen-runs/` | Self-contained directory for this run |
| `--append-from <FILE>` | none | Seed a new run from an existing dataset and add exactly N records |
| `--multilingual` | off | Sample one of eight languages |

GPT-5, o-series, and Luna request bodies omit unsupported sampling fields. Qwen requests use no/low-thinking controls, provider reasoning fields are discarded, and only final prompt content enters the dataset. Empty, truncated, overlong, or exposed-planning completions are rejected and replaced.

### Throughput tuning

Keep review enabled for production datasets. To improve throughput, first use a fast independent non-reasoning reviewer, then raise `--workers` gradually while watching `requests.*.rate_limits` in `run.json`. Separate generation and review endpoints avoid sharing one provider quota. A higher worker count does not help when it produces sustained 429s; reduce concurrency or use a provider route with higher limits. Reducing technical-review rejections can also outperform raw concurrency because each rejected candidate requires another generation and review cycle.

## Standalone Rust dedup

Deduplicate an existing JSONL without top-up generation:

```bash
taskgen dedup \
  --input data/raw.jsonl \
  --output data/raw.dedup.jsonl \
  --dropped data/raw.dropped.jsonl \
  --report data/raw.dedup-report.json
```

The kept and dropped files are written atomically. Dropped records include `_dedup` metadata with the reason, score/threshold when applicable, bucket, and accepted prompt hash. If output paths are omitted, Taskgen derives `<stem>.dedup.jsonl` and `<stem>.dropped.jsonl`.

## Teacher trajectory and ATIF contracts

Prompt generation is followed by a separately governed trajectory pipeline:

```text
accepted Taskgen prompt seed
  -> teacher candidate trajectory
  -> deterministic tool execution and evidence capture
  -> approval/policy gate
  -> independent verification and safety grading
  -> canonical audit record
  -> accepted SFT projection
  -> optional ATIF-v1.7 interchange
```

Schemas:

- `schemas/task-v2.schema.json`: Taskgen prompt seed.
- `schemas/prompt-review-v1.schema.json`: model-review decision.
- `schemas/netops-teacher-trajectory-audit-v1.schema.json`: full canonical trajectory audit.
- `schemas/netops-teacher-trajectory-sft-v1.schema.json`: accepted trainable projection only.

ATIF is an interchange representation for completed trajectories, not the prompt-generation schema or Scogo's canonical audit object.

```bash
taskgen atif export \
  --input data/netops-teacher.audit.v1.jsonl \
  --output data/netops-teacher.atif.v1.7.jsonl

taskgen atif import \
  --input data/external.atif.v1.7.jsonl \
  --output data/external.audit.v1.jsonl
```

ATIF import/export validates v1.7 and writes atomically. External imports receive the `external_atif_unverified` rejection reason and remain unaccepted until independently replayed and evaluated. See `docs/netops-data-contract.md` for evidence, safety, and SFT projection rules.

## Tests

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

The suite covers taxonomy migration counts, eligibility, schemas, ATIF round trips, provider isolation, reviewer decisions, native dedup, atomic artifacts, and replacement to an exact accepted count.
