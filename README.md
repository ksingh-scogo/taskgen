# taskgen

A fast, concurrent SFT (Supervised Fine-Tuning) task generator for distillation datasets. Generates diverse, difficulty-weighted IT Ops / SRE prompts across infra, observability, network, secops, identity, OEM/ISV products, and related domains — via any OpenAI-compatible API.

## Features

- 129 domains, 884 subdomains across 14 IT Ops categories (from `docs/it-ops-taxonomy.yaml`)
- Vendor-first `oem` category: product lines as subdomains, SKU/firmware/CLI/TAC prompt voice
- Weighted difficulty sampling (1–10 scale)
- Configurable category distribution
- Concurrent generation with lock-free atomic stats and pre-sampled task batches
- Live progress bar with speed, token count, and error tracking
- OpenAI-compatible API (works with OpenAI, Together, Mistral, local vLLM, etc.)
- Free model discovery via OpenRouter with automatic health checks and periodic rescanning
- Proxy support with round-robin or sticky rotation
- Multiple API key rotation for load balancing across routers
- Post-run deduplication (exact + semantic similarity via word-trigram Jaccard)
- Graceful shutdown on API outages (5+ timeouts) or billing errors
- Automatic retry with exponential backoff on rate limits (429)
- JSONL output with metadata per task, flushed to disk after each write
- Optional budget cap with per-token cost tracking
- Append mode to resume interrupted runs
- Auto-generated dataset README next to the JSONL (`{stem}.README.md`) with **observed** category / domain / subdomain / difficulty counts vs target weights
- Filter existing Hub tasklists to selected categories (`filter_tasklist.py`)
- GPT-5 / o-series / luna sampling: omit `temperature` and `max_tokens`, send `max_completion_tokens`
- Always request `stream: false`; if a gateway still returns SSE, assemble `delta.content` across chunks

## Install

```bash
git clone https://github.com/ksingh-scogo/taskgen.git
cd taskgen
cargo build --release
```

