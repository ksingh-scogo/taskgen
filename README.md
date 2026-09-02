# taskgen

`taskgen` creates reviewed, schema-valid, deduplicated prompt seeds for teacher-model SFT datasets through any OpenAI-compatible Chat Completions API.

It owns the prompt text, taxonomy coordinates, deterministic validation, local deduplication, model review, and final publication. It does **not** connect to live infrastructure, invent approvals, or generate completed teacher trajectories. The later trajectory pipeline is described in [`docs/netops-data-contract.md`](docs/netops-data-contract.md).

## Start here: create your first dataset

Follow these steps in order.

### 1. Install `taskgen`

Prebuilt releases are available for Linux ARM64 and macOS Apple Silicon.

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

The release also contains `SHA256SUMS`; verify the downloaded asset before installing it. There are no prebuilt x86_64 or Windows binaries.

After this first manual installation, future upgrades are one command:

```bash
sudo taskgen upgrade
```

Use `taskgen upgrade` without `sudo` when the current executable's directory is writable by your user. The command selects the matching ARM64 release asset, compares the current executable with `SHA256SUMS`, downloads only when needed, verifies SHA-256, and atomically replaces the executable.

To build from source instead, install the current stable Rust toolchain and run:

```bash
git clone https://github.com/ksingh-scogo/taskgen.git
cd taskgen
cargo build --release
./target/release/taskgen --version
```

The rest of this README assumes `taskgen` is on your `PATH`. Replace it with `./taskgen` or `./target/release/taskgen` if you kept the binary local.

### 2. Choose a taxonomy

Taskgen supports two bundled taxonomies:

| Use case | Taxonomy ID | How to select it |
|---|---|---|
| IT Operations | `scogo-itops-v4` | Embedded generation default; omit `--taxonomy` |
| Enterprise NetOps | `scogo-enterprise-netops-v2` | Pass `--taxonomy <FILE>` |

The taxonomy rules are:

- `taskgen generate` without `--taxonomy` uses the complete IT Ops taxonomy embedded in the binary. The command does not run without a taxonomy; it uses the embedded copy.
- `taskgen generate --taxonomy <FILE>` uses that YAML instead. NetOps generation always follows this path.
- Standalone `taskgen review` always requires `--taxonomy <FILE>` so existing records can be validated against the exact taxonomy used to create them.

For NetOps, use the file in this repository and validate it before generating:

```bash
taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

If you installed only the binary, download the file first:

```bash
mkdir -p config
curl -fsSL -o config/netops-taxonomy.yaml \
  https://raw.githubusercontent.com/ksingh-scogo/taskgen/master/docs/netops-taxonomy.yaml
taskgen taxonomy validate --taxonomy config/netops-taxonomy.yaml
```

Expected result:

```text
valid taxonomy: scogo-enterprise-netops-v2 (Compositional, 25 domains, 531 subdomains)
```

### 3. Configure an OpenAI-compatible provider

The default endpoint is `https://api.openai.com/v1`. This guide recommends `gpt-5.6-luna-max` and passes it explicitly with `--model`; the generation key can come from `OPENAI_API_KEY`. The configured provider must expose that exact model ID—replace it with the provider's actual model name when necessary.

Provider selection and taxonomy selection are independent. The provider examples below omit `--taxonomy`, so they use embedded IT Ops. Add `--taxonomy docs/netops-taxonomy.yaml` (or the downloaded `config/netops-taxonomy.yaml`) to run the same examples for NetOps.

```bash
export OPENAI_API_KEY="your-api-key"
```

For another provider, pass its base URL and model on the command line:

```bash
export OPENAI_API_KEY="your-provider-key"
taskgen generate \
  --api-base https://api.example.com/v1 \
  --model provider/model-name \
  --count 10
```

For a local vLLM server that ignores credentials, pass any non-empty placeholder key:

```bash
taskgen generate \
  --api-base http://localhost:8000/v1 \
  --api-key none \
  --model your-served-model \
  --count 10
```

The provider-specific examples above are alternatives to the default-provider smoke test in step 4.

### 4. Run a small IT Ops smoke test with the embedded taxonomy

This command intentionally omits `--taxonomy`: Taskgen loads `scogo-itops-v4` from the binary. Nothing else needs to be downloaded for IT Ops generation.

```bash
taskgen generate \
  --model gpt-5.6-luna-max \
  --count 10
```

`--count 10` means **10 newly accepted records**, not 10 API requests. Taskgen continues generation, validation, review, repair, and top-up until it has exactly 10 accepted unique prompts or reaches the candidate/budget limit.

The default semantic deduplication model is local. On its first use, FastEmbed may download `sentence-transformers/all-MiniLM-L6-v2`. Use `--dedup-mode lexical` if the machine is offline or you want a no-download smoke test.

Keep review enabled for real datasets. `--skip-review` is only a diagnostic mode.

