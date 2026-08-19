# taskgen

`taskgen` creates diverse prompt seeds for teacher-model data generation through any OpenAI-compatible API. It supports the general Scogo IT Operations taxonomy and a dedicated compositional Enterprise NetOps taxonomy.

Taskgen owns only the user prompt and sampled taxonomy coordinates. It does **not** fabricate tool results, live state, approvals, state hashes, ground truth, verification, safety grades, rewards, or acceptance decisions. Those fields belong to the downstream teacher, harness, policy gate, verifier, and dataset pipeline.

## What is included

- Runtime YAML taxonomy loading and validation.
- General IT Ops hierarchical sampling from `docs/it-ops-taxonomy.yaml`.
- Enterprise NetOps compositional sampling from `docs/netops-taxonomy.yaml`.
- Reproducible coordinate sampling with `--seed`.
- Prompt files loaded from the CLI or taxonomy defaults.
- Concurrent generation through OpenAI-compatible chat-completion APIs.
- Schema validation before NetOps prompt records are written.
- Canonical audit and accepted-SFT JSON Schema contracts.
- Guarded ATIF-v1.7 import/export for completed trajectories.
- JSON and JSONL ATIF conversion with atomic output replacement.
- API-key and proxy rotation, retry/backoff, budget tracking, multilingual output, and deduplication.

## Build

```bash
cargo build --release
./target/release/taskgen --version
```

