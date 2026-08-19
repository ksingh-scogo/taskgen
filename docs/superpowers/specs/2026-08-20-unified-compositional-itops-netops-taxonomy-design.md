# Unified Compositional IT Ops and NetOps Taxonomy Design

Date: 2026-08-20

Status: direction approved in chat; written-spec review pending

Branch: `codex/netops-taxonomy-atif` from `origin/master`

## 1. Decision

Convert the general IT Ops taxonomy from hierarchy-only sampling to compositional sampling.

The conversion must preserve its valuable hierarchy:

```text
14 weighted categories
  -> 129 operational domains
    -> 884 domain-scoped subdomains
```

and add cross-cutting operational coordinates:

```text
task family
+ environment
+ platform scope and selected platforms
+ incident mechanism
+ evidence condition
+ evidence bundle
+ action risk
+ difficulty
+ presentation
```

NetOps will use the same generalized schema and sampling code. Its existing 25 domains and 531 subdomains remain intact under one `enterprise_netops` category.

The result is one universal compositional task-coordinate model for both Scogo IT Ops and Enterprise NetOps prompt generation.

## 2. Why conversion is better

The hierarchy answers only:

```text
What operational area is this prompt about?
```

It does not control:

- what the operator must do;
- where the incident occurs;
- which platform or product context applies;
- which failure mechanism is exercised;
- which evidence is present, missing, stale, or contradictory;
- whether the safe result is diagnosis, evidence collection, abstention, escalation, or an approval-gated change;
- how the task is presented;
- whether two prompts in one subdomain exercise materially different operational behavior.

Those dimensions are exactly what the sovereign SLM must learn. Compositional coordinates make them measurable, balanceable, reviewable, and usable as split/evaluation metadata.

The conversion must not mechanically combine every axis value. Implausible combinations would lower dataset quality. Category-level eligibility defaults plus domain-level overrides constrain sampling to operationally credible coordinates.

## 3. Schema version and compatibility

Both taxonomy files move to:

```yaml
schema_version: scogo.taskgen.taxonomy.v2
kind: compositional
```

`scogo.taskgen.taxonomy.v1` and `kind: hierarchical` are removed from the runtime. No backward compatibility is required.

Both generated prompt-record families move to:

```json
{"schema_version":"scogo.taskgen.task.v2"}
```

The generic v2 task schema replaces the split behavior where NetOps had coordinates but IT Ops did not. Both outputs always contain complete coordinates.

ATIF remains an import/export format for completed trajectories. The prompt-coordinate rename is mapped at the canonical Scogo audit boundary and does not change ATIF v1.7 itself.

## 4. Universal taxonomy structure

```yaml
schema_version: scogo.taskgen.taxonomy.v2
id: scogo-itops-v4
kind: compositional
label: Scogo IT Operations

scope:
  include: [...]
  exclude: [...]

defaults:
  system_prompt_file: ../prompts/itops-taskgen-system-v2.txt
  review_system_prompt_file: ../prompts/itops-prompt-review-system-v2.txt
  difficulty_distribution: {...}

platform_groups:
  - id: public_cloud
    weight: 1.0
    platforms:
      - {id: aws, label: Amazon Web Services, weight: 0.24}
      - {id: azure, label: Microsoft Azure, weight: 0.24}
      - {id: google_cloud, label: Google Cloud, weight: 0.20}
      - {id: alibaba_cloud, label: Alibaba Cloud, weight: 0.12}
      - {id: oracle_cloud, label: Oracle Cloud Infrastructure, weight: 0.12}
      - {id: openstack, label: OpenStack, weight: 0.08}

axes:
  task_families: [...]
  environments: [...]
  platform_scopes: [...]
  incident_mechanisms: [...]
  evidence_conditions: [...]
  evidence_bundles: [...]
  action_risks: [...]
  presentations: [...]

categories:
  - id: workplace
    label: Digital Workplace
    weight: 0.08
    eligibility:
      environments: [saas_tenant, hybrid, remote_work, endpoint_fleet]
      platform_scopes: [platform_neutral, single_platform, multi_platform]
      platform_groups: [microsoft_365, google_workspace, collaboration]
      incident_mechanisms: [misconfiguration, policy_or_access, dependency_failure, change_regression, service_degradation, multi_fault, unknown_requires_evidence]
      evidence_bundles: [ticket_cmdb, audit_logs, service_health, client_telemetry, multi_source, intentionally_missing]
    domains:
      - id: email_communication
        label: Email Communication
        subdomains: [mailbox_full, mail_flow, shared_mailbox, alias, journaling, transport_rule]
        eligibility:
          platform_groups: [microsoft_365, google_workspace]
          evidence_bundles: [mail_trace, audit_logs, service_health, dns_tls, multi_source, intentionally_missing]
```