### 5. Check the result

On success, the run directory looks like this:

```text
taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c10+250826-14-35/
├── tasks.jsonl       # final accepted dataset; exists only after a successful run
├── candidates.jsonl  # every generated candidate, persisted before review
├── reviews.jsonl     # reviewer/adjudicator decisions and token usage
├── rejected.jsonl    # deterministic, dedup, generation, and review rejections
├── run.json          # status, configuration, counts, timing, cost, and checksums
└── run.log           # live timestamped text progress and debug events
```

Follow a live run from another terminal:

```bash
tail -f taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c10+250826-14-35/run.log
```

Every line is flushed immediately and uses a simple UTC text format:

```text
2026-08-25T08:31:04.127Z INFO   wave_start                   wave=1 queued=10 accepted=0 attempts=0 remaining_capacity=100
2026-08-25T08:31:04.130Z DEBUG  generation_start             sequence=1 wave=1 model="gpt-5.6-luna-max" category="network" domain="routing" subdomain="bgp" difficulty=7 repair_count=0 language="en"
2026-08-25T08:31:19.512Z WARN   generation_retry             sequence=1 wave=1 reason=connect_timeout elapsed_seconds=15.0 taskgen_limit_seconds=15 retry=1/5 wait_seconds=3
2026-08-25T08:34:10.842Z INFO   review_complete              candidate_id=... sequence=1 wave=1 outcome=Accept adjudicated=false
2026-08-25T08:34:10.850Z INFO   candidate_accepted           candidate_id=... sequence=1 wave=1 accepted=1/10
```

The beginning of `run.log` records every command option plus effective defaults, provider models/endpoints, taxonomy, concurrency, seed, prompt fingerprints, dedup settings, limits, and artifact paths. API keys and keyfile contents are always redacted; inline prompt contents are represented only by character count and SHA-256.

At startup, the command prints and logs the generation start time. At success or failure, it prints a final operator summary and writes the same lines as timestamped `run_summary` events in `run.log`:

```text
================ Taskgen Run Summary ================
Status: SUCCESS
Started: 2026-08-25T09:00:00+00:00
Finished: 2026-08-25T09:05:31+00:00
Overall wall time: 5.52 minutes (331.4 seconds)
Results: requested=2 accepted=2 rejected=0 attempts=2 final_records=2 acceptance_rate=100.0% throughput=0.36 tasks/min
Review outcomes: accept=2 revise=0 reject=0 needs_verification=0
Recovery: top_up_waves=0 coordinate_replacements=0
Tokens:
  Generation: input=2837 output=13490 total=16327
  Review: input=3981 output=2416 total=6397
  Adjudication: input=0 output=0 total=0
  Overall: input=6818 output=15906 total=22724
Cumulative stage time:
  Generation requests: 4.64 minutes (278.6 seconds)
  Generation pipeline (requests + retry waits): 4.68 minutes (280.6 seconds)
  Review requests: 1.45 minutes (86.9 seconds)
  Adjudication requests: 0.00 minutes (0.0 seconds)
  Regeneration for unaccepted prompts: 0.00 minutes (0.0 seconds), candidates=0 repairs=0 fresh_replacements=0
Requests: generation=3 retries=1 timeouts=1 connect_timeouts=1 errors=0 | review=2 retries=0 timeouts=0 connect_timeouts=0 errors=0 | adjudication=0
Timing note: stage times are cumulative request/pipeline time and may overlap under concurrency; they are not expected to sum to wall time
======================================================
```

`run.json` retains the same machine-readable data under `operator_summary`, `timing`, and `regeneration`. Regeneration means model time spent on a reviewer-requested repair or a fresh top-up candidate after an initial prompt was not accepted. Stage times are cumulative across concurrent requests and can overlap, so they are not expected to add up to wall-clock time.

Check the accepted count and terminal status:

```bash
wc -l taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c10+250826-14-35/tasks.jsonl
jq '{status, requested_new_records, accepted_new_records, candidate_attempts}' \
  taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c10+250826-14-35/run.json
```

If a run is interrupted or cannot reach the exact count, it keeps `accepted.partial.jsonl` and all audit sidecars, but it does not publish `tasks.jsonl`. A user-supplied `--run-dir` must be new or empty.

By default, generation creates `taskgen/runs/<taxonomy>+<generation-model>+c<count>+<ddmmyy-hh-mm>/` under the current working directory. Model separators such as `/` are converted to `-`; a same-minute collision receives `+02`, `+03`, and so on. The CLI prints the exact path at startup. `run.json` includes models, sanitized endpoints, concurrency, review outcomes, request timing/retries, token counts, priced cost, accepted-coordinate distributions, and artifact SHA-256 hashes.

If the current directory already contains a file named `taskgen`—as the source repository does—the base automatically becomes `./runs/` because `./taskgen` cannot simultaneously be a binary and a directory. Always use the exact `Run directory:` path printed by the command when tailing or inspecting artifacts.

