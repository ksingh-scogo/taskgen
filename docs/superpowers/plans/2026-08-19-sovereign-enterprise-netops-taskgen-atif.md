# Sovereign Enterprise NetOps Taskgen and ATIF Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add runtime compositional NetOps taxonomy generation, exact prompt/data contracts, and validated ATIF v1.7 interchange to Taskgen, then prove the generation path with a live TokenRouter canary.

**Architecture:** YAML becomes the single taxonomy source of truth. A focused taxonomy module loads, validates, and samples hierarchical or compositional coordinates; generation remains in the existing async path. Separate schema and ATIF modules validate canonical records and map ordered messages/tool results to and from ATIF without making imported trajectories trainable.

**Tech Stack:** Rust 2024, Clap, Serde/serde_json, serde_yaml_ng 0.10, jsonschema 0.49, sha2 0.10, Tokio, Reqwest, JSON Schema draft 2020-12, ATIF v1.7.

**Spec:** `docs/superpowers/specs/2026-08-19-sovereign-enterprise-netops-taskgen-atif-design.md`

## Global Constraints

- Taskgen outputs prompt seeds only; it never fabricates tool results, approvals, ground truth, state hashes, verification, safety grades, rewards, or acceptance decisions.
- NetOps scope excludes 3GPP RAN, EPC, 5GC, IMS, carrier OSS/BSS, mobile spectrum planning, carrier optical backbone, and service-provider core operations.
- `docs/it-ops-taxonomy.yaml` and `docs/netops-taxonomy.yaml` both use `scogo.taskgen.taxonomy.v1`; no generated Rust taxonomy catalog remains.
- The NetOps inventory contains exactly 25 domains and 531 domain-scoped subdomain entries from the approved spec.
- All default distributions sum to `1.0 +/- 0.000001`; malformed complete distributions fail instead of being silently normalized.
- ATIF support is pinned to `ATIF-v1.7`; unknown ATIF versions fail.
- External ATIF imports are audit-only, `accepted=false`, and cannot enter SFT without independent replay/verification.
- Hidden reasoning, copied ATIF context, deterministic `llm_call_count=0` steps, secrets, grader output, and hidden ground truth never enter the SFT projection.
- Use only fictional organizations, documentation IP ranges, and redacted identifiers in generated tasks and fixtures.
- Preserve the user's dirty original `master` checkout; all work stays in the isolated `codex/netops-taxonomy-atif` worktree.

---

### Task 1: Runtime Taxonomy Loader and CLI Skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/taxonomy.rs`
- Modify: `src/main.rs`
- Modify: `docs/it-ops-taxonomy.yaml`
- Delete: `scripts/codegen_domains.py`

**Interfaces:**
- Produces: `taxonomy::TaxonomyCatalog::from_path`, `taxonomy::TaxonomyCatalog::embedded_itops`, `TaxonomyCatalog::validate`, and `TaxonomyCatalog::sample`.
- Produces: `Cli`, `Command::Generate`, `Command::Atif`, and `Command::Taxonomy` Clap subcommands.
- Consumes: the current task-generation functions in `src/main.rs` without changing network request behavior.

- [ ] **Step 1: Add failing taxonomy parser and CLI parsing tests**

Add tests for this public contract:

```rust
let catalog = TaxonomyCatalog::from_yaml(include_str!("../docs/it-ops-taxonomy.yaml"), None)?;
assert_eq!(catalog.id(), "scogo-itops-v3");
assert_eq!(catalog.kind(), TaxonomyKind::Hierarchical);
catalog.validate()?;

let cli = Cli::try_parse_from(["taskgen", "taxonomy", "validate", "--taxonomy", "docs/it-ops-taxonomy.yaml"])?;
assert!(matches!(cli.command, Command::Taxonomy { .. }));
```

Add negative fixtures in test strings for duplicate IDs, a `NaN`/negative weight, a distribution sum of `0.9`, and an unknown allow-list reference.

- [ ] **Step 2: Run the focused tests and confirm they fail**

Run: `cargo test taxonomy -- --nocapture`

Expected: compilation fails because `src/taxonomy.rs`, `TaxonomyCatalog`, and subcommands do not exist.

