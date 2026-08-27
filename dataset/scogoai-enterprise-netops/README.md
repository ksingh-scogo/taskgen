---
pretty_name: Scogo AI Sovereign Enterprise NetOps — Prompt Seeds
license: other
license_name: proprietary-scogo-internal
language:
  - en
size_categories:
  - 10K<n<100K
annotations_creators:
  - machine-generated
language_creators:
  - machine-generated
source_datasets:
  - original
task_categories:
  - text-generation
tags:
  - synthetic
  - networking
  - netops
  - enterprise-it
  - incident-response
  - troubleshooting
  - prompt-seeds
  - agentic
configs:
  - config_name: default
    data_files:
      - split: part_1
        path: part-1/tasks.jsonl
      - split: part_2
        path: part-2/tasks.jsonl
      - split: part_3
        path: part-3/tasks.jsonl
---

# Scogo AI Sovereign Enterprise NetOps — Prompt Seeds

12,510 synthetic enterprise network-operations task prompts, sampled from the compositional taxonomy `scogo-enterprise-netops-v2`.

Each record is a self-contained fictional operational scenario — an incident ticket, on-call chat, CLI session, config review, change request, war room, audit, architecture review, or automation brief — with embedded evidence fixtures (configs, logs, telemetry, packet summaries, routing tables, topology) and a task instruction.

**These are prompts only.** No reference answers, no assistant messages, no tool results, no ground-truth labels.

## What this is not

- Not usable for supervised fine-tuning on its own. There are no targets.
- Not a record of any real network, device, or incident. Every fixture is invented.
- Not human-reviewed. Quality was judged by LLM review and LLM adjudication only.
- Not a train/validation/test split. The three splits are generation cohorts.

## Splits

The splits are **generation cohorts**, not evaluation splits. Each is one generator/reviewer configuration produced in its own window.

| Split | Records | Generator | Reviewer | Adjudication | Assurance |
|---|---|---|---|---|---|
| `part_1` | 3,500 | `deepseek-v4-flash-0731` | `qwen/qwen3.8-max` (3,000) + self-review (500) | no | weakest |
| `part_2` | 4,010 | `cx/gpt-5.6-luna-max` | `cx/gpt-5.6-sol-xhigh` | yes (90 calls) | strongest |
| `part_3` | 5,000 | `deepseek-v4-flash-0731` | `qwen38-nvfp4` | yes (4,260 calls) | mixed |

Assurance is **not uniform**. Part 1 contains 500 records reviewed by the same model that wrote them. Part 3's reviewer returned `needs_verification` on roughly 41% of candidates, so adjudication using the same model family made the effective call. Filter or weight by split when assurance matters.

## Schema

Every record conforms to `scogo.taskgen.task.v2`. Key order is stable across all 12,510 records.

```json
{
  "schema_version": "scogo.taskgen.task.v2",
  "prompt": "string, 948-5262 chars",
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
    "incident_mechanism": "policy_conflict",
    "evidence_condition": "partial",
    "evidence_bundle": "routing_tables",
    "action_risk": "read_only_investigation",
    "presentation": "incident_ticket"
  },
  "taskgen_model": "deepseek-v4-flash-0731",
  "temperature": 0.9
}
```

`coordinates` is the compositional taxonomy coordinate the prompt was generated to realize. `platform_scope` constrains `platforms` cardinality to 0, 1, or 2 respectively.

`evidence_condition` values other than `sufficient` are deliberate — `partial`, `contradictory`, `stale`, and `missing_live_state` exist to test abstention and escalation rather than answer extraction.

## Composition

**Difficulty** (1-10, mean 6.09, median 6): skews harder than the taxonomy target. Difficulty 1 is nearly absent (38 records).

**25 domains, 527 subdomains.** Largest: `cloud_hybrid_networking` (868), `firewall_network_security` (852), `network_observability` (768). Thinnest: `enterprise_realtime_networking` (111), `packet_protocol_foundations` (117).

**7 task families:** troubleshooting_rca (3,015), telemetry_config_log_interpretation (2,567), tool_selection_next_best_action (1,934), config_iac_review_repair (1,822), abstention_uncertainty_escalation (1,359), change_approval_verification_rollback (1,198), architecture_capacity_migration_optimization (615).

**Action risk:** read_only_investigation (6,796), advisory_plan_only (2,551), approval_gated_change (1,889), staging_or_simulation (906), emergency_change_decision (368).

**101 vendor/platform identifiers** across 9,505 mentions. 42.5% of records are `platform_neutral`.

Full per-axis counts are in `metadata.json` at the repo root; per-split counts are in each `part-N/metadata.json`.

## Splitting this data — read before you shuffle

**Do not split randomly.**

Across the corpus there are 9,903 distinct coordinate tuples for 12,510 records. **2,078 tuples repeat, covering 4,685 records (37.4%).** Parts 2 and 3 used the same coordinate seed (20260820), so 1,645 tuples appear in both.