### 6. Scale up with an independent reviewer

Both examples in this step also omit `--taxonomy`, so they generate IT Ops records with the embedded taxonomy. For NetOps, add `--taxonomy docs/netops-taxonomy.yaml` to either command.

For a release-quality dataset, use an independent reviewer. If generation and review use the same endpoint, only the review model needs to change:

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --model gpt-5.6-luna-max \
  --review-model strong/reviewer \
  --count 1000 \
  --workers 10 \
  --review-workers 10 \
  --seed 20260820
```

If the reviewer uses a different endpoint, its credentials must be supplied explicitly:

```bash
taskgen generate \
  --api-base https://generator.example.com/v1 \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-api-base https://reviewer.example.com/v1 \
  --review-api-key "$REVIEW_API_KEY" \
  --review-model strong/reviewer \
  --count 1000
```

Tune `--workers` and `--review-workers` independently. When one server handles both phases, their combined load matters.

## Command map

Taskgen has **6 top-level commands**:

```text
taskgen
├── generate
├── upgrade
├── review
├── dedup
├── atif
│   ├── export
│   └── import
└── taxonomy
    └── validate
```

`atif` and `taxonomy` are command groups. Excluding Clap's generated `help` command, there are **7 actionable command paths**:

| # | Command path | What it does | API calls? |
|---:|---|---|---|
| 1 | `taskgen generate` | Generates, validates, deduplicates, reviews, and publishes prompt seeds | Yes |
| 2 | `taskgen upgrade` | Downloads, verifies, and installs the latest official release | GitHub only |
| 3 | `taskgen review` | Replays the review/adjudication gate over existing candidates | Yes |
| 4 | `taskgen dedup` | Deduplicates any JSONL prompt dataset | No |
| 5 | `taskgen taxonomy validate` | Validates taxonomy structure, weights, references, and reachability | No |
| 6 | `taskgen atif export` | Converts canonical completed audit records to ATIF-v1.7 | No |
| 7 | `taskgen atif import` | Converts ATIF-v1.7 to canonical unverified audit records | No |

Run these at any time to see the authoritative flags for your installed version:

```bash
taskgen --help
taskgen generate --help
taskgen upgrade --help
taskgen review --help
taskgen dedup --help
taskgen atif export --help
taskgen atif import --help
taskgen taxonomy validate --help
```

## 1. `taskgen generate`

Use this for the normal end-to-end workflow. For each taxonomy coordinate, Taskgen:

1. Generates a prompt with the selected model.
2. Validates its JSON schema and taxonomy coordinates.
3. Rejects unsafe deterministic failures.
4. Checks exact, lexical, and optionally semantic duplicates.
5. Sends the candidate to the reviewer unless `--skip-review` is set.
6. Adjudicates only `needs_verification` outcomes, using local references when supplied.
7. Repairs a `revise` result at most once by default, then samples fresh compatible coordinates for remaining deficits.
8. Publishes only when the exact requested accepted count is valid and unique.

Generation and review overlap as a streaming pipeline. Candidates and decisions are flushed while the run is active.

### Generation examples

#### Generate Enterprise NetOps prompts

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.example.com/v1 \
  --api-key "$OPENAI_API_KEY" \
  --model teacher/model \
  --count 1000 \
  --workers 5 \
  --seed 20260820
```

### What practical value does `--seed` provide?

The seed controls Taskgen's local random sampling, not the model's text generation. With the same taxonomy version, seed, count, distribution, difficulty settings, and language mode, Taskgen starts from the same ordered workload: the same categories, domains, subdomains, difficulty levels, task families, platforms, evidence conditions, risks, presentations, and sampled languages.

This is useful for:

- **A/B model or prompt comparisons:** give two models the same task mix so quality differences are not caused by one model receiving easier coordinates.
- **Regression testing:** rerun a known coordinate schedule after changing prompts, review policy, or provider code.
- **Auditability:** `run.json` records the seed, allowing an operator to reconstruct how the taxonomy workload was sampled.

For example, these runs start from the same NetOps coordinate schedule:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --model model-a \
  --seed 20260820 \
  --count 100

taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --model model-b \
  --seed 20260820 \
  --count 100
```

The seed does **not** make remote completions, review decisions, retries, or the final accepted text identical. Those can still vary because model APIs are nondeterministic and different candidates may be rejected or require top-up sampling. Omit `--seed` when you want Taskgen to choose a fresh random seed automatically.

#### Use local references for factual adjudication

Put `.md`, `.txt`, `.json`, `.yaml`, or `.yml` reference files under one directory:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-key "$GENERATION_API_KEY" \
  --model fast/generator \
  --review-model strong/reviewer \
  --review-reference-dir references/vendor-docs \
  --adjudication-model strong/adjudicator \
  --count 500
```

