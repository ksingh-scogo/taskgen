# Scogo Sovereign Enterprise NetOps data contract

Version: 2

This document defines the boundary between prompt-seed generation, teacher behavior, deterministic tool execution, verification, canonical audit storage, trainable SFT projection, and ATIF-v1.7 interchange.

## Pipeline boundary

```text
Taskgen candidate prompt
  -> schema validation and mandatory local deduplication
  -> separate NetOps prompt-quality model call
  -> Taskgen accepted prompt seed and audit sidecars
  -> teacher candidate messages and tool-call requests
  -> harness executes tools and captures returned state
  -> policy gate supplies approval decisions
  -> verifier checks outcomes, regressions, and rollback
  -> pipeline assembles the canonical audit record
  -> only independently accepted records become SFT projections
  -> canonical audit records may be exported as ATIF-v1.7
```

Taskgen produces a prompt and capability-compiled coordinates, persists the candidate, and owns deterministic prompt checks, deduplication, rubric review, and selective adjudication. Embedded logs, telemetry, configuration, or command output are explicitly supplied fictional scenario fixtures and must not imply live access. A teacher produces candidate assistant messages and tool-call requests. Neither component owns live tool results, approval, ground truth, state hashes, verification results, safety grades, rewards, or trajectory acceptance.

A schema-valid candidate is not automatically publishable. Taskgen invokes a separate rubric review for coordinate realization, internal consistency, operational quality, safety, and technical authenticity. Each dimension is `pass`, `fail`, or `unknown`; the outcome is `accept`, `revise`, `reject`, or `needs_verification`. Only `needs_verification` may invoke claim-level adjudication against supplied candidate evidence and an optional local reference corpus. All candidates live in `<run-dir>/candidates.jsonl`, all valid review decisions in `reviews.jsonl`, and rejected/error candidates in `rejected.jsonl`. This prompt gate does not make a teacher trajectory correct, verified, or trainable.

## Normative files

| Artifact | Source of truth |
|---|---|
| Prompt-seed schema | `schemas/task-v2.schema.json` |
| Prompt-review schema | `schemas/prompt-review-v3.schema.json` |
| Prompt-adjudication schema | `schemas/prompt-adjudication-v1.schema.json` |
| Canonical audit schema | `schemas/netops-teacher-trajectory-audit-v1.schema.json` |
| Accepted SFT schema | `schemas/netops-teacher-trajectory-sft-v1.schema.json` |
| Taskgen system prompt | `prompts/netops-taskgen-system-v2.txt` |
| Taskgen review prompt | `prompts/netops-prompt-review-system-v3.txt` |
| Teacher system prompt | `prompts/netops-teacher-system-v1.txt` |
| NetOps sampling taxonomy | `docs/netops-taxonomy.yaml` |
| Complete prompt fixture | `tests/fixtures/canonical/valid-task.json` |
| Complete audit fixture | `tests/fixtures/canonical/valid-audit.json` |
| Complete accepted SFT fixture | `tests/fixtures/canonical/valid-sft.json` |
| Complete ATIF export fixture | `tests/fixtures/atif-v1.7/valid-scogo-roundtrip.json` |

The JSON files above form one fictional BGP route-leak walkthrough and are intended to be read together.

## Field ownership

| Owner | Fields and decisions |
|---|---|
| Taskgen | prompt text, taxonomy coordinates, sampled difficulty, generator metadata, local dedup result, configured prompt-review acceptance and sidecars |
| Teacher | assistant-authored visible rationale, hypotheses, evidence requests, tool-call requests, remediation proposal |
| Harness | tool results, environment identity, state hashes, reset result, raw artifact references |
| Policy gate | whether approval is required, the decision, scope, source, and time |
| Fixture | hidden simulated ground truth and deterministic state transition |
| Verifier | independent checks, pre/post state comparison, regressions, rollback verification |
| Safety grader | read-before-write, approval compliance, prohibited/destructive actions, secret exposure, policy result |
| Independent quality grader | groundedness, terminal-claim validity, semantic quality |
| Dataset pipeline | stable IDs, hashes, split group, provenance, schema validation, accept/reject decision, SFT projection |

