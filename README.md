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

Tune `--workers` (generation) and `--review-workers` (review) independently. On one local endpoint the staged pipeline already keeps the two phases from overlapping inside a wave. False-reject rate usually costs more wall time than too few workers.

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

Prompt files: `--system-prompt` > `--system-prompt-file` > taxonomy `defaults.system_prompt_file` > built-in. Reviewer prompts follow the same pattern (`--review-system-prompt*`). Defaults live in `prompts/itops-taskgen-system-v2.txt` and `prompts/netops-taskgen-system-v2.txt` (embedded for IT Ops generate; needed on disk only if you override).

## Output record

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

| Flag | Default | Notes |
|---|---|---|
| `--taxonomy` | embedded IT Ops | YAML for NetOps / custom |
| `-c, --count` | `250` | accepted records required |
| `-w, --workers` | `5` | generation concurrency |
| `--review-workers` | `5` | review concurrency |
| `--max-candidates` | `max(100, 20×count)` | global ceiling |
| `--request-timeout-seconds` | `120` | per HTTP call |
| `--review-model` | generation model | set this for real datasets |
| `--dedup-mode` | `semantic` | `lexical` skips FastEmbed |
| `--jaccard-threshold` | `0.80` | |
| `--semantic-threshold` | `0.90` | |
| `--run-dir` | `taskgen-runs/…` | one directory per invocation |
| `--append-from` | none | copy + N new, never mutates source |
| `--skip-review` | off | smoke only |
| `--max-repairs-per-coordinate` | `1` | `revise` only; cannot exceed 1 |

`--api-key` reads `OPENAI_API_KEY`. Reviewer/adjudicator: `TASKGEN_REVIEW_API_KEY`, `TASKGEN_ADJUDICATION_API_KEY`. `--keyfile` / `--review-keyfile` beat single keys. Env secret values are hidden in `--help`.

GPT-5 / o-series / Luna omit unsupported sampling fields. Qwen and DeepSeek-v4 force direct output (`reasoning_effort=none`, thinking off). DeepSeek-v4 generation uses a 2048-token cap and `<END_TASK>` stop. Provider reasoning traces are never stored.

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

ATIF is interchange for **completed trajectories**, not prompt-generation. Imports are tagged `external_atif_unverified` until independently replayed. Schemas under `schemas/`.

## Contributors

```bash
cargo fmt --check && cargo test && cargo clippy --all-targets --all-features -- -D warnings
```