References are retrieved only for claims marked `needs_verification`. A different adjudication endpoint follows the same rule as a different review endpoint: pass `--adjudication-api-base` and explicit `--adjudication-api-key` or `--adjudication-keyfile`.

The adjudication model is a **conditional second-stage fact checker**, not a second reviewer over every candidate:

1. The review model evaluates every candidate and returns `accept`, `revise`, `reject`, or `needs_verification`.
2. `accept`, `revise`, and `reject` do not invoke adjudication.
3. For `needs_verification`, Taskgen retrieves relevant excerpts from `--review-reference-dir` using the reviewer's verification queries.
4. The adjudication model sees the candidate, disputed claims, and retrieved evidence, then decides whether the evidence is sufficient for acceptance. Only an adjudication acceptance can publish that candidate.

If `--adjudication-model` is omitted, adjudication inherits the review model, endpoint, and credentials. Set it when a different model is better at evidence-grounded verification, or when adjudication must run through a separately audited provider. It does not replace human review for high-risk platform syntax, capacity, pricing, safety, or causal claims.

#### Extend an existing dataset

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --append-from taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c1000+240826-10-30/tasks.jsonl \
  --count 500
```

The source file is not changed. It is copied into the new run and loaded into the dedup index; success produces the existing records plus exactly 500 new accepted records.

#### Generate multilingual prompts

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --multilingual \
  --count 2000 \
  --workers 10
```

Languages are `en`, `de`, `fr`, `es`, `nl`, `zh`, `ar`, and `ru`. Semantic mode automatically selects `intfloat/multilingual-e5-small`.

#### Choose the right `--dedup-mode`

There are exactly two modes: `lexical` and `semantic`. Deduplication cannot be disabled, and both modes always perform global normalized exact matching first.

| Mode | Checks performed | Best fit | Trade-off |
|---|---|---|---|
| `lexical` | Global exact match, then word n-gram Jaccard within the same `(language, domain, subdomain)` bucket | Offline/air-gapped runs, fast smoke tests, or very large corpora where word overlap is sufficient | Cannot reliably catch paraphrases that use different wording |
| `semantic` | Everything in `lexical`, plus local embedding cosine similarity in the same bucket | Production dataset generation and paraphrase detection | Downloads/loads a local FastEmbed model and uses additional CPU, memory, and disk |

What each check is designed to catch:

- **Exact:** `"Restart  BGP after the change"` and `"restart bgp after the change"` normalize to the same lowercase, whitespace-collapsed text. Exact matching is global, even if record buckets differ.
- **Jaccard:** two longer prompts share most of the same word sequences, such as `"Investigate the BGP route leak after the scheduled maintenance window using the supplied routing table now"` and the same sentence ending in `today`.
- **Semantic:** two prompts express the same task with different words, such as `"Diagnose why BGP prefixes disappeared after maintenance"` and `"Find the cause of routes missing since the network change"`.

Use lexical mode when no embedding download is allowed:

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --model gpt-5.6-luna-max \
  --dedup-mode lexical \
  --count 250
```

Use semantic mode for the strongest local duplicate filtering. It is the default, but specifying it can make production scripts self-documenting:

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --model gpt-5.6-luna-max \
  --dedup-mode semantic \
  --semantic-threshold 0.90 \
  --semantic-model-cache data/model-cache \
  --count 1000
```

Thresholds are inclusive similarity cutoffs. Lowering `--jaccard-threshold` or `--semantic-threshold` drops more records as duplicates; raising one keeps more borderline records. For example, cosine `0.85` is more aggressive than `0.95`. Review samples from `rejected.jsonl` before changing thresholds for a production run.

#### Rotate keys and proxies, and stop at a priced budget

Key and proxy files contain one non-empty value per line.

```bash
taskgen generate \
  --keyfile secrets/generation-keys.txt \
  --review-keyfile secrets/review-keys.txt \
  --proxies secrets/proxies.txt \
  --input-price 0.20 \
  --output-price 0.60 \
  --review-input-price 1.00 \
  --review-output-price 2.00 \
  --budget 10.00 \
  --count 5000 \
  --workers 20
```

Prices are currency units per one million tokens. `--budget` is useful only when price flags are supplied; Taskgen checks priced spend before scheduling each new top-up wave. Proxy lines must use `host:port` or `host:port:user:pass`; blank lines and `#` comments are ignored. By default, proxies are used round-robin, while `--rotating-proxy` chooses one random sticky proxy for the run. Keyfiles take precedence over single-key flags or their environment variables.

#### Discover and rotate through OpenRouter free models

```bash
OPENAI_API_KEY="$OPENROUTER_API_KEY" taskgen generate \
  --free-models \
  --count 100
```

`--free-models` switches the generation endpoint to OpenRouter, discovers eligible free text models, and rotates through them. Pass an explicit `--review-model` if you do not want the reviewer to follow the selected free generation models.