No component may self-award a field owned by another component. In particular, a teacher cannot declare its tool output authentic, grant its own approval, manufacture state hashes, or mark its own trajectory accepted.

## Stage 1: prompt seed

The example task is generated from a fixed compositional coordinate:

```json
{
  "schema_version": "scogo.taskgen.task.v2",
  "prompt": "Investigate a suspected BGP route leak using read-only evidence before proposing a bounded repair.",
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

The prompt record deliberately has no task ID, split, tools, results, evidence registry, approval, state hash, ground truth, outcome, reward, verification, safety grade, or trajectory-acceptance decision. Prompt acceptance is retained in the separate Taskgen review manifest rather than embedded in this record.

## Stage 2: teacher candidate

The teacher receives the user prompt, visible evidence, tool definitions, and returned tool results. Its behavioral contract is:

1. identify observed facts and cite their evidence;
2. separate facts from hypotheses;
3. state material uncertainty;
4. select the smallest useful next action;
5. prefer read-only inspection;
6. explain what the action should establish;
7. interpret the returned result before proceeding;
8. request approval before a state-changing call when required;
9. verify independently after a change;
10. abstain or escalate when live state, topology, permissions, pricing, or vendor behavior is unavailable.

A teacher may request this call:

```json
{
  "id": "call-1",
  "type": "function",
  "function": {
    "name": "show_route",
    "arguments": {
      "device": "edge-r1",
      "prefix": "203.0.113.0/24"
    }
  }
}
```

It may not write the matching tool message. The harness owns that message after executing the approved deterministic tool against the simulated, replay, or authorized-live environment.

## Stage 3: harness, approval, and verifier

The harness must capture enough evidence to reproduce the tool result:

- tool name and exact JSON arguments;
- correlation ID;
- environment and fixture reference;
- returned content or focused excerpt;
- artifact reference and content hash when available;
- observation timestamp;
- initial and post-action state hashes when relevant.

For any write-capable action, the gate must establish the approved scope before execution. The verifier then checks the intended result, retained reachability, regressions, and rollback. A successful model message is never a substitute for an independent check.

The example remains read-only. Its approval object records that a future production rollback would require approval, but no approval was requested or invented:

```json
{
  "required": true,
  "requested": false,
  "granted": null,
  "scope": null,
  "decision_source": null,
  "decided_at": null
}
```

## Stage 4: canonical audit record

The canonical audit record is the full evidence-bearing object. The [complete example](../tests/fixtures/canonical/valid-audit.json) includes:

- stable sample, trajectory, prompt, and split-group identifiers;
- the exact source prompt and coordinates;
- generation model, prompt version, time, and raw artifact reference;
- environment mode, fixture/reset references, allowed tools, and initial state hash;
- ordered system, user, assistant, and harness-authored tool messages;
- an evidence registry with lineage, excerpts, hashes, timestamps, producer, and sensitivity;
- approval state;
- outcome, uncertainty, and remediation;
- independent verification and state comparison;
- safety evaluation;
- quality gates and the final acceptance decision;
- provenance, license review, contamination review, and content hash.

Unknown evaluation results are represented as `null`, not omitted and not treated as passed. Collections use empty arrays. `record_kind` is one of `candidate`, `imported`, `accepted`, or `rejected`.

An external ATIF import uses `record_kind: imported`, permits `task.difficulty: null`, requires `outcome.status: unknown`, and remains `quality.accepted: false` until independently replayed and evaluated.

## Stage 5: accepted SFT projection

Only an accepted canonical audit record may be projected. The [complete example](../tests/fixtures/canonical/valid-sft.json) contains:

```json
{
  "schema_version": "scogo.netops.teacher-trajectory.sft.v1",
  "id": "sample-bgp-001",
  "trajectory_id": "trajectory-bgp-001",
  "messages": [],
  "tools": [],
  "metadata": {
    "taxonomy_id": "scogo-enterprise-netops-v2",
    "domain": "layer3_routing",
    "subdomain": "bgp_route_leak",
    "task_family": "troubleshooting_rca",
    "difficulty": 8,
    "split_group_id": "bgp-route-leak-family-001"
  }
}
```

The abbreviated arrays above indicate the envelope; the linked fixture contains the complete message and tool sequence.

The projection excludes:

- hidden fixture ground truth;
- state internals and private environment identifiers;
- grader output, rewards, acceptance, and rejection reasons;
- raw provider hidden reasoning or long chain-of-thought;
- secrets, credentials, and customer identifiers;
- redundant raw telemetry when a focused excerpt is sufficient;
- benchmark source identity;
- copied ATIF steps with `is_copied_context=true`;
- deterministic ATIF agent steps with `llm_call_count=0`;
- every unverified external ATIF import.

Training masks system, user, and tool tokens and trains only assistant-authored outputs according to the target model's verified chat template.

## Stage 6: ATIF-v1.7 interchange

ATIF is an interoperability representation for a completed trajectory, not the canonical Scogo audit object. The [complete corresponding export](../tests/fixtures/atif-v1.7/valid-scogo-roundtrip.json) uses:

- `session_id` for the generation run;
- `trajectory_id` for the document identity;
- canonical tools as `agent.tool_definitions`;
- system and user messages as system/user steps;
- assistant messages as agent steps;
- canonical tool calls as ATIF `tool_calls`;
- harness tool messages as correlated `observation.results`;
- concise visible rationale in `message`;
- no `reasoning_content` export;
- canonical-only approval, evidence, hashes, verification, safety, quality, and provenance under `extra.scogo`.

Round-tripping a Scogo export restores `extra.scogo`. An external ATIF import is different: the full original object is retained under `interop.original_atif`, while the canonical envelope is marked:

```json
{
  "record_kind": "imported",
  "outcome": { "status": "unknown" },
  "quality": {
    "accepted": false,
    "rejection_reasons": ["external_atif_unverified"]
  }
}
```

Imported reasoning, copied context, context-management events, continuations, and embedded subagents remain audit-only. They are never silently converted into trainable messages.

## Validation and release gates

Every release must prove:

1. both taxonomy files parse and their distributions validate;
2. the NetOps taxonomy has exactly 25 domains and 531 subdomain entries;
3. every prompt-seed fixture validates and a missing coordinate is rejected;
4. all three JSON Schemas validate against the draft 2020-12 meta-schema;
5. positive audit and SFT fixtures validate;
6. hidden grader fields are rejected from SFT;
7. ATIF versions other than `ATIF-v1.7` are rejected;
8. ATIF step IDs, tool-call references, content parts, copied context, deterministic steps, and embedded subagent IDs validate;
9. canonical-to-ATIF-to-canonical preserves Scogo audit sections;
10. external imports stay unverified and unaccepted;
11. JSONL conversion failure does not publish a partial destination;
12. generated NetOps tasks validate before write;
13. every accepted prompt has one review manifest entry with final disposition `accepted`, including adjudication evidence when used;
14. exact, lexical, semantic, and final serialized dedup gates reject duplicates;
15. a successful `generate -c N` publishes exactly N new accepted prompts;
16. incomplete generation retains work artifacts and does not replace the requested final output.

## Dataset split and contamination controls

All variants of the same causal incident family must share `split_group_id`. Paraphrases, vendor translations, evidence-ablation variants, difficulty variants, and remediation variants of the same underlying case cannot cross train, validation, and test splits.

Provenance must record source references, license review, contamination review, and a content hash. Customer incidents may be used only under an explicit governed process with redaction and rights review. Default synthetic tasks use fictional organizations, documentation IP ranges, and redacted identifiers.

## Safety invariants

- Investigation is read-only by default.
- A state-changing call needs bounded scope, evidence, preconditions, approval when required, retained reachability, post-change checks, and rollback.
- Taskgen never executes network tools.
- The teacher never fabricates results or approvals.
- The harness never treats model text as executed state.
- The verifier is independent of the teacher's terminal claim.
- An ATIF import is never proof of success or trainability.
- Missing live state, permissions, topology, current price, or vendor behavior leads to an evidence request, abstention, staging, or escalation.

ATIF support follows the active [Harbor Agent Trajectory Interchange Format specification](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md).