## 5. Category and domain weighting

Sampling is hierarchical only for subject selection, then compositional for task construction:

1. sample category by category weight;
2. sample domain within that category;
3. sample subdomain within that domain;
4. sample eligible cross-cutting coordinates;
5. sample difficulty within task-family bounds.

Category weights must be explicit, finite, non-negative, and sum to `1.0 +/- 0.000001`.

Domain weights follow one of two modes per category:

- no domain in the category has a `weight`: domains are uniform;
- every domain has a `weight`: enabled domain weights must sum to `1.0 +/- 0.000001`.

Mixing weighted and unweighted domains in one category is invalid.

Subdomain weights remain optional. Missing weights are `1.0`; positive values are normalized within the selected domain.

The IT Ops conversion retains all existing category weights and initially uses uniform domains inside each category. This preserves the existing sampler's subject distribution.

The NetOps conversion uses one category with weight `1.0` and retains all existing domain weights.

## 6. Universal task coordinates

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCoordinates {
    pub taxonomy_id: String,
    pub category_id: String,
    pub task_family: String,
    pub environment: String,
    pub platform_scope: String,
    pub platforms: Vec<String>,
    pub incident_mechanism: String,
    pub evidence_condition: String,
    pub evidence_bundle: String,
    pub action_risk: String,
    pub presentation: String,
}
```

The old NetOps-specific names are generalized:

| v1 | v2 |
|---|---|
| `vendor_scope` | `platform_scope` |
| `vendors` | `platforms` |
| `vendor_groups` | `platform_groups` |

Network operating systems, firewall products, cloud providers, SaaS tenants, databases, hypervisors, endpoint managers, observability stacks, and ITSM systems are all represented as platforms.

Platform-scope cardinality is:

```text
platform_neutral -> 0 platforms
single_platform  -> 1 platform
multi_platform   -> 2 distinct platforms
```

The validator proves that each domain can satisfy every eligible platform scope.

## 7. Eligibility and inheritance

The constrainable axes are:

```text
task_families
environments
platform_scopes
platform_groups
incident_mechanisms
evidence_conditions
evidence_bundles
action_risks
presentations
```

For each axis:

1. a domain eligibility list, when present, replaces the category list;
2. otherwise the category list applies;
3. otherwise all globally enabled options on that axis apply.

An explicitly empty list is invalid. Unknown or duplicate references are invalid. The resolved eligible set must be non-empty.

Eligibility does not change global option weights. The eligible subset is filtered and re-normalized at sampling time.

This lets broad categories carry reusable defaults while exceptional domains remain precise. Examples:

- `Email Communication` narrows platforms to Microsoft 365 and Google Workspace;
- `Print Workplace Devices` narrows environments to on-premises, branch, and endpoint fleet;
- `Cloud Cost FinOps` enables cloud-bill evidence and optimization task families;
- `Backup Recovery` enables approval-gated restore actions and recovery-verification evidence;
- `BGP Route Leak` narrows platform groups to routing/network platforms and evidence to routing tables, configurations, topology, and packet/path data.

## 8. Shared axes

### 8.1 Task families

The initial shared task families are:

```text
incident_triage
troubleshooting_rca
hypothesis_and_evidence_plan
telemetry_interpretation
configuration_review
infrastructure_as_code_review
tool_selection_and_execution_plan
safe_remediation_plan
change_risk_and_approval
post_change_verification
rollback_planning
capacity_and_performance
cost_and_resource_optimization
security_and_compliance_investigation
runbook_execution
incident_communication_and_handoff
postmortem_and_prevention
abstention_and_escalation
```

Each option keeps `difficulty_min`, `difficulty_max`, and optional per-level multipliers.

### 8.2 Environments

```text
on_premises
branch
campus
data_center
private_cloud
public_cloud
multi_cloud
hybrid
saas_tenant
remote_work
endpoint_fleet
mobile_fleet
kubernetes
edge
ot_iot
backup_recovery_site
identity_control_plane
developer_platform
ai_hpc
lab_staging
```

### 8.3 Platform scopes

```text
platform_neutral
single_platform
multi_platform
```

### 8.4 Incident mechanisms

```text
misconfiguration
configuration_drift
policy_or_access
credential_or_secret_lifecycle
certificate_or_key_expiry
capacity_exhaustion
resource_contention
dependency_failure
network_or_path_failure
service_degradation
software_defect
data_or_state_inconsistency
rate_limit_or_quota
change_regression
hardware_failure
security_event
human_process_failure
telemetry_or_monitoring_gap
multi_fault
unknown_requires_evidence
```

### 8.5 Evidence conditions

```text
complete
partial
stale
noisy
contradictory
misleading
intentionally_missing
live_state_required
```

### 8.6 Evidence bundles

The global catalogue includes reusable evidence types; eligibility prevents nonsense combinations:

```text
ticket_cmdb
runbook_change_history
logs_metrics_traces
service_health
configuration_state
infrastructure_as_code
cloud_inventory
cloud_bill_and_usage
identity_audit
endpoint_telemetry
network_path_and_flow
dns_tls
mail_trace
database_state_and_queries
storage_backup_jobs
security_alerts
application_deployment_state
user_reports
multi_source
intentionally_missing
```

### 8.7 Action risks

```text
read_only_investigation
recommendation_only
reversible_low_risk
approval_gated_change
high_risk_change_plan_only
rollback_required
escalation_required
```

### 8.8 Presentations

```text
service_desk_ticket
monitoring_alert
war_room
shift_handover
change_request
runbook_gap
audit_finding
capacity_review
cost_review
customer_escalation
postmortem
tool_execution_task
```

## 9. IT Ops platform groups

The IT Ops v2 taxonomy must include weighted groups covering at least:

```text
public_cloud
microsoft_365
google_workspace
collaboration
itsm_cmdb
endpoint_management
endpoint_operating_systems
identity_access
security_operations
network_security
networking
observability
automation_iac
containers_kubernetes
virtualization_private_cloud
servers_operating_systems
storage_backup
databases_data_platforms
devops_delivery
enterprise_applications
ai_agent_platforms
oem_hardware_software
open_source_platforms
```

Every named product in the existing OEM/ISV category is retained as a platform ID in an appropriate group. Domain eligibility determines which groups are operationally credible.

Vendor-neutral tasks remain first-class through `platform_neutral`; the taxonomy does not force a product into every prompt.

## 10. NetOps migration

NetOps becomes:

```yaml
schema_version: scogo.taskgen.taxonomy.v2
id: scogo-enterprise-netops-v2
kind: compositional