- [ ] **Step 3: Add maintained YAML/schema dependencies**

Add:

```toml
serde_yaml_ng = "0.10"
sha2 = "0.10"
jsonschema = { version = "0.49", default-features = false }
```

- [ ] **Step 4: Implement typed taxonomy loading and validation**

Use these signatures:

```rust
pub enum TaxonomyKind { Hierarchical, Compositional }

pub struct TaxonomyCatalog;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCoordinates {
    pub taxonomy_id: String,
    pub task_family: String,
    pub environment: String,
    pub vendor_scope: String,
    pub vendors: Vec<String>,
    pub incident_mechanism: String,
    pub evidence_condition: String,
    pub evidence_bundle: String,
    pub action_risk: String,
    pub presentation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledTask {
    pub taxonomy_id: String,
    pub category_id: String,
    pub domain_id: String,
    pub domain_label: String,
    pub subdomain_id: String,
    pub coordinates: Option<TaskCoordinates>,
    pub difficulty: u8,
}

impl TaxonomyCatalog {
    pub fn from_yaml(source: &str, source_path: Option<&Path>) -> Result<Self>;
    pub fn from_path(path: &Path) -> Result<Self>;
    pub fn embedded_itops() -> Result<Self>;
    pub fn validate(&self) -> Result<()>;
    pub fn id(&self) -> &str;
    pub fn kind(&self) -> TaxonomyKind;
    pub fn default_system_prompt_path(&self) -> Option<&Path>;
    pub fn available_distribution_ids(&self) -> Vec<&str>;
    pub fn default_distribution(&self) -> HashMap<String, f64>;
    pub fn default_difficulty(&self) -> HashMap<u8, f64>;
    pub fn domain_count(&self) -> usize;
    pub fn subdomain_count(&self) -> usize;
    pub fn default_domain_weight_sum(&self) -> f64;
    pub fn sample<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask>;
    pub fn sample_defaults<R: Rng + ?Sized>(&self, rng: &mut R) -> Result<SampledTask>;
}
```

The sampler deliberately renormalizes only the eligible subset after domain allow-list filtering. It errors if that subset is empty.

- [ ] **Step 5: Migrate the general taxonomy and remove generated catalogs**

Set:

```yaml
schema_version: scogo.taskgen.taxonomy.v1
id: scogo-itops-v3
kind: hierarchical
label: Scogo IT Operations
defaults:
  difficulty_distribution:
    1: 0.05
    2: 0.05
    3: 0.10
    4: 0.15
    5: 0.20
    6: 0.15
    7: 0.10
    8: 0.08
    9: 0.07
    10: 0.05
```

Move each current default category weight onto its category as `weight`. Delete the generated `DOMAINS` and `DEFAULT_DISTRIBUTION` blocks and delete `scripts/codegen_domains.py`.

- [ ] **Step 6: Introduce explicit CLI subcommands**

Use:

```rust
#[derive(Parser)]
struct Cli { #[command(subcommand)] command: Command }

#[derive(Subcommand)]
enum Command {
    Generate(GenerateArgs),
    Atif { #[command(subcommand)] command: AtifCommand },
    Taxonomy { #[command(subcommand)] command: TaxonomyCommand },
}
```

Add `GenerateArgs.taxonomy: Option<PathBuf>`, `system_prompt_file: Option<PathBuf>`, and `seed: Option<u64>`. Mark `system_prompt` and `system_prompt_file` as conflicting Clap arguments.

- [ ] **Step 7: Run focused and full tests**

Run: `cargo fmt --check && cargo test taxonomy -- --nocapture && cargo test`

Expected: all existing and new tests pass.