#### Diagnostic generation without model review

```bash
taskgen generate \
  --api-key "$OPENAI_API_KEY" \
  --skip-review \
  --count 25
```

Schema, coordinate, safety, and dedup checks still run, but `reviews.jsonl` is empty. Do not use this mode to publish a training dataset.

### Important `generate` options

| Area | Options | Meaning |
|---|---|---|
| Generation provider | `--api-base`, `--api-key`, `--keyfile`, `-m/--model` | Endpoint, credentials, and model; this guide recommends passing `--model gpt-5.6-luna-max` explicitly |
| Volume | `-c/--count`, `--max-candidates` | Required new accepted rows; candidate ceiling defaults to `max(100, 20 × count)` |
| Concurrency | `-w/--workers`, `--review-workers`, `--review-requests-per-minute` | Enabled by default: independent generation/review worker pools plus an optional review/adjudication rate limit |
| Reliability | `--request-timeout-seconds`, `--connect-timeout-seconds`, `--max-repairs-per-coordinate` | Whole-request timeout, TCP connection timeout, and revise repairs (`0` or `1`, default `1`) |
| Taxonomy sampling | `--taxonomy`, `--seed`, `--distribution`, `--difficulty`, `--multilingual` | Omit `--taxonomy` for embedded IT Ops; pass a YAML for NetOps or another taxonomy; other flags control reproducible sampling |
| Generation prompt | `--system-prompt`, `--system-prompt-file`, `-t/--temperature`, `--max-output-tokens` | Prompt and completion controls |
| Review | `--review-model`, `--review-api-base`, `--review-api-key`, `--review-keyfile`, `--review-system-prompt`, `--review-system-prompt-file`, `--review-max-output-tokens`, `--skip-review` | Reviewer provider and policy controls |
| Adjudication | `--review-reference-dir`, `--adjudication-model`, `--adjudication-api-base`, `--adjudication-api-key`, `--adjudication-keyfile` | Conditional evidence checking for `needs_verification`, with optional model/provider overrides |
| Dedup | `--dedup-mode`, `--jaccard-threshold`, `--semantic-threshold`, `--dedup-ngram`, `--semantic-model`, `--semantic-model-cache` | `lexical` or `semantic` local uniqueness checks; semantic is the default |
| Run management | `--run-dir`, `--append-from` | Optional artifact-directory override and source dataset extension |
| Network | `--proxies`, `--rotating-proxy`, `--free-models` | Proxy and OpenRouter discovery controls |
| Cost | `--input-price`, `--output-price`, `--review-input-price`, `--review-output-price`, `--budget` | Per-million-token prices and a total run cap |

Defaults worth knowing:

| Setting | Default |
|---|---|
| Accepted count | `250` |
| Generation workers | `5` |
| Review workers | `5` |
| Request timeout | `600` seconds for GPT-5/o-series/Luna; `120` seconds otherwise; `--request-timeout-seconds` overrides it |
| TCP connect timeout | `15` seconds; `--connect-timeout-seconds` overrides it without changing the model-response deadline |
| Candidate ceiling | `max(100, 20 × count)` |
| Temperature | `0.9` |
| Reviewer endpoint/model/key | Inherit generation settings when the endpoint is unchanged |
| Repairs per coordinate | `1` (maximum allowed is `1`) |
| Dedup | Semantic; Jaccard `0.80`, cosine `0.90`, word n-gram `5` |
| Run directory | `taskgen/runs/<taxonomy>+<model>+c<count>+<ddmmyy-hh-mm>/`; falls back to `runs/...` when `./taskgen` is an existing file |

#### Override the automatic run directory

Most runs should omit `--run-dir`. Supply it only when an external workflow requires a specific destination; the directory must be new or empty:

```bash
taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --model gpt-5.6-luna-max \
  --count 100 \
  --run-dir /data/taskgen-runs/netops-release-001
```

#### How concurrency works

Concurrency is **on by default**; there is no enable switch.

- `--workers` is the maximum number of generation coordinate slots processed concurrently. Its default is `5`.
- `--review-workers` is the maximum number of candidate review pipelines processed concurrently. A `needs_verification` adjudication stays within that candidate's review pipeline. Its default is `5`.
- Generation and review overlap. As soon as a generated candidate is persisted to `candidates.jsonl`, it can enter review while later generation requests continue. With both defaults, Taskgen can have up to 5 generation items and 5 review pipelines in flight at the same time.
- `--review-requests-per-minute` is optional and is **not** a worker count. It limits the combined rate of review and adjudication HTTP requests, including retries and structured-output fallback requests. It does not limit generation requests.
- TCP connection establishment has its own 15-second default deadline. A connect timeout means no HTTP request reached the model endpoint; Taskgen retries it automatically with jittered exponential backoff so concurrent workers do not reconnect in lockstep.

For one local GPU serving both generation and review, start conservatively:

```bash
taskgen generate \
  --api-base http://localhost:8000/v1 \
  --api-key none \
  --model your-served-model \
  --workers 2 \
  --review-workers 2 \
  --count 100
```

If generation and review use separate providers, each pool can be tuned for its endpoint. For example, `--workers 20 --review-workers 5 --review-requests-per-minute 60` allows high generation parallelism while capping the reviewer/adjudicator to five concurrent pipelines and 60 total review-stage requests per minute. Higher values are not automatically faster: watch provider 429s, timeouts, GPU saturation, latency, and acceptance yield in `run.json`.

Prompt precedence is:

```text
--system-prompt
  > --system-prompt-file
  > taxonomy defaults.system_prompt_file
  > built-in prompt
```

Reviewer prompts follow the same order with `--review-system-prompt*`.

`--distribution` accepts comma-separated `category=weight` pairs. Every taxonomy category must appear exactly once and the weights must sum to `1.0`. `--difficulty` accepts `d1=weight,...,d10=weight` (or numeric keys); supplied weights must sum to `1.0`. For long distributions, editing the taxonomy YAML is easier and safer.

## 2. `taskgen upgrade`

Use this after the first manual installation to replace the currently running Taskgen executable with the latest official GitHub release:

```bash
taskgen upgrade
```

For a binary installed under `/usr/local/bin`, the containing directory is normally writable only by root:

```bash
sudo taskgen upgrade
```

The upgrade process:

1. Detects Linux ARM64 or macOS Apple Silicon and selects the corresponding release asset.
2. Downloads the latest `SHA256SUMS` and compares it with the current executable.
3. Stops immediately without downloading the binary when it is already current.
4. Streams a changed release into a temporary file beside the executable, with a 128 MiB safety limit.
5. Verifies the downloaded SHA-256 before changing the installation.
6. Sets executable permissions and atomically replaces the old binary. A failed download or checksum mismatch leaves the current executable untouched.

The command supports the same platforms as the prebuilt releases. Source-only x86_64 or Windows installations must still be updated by rebuilding. The first release containing `taskgen upgrade` must be installed manually once; all later releases can use the subcommand.

## 3. `taskgen review`

Use `review` to evaluate an existing candidate file with a new reviewer without regenerating prompts. It accepts either plain task-v2 JSONL records or the envelopes in a generation run's `candidates.jsonl`.

Unlike `generate`, standalone `review` always requires `--taxonomy`, including for IT Ops.

```bash
taskgen review \
  --input taskgen/runs/scogo-enterprise-netops-v2+teacher-model+c1000+240826-10-30/candidates.jsonl \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://reviewer.example.com/v1 \
  --api-key "$REVIEW_API_KEY" \
  --model strong/reviewer \
  --review-workers 10 \
  --run-dir data/runs/netops-review-002
```

For an IT Ops replay after a binary-only installation, download its taxonomy first:

```bash
curl -fsSL -o config/it-ops-taxonomy.yaml \
  https://raw.githubusercontent.com/ksingh-scogo/taskgen/master/docs/it-ops-taxonomy.yaml

taskgen review \
  --input taskgen/runs/scogo-itops-v4+gpt-5.6-luna-max+c1000+240826-10-30/candidates.jsonl \
  --taxonomy config/it-ops-taxonomy.yaml \
  --api-key "$REVIEW_API_KEY" \
  --model strong/reviewer \
  --run-dir data/runs/itops-review-002
```

The review run writes accepted rows to `tasks.jsonl`, all decisions to `reviews.jsonl`, rejections/errors to `rejected.jsonl`, calibration/telemetry to `run.json`, and live progress to `run.log`. It validates input schema and taxonomy coordinates before making review calls. It does not regenerate, repair, or deduplicate candidates.

### Bounded Phase-B source review

Phase-B mode turns `review` into a resumable exact-target gate over one complete, ordered private-HF source file. The Phase-B options are all-or-none: ordinary standalone review is unchanged when `--accepted-target` is absent.

```bash
taskgen review \
  --input data/full-source-population.jsonl \
  --taxonomy docs/netops-taxonomy.yaml \
  --accepted-target 100 \
  --run-id netops-phase-b-100 \
  --work-dir work/netops-phase-b-100 \
  --final-run-dir runs/netops-phase-b-100 \
  --source-repo-id ScogoAI/netops-prompt-seed \
  --source-revision 0123456789abcdef0123456789abcdef01234567 \
  --source-file part-3/tasks.jsonl \
  --source-selection unused-phase-b-100 \
  --source-exclusion-authority evidence/source-exclusion-authority.json \
  --api-key "$TASKGEN_REVIEW_API_KEY" \
  --model strong/reviewer
```