Prompt text is never duplicated — the corpus-wide near-duplicate scan (3,725,164 within-domain pairs, 5-gram Jaccard) found a maximum similarity of **0.0835**, with nothing at or above 0.30. But records sharing a coordinate cover the same scenario shape. A random split leaks topically related items across the boundary.

Group by the full coordinate tuple:

```python
def group_key(rec):
    c = rec["coordinates"]
    return (rec["domain"], rec["subdomain"], c["task_family"], c["environment"],
            c["incident_mechanism"], c["evidence_condition"], c["evidence_bundle"],
            c["action_risk"], c["presentation"], c["platform_scope"],
            tuple(sorted(c["platforms"])))
```

## Loading

```python
from datasets import load_dataset

ds = load_dataset("<org>/scogoai-enterprise-netops")   # private: needs an auth token
print(ds)          # part_1, part_2, part_3

high_assurance = ds["part_2"]
everything = concatenate_datasets([ds["part_1"], ds["part_2"], ds["part_3"]])
```

Records carry no split field. If you concatenate, add provenance yourself:

```python
ds = ds.map(lambda r, split=name: {**r, "_split": split})
```

## Provenance

Generated by `taskgen` (Rust, versions 0.5.0 / 0.5.1 / 0.5.8) across 7 successful runs between 2026-08-20 and 2026-08-26, roughly 81 wall-clock hours.

Pipeline per candidate: sample a taxonomy coordinate → generate prompt → schema validation → local deduplication (5-gram Jaccard ≥ 0.80, embedding cosine ≥ 0.90 via `all-MiniLM-L6-v2`) → rubric review → optional claim-level adjudication. Only `accept` outcomes are here.

Rejection reasons observed across runs: `internal_contradiction`, `technical_inaccuracy`, `coordinate_mismatch`, `protocol_or_architecture_error`, `invalid_command_or_syntax`, `invented_platform_feature`, `hidden_answer_or_solution_leakage`, `unsupported_causality`, `insufficient_or_invalid_evidence`, `numerical_or_temporal_inconsistency`, `ambiguous_or_unanswerable`, `unsafe_or_unapproved_change`, `not_operational`.

## Validation

Re-run over the concatenated corpus, not inherited from the parts:

- 12,510 records checked against the schema. **0 failures.**
- 12,510 unique prompts. 0 exact duplicates.
- Difficulty inside per-`task_family` bounds everywhere.
- `platform_scope` ↔ `platforms` cardinality holds in every record.
- Max lexical similarity between any two same-domain prompts: **0.0835**.

## Privacy and safety

Fully synthetic. Of 31,042 IPv4 mentions, 30,666 are private, documentation, or reserved; the 376 globally-routable ones are public infrastructure anchors (8.8.8.8, 1.1.1.1, Azure 168.63.129.16, GCP health-check ranges). 73 email-like strings, all on RFC 2606 reserved domains or generic placeholders. No personal data found.

Credential-shaped strings in the config fixtures are synthetic. Do not treat any token in this corpus as valid.

Vendor behaviours are model-generated. An `invented_platform_feature` rejection reason gated them during review, but nothing was checked against vendor documentation. Treat platform specifics as plausible, not authoritative.

## Known limitations

1. Assurance differs by split — see the table above.
2. Two generator models cover the corpus unevenly: `deepseek-v4-flash-0731` wrote 8,500 records (parts 1 and 3), `cx/gpt-5.6-luna-max` wrote 4,010 (part 2). Parts 1 and 3 share that model's biases.
3. Parts 2 and 3 share coordinate seed 20260820 — 1,645 tuples in common.
4. 37.4% of records share a coordinate with another record. Group before splitting.
5. Temperature is uneven: 9,600 records at 0.9, 2,910 at 0.4 (all part 1).
6. Axis distributions drift from taxonomy target weights; coordinate replacement on rejection reshuffles the sampler.
7. Difficulty skews hard; difficulty 1 is nearly absent.
8. Prompts are unlabelled. Nothing validates the sampled `difficulty` integer.
9. No human review pass on any part.

## Files

```
metadata.json              consolidated metadata for the whole collection
README.md                  this card
part-1/tasks.jsonl         3,500 records   10.4 MiB
part-1/metadata.json
part-2/tasks.jsonl         4,010 records   12.2 MiB
part-2/metadata.json
part-3/tasks.jsonl         5,000 records   15.0 MiB
part-3/metadata.json
```

Checksums (SHA-256):

```
363cb77156a13308c4973d26d94eebc6992dca2d8def7632e9a4f2524ec10805  part-1/tasks.jsonl
d6c3a0aaab0aff2bc6057e70be565c8a68c81d4b6b0861b21a7d615771a2e617  part-2/tasks.jsonl
25667b0607935b94fee4846ada06780f876fa4edca1637f6b38cd9205335e253  part-3/tasks.jsonl
```

## License

Proprietary — Scogo internal. Not for redistribution.