- [ ] **Step 8: Commit the loader and CLI skeleton**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/taxonomy.rs docs/it-ops-taxonomy.yaml scripts/codegen_domains.py
git commit -m "Add runtime taxonomy loading"
```

### Task 2: Exact NetOps Taxonomy and Prompt Files

**Files:**
- Create: `docs/netops-taxonomy.yaml`
- Create: `prompts/netops-taskgen-system-v1.txt`
- Create: `prompts/netops-teacher-system-v1.txt`
- Modify: `src/taxonomy.rs`

**Interfaces:**
- Consumes: `TaxonomyCatalog::from_path` and `TaxonomyCatalog::sample` from Task 1.
- Produces: `SampledTask.coordinates: Option<TaskCoordinates>` populated for `kind: compositional`.

- [ ] **Step 1: Add failing inventory and deterministic sampling tests**

Tests must assert:

```rust
let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))?;
assert_eq!(catalog.id(), "scogo-enterprise-netops-v1");
assert_eq!(catalog.domain_count(), 25);
assert_eq!(catalog.subdomain_count(), 531);
assert_eq!(catalog.default_domain_weight_sum(), 1.0);

let mut a = StdRng::seed_from_u64(42);
let mut b = StdRng::seed_from_u64(42);
assert_eq!(catalog.sample_defaults(&mut a)?, catalog.sample_defaults(&mut b)?);
```

Also scan the prompt and taxonomy exclusions for `3GPP`, `EPC`, `5GC`, `IMS`, `OSS/BSS`, and `service-provider core`.

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test netops_taxonomy -- --nocapture`

Expected: failure because the NetOps YAML and prompt files are absent.

- [ ] **Step 3: Add the exact approved prompt files**

Copy sections 7.1 and 7.2 of the spec byte-for-byte, including the teacher sentence: `The harness, not you, supplies tool results, state hashes, ground truth, approval decisions, safety grades, verification status, and ATIF serialization.`

- [ ] **Step 4: Add the exact 25-domain, 531-subdomain YAML**

Use the IDs in spec section 6.5 and weights in sections 6.1-6.4. Define global weighted catalogs for task families, environments, vendor scopes, incident mechanisms, evidence conditions, evidence bundles, action risks, and presentations. Define vendor groups for routing/switching, wireless, SD-WAN, security, NAC, application delivery, cloud, Kubernetes, automation/observability, OT, AI/HPC, and enterprise real-time platforms. Each domain lists eligible groups, environments, mechanisms, and evidence bundles.

- [ ] **Step 5: Implement filtered compositional sampling**

Populate every field of Task 1's `TaskCoordinates`. Vendor-neutral selects zero vendors, single-vendor selects one, and multi-vendor selects two distinct vendors from eligible groups. Reject a single- or multi-vendor sample if the selected domain cannot supply the required number of distinct vendors.

- [ ] **Step 6: Run validation and sampling tests**

Run: `cargo run -- taxonomy validate --taxonomy docs/netops-taxonomy.yaml && cargo test netops_taxonomy -- --nocapture`

Expected: 25 domains, 531 subdomains, all references valid, tests pass.

- [ ] **Step 7: Commit the taxonomy and prompts**

```bash
git add docs/netops-taxonomy.yaml prompts src/taxonomy.rs
git commit -m "Add compositional NetOps taxonomy"
```

### Task 3: NetOps Prompt Generation and Task Record

**Files:**
- Modify: `src/main.rs`
- Modify: `src/taxonomy.rs`

**Interfaces:**
- Consumes: `SampledTask` and `TaskCoordinates` from Tasks 1-2.
- Produces: `TaskEntry` conforming to `scogo.netops.task.v1` for compositional generation.

- [ ] **Step 1: Add failing user-message and serialization tests**

Construct a fixed `SampledTask` and assert the generated model request names every coordinate, states that they are constraints, and asks for only the prompt. Serialize a NetOps `TaskEntry` and assert:

```rust
assert_eq!(value["schema_version"], "scogo.netops.task.v1");
assert_eq!(value["domain"], "enterprise_netops::layer3_routing");
assert_eq!(value["subdomain"], "bgp_route_leak");
assert_eq!(value["coordinates"]["action_risk"], "read_only_investigation");
```

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test netops_task -- --nocapture`

Expected: failures because TaskEntry has no schema version or coordinates and the user message is hierarchical-only.

- [ ] **Step 3: Implement prompt-file precedence and generation integration**

Implement:

```rust
fn resolve_system_prompt(args: &GenerateArgs, taxonomy: &TaxonomyCatalog) -> Result<String>;
fn task_user_message(sample: &SampledTask, language: Option<&str>) -> String;
```

Precedence is inline flag, prompt-file flag, taxonomy-relative default file, built-in prompt. Resolve taxonomy-relative paths against the selected taxonomy file. Read every prompt before opening the output file or making an API request.

- [ ] **Step 4: Extend the task record only for compositional runs**

Add optional `schema_version` and `coordinates` fields. Hierarchical generation retains the current domain/subdomain semantics; compositional generation writes the exact NetOps schema fields.

- [ ] **Step 5: Seed the pre-sampling PRNG**

Use `StdRng::seed_from_u64(seed)` when `--seed` is supplied and `StdRng::from_entropy()` otherwise. The seed governs coordinates and multilingual selection only.

- [ ] **Step 6: Run focused and regression tests**

Run: `cargo fmt --check && cargo test netops_task -- --nocapture && cargo test`

Expected: all tests pass and no existing HTTP request behavior regresses.

- [ ] **Step 7: Commit generation integration**

```bash
git add src/main.rs src/taxonomy.rs
git commit -m "Generate compositional NetOps prompt seeds"
```

### Task 4: JSON Schema Contracts and Validation

**Files:**
- Create: `schemas/netops-task-v1.schema.json`
- Create: `schemas/netops-teacher-trajectory-audit-v1.schema.json`
- Create: `schemas/netops-teacher-trajectory-sft-v1.schema.json`
- Create: `src/schema.rs`
- Modify: `src/main.rs`
- Create: `tests/fixtures/canonical/valid-task.json`
- Create: `tests/fixtures/canonical/valid-audit.json`
- Create: `tests/fixtures/canonical/valid-sft.json`

**Interfaces:**
- Produces: `schema::SchemaKind` and `schema::validate_instance(kind, value) -> Result<()>`.
- Consumes: Task 3 `TaskEntry` serialization.

- [ ] **Step 1: Add failing meta-schema and fixture tests**

For each schema, parse it, call `jsonschema::draft202012::meta::validate`, validate the positive fixture, then mutate a required field and assert rejection. Add explicit negatives for hidden grader fields in SFT and missing coordinates in NetOps tasks.

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test schema -- --nocapture`

Expected: failure because schemas, fixtures, and module are absent.

- [ ] **Step 3: Implement the three self-contained draft 2020-12 schemas**

Use `$defs` for content parts, tool calls, messages, evidence references, hashes, timestamps, and coordinates. Set `additionalProperties: false` except OpenAI tool parameter schemas and explicit `extra`/`original_atif` preservation objects. Audit `task.difficulty` accepts `null` only when `record_kind=imported`.

- [ ] **Step 4: Implement embedded schema validation**

Use:

```rust
pub enum SchemaKind { NetOpsTask, AuditTrajectory, SftTrajectory }

pub fn validate_instance(kind: SchemaKind, instance: &serde_json::Value) -> Result<()> {
    let schema = schema_value(kind)?;
    jsonschema::draft202012::validate(&schema, instance)
        .map_err(|error| anyhow!("{}: {}", error.instance_path(), error))
}
```

Validate every compositional task before writing JSONL.

- [ ] **Step 5: Run focused and full tests**

Run: `cargo fmt --check && cargo test schema -- --nocapture && cargo test`

Expected: schemas pass their meta-schema, positives pass, negatives fail, all tests pass.

- [ ] **Step 6: Commit schema contracts**

```bash
git add schemas src/schema.rs src/main.rs tests/fixtures/canonical Cargo.toml Cargo.lock
git commit -m "Add NetOps trajectory schemas"
```

### Task 5: ATIF v1.7 Import and Export

**Files:**
- Create: `src/atif.rs`
- Modify: `src/main.rs`
- Create: `tests/fixtures/atif-v1.7/valid-tool-trajectory.json`
- Create: `tests/fixtures/atif-v1.7/valid-copied-context.json`
- Create: `tests/fixtures/atif-v1.7/invalid-step-id.json`
- Create: `tests/fixtures/atif-v1.7/invalid-tool-reference.json`

**Interfaces:**
- Produces: `atif::validate_trajectory`, `atif::export_audit`, `atif::import_trajectory`, and `atif::convert_file`.
- Consumes: `schema::validate_instance(SchemaKind::AuditTrajectory, ...)`.