When the exclusion authority is non-empty, also pass `--historical-import-reservation` and repeat `--prior-evidence NAME=PATH` for every exact logical artifact named by the authority. A `pinned_external_legacy` release requires `prior_release_set.<run-id>`, `prior_canonical_tasks.<run-id>`, `prior_legacy_source_receipt.<run-id>`, `prior_taskgen_run.<run-id>`, `prior_taskgen_tasks.<run-id>`, and `prior_taskgen_reviews.<run-id>`.

The work directory contains an immutable credential-free `config.json` and fsynced hash-chained `stage.journal.jsonl`. Resume only with the same inputs and `--resume`; changed source, evidence, taxonomy, target, prompt, model, endpoint, reference corpus, or concurrency settings are rejected before provider setup. API keys and keyfile contents are deliberately excluded from the fingerprint so credentials can rotate.

Taskgen admits at most `target - accepted` concurrent rows. A genuine deterministic or model rejection opens one slot for the next unused source row. Provider or transport exhaustion leaves the row pending and stops without converting it to a quality rejection. A successful run publishes the final directory atomically only at exactly the accepted target with no pending work. It includes `tasks.jsonl`, `reviews.jsonl`, `candidates.jsonl`, `rejected.jsonl`, the complete `source_population.jsonl`, `source_receipt.json`, exclusion/history evidence, prior evidence, and a last-written `run.json`. Repeating the same successful invocation with `--resume` verifies these artifacts without constructing a provider or making API calls.

### Calibrate a reviewer against human labels

Create a JSONL file with one label per candidate:

```json
{"candidate_id":"candidate-sha256-or-record-id","expected_outcome":"accept"}
```

Valid rubric outcomes are `accept`, `revise`, `reject`, and `needs_verification`.

```bash
taskgen review \
  --input taskgen/runs/scogo-enterprise-netops-v2+teacher-model+c1000+240826-10-30/candidates.jsonl \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-key "$REVIEW_API_KEY" \
  --model strong/reviewer \
  --gold-labels data/review-gold.jsonl \
  --run-dir data/runs/netops-calibration-001
```

`run.json` includes the confusion matrix, per-outcome precision/recall, false-accept rate, false-reject rate, invalid-response rate, and adjudication rate.

Key `review` options are `--input`, `--taxonomy`, provider/model/key flags, `--system-prompt*`, `--max-output-tokens`, `--review-workers`, `--review-requests-per-minute`, `--review-reference-dir`, adjudicator overrides, `--gold-labels`, and `--run-dir`. The review API key can come from `TASKGEN_REVIEW_API_KEY`; the adjudication key can come from `TASKGEN_ADJUDICATION_API_KEY`.

## 4. `taskgen dedup`

Use `dedup` for an existing JSONL dataset. It does not require a taxonomy or an API key.

It supports the same two modes used during generation: `lexical` runs exact plus Jaccard checks, while the default `semantic` mode adds local embedding cosine checks. In both modes, malformed records and records missing the configured prompt field are written to the dropped file with `_dedup.reason: "invalid_record"`.

```bash
taskgen dedup \
  --input data/raw.jsonl \
  --output data/raw.dedup.jsonl \
  --dropped data/raw.dropped.jsonl \
  --report data/raw.dedup-report.json
```

If `--output` and `--dropped` are omitted, the defaults are `<input-stem>.dedup.jsonl` and `<input-stem>.dropped.jsonl` beside the input. The report is written only when `--report` is supplied.

Use another prompt field or lexical-only mode like this:

```bash
taskgen dedup \
  --input data/external.jsonl \
  --prompt-field instruction \
  --dedup-mode lexical
```

Run full semantic deduplication explicitly like this:

```bash
taskgen dedup \
  --input data/raw.jsonl \
  --dedup-mode semantic \
  --semantic-threshold 0.90 \
  --semantic-model-cache data/model-cache
```

Deduplication applies:

- Global normalized exact matching.
- Word n-gram Jaccard matching inside `(language, domain, subdomain)` buckets; default n-gram `5`, threshold `0.80`.
- Local FastEmbed cosine matching in the same buckets in semantic mode; default threshold `0.90`.

Dropped or invalid records receive `_dedup` metadata with their reason, line, score, threshold, and bucket when applicable. Writes are atomic. Existing destinations are refused unless `--overwrite` is present.

All options are `--input`, `--output`, `--dropped`, `--report`, `--prompt-field`, `--dedup-mode`, `--jaccard-threshold`, `--semantic-threshold`, `--dedup-ngram`, `--semantic-model`, `--semantic-model-cache`, and `--overwrite`.

## 5. `taskgen taxonomy validate`

Use this before generation whenever a taxonomy YAML has been added or edited.