categories:
  - id: enterprise_netops
    label: Enterprise NetOps
    weight: 1.0
    domains:
      - id: layer3_routing
        label: Layer 3 Routing
        weight: 0.07
        subdomains: [...]
        eligibility:
          platform_groups: [routing_switching, open_networking]
          environments: [campus, branch, data_center, hybrid, public_cloud, lab_staging]
          incident_mechanisms: [misconfiguration, configuration_drift, network_or_path_failure, service_degradation, software_defect, multi_fault, unknown_requires_evidence]
          evidence_bundles: [configuration_state, network_path_and_flow, logs_metrics_traces, multi_source, intentionally_missing]
```

All 25 current domain weights, 531 subdomains, platform entries, and domain-specific constraints are retained. Existing labels such as `vendor_neutral` are renamed to their v2 platform equivalents.

## 11. IT Ops migration

IT Ops becomes `scogo-itops-v4` and remains the default embedded taxonomy.

Migration rules:

1. preserve all 14 category IDs, labels, weights, descriptions, and source metadata;
2. assign every domain a stable snake-case ID and preserve its label;
3. preserve all 884 subdomain IDs;
4. use uniform domains within each category initially, matching current sampling;
5. map category-level eligibility from the category's operational scope;
6. add narrower domain overrides for platform groups, environments, task families, evidence bundles, and action risks where broad defaults would create invalid combinations;
7. convert OEM/ISV product lines into platform eligibility while retaining product-specific task coverage;
8. validate that every category/domain/subdomain is reachable by sampling;
9. produce a machine-readable migration report proving no category, domain label, or subdomain was lost.

The migration report contains:

```json
{
  "source_taxonomy": "scogo-itops-v3",
  "target_taxonomy": "scogo-itops-v4",
  "source_counts": {"categories": 14, "domains": 129, "subdomains": 884},
  "target_counts": {"categories": 14, "domains": 129, "subdomains": 884},
  "missing_domains": [],
  "missing_subdomains": [],
  "duplicate_target_ids": []
}
```

## 12. Sampling order

For both taxonomies, a seeded sampler consumes randomness in this fixed order:

1. category;
2. domain within category;
3. subdomain within domain;
4. task family;
5. environment;
6. platform scope;
7. zero, one, or two distinct platforms;
8. incident mechanism;
9. evidence condition;
10. evidence bundle;
11. action risk;
12. difficulty within task-family bounds;
13. presentation;
14. language when multilingual mode is enabled.

All filtered option sets are re-normalized without changing relative weights.

The same `--seed` and taxonomy file reproduce sampled coordinates and language. Remote model text remains non-deterministic.

## 13. Prompt and reviewer behavior

Both generation prompts must treat every sampled coordinate as mandatory and operationally relevant. They must not recite coordinate labels.

The IT Ops prompt specifically requires:

- interpretation of tickets, telemetry, configurations, incidents, runbooks, bills, audit evidence, and IaC when supplied;
- evidence requests and deterministic tool selection;
- explicit uncertainty and abstention when live state, pricing, permissions, or required evidence is missing;
- read-only investigation by default;
- approvals, rollback, and post-change verification for mutations;
- realistic product syntax only when a platform is selected;
- a user-facing prompt seed, never a hidden answer or fabricated tool result.

The taxonomy-specific reviewer verifies coordinate fidelity and rejects implausible category/domain/axis combinations that escaped static eligibility validation.

## 14. Task-record contract

```json
{
  "schema_version": "scogo.taskgen.task.v2",
  "prompt": "Investigate the cross-tenant mail-flow incident...",
  "category": "workplace",
  "domain": "email_communication",
  "subdomain": "mail_flow",
  "difficulty": 7,
  "coordinates": {
    "taxonomy_id": "scogo-itops-v4",
    "category_id": "workplace",
    "task_family": "troubleshooting_rca",
    "environment": "saas_tenant",
    "platform_scope": "multi_platform",
    "platforms": ["microsoft_exchange_online", "google_workspace_gmail"],
    "incident_mechanism": "policy_or_access",
    "evidence_condition": "contradictory",
    "evidence_bundle": "mail_trace",
    "action_risk": "read_only_investigation",
    "presentation": "customer_escalation"
  },
  "taskgen_model": "generator/model",
  "temperature": 0.9
}
```

Static validation proves:

- category/domain/subdomain membership;
- coordinate membership and eligibility;
- platform group membership and platform-scope cardinality;
- difficulty bounds;
- complete non-empty prompt content;
- schema shape.

Model review remains responsible for semantic authenticity, causality, product syntax, evidence coherence, and operational realism.

## 15. Distribution override

`--distribution` remains a category distribution override because categories are the stable business-level balancing unit for IT Ops.

It must provide a complete category distribution summing to `1.0 +/- 0.000001`.

For NetOps, the only category is `enterprise_netops=1.0`; domain weighting comes from the taxonomy. A future domain-filter feature is out of scope for this change.

This is deliberate: a 129-domain CLI distribution is not a practical operator interface, and domain-level rebalancing belongs in a reviewed taxonomy file.

## 16. Validation and tests

The migration is incomplete until tests prove:

- both files use `scogo.taskgen.taxonomy.v2` and `kind: compositional`;
- v1 and hierarchical taxonomies are rejected;
- IT Ops counts are exactly `14/129/884`;
- NetOps counts are exactly `1/25/531`;
- the migration report has no missing or duplicate entries;
- category weights and explicit per-category domain weights validate exactly;
- uniform domain mode is deterministic and cannot mix with explicit weights;
- eligibility inheritance and domain replacement work per axis;
- empty and unknown eligibility references are rejected;
- every domain resolves at least one valid option on every axis;
- every eligible platform scope is satisfiable;
- seeded snapshots prove fixed sampling order;
- every domain and subdomain is reachable across a deterministic exhaustive sampler test;
- all generated IT Ops and NetOps records validate against `scogo.taskgen.task.v2`;
- both taxonomy-specific prompts resolve from YAML;
- review and dedup operate identically for both taxonomies.

## 17. Documentation changes

Documentation will describe both taxonomies as compositional and distinguish:

- the subject hierarchy: category, domain, subdomain;
- the behavioral coordinates: task, environment, platform, incident, evidence, risk, difficulty, and presentation.

Examples must include at least:

- platform-neutral ITSM incident investigation;
- multi-platform Microsoft 365 and Google Workspace mail-flow task;
- cloud-bill FinOps evidence task;
- endpoint fleet task;
- database/storage task;
- single-vendor NetOps task;
- multi-platform NetOps task;
- evidence-missing abstention task;
- approval-gated change plan.

## 18. Alternatives rejected

### Keep IT Ops hierarchical

Rejected because the existing sampler cannot intentionally balance operational behaviors, evidence states, platform contexts, action risks, or presentations.

### Flatten IT Ops into 129 unrelated domains

Rejected because the 14 category weights are useful product-level balancing controls and encode meaningful Scogo domain structure.

### Copy NetOps axes and platform lists unchanged

Rejected because ITSM, SaaS, endpoint, identity, security, data, FinOps, delivery, and enterprise applications require different platform groups and evidence bundles.

### Allow arbitrary Cartesian products

Rejected because syntactically valid but operationally impossible combinations would increase reviewer rejection rates and contaminate prompts.

### Maintain v1 and v2 loaders

Rejected because backward compatibility is not required and dual sampling paths would preserve the complexity this unification is meant to remove.
