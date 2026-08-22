# taskgen

Single binary that generates **reviewed, unique prompt seeds** for teacher-model SFT through any OpenAI-compatible API. IT Operations is embedded; Enterprise NetOps is a YAML you pass in.

It owns prompt text, taxonomy coordinates, local dedup, and the review/adjudication gate. It does **not** query live infra, invent approvals, or emit trainable teacher trajectories. Those are a later pipeline (`docs/netops-data-contract.md`).

## Install

Prebuilt ARM64 binaries from [GitHub Releases](https://github.com/ksingh-scogo/taskgen/releases/latest). Do not build from source unless you are changing Taskgen.

| Machine | Asset |
|---|---|
| Linux aarch64 (DGX Spark, Graviton, Ampere) | `taskgen-linux-arm64` |
| macOS Apple Silicon | `taskgen-darwin-arm64` |

No x86_64 or Windows builds. The two assets are not interchangeable.

`-c N` is not a request for N model attempts. Taskgen runs bounded top-up waves, but each coordinate slot now flows through generation and review independently: a completed candidate is written and flushed immediately, then enters review while later generation requests continue. It repairs only `revise` outcomes once, samples fresh compatible coordinates for every remaining deficit, and publishes only after exactly N unique candidates are accepted. `--max-candidates` bounds the entire run and defaults to `max(100, 20 × count)`.

The model reviewer is an auditable gate, not an infallible source of truth. Its model, prompt, token usage, decisions, rejection reasons, and retry guidance are retained in sidecars.

## Build and commands

```bash
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) asset=taskgen-darwin-arm64 ;;
  Linux-aarch64|Linux-arm64) asset=taskgen-linux-arm64 ;;
  *) echo "No prebuilt binary for $(uname -s) $(uname -m)"; exit 1 ;;
esac

curl -fsSL -o taskgen \
  "https://github.com/ksingh-scogo/taskgen/releases/latest/download/${asset}"
chmod +x taskgen
./taskgen --version
sudo install -m 0755 taskgen /usr/local/bin/taskgen
```

Checksums: `SHA256SUMS` on the same release. Linux: `sha256sum -c`. macOS: `shasum -a 256 -c`.

IT Ops needs no extra files. For NetOps:

```bash
curl -fsSL -o netops-taxonomy.yaml \
  https://raw.githubusercontent.com/ksingh-scogo/taskgen/master/docs/netops-taxonomy.yaml
```

## First run

`-c N` is N **accepted** records, not N API calls. Generation, review, repair, and top-up continue until N unique prompts pass, or `--max-candidates` is exhausted (`max(100, 20×N)` by default). Failed runs never publish a partial `tasks.jsonl`.

```bash
export OPENAI_API_KEY=...
taskgen generate -c 250 --run-dir data/runs/itops-001
```

```text
data/runs/itops-001/
├── tasks.jsonl        # published only on success
├── candidates.jsonl   # every generated prompt, durable before review
├── reviews.jsonl
├── rejected.jsonl
└── run.json           # status, yield, tokens, sanitized endpoints — never credentials
```

Omit `--run-dir` and Taskgen creates `taskgen-runs/<utc>-<taxonomy>-<id>/`. Incomplete runs keep `accepted.partial.jsonl` in the same directory.

Default reviewer is the **same model** you generated with. That is a convenience default, not a release gate. Use a stronger `--review-model` for datasets you will train on.

## Recipes

Any OpenAI-compatible `--api-base` works (OpenAI, Together, OpenRouter, vLLM, Ollama).

**Local vLLM**

```bash
taskgen generate \
  --api-base http://localhost:8000/v1 --api-key none \
  -m your-served-model -c 1000 -w 10 --review-workers 10 \
  --run-dir data/runs/local-001
```

**NetOps**

```bash
taskgen generate \
  --taxonomy netops-taxonomy.yaml \
  --api-base https://api.example.com/v1 --api-key "$GENERATION_API_KEY" \
  -m teacher/model -c 1000 -w 5 --seed 20260820 \
  --run-dir data/runs/netops-001
```

`--seed` makes coordinate/language sampling reproducible. It cannot make a remote completion deterministic.

**Independent reviewer (same provider)**

```bash
taskgen generate \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator --review-model strong/reviewer \
  -c 1000 --run-dir data/runs/reviewed-001
```

**Independent reviewer (different endpoint)** — credentials never cross bases. A different `--review-api-base` requires `--review-api-key` or `--review-keyfile`.

```bash
taskgen generate \
  --api-base https://generator.example/v1 --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-api-base https://reviewer.example/v1 --review-api-key "$REVIEW_API_KEY" \
  --review-model strong/reviewer \
  -c 1000 --run-dir data/runs/split-001
```

Adjudication runs only on `needs_verification`. Point `--review-reference-dir` at local `.md`/`.txt`/`.json`/`.yaml` vendor docs; a separate adjudicator is `--adjudication-model` / `--adjudication-api-base` / `--adjudication-api-key`.

Reviewer and adjudicator calls negotiate structured output without assuming a specific provider: Taskgen prefers strict `json_schema`, falls back to `json_object` when a gateway or upstream model rejects the schema, and finally falls back to prompt-only JSON while retaining canonical schema validation. Qwen models start with `json_object`; DeepSeek-v4 and other models start with the strict schema. Normalized enum aliases and repaired internal claim IDs are recorded in `decision_normalization`, never silently treated as model-perfect output.

If a provider publishes a request limit, set `--review-requests-per-minute` to that shared review/adjudication budget. For example, a 10-request/minute reviewer should use `--review-requests-per-minute 10`; this limiter is independent of `--review-workers` and counts retries and structured-output fallback requests.

**Keys, proxies, budget**

```bash
taskgen generate \
  --keyfile keys.txt --review-keyfile review-keys.txt \
  --proxies proxies.txt \
  --input-price 0.20 --output-price 0.20 --budget 10.00 \
  -c 5000 -w 20 --run-dir data/runs/rotated-001
```

`--rotating-proxy` pins one random proxy (sticky) instead of round-robin. `--free-models` discovers OpenRouter free models and overrides `--api-base`.

**Extend an existing dataset** — source file is not modified; it is loaded into the dedup index.

```bash
taskgen generate --api-key "$OPENAI_API_KEY" \
  --append-from data/runs/itops-001/tasks.jsonl \
  -c 500 --run-dir data/runs/itops-002
```

**Multilingual** — `en de fr es nl zh ar ru`. Semantic dedup switches to `intfloat/multilingual-e5-small` automatically.

```bash
taskgen generate --api-key "$OPENAI_API_KEY" --multilingual \
  -c 2000 -w 10 --run-dir data/runs/i18n-001
```

**Air-gap / no ONNX download** — semantic models cache under FastEmbed’s default dir (or `--semantic-model-cache`). Pre-populate that dir, or skip embeddings:

```bash
taskgen generate --api-key "$OPENAI_API_KEY" --dedup-mode lexical \
  -c 250 --run-dir data/runs/lexical-001
```

Exact match + Jaccard still run. Dedup cannot be turned off.

**Smoke only** — `--skip-review` is a diagnostic. Schema, coordinates, and dedup still apply; `reviews.jsonl` is empty. Do not ship this as a training set.

Tune `--workers` (generation) and `--review-workers` (review) independently. The streaming pipeline overlaps the two phases safely; on one local endpoint benchmark the combined load rather than assuming that more workers is always faster. False-reject rate usually costs more wall time than too few workers.

## What “accepted” means

Each published line is schema-valid (`schemas/task-v2.schema.json`), coordinate-legal, unique, and review-accepted.

**Dedup** (always): global exact match (lowercase, collapsed whitespace); word 5-gram Jaccard in `(language, domain, subdomain)` default `0.80`; local FastEmbed cosine in the same bucket default `0.90`; a final serialized recheck before insert. Embeddings stay on box — nothing is sent to an embedding API. English default: `sentence-transformers/all-MiniLM-L6-v2`.

**Review** (unless `--skip-review`): separate call. v3 scores coordinate realization, consistency, operational quality, safety, and authenticity as `pass` / `fail` / `unknown`. Outcome is `accept` / `revise` / `reject` / `needs_verification`. Uncertainty is never coerced into a technical fail. `revise` gets one repair (`--max-repairs-per-coordinate`, max `1`). Malformed reviewer JSON and infra errors retry; they are never implicit accepts.

Replay without regenerating:

```bash
taskgen review \
  --input data/runs/netops-001/candidates.jsonl \
  --taxonomy netops-taxonomy.yaml \
  --api-base https://reviewer.example/v1 --api-key "$REVIEW_API_KEY" \
  --model strong/reviewer --gold-labels data/review-gold.jsonl \
  --run-dir data/runs/netops-review-002
```

Gold labels are JSONL `{ "candidate_id", "expected_outcome" }`. `run.json` reports the confusion matrix, false-accept/reject, and adjudication rate. For a release set: independent `--review-model`, gold replay, plus human sample of platform syntax, capacity, pricing, and causal claims.

## Taxonomies

Both files are `schema_version: scogo.taskgen.taxonomy.v3`, `kind: compositional`.

| Taxonomy | ID | Categories | Domains | Subdomains |
|---|---|---:|---:|---:|
| IT Operations (embedded) | `scogo-itops-v4` | 14 | 129 | 884 |
| Enterprise NetOps | `scogo-enterprise-netops-v2` | 1 | 25 | 531 |

Every task composes `category + domain + subdomain + task family + environment + platform scope + platforms + incident mechanism + evidence + action risk + difficulty + presentation`. The compiler rejects combinations outside the inherited capability set.

IT Ops categories: `itsm workplace endpoint identity secops secure_edge network infra observe data delivery enterprise agentic oem`. NetOps covers enterprise campus/branch/DC/cloud/hybrid/K8s/edge/OT — not telecom RAN, packet core, IMS, or OSS/BSS.

```bash
taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
taskgen taxonomy validate --taxonomy netops-taxonomy.yaml
```

`--distribution` is optional. If you pass it, **every** category ID must appear exactly once and weights must sum to `1.0`. Same for `--difficulty` vs levels 1–10: listed weights must sum to `1.0`. Prefer editing YAML `weight` / `difficulty_distribution` over a huge CLI string.

`run.json` is created with `status: running` before generation and atomically updated to `success` or `failed`. `candidates.jsonl`, `reviews.jsonl`, `rejected.jsonl`, and `accepted.partial.jsonl` are flushed after each completed pipeline item so operators can tail them while the run is active; the terminal flush adds the durability barrier. `tasks.jsonl` exists only after every staged row passes the task schema, coordinate compiler, review/adjudication policy, and final deduplication and the exact accepted count is reached. Incomplete runs retain `accepted.partial.jsonl`, candidates, reviews, rejections, and their terminal report in the same directory.

Prompt files: `--system-prompt` > `--system-prompt-file` > taxonomy `defaults.system_prompt_file` > built-in. Reviewer prompts follow the same pattern (`--review-system-prompt*`). Defaults live in `prompts/itops-taskgen-system-v2.txt` and `prompts/netops-taskgen-system-v2.txt` (embedded for IT Ops generate; needed on disk only if you override).

## Output record

The report records start/end timestamps, duration, seed, generation/review/adjudication models, separate generation and review concurrency, streaming/overlap mode and in-flight limit, top-up waves, review outcome counts, request timing, retries, 429s, timeouts, rejection reasons, coordinate replacements, candidate yield, accepted coordinate distributions, throughput, token usage, sanitized endpoint origins, and artifact size/SHA-256 metadata. It never records credentials.

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

`language` is present only with `--multilingual`.

## Flags

`taskgen generate --help` is authoritative. Useful defaults:

| Flag | Default | Purpose |
|---|---:|---|
| `--taxonomy <FILE>` | embedded IT Ops | Runtime taxonomy |
| `-c, --count <N>` | `250` | Newly accepted records required for success |
| `-w, --workers <N>` | `5` | Maximum concurrent generation requests |
| `--request-timeout-seconds <N>` | `120` | Per generation/review HTTP request timeout |
| `--max-candidates <N>` | `max(100, 20 × count)` | Global candidate ceiling across all top-up waves |
| `--review-workers <N>` | `5` | Maximum concurrent review/adjudication pipelines |
| `--review-requests-per-minute <N>` | none | Shared client-side review/adjudication request rate limit |
| `--max-repairs-per-coordinate <0|1>` | `1` | Bounded repair count for `revise` only |
| `--review-model <MODEL>` | generation model | Separate review-call model |
| `--review-api-base <URL>` | generation endpoint | Reviewer provider endpoint |
| `--review-api-key <KEY>` | inherited on same endpoint | Reviewer credential |
| `--review-keyfile <FILE>` | none | Reviewer keys, round-robin |
| `--review-max-output-tokens <N>` | `1024` | Structured reviewer completion limit |
| `--review-reference-dir <DIR>` | none | Local corpus for selective adjudication |
| `--adjudication-model <MODEL>` | reviewer model | Optional separate adjudicator |
| `--adjudication-api-base <URL>` | reviewer endpoint | Optional separate adjudicator endpoint |
| `--skip-review` | off | Skip all reviewer calls for smoke/performance diagnostics |
| `--dedup-mode <MODE>` | `semantic` | `semantic` or `lexical` |
| `--jaccard-threshold <F>` | `0.80` | Inclusive lexical threshold |
| `--semantic-threshold <F>` | `0.90` | Inclusive cosine threshold |
| `--dedup-ngram <N>` | `5` | Lexical word n-gram size |
| `--semantic-model <MODEL>` | language-dependent | Local FastEmbed model |
| `--semantic-model-cache <DIR>` | FastEmbed default | Local model cache |
| `--run-dir <DIR>` | generated under `taskgen-runs/` | Self-contained directory for this run |
| `--append-from <FILE>` | none | Seed a new run from an existing dataset and add exactly N records |
| `--multilingual` | off | Sample one of eight languages |

`--api-key` reads `OPENAI_API_KEY`. Reviewer/adjudicator: `TASKGEN_REVIEW_API_KEY`, `TASKGEN_ADJUDICATION_API_KEY`. `--keyfile` / `--review-keyfile` beat single keys. Env secret values are hidden in `--help`.

GPT-5 / o-series / Luna omit unsupported sampling fields. Qwen and DeepSeek-v4 force direct output (`reasoning_effort=none`, thinking off). DeepSeek-v4 generation uses a 2048-token cap and `<END_TASK>` stop. Provider reasoning traces are never stored. Structured reviewer calls negotiate `json_schema`, `json_object`, and prompt-only JSON per provider capability; final responses always pass Taskgen’s canonical review schema and policy checks.

Keep review enabled for production datasets. `--skip-review` is an explicit diagnostic mode: generated prompts still pass schema, coordinate compilation, deterministic checks, and deduplication; `reviews.jsonl` remains empty; and `run.json` records review as skipped. Generation and review are now overlapped safely with independent semaphores: at most `--workers` generation requests and at most `--review-workers` review/adjudication pipelines are active at once. A candidate is persisted before its review request begins, so `candidates.jsonl`, `reviews.jsonl`, `rejected.jsonl`, and the CLI counters show live progress instead of waiting for a whole wave. Tune both limits to the provider/GPU capacity; if one endpoint serves both phases, benchmark the combined load rather than assuming that more workers is always faster. Lowering false rejection through capability constraints, the four-outcome rubric, and calibration usually saves more time than blind concurrency.

## Other commands

```bash
taskgen dedup --input data/raw.jsonl \
  --output data/raw.dedup.jsonl \
  --dropped data/raw.dropped.jsonl \
  --report data/raw.dedup-report.json
```

Atomic writes. Dropped rows include `_dedup` reason/score/bucket. Omit output paths and Taskgen derives `<stem>.dedup.jsonl` / `<stem>.dropped.jsonl`. `--overwrite` replaces existing files. `--dedup-mode lexical` as above.

```bash
taskgen atif export --input data/audit.v1.jsonl --output data/atif.v1.7.jsonl
taskgen atif import --input data/external.atif.v1.7.jsonl --output data/audit.v1.jsonl
```

ATIF-v1.7 is the interchange format for **completed trajectories**, not prompt-generation. Imports are tagged `external_atif_unverified` until independently replayed. Schemas under `schemas/`.

## Contributors

```bash
cargo fmt --check && cargo test && cargo clippy --all-targets --all-features -- -D warnings
```