Binary will be at `target/release/taskgen`. Fork of [empero-org/taskgen](https://github.com/empero-org/taskgen), retargeted at IT Ops / SRE prompts.

Python tooling (taxonomy codegen + Hub filter):

```bash
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

## Usage

```bash
taskgen [OPTIONS]
```

### Required

| Flag | Env | Description |
|---|---|---|
| `--api-key <KEY>` | `OPENAI_API_KEY` | API key for the target provider (not needed if using `--keyfile`) |

### Options

| Flag | Default | Description |
|---|---|---|
| `--api-base <URL>` | `https://api.openai.com/v1` | API base URL |
| `-m, --model <MODEL>` | `gpt-4o-mini` | Model to use |
| `-c, --count <N>` | `250` | Number of tasks to generate |
| `-w, --workers <N>` | `5` | Concurrent workers |
| `-o, --output <FILE>` | `output.jsonl` | Output file path |
| `-t, --temperature <F>` | `0.9` | Sampling temperature |
| `--append` | — | Append to existing output file |
| `--distribution <STR>` | see Domains | Category weights (`cat=w,cat=w`, must sum to ~1.0) |
| `--difficulty <STR>` | bell curve | Difficulty weights (see below) |
| `--multilingual` | — | Generate tasks in 8 languages and split output by language |
| `--system-prompt <STR>` | built-in | Override the system prompt. `oem` tasks still get the product-voice addendum |
| `--input-price <F>` | — | Input token price per 1M tokens (for cost tracking) |
| `--output-price <F>` | — | Output token price per 1M tokens |
| `--budget <F>` | — | Hard cost cap in USD (requires price flags) |

### Proxy & Key Rotation

| Flag | Default | Description |
|---|---|---|
| `--proxies <FILE>` | — | Proxy list file, one per line: `host:port` or `host:port:user:pass` |
| `--rotating-proxy` | — | Use a single random proxy for all requests (sticky mode) |
| `--keyfile <FILE>` | — | API key file, one key per line, rotated round-robin |

### Free Models (OpenRouter)

| Flag | Default | Description |
|---|---|---|
| `--free-models` | — | Auto-discover and use free models from OpenRouter |
| `--free-rescan <MIN>` | `10` | Rescan interval in minutes for free model availability |

When `--free-models` is set, taskgen will:
1. Override `--api-base` to `https://openrouter.ai/api/v1`
2. Fetch all available models and filter for free, text-capable models with 16k+ context
3. Health-check each candidate with a test request (429 = live, 502/timeout = offline)
4. Rotate verified models round-robin across tasks
5. Track per-model failures — if a model errors 3+ times, it triggers an immediate rescan
6. Periodically rescan on `--free-rescan` interval to pick up newly available models

Each task records the actual model name in the `taskgen_model` metadata field.

### Multilingual

When `--multilingual` is set, each task is randomly assigned one of 8 languages:

| Code | Language |
|---|---|
| `en` | English |
| `de` | German |
| `fr` | French |
| `es` | Spanish |
| `nl` | Dutch |
| `zh` | Chinese |
| `ar` | Arabic |
| `ru` | Russian |

The LLM is instructed to write the task in the assigned language. A `"language"` field is added to each JSON entry's metadata. After generation (and dedup if enabled), the output is split into per-language files:

```
output_en.jsonl
output_de.jsonl
output_fr.jsonl
...
```

The generated dataset README includes a language distribution table with per-language task counts.

### Deduplication

| Flag | Default | Description |
|---|---|---|
| `--dedup` | — | Run deduplication after generation |
| `--dedup-threshold <F>` | `0.6` | Semantic similarity threshold (0.0–1.0) |

Two-pass dedup:
1. **Exact match** — normalized (lowercase, whitespace-collapsed) string comparison
2. **Semantic match** — word-trigram Jaccard similarity, removes entries above the threshold

### Error Handling

- **429 Rate Limits** — exponential backoff with up to 5 retries, respects `Retry-After` header
- **Billing Errors** (402, `insufficient_quota`, etc.) — immediate graceful shutdown
- **Timeouts** — retries with backoff; 5 consecutive timeouts trigger graceful shutdown
- **Graceful Shutdown** — all workers drain, completed tasks are saved, dedup runs if enabled, dataset README is written

### Provider notes (GPT-5 / Omniroute)

Requests always include `"stream": false`. Some gateways (Omniroute included) still return SSE `chat.completion.chunk` events; taskgen concatenates `delta.content` across chunks instead of keeping only the last (often empty) usage event.

For model names matching `gpt-5`, `luna`, `o1`, `o3`, or `o4` (including `scogoai/gpt-5.6-luna-max`):

- `temperature` and `max_tokens` are omitted (those models reject them)
- `max_completion_tokens` is sent instead of `max_tokens`
- `--temperature` is still recorded in JSONL metadata; it is not forwarded to the API

Content parts, null usage fields, and `prompt_tokens`/`input_tokens` aliases are accepted. API error payloads are surfaced instead of a generic parse failure.

## Examples

**Basic — generate 500 tasks with GPT-4o-mini:**
```bash
taskgen --api-key $OPENAI_API_KEY -c 500
```

**Free models via OpenRouter (no cost):**
```bash
taskgen --free-models --api-key $OPENROUTER_KEY -c 5000 -w 10
```

**Free models with faster rescan and dedup:**
```bash
taskgen --free-models --api-key $OPENROUTER_KEY -c 10000 -w 20 \
  --free-rescan 5 --dedup --dedup-threshold 0.5
```

**Multilingual dataset — tasks in 8 languages:**
```bash
taskgen --api-key $OPENAI_API_KEY -c 2000 -w 10 --multilingual --dedup
```

**Local vLLM / Ollama:**
```bash
taskgen --api-base http://localhost:8000/v1 --api-key none -m mistral-7b-instruct -c 1000 -w 10
```

**Together AI with cost tracking and budget cap:**
```bash
taskgen \
  --api-base https://api.together.xyz/v1 \
  --api-key $TOGETHER_API_KEY \
  -m meta-llama/Llama-3-8b-chat-hf \
  -c 2000 -w 20 \
  --input-price 0.20 --output-price 0.20 \
  --budget 1.00
```

**Omniroute / GPT-5-class (no `temperature` / `max_tokens` on the wire):**
```bash
taskgen \
  --api-base https://omniroute.scogo.ai/v1 \
  --api-key $OPENAI_API_KEY \
  -m scogoai/gpt-5.6-luna-max \
  -c 250 -w 5 \
  -o data/itops.jsonl
```

**With proxies and multiple API keys:**
```bash
taskgen \
  --api-key none \
  --keyfile keys.txt \
  --proxies proxies.txt \
  -c 5000 -w 20
```

**Custom distribution — 40% infra, 30% observe, 30% network:**
```bash
taskgen --api-key $OPENAI_API_KEY --distribution "infra=0.4,observe=0.3,network=0.3" -c 500
```

**OEM / ISV only — vendor-product prompts (FortiGate, EKS, Falcon, …):**
```bash
taskgen --api-key $OPENAI_API_KEY --distribution "oem=1.0" -c 250 -o data/oem.jsonl
```

**Custom difficulty — only hard tasks (levels 7–10):**
```bash
taskgen --api-key $OPENAI_API_KEY --difficulty "7=0.25,8=0.25,9=0.25,10=0.25" -c 500
```

**Append mode — resume a previous run:**
```bash
taskgen --api-key $OPENAI_API_KEY -c 1000 --append -o my_dataset.jsonl
```

## Output Format

Each line in the JSONL file is a self-contained task record:

```json
{
  "prompt": "zabbix is paging every 30s on the same host, I already restarted the agent, still flapping—mute or real disk?",
  "domain": "infra::Storage",
  "subdomain": "raid_degrade",
  "difficulty": 4,
  "language": "en",
  "taskgen_model": "gpt-4o-mini",
  "temperature": 0.9
}
```

Capability categories use failure-mode subdomains (`raid_degrade`). `oem` uses product lines:

```json
{
  "prompt": "FortiGate 200F HA split-brain after 7.4.5, FGSP config sync stuck, TAC wants diagnose debug flow before we fail over—cut or wait?",
  "domain": "oem::Fortinet",
  "subdomain": "fortigate",
  "difficulty": 6,
  "taskgen_model": "scogoai/gpt-5.6-luna-max",
  "temperature": 0.9
}
```

The `language` field is only present when `--multilingual` is used.

A dataset README is written next to the JSONL as `{stem}.README.md` (for `-o data/itops.jsonl` that is `data/itops.README.md`). It does **not** overwrite this project `README.md`. The card reports run parameters, token usage, cost, and **observed** mix (category / `category::domain` / subdomain / difficulty counts vs the sampling targets). JSONL and `{stem}.README.md` under `data/` are gitignored.

## Filter Hub datasets

`filter_tasklist.py` keeps rows whose `domain` prefix (or full cell) matches `--keep-categories` and writes UTF-8 JSONL. Auth via `HF_TOKEN` or `hf auth login` (`huggingface-cli` is deprecated).

```bash
.venv/bin/python filter_tasklist.py \
  --dataset empero-ai/tasklist-haiku4.5-6000x-unfiltered \
  --keep-categories coding,cs,Conversation \
  --output ./data/tasklist-haiku45
```

Optional: `--category-column domain`, `--split all`, `--max-rows N`, `--push-to-hub ORG/REPO --private`.

Current filtered mix (coding / cs / conversation, `language` backfilled to `en` on non-Grok shards) is the private Hub dataset [ScogoAI/synthetic-sft-prompts-coding-cs-conversation](https://huggingface.co/datasets/ScogoAI/synthetic-sft-prompts-coding-cs-conversation) (34,209 rows). Card lives with the JSONL on the Hub.

```python
from datasets import load_dataset
ds = load_dataset("ScogoAI/synthetic-sft-prompts-coding-cs-conversation", token=True)
```

Publish a local `data/` folder (needs the `hf` CLI from `huggingface_hub`, not `huggingface-cli`):

```bash
.venv/bin/hf upload ScogoAI/synthetic-sft-prompts-coding-cs-conversation data/ . \
  --repo-type dataset --private
```

## Domains

Source of truth: `docs/it-ops-taxonomy.yaml`. Regenerates both the Rust `DOMAINS` catalog and `DEFAULT_DISTRIBUTION`:

```bash
.venv/bin/python scripts/codegen_domains.py --write
```

Default `--distribution` is biased toward the Scogo desk (tickets, endpoints, identity, workplace), not APM/SASE volume and not CRM/HR/ESM. Weights sum to 1.0.

Two sampling axes share the same 3-level schema (`category` → `domain` → `subdomain`):

| Axis | Categories | Subdomain meaning | Example |
|---|---|---|---|
| Capability | everything except `oem` | failure mode | `network::Firewall` / `unused_rule` |
| Vendor / platform | `oem` | product line | `oem::Fortinet` / `fortigate` |

Overlap is intentional. A generic firewall ticket and a FortiGate TAC ticket train different muscle. Do not dedup across categories.

| Category | Weight | Domains |
|---|---|---|
| `infra` | 0.12 | Cloud Infrastructure, FinOps, CNAPP, Virtualization, Storage, Backup, BCDR Continuity, DCIM Facilities |
| `endpoint` | 0.11 | RMM, UEM MDM, VDI DaaS, Endpoint Health |
| `itsm` | 0.10 | Service Desk, Incident Management, Problem Management, Change Enablement, Request Catalog, CMDB Configuration, Knowledge Management, Task Project Management, SLA Measurement |
| `identity` | 0.10 | Identity Access, Privileged Access, Identity Governance, Directory Services |
| `oem` | 0.10 | 33 named vendors + 14 long-tail buckets (see below) |
| `workplace` | 0.08 | Collaboration Messaging, Email Communication, Calendar Scheduling, Document Management, Content Website, Print Workplace Devices, UCaaS Voice, Digital Experience |
| `network` | 0.08 | Networking, DNS CDN, Firewall, Load Balancer, Network Management, Routers, SD-WAN, Wireless, NAC |
| `secops` | 0.07 | SIEM, SOAR, EDR XDR, Vulnerability Management, Threat Intel NDR, GRC Audit, Forensics IR |
| `observe` | 0.06 | Monitoring, Observability APM, AIOps, Synthetics DEM, AI Agent Observability |
| `delivery` | 0.06 | DevOps, Kubernetes, IaC GitOps, Release Orchestration, AppSec ASPM, Mainframe Midrange |
| `data` | 0.05 | Database, Analytics, Messaging Streaming, iPaaS API, Data Governance |
| `agentic` | 0.03 | Agent Fabric, SIA Guardrails, Knowledge Graph, Channels Knowledge, Platform Deploy |
| `secure_edge` | 0.03 | SASE SSE, CASB, Data Loss Prevention, Email Security, Web Security, WAF DDoS, DSPM, SSPM |
| `enterprise` | 0.01 | CRM Sales, HR Payroll, ERP Finance, Supplier Contract |

### OEM / ISV / Platform

47 domains, 386 product-line subdomains. Named vendors are specialist portfolios (Cisco, AWS, CrowdStrike). Buckets hold long-tail brands (Veeam, Grafana, Jamf). Kubernetes and Linux distros count as platforms even though they are not companies.

`oem` generation appends a product-voice addendum to the system prompt (even if `--system-prompt` is set) and asks for SKU / firmware / CLI / console / TAC / license language. It must not emit a generic capability ticket.

| Group | Named vendors | Long-tail buckets |
|---|---|---|
| Hyperscale cloud | AWS, Azure, Google Cloud | Other Cloud (`oci`, `ibm_cloud`, `alibaba`, `digitalocean`, `akamai_linode`, `ovh`, `hetzner`) |
| OS & distros | Microsoft, IBM, Red Hat, Canonical | Linux Distros (`debian`, `rocky`, `alma`, `oracle_linux`, `amazon_linux`, `sles`, `freebsd`) |
| Containers | Kubernetes | — |
| Network OEM | Cisco, Juniper, Fortinet, Versa, Palo Alto, HPE Aruba | Other Network OEM (`arista_eos`, `extreme`, `ubiquiti`, `mikrotik`, `f5_bigip`, `checkpoint`, `nokia_srlinux`, `sonicwall`, `ruckus`, `infoblox`) |
| Secure edge | Cloudflare, Zscaler | Other SASE (`netskope`, `cato`, `forcepoint`, `skyhigh`, `perimeter81`) |
| Security & identity | CrowdStrike, CyberArk, Okta | Other Security ISV (`sentinelone`, `tenable`, `qualys`, `rapid7`, `proofpoint`, `mimecast`, `trend_micro`, `sophos`, `tanium`, `darktrace`); Other Identity ISV (`ping`, `sailpoint`, `beyondtrust`, `delinea`, `forgerock`, `jumpcloud`) |
| Compute & storage | Dell, HPE, NetApp, Pure Storage | Other Compute OEM (`lenovo_thinksystem`, `supermicro`, `hitachi_vantara`) |
| Virtualization | VMware, Nutanix | Other Hypervisor (`proxmox`, `citrix_cvad`, `kvm_libvirt`, `oracle_virtualization`) |
| Data & databases | Oracle | Database ISVs (`postgresql`, `mysql`, `mariadb`, `mongodb`, `redis`, `elasticsearch`, `snowflake`, `databricks`, `cockroach`, `cassandra`, `neo4j`, `clickhouse`) |
| Observability | Datadog, Splunk | Other Observability (`dynatrace`, `new_relic`, `grafana`, `zabbix`, `elastic`, `pagerduty`, `solarwinds`, `manageengine`, `prometheus`) |
| Backup & DR | — | Backup ISVs (`veeam`, `rubrik`, `cohesity`, `commvault`, `veritas`, `acronis`, `datto`, `druva`) |
| ITSM & workplace SaaS | ServiceNow, Atlassian, Google Workspace | Workplace SaaS (`slack`, `zoom`, `box`, `dropbox`, `adobe`, `freshservice`, `zendesk`, `bmc_helix`) |
| Endpoint / RMM / UEM | — | RMM UEM ISVs (`jamf`, `ninjaone`, `connectwise`, `kaseya`, `datto_rmm`, `omnissa`, `addigy`, `hexnode`) |
| DevOps & IaC | GitHub, HashiCorp | Other DevOps ISV (`gitlab`, `jfrog`, `harness`, `circleci`, `pulumi`, `argo`, `flux`, `jenkins`, `azure_devops`, `docker`) |

Product lines live on each domain in the YAML (e.g. Fortinet → `fortigate`, `fortimanager`, `fortisase`; AWS → `ec2`, `eks`, `rds`). Edit the YAML, then re-run codegen.

## Difficulty Scale

| Level | Label |
|---|---|
| 1 | Very Easy (junior on-call) |
| 2 | Easy (runbook exists) |
| 3 | Basic (one failing check) |
| 4 | Intermediate (mid SRE) |
| 5 | Standard (incomplete metrics) |
| 6 | Skilled (senior, SLO tradeoffs) |
| 7 | Proficient (blast-radius / freeze) |
| 8 | Advanced (principal, multi-system) |
| 9 | Expert (unknown-unknown) |
| 10 | Principal (no runbook, synthesis) |