- [ ] **Step 1: Add failing ATIF validator and round-trip tests**

Tests cover exact v1.7 version, step sequence, source-specific fields, unique tool-call IDs, observation references, content parts, `is_copied_context`, `llm_call_count=0`, unique embedded subagent trajectory IDs, and this round trip:

```rust
let audit = fixture("tests/fixtures/canonical/valid-audit.json")?;
let atif = export_audit(&audit)?;
validate_trajectory(&atif)?;
let imported = import_trajectory(&atif)?;
assert_eq!(imported["trajectory_id"], audit["trajectory_id"]);
assert_eq!(imported["approval"], audit["approval"]);
assert_eq!(imported["verification"], audit["verification"]);
```

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test atif -- --nocapture`

Expected: compilation fails because `src/atif.rs` is absent.

- [ ] **Step 3: Implement typed ATIF v1.7 models and recursive validation**

Model root metadata, agent, steps, tool calls, observations, metrics as flexible JSON, content parts, continuations, extras, and recursive subagent trajectories. Reject any `schema_version` other than `ATIF-v1.7`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifTrajectory {
    pub schema_version: String,
    pub session_id: Option<String>,
    pub trajectory_id: Option<String>,
    pub agent: AtifAgent,
    pub steps: Vec<AtifStep>,
    pub notes: Option<String>,
    pub final_metrics: Option<Value>,
    pub continued_trajectory_ref: Option<String>,
    pub extra: Option<Map<String, Value>>,
    #[serde(default)]
    pub subagent_trajectories: Vec<AtifTrajectory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifAgent {
    pub name: String,
    pub version: String,
    pub model_name: Option<String>,
    pub tool_definitions: Option<Vec<Value>>,
    pub extra: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtifStep {
    pub step_id: u64,
    pub timestamp: Option<String>,
    pub source: String,
    pub model_name: Option<String>,
    pub reasoning_effort: Option<Value>,
    pub message: Value,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<Value>>,
    pub observation: Option<Value>,
    pub metrics: Option<Value>,
    pub extra: Option<Map<String, Value>>,
    pub llm_call_count: Option<u64>,
    pub is_copied_context: Option<bool>,
}

pub fn validate_trajectory(trajectory: &AtifTrajectory) -> Result<()>;
```

- [ ] **Step 4: Implement canonical export**

Map system/user/assistant messages to ATIF steps, attach matching tool messages to their issuing agent step as observations, and store unmapped Scogo audit sections under `extra.scogo`. Put per-message IDs, evidence references, and timestamps under step `extra.scogo_message`. Omit `reasoning_content`.

```rust
pub fn export_audit(audit: &Value) -> Result<AtifTrajectory>;
```

- [ ] **Step 5: Implement guarded import**

Restore `extra.scogo` when present. For external ATIF, compute prompt/content SHA-256 values, use `record_kind=imported`, `difficulty=null`, `outcome.status=unknown`, `quality.accepted=false`, and rejection reason `external_atif_unverified`. Preserve hidden reasoning, copied context, continuation, context management, and embedded subagents only under `interop.original_atif`.

```rust
pub fn import_trajectory(trajectory: &AtifTrajectory) -> Result<Value>;
```

- [ ] **Step 6: Implement atomic JSON/JSONL conversion**

Use a sibling temporary file created with a random suffix, flush and `sync_all`, then rename only after every record validates. Refuse an existing destination without `--overwrite`. Report JSONL line numbers on failure.

```rust
pub enum ConversionDirection { Export, Import }
pub enum Container { Json, Jsonl }
pub struct ConversionStats { pub records: usize }

pub fn convert_file(
    direction: ConversionDirection,
    input: &Path,
    output: &Path,
    container: Container,
    overwrite: bool,
) -> Result<ConversionStats>;
```

- [ ] **Step 7: Run ATIF and full tests**

Run: `cargo fmt --check && cargo test atif -- --nocapture && cargo test`

Expected: valid fixtures and round trips pass; invalid step and tool references fail.

- [ ] **Step 8: Commit ATIF support**

```bash
git add src/atif.rs src/main.rs tests/fixtures/atif-v1.7
git commit -m "Add ATIF v1.7 interchange"
```