```bash
taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

Validation checks the `scogo.taskgen.taxonomy.v3` compositional structure, IDs, references, weight totals, eligible combinations, platform capabilities, and whether configured sampling distributions are reachable. It makes no API calls and writes no files.

Current bundled inventory:

| Taxonomy | Categories | Domains | Subdomains |
|---|---:|---:|---:|
| IT Operations v4 | 14 | 129 | 884 |
| Enterprise NetOps v2 | 1 | 25 | 531 |

Every task composes category, domain, subdomain, task family, environment, platform scope, platforms, incident mechanism, evidence condition/bundle, action risk, difficulty, and presentation. The compiler rejects combinations outside the taxonomy's inherited capability set.

## 6. `taskgen atif export`

ATIF conversion is for **completed teacher trajectories later in the pipeline**, not the prompt seeds produced directly by `generate`.

Export one canonical audit JSON record to ATIF-v1.7:

```bash
taskgen atif export \
  --input data/audit-record.json \
  --output data/trajectory.atif.json
```

Export a multi-record JSONL file:

```bash
taskgen atif export \
  --input data/audit-records.jsonl \
  --output data/trajectories.atif.jsonl
```

## 7. `taskgen atif import`

Import external ATIF-v1.7 trajectories into canonical audit records:

```bash
taskgen atif import \
  --input data/external.atif.jsonl \
  --output data/external.audit.jsonl
```

Imported trajectories are marked `external_atif_unverified` and are not accepted for SFT until independently replayed and verified.

For both ATIF commands, the container is inferred from the **input** extension (`.json` or `.jsonl`). Use `--container json` or `--container jsonl` when the extension is different. A JSON container must contain exactly one record; JSONL supports multiple records. Output is atomic, and an existing destination requires `--overwrite`.

## What counts as an accepted prompt?

Every row in a successful `tasks.jsonl`:

- Validates against [`schemas/task-v2.schema.json`](schemas/task-v2.schema.json).
- Uses legal coordinates from the selected taxonomy.
- Passes deterministic safety and fixture checks.
- Is unique under exact and lexical checks, plus semantic checks in the default mode.
- Has an `accept` review outcome, or a `needs_verification` review followed by an `accept` adjudication.

The reviewer uses the v3 rubric for coordinate realization, internal consistency, operational quality, safety, and authenticity. Its outcomes are `accept`, `revise`, `reject`, and `needs_verification`; uncertainty is not silently converted to a technical failure, and malformed review responses or provider errors are never implicit accepts.

Structured review requests negotiate provider capabilities: strict JSON Schema first where supported, then JSON object, then prompt-only JSON. Final responses still pass Taskgen's canonical schema and policy validation. Provider reasoning traces are never written to the dataset.

Example accepted task-v2 record:

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

`language` is added when `--multilingual` is enabled.

## Operational notes

- Credentials are never written to artifacts. `run.json` records only sanitized endpoint origins.
- Generation uses `OPENAI_API_KEY`; standalone review or review overrides use `TASKGEN_REVIEW_API_KEY`; adjudication uses `TASKGEN_ADJUDICATION_API_KEY`.
- A separate review/adjudication endpoint requires separate credentials. Credentials are inherited only when the endpoint is unchanged.
- Default reviewer model and endpoint are the generation model and endpoint. This is convenient for testing, not the strongest release gate.
- `--review-requests-per-minute` limits the shared review/adjudication request budget and counts retries and structured-output fallback attempts.
- Semantic embeddings run locally; prompt contents are not sent to an embedding service.
- `candidates.jsonl`, `reviews.jsonl`, `rejected.jsonl`, and partial accepted rows are visible during a live run. `tasks.jsonl` is the atomic success marker.
- GPT-5, o-series, and Luna models omit unsupported sampling fields. Qwen and DeepSeek-v4 use bounded direct-output controls. The installed binary's `--help` remains authoritative for flags.

## Troubleshooting

| Symptom | What to do |
|---|---|
| `generation API key is required` | Set `OPENAI_API_KEY`, pass `--api-key`, or use `--keyfile`; local servers still need a non-empty placeholder such as `none` |
| Reviewer endpoint differs from generation endpoint | Add `--review-api-key` or `--review-keyfile` |
| A `[TIMEOUT]` line appears but the run continues | This is a retryable failure, not a terminal run failure. `connect_timeout` means TCP connection establishment failed before reaching the model; `request_timeout` means an established request exceeded its deadline. Taskgen retries with jittered backoff and fails only after active candidates exhaust their retry budgets. |
| `run directory is not empty` | Choose a new `--run-dir`; Taskgen never merges into a non-empty run directory |
| Semantic model cannot download | Pre-populate `--semantic-model-cache` or use `--dedup-mode lexical` |
| Candidate limit exhausted | Inspect `rejected.jsonl` and `run.json`, improve the generator/reviewer setup, or raise `--max-candidates` deliberately |
| No `tasks.jsonl` after failure/interruption | Use sidecars for diagnosis; accepted rows remain in `accepted.partial.jsonl` and were intentionally not published |
| ATIF/dedup output already exists | Choose another destination or pass `--overwrite` after checking the target |

## Contributing

Run all local quality gates before opening a change:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