Prebuilt Apple Silicon and Linux ARM64 artifacts are published in [GitHub Releases](https://github.com/ksingh-scogo/taskgen/releases/latest).

## Commands

```text
taskgen generate [OPTIONS]
taskgen taxonomy validate --taxonomy <FILE>
taskgen atif export --input <FILE> --output <FILE>
taskgen atif import --input <FILE> --output <FILE>
```

The old root-level generation syntax has been replaced by the explicit `generate` subcommand.

## Generate general IT Ops prompts

When `--taxonomy` is omitted, the general taxonomy is embedded in the binary:

```bash
taskgen generate \
  --api-base https://api.example.com/v1 \
  --api-key "$OPENAI_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --output data/itops-prompts.jsonl
```

The general taxonomy is hierarchical:

```text
category -> domain -> subdomain
```

For capability categories, a subdomain is a failure mode. In the `oem` category, it is a product line and the product-voice addendum requests realistic SKU, firmware, console, CLI, TAC, and licensing context.

## Generate Enterprise NetOps prompts

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.example.com/v1 \
  --api-key "$OPENAI_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --seed 20260819 \
  --output data/netops-prompts.jsonl
```

`docs/netops-taxonomy.yaml` contains 25 domains and 531 domain-scoped subdomains. Each prompt seed composes:

```text
domain
+ subdomain
+ task family
+ environment
+ vendor scope and selected vendors/platforms
+ incident mechanism
+ evidence condition
+ evidence bundle
+ action risk
+ difficulty
+ presentation
```

The model request identifies every sampled coordinate as mandatory. The resulting prompt is expected to make each one operationally relevant rather than recite the labels.

Enterprise, campus, branch, data-center, cloud, hybrid, multicloud, Kubernetes, remote-access, edge, OT/IoT, AI/HPC, and enterprise real-time networks are included. Telecom-provider operations such as 3GPP RAN, EPC, 5GC, IMS, carrier OSS/BSS, carrier optical backbone, and service-provider core operations are excluded. Enterprise LTE/5G WAN underlays remain in scope.

### Prompt selection

The complete system prompt is resolved before the output file is opened or an API request is made. Precedence is:

1. `--system-prompt <TEXT>`
2. `--system-prompt-file <FILE>`
3. `defaults.system_prompt_file` in the selected taxonomy, relative to that taxonomy file
4. the built-in general IT Ops prompt

For NetOps, the taxonomy default is `prompts/netops-taskgen-system-v1.txt`:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --system-prompt-file prompts/netops-taskgen-system-v1.txt \
  --api-key "$OPENAI_API_KEY" \
  --model teacher/model \
  --count 100 \
  --output data/netops-prompts.jsonl
```

`--system-prompt` and `--system-prompt-file` are mutually exclusive. ATIF is not mentioned in the task-generation prompt because interchange happens only after a completed trajectory exists. The teacher contract is `prompts/netops-teacher-system-v1.txt`.

### NetOps output record

Every NetOps JSONL line is validated against `schemas/netops-task-v1.schema.json` before write:

```json
{
  "schema_version": "scogo.netops.task.v1",
  "prompt": "Investigate the suspected route leak using read-only evidence...",
  "domain": "enterprise_netops::layer3_routing",
  "subdomain": "bgp_route_leak",
  "difficulty": 8,
  "coordinates": {
    "taxonomy_id": "scogo-enterprise-netops-v1",
    "task_family": "troubleshooting_rca",
    "environment": "hybrid",
    "vendor_scope": "multi_vendor",
    "vendors": ["cisco_ios_xe", "juniper_junos"],
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

Schema acceptance is not semantic acceptance. The write gate proves record shape, coordinate membership, vendor cardinality, difficulty bounds, completion integrity, and basic leakage/length rules; it cannot prove that a generated vendor command exists or that supplied evidence really supports the scenario's causal claim. Before prompt seeds are submitted to a teacher, an independent NetOps prompt-quality review must reject vendor or protocol inventions, arithmetic and condition errors, unsupported root causes, coordinate mismatches, unsafe change framing, and internally inconsistent evidence. Keep that decision in a separate review manifest; the prompt-seed schema deliberately does not let Taskgen mark its own output accepted.

`language` is present only for multilingual generation. The seed controls coordinate and language sampling; it cannot make a remote model response deterministic.

## Validate taxonomies

Validation requires no API key and makes no model call:

```bash
taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

Validation rejects duplicate IDs, unknown references, empty eligible sets, negative or non-finite weights, and complete distributions that do not sum to `1.0 +/- 0.000001`. A subdomain ID is unique within its parent domain; its stable key is `domain_id/subdomain_id`.

To override a distribution, provide every non-zero category for a hierarchical taxonomy or domain for a compositional taxonomy, with a total of exactly 1.0:

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --distribution "infra=0.4,observe=0.3,network=0.3" \
  --count 500
```

Difficulty overrides use the same exact-sum rule:

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --difficulty "7=0.25,8=0.25,9=0.25,10=0.25" \
  --count 500
```

## Generation options

| Flag | Default | Purpose |
|---|---:|---|
| `--taxonomy <FILE>` | embedded IT Ops | Runtime taxonomy source |
| `--system-prompt <TEXT>` | taxonomy/built-in | Inline complete system prompt |
| `--system-prompt-file <FILE>` | taxonomy/built-in | UTF-8 complete system prompt file |
| `--seed <U64>` | random | Reproducible coordinate sampling |
| `--api-base <URL>` | OpenAI | OpenAI-compatible API base |
| `--api-key <KEY>` | `OPENAI_API_KEY` | Provider credential |
| `-m, --model <MODEL>` | `gpt-4o-mini` | Teacher model |
| `-c, --count <N>` | `250` | Requested prompt count |
| `-w, --workers <N>` | `5` | Concurrent requests |
| `-o, --output <FILE>` | `output.jsonl` | Output JSONL |
| `-t, --temperature <F>` | `0.9` | Sampling temperature when supported |
| `--max-output-tokens <N>` | `2048` (`4096` for Qwen) | Provider completion-token budget; must be positive |
| `--append` | off | Append to an existing generation JSONL |
| `--multilingual` | off | Sample one of eight supported languages |
| `--dedup` | off | Exact and trigram-Jaccard deduplication |
| `--dedup-threshold <F>` | `0.6` | Jaccard removal threshold |
| `--keyfile <FILE>` | none | Round-robin API keys, one per line |
| `--proxies <FILE>` | none | Proxy list |
| `--rotating-proxy` | off | Sticky random proxy |
| `--input-price <F>` | none | Input price per million tokens |
| `--output-price <F>` | none | Output price per million tokens |
| `--budget <F>` | none | Stop at the configured USD budget |

For GPT-5, o-series, and model names containing `luna`, Taskgen omits unsupported `temperature` and `max_tokens` request fields and uses `max_completion_tokens`. It still records the requested temperature as generation metadata.

For Qwen models, Taskgen requests low/no-thinking behavior, discards provider `reasoning` and `reasoning_content`, and uses a 4096-token completion budget by default because some OpenAI-compatible routes count private reasoning against that budget. `--max-output-tokens` overrides the model default. The saved prompt remains independently capped at 800 words. Empty output, exposed planning, non-stop finish reasons such as `length`, and overlong prompts are rejected and retried rather than written to the dataset. Retryable HTTP 408 and 5xx responses use bounded exponential backoff.

## Teacher trajectory contracts

The downstream pipeline is:

```text
Taskgen prompt seeds
  -> teacher candidate trajectory
  -> harness-owned tool execution and state capture
  -> approval/policy gate
  -> independent verification and safety grading
  -> canonical audit record
  -> accepted SFT projection
  -> optional ATIF-v1.7 interchange
```

Schemas:

- `schemas/netops-task-v1.schema.json`: Taskgen prompt seed.
- `schemas/netops-teacher-trajectory-audit-v1.schema.json`: full canonical candidate/imported/accepted/rejected audit record.
- `schemas/netops-teacher-trajectory-sft-v1.schema.json`: accepted trainable projection only.

The SFT projection excludes hidden reasoning, hidden ground truth, rewards, grader output, customer identifiers, secrets, copied ATIF context, deterministic `llm_call_count=0` steps, and unverified imports. See [docs/netops-data-contract.md](docs/netops-data-contract.md) for an end-to-end example and ownership rules.

## ATIF-v1.7 import and export

ATIF is an interchange format, not Scogo's canonical audit schema.

Export one canonical audit object:

```bash
taskgen atif export \
  --input data/netops-teacher.audit.v1.json \
  --output data/netops-teacher.atif.v1.7.json
```

Export JSONL:

```bash
taskgen atif export \
  --input data/netops-teacher.audit.v1.jsonl \
  --output data/netops-teacher.atif.v1.7.jsonl
```

Import external ATIF:

```bash
taskgen atif import \
  --input data/external.atif.v1.7.jsonl \
  --output data/external.audit.v1.jsonl
```

`.json` and `.jsonl` infer their container. For another extension, provide `--container json` or `--container jsonl`. Existing destinations are refused unless `--overwrite` is set.

The converter validates all input records before publishing the output, writes through a sibling temporary file, flushes and synchronizes it, and atomically renames it. JSONL errors report their source line.

ATIF validation enforces:

- exactly `schema_version: ATIF-v1.7`;
- sequential step IDs starting at 1;
- valid source-specific step fields and multimodal content parts;
- unique tool-call IDs and matching observations;
- `llm_call_count=0` restrictions;
- resolvable subagent references and unique embedded trajectory IDs;
- copied-context preservation.

Canonical exports omit hidden reasoning and preserve Scogo-only audit sections in `extra.scogo`. External imports preserve the original object under `interop.original_atif`, use `record_kind: imported`, set `outcome.status: unknown`, set `quality.accepted: false`, and add the rejection reason `external_atif_unverified`. Importing ATIF never proves that actions succeeded and never makes a trajectory trainable.

The implementation targets the active [Harbor ATIF specification](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md).

## Operational notes

- Credentials are read from the CLI, `OPENAI_API_KEY`, or a key file. They are not written into prompt records.
- API errors are surfaced; 429 responses use bounded exponential backoff.
- Five consecutive timeouts or a billing error stop generation gracefully.
- Each accepted task is flushed to JSONL as it is generated.
- A dataset card is written beside the output as `{stem}.README.md` with observed distributions and token/cost totals.
- Generated files under `data/` are ignored by Git.
- `--append` applies only to generation. ATIF conversion always produces a complete atomic destination.

## General IT Ops taxonomy

`docs/it-ops-taxonomy.yaml` remains the general 14-category source. It uses `kind: hierarchical`, while `docs/netops-taxonomy.yaml` uses `kind: compositional`. Both follow `schema_version: scogo.taskgen.taxonomy.v1` and are parsed at runtime; there is no generated Rust taxonomy catalog.

The general default distribution favors infrastructure, endpoints, ITSM, identity, workplace, network, security, observability, delivery, data, agentic operations, secure edge, enterprise applications, and OEM/platform tasks. Edit the YAML and validate it directly; no code-generation step is required.

## License and origin

Fork of [empero-org/taskgen](https://github.com/empero-org/taskgen), retargeted for Scogo IT Operations and sovereign Enterprise NetOps dataset work.