### Task 6: Documentation and End-to-End Offline Verification

**Files:**
- Modify: `README.md`
- Create: `docs/netops-data-contract.md`

**Interfaces:**
- Documents all interfaces from Tasks 1-5.

- [ ] **Step 1: Add failing README contract tests**

Assert README contains `taskgen generate`, both taxonomy paths, `--system-prompt-file`, `taskgen atif export`, `taskgen atif import`, `ATIF-v1.7`, `external_atif_unverified`, and the statement that Taskgen generates prompt seeds rather than tool results.

- [ ] **Step 2: Run and confirm failure**

Run: `cargo test readme -- --nocapture`

Expected: failure because new commands are undocumented.

- [ ] **Step 3: Rewrite CLI, taxonomy, output, and ATIF documentation**

Update every old root-command example to `taskgen generate`. Document prompt precedence, compositional coordinates, the exact output object, validation, ATIF audit-only imports, and the safe teacher/harness/verifier boundary.

- [ ] **Step 4: Add one linked data-contract walkthrough**

Use one fictional BGP route-leak incident to show a prompt record, canonical audit trajectory, accepted SFT projection, and ATIF export. Keep raw tool output focused and synthetic.

- [ ] **Step 5: Run the complete offline gate**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run -- taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
cargo run -- taxonomy validate --taxonomy docs/netops-taxonomy.yaml
cargo build --release
```

Expected: all commands exit 0.

- [ ] **Step 6: Commit documentation**

```bash
git add README.md docs/netops-data-contract.md src/main.rs
git commit -m "Document NetOps generation and ATIF"
```

### Task 7: Live TokenRouter Canary, Validation, and Repair Loop

**Files:**
- Create locally, do not commit: `data/netops-tokenrouter-canary.jsonl`
- Create locally, do not commit: `data/netops-tokenrouter-canary.README.md`

**Interfaces:**
- Consumes: release binary and NetOps taxonomy/prompt from Tasks 1-6.
- Produces: live validation evidence only; no credential or generated dataset is committed.

- [ ] **Step 1: Confirm the credential is not written or printed**

Use a silent interactive shell variable named `NETOPS_TOKENROUTER_KEY`. Pass `--api-key "$NETOPS_TOKENROUTER_KEY"`. Do not place the key in a file, command transcript, README, Git diff, or final answer.

- [ ] **Step 2: Run a five-task connectivity canary**

Run the equivalent of:

```bash
./target/release/taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.tokenrouter.com/v1 \
  --api-key "$NETOPS_TOKENROUTER_KEY" \
  --model qwen/qwen3.8-max-free \
  --count 5 \
  --workers 1 \
  --seed 20260819 \
  --output data/netops-tokenrouter-canary.jsonl
```

Expected: five non-empty records, no API or parsing errors.

- [ ] **Step 3: Validate machine contracts**

Check line count, JSON parsing, schema validation, 25-domain taxonomy membership, subdomain membership, difficulty range, coordinate completeness, no empty prompt, no duplicate prompt, and absence of credential-shaped strings.

- [ ] **Step 4: Review prompt behavior manually**

For every prompt, verify it is operational rather than certification recall; contains concrete environment, symptom/change, impact, and constraints; respects domain/subdomain and all sampled axes; does not invent executed commands/approvals/results; defaults to read-only or approval-gated behavior; includes sufficient focused machine evidence only when the evidence coordinates require it; and uses no telecom-provider scope.

- [ ] **Step 5: Fix and repeat on any failure**

Add a failing regression test reproducing the defect, implement the smallest fix, run focused and full offline tests, rebuild release, delete only the failed canary outputs, and rerun five live tasks. Repeat until all five pass.

- [ ] **Step 6: Run a 20-task distribution canary**

After the five-task pass, generate 20 tasks with `--workers 2` and a different seed. Validate the same contracts and inspect at least one task from each observed task family and every difficulty band that appears.

- [ ] **Step 7: Run final repository checks and commit any canary-driven fixes**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test && git status --short`

Stage only source, schema, taxonomy, prompt, fixture, and documentation fixes. Never stage credentials or `data/` outputs.
