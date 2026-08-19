# Taskgen Exact Acceptance, Quality Review, and Native Dedup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `taskgen generate -c N` publish exactly `N` newly accepted prompt records after mandatory operational-quality review and native Rust exact, lexical, and local-semantic deduplication.

**Architecture:** Pre-sample `N` immutable taxonomy slots, then retry each slot until a candidate passes task validation, deterministic dedup, local embedding dedup, a separate model review, and a serialized final dedup commit. Provider resolution, review parsing, dedup indexes, working artifacts, and acceptance scheduling live in focused Rust modules and are tested through real module behavior plus local fake HTTP providers.

**Tech Stack:** Rust 2024, Tokio, Reqwest, Clap, Serde/JSON Schema, SHA-256, FastEmbed local ONNX embeddings, tempfile, async-trait, wiremock.

**Spec:**

- `docs/superpowers/specs/2026-08-20-unified-compositional-itops-netops-taxonomy-design.md`
- `docs/superpowers/specs/2026-08-20-taskgen-exact-acceptance-review-dedup-design.md`

## Global Constraints

- `taskgen generate -c N` succeeds only after exactly `N` new records pass every enabled gate.
- Both taxonomies use `scogo.taskgen.taxonomy.v2`, `kind: compositional`, and one sampling/validation code path.
- IT Ops preserves exactly 14 categories, 129 domains, and 884 subdomains.
- NetOps preserves exactly 1 category, 25 domains, and 531 subdomains.
- Both prompt-record families use `scogo.taskgen.task.v2` and complete universal coordinates.
- Universal coordinates use `platform_scope`, `platforms`, and `platform_groups`; v1 vendor-specific coordinate names are removed.
- `--append -c N` adds exactly `N` new records and deduplicates them against all existing records.
- Review defaults to the effective generation model, endpoint, and credentials.
- A different normalized review endpoint requires explicit review credentials; generation credentials must never cross that endpoint boundary implicitly.
- Review failures and malformed review output never become implicit accepts.
- Dedup is mandatory; `--dedup` and `--dedup-threshold` are removed.
- Exact dedup is global; lexical and semantic dedup use `(language, domain, subdomain)` buckets.
- Default lexical configuration is word 5-gram Jaccard at inclusive threshold `0.80`.
- Default semantic configuration is local cosine similarity at inclusive threshold `0.90`.
- Default English embedding model is `sentence-transformers/all-MiniLM-L6-v2`.
- Default multilingual embedding model is `intfloat/multilingual-e5-small`.
- The final dataset is published atomically only after final count and schema validation.
- Incomplete runs retain a working directory and exit non-zero without replacing the requested final output.
- Credentials must never appear in records, sidecars, reports, debug output, errors, or commits.
- Both Linux ARM64 and macOS ARM64 release builds include semantic dedup.
- No backward compatibility is required.

---

### Task A: Universal Compositional Taxonomy v2 Loader

**Files:**
- Modify: `src/taxonomy.rs`
- Modify: `src/schema.rs`
- Create: `schemas/task-v2.schema.json`
- Test: `src/taxonomy.rs`
- Test: `src/schema.rs`

**Interfaces:**
- Produces: the only supported `scogo.taskgen.taxonomy.v2` loader, `ResolvedEligibility`, universal `TaskCoordinates`, category/domain/subdomain sampling, and `scogo.taskgen.task.v2` validation.
- Consumes: v2 YAML with global axes, platform groups, weighted categories, category eligibility, and domain eligibility overrides.

- [ ] **Step 1: Write failing v2 parsing and v1 rejection tests**

```rust
#[test]
fn rejects_v1_and_hierarchical_taxonomies() {
    let v1 = include_str!("../tests/fixtures/taxonomy/hierarchical-valid.yaml");
    let error = TaxonomyCatalog::from_yaml(v1, None).unwrap_err();
    assert!(error.to_string().contains("scogo.taskgen.taxonomy.v2"));
}

#[test]
fn parses_nested_compositional_category_domain_and_coordinates() {
    let catalog = TaxonomyCatalog::from_yaml(V2_FIXTURE, None).unwrap();
    assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
    assert_eq!(catalog.category_count(), 1);
    assert_eq!(catalog.domain_count(), 2);
    let sample = catalog.sample_defaults(&mut StdRng::seed_from_u64(7)).unwrap();
    let coordinates = sample.coordinates;
    assert_eq!(coordinates.category_id, sample.category_id);
    assert!(matches!(coordinates.platform_scope.as_str(), "platform_neutral" | "single_platform" | "multi_platform"));
}
```

- [ ] **Step 2: Run taxonomy tests and verify RED**

Run: `cargo test taxonomy::tests -- --nocapture`

Expected: v2 fixture fails because the current loader accepts v1 and separates hierarchical/compositional layouts.

- [ ] **Step 3: Replace the v1 raw types with the universal v2 types**

Implement these shapes:

```rust
const SCHEMA_VERSION: &str = "scogo.taskgen.taxonomy.v2";

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

#[derive(Debug, Clone, Default, Deserialize)]
struct RawEligibility {
    task_families: Option<Vec<String>>,
    environments: Option<Vec<String>>,
    platform_scopes: Option<Vec<String>>,
    platform_groups: Option<Vec<String>>,
    incident_mechanisms: Option<Vec<String>>,
    evidence_conditions: Option<Vec<String>>,
    evidence_bundles: Option<Vec<String>>,
    action_risks: Option<Vec<String>>,
    presentations: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCategory {
    id: String,
    label: String,
    weight: f64,
    #[serde(default)]
    eligibility: RawEligibility,
    domains: Vec<RawDomain>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDomain {
    id: String,
    label: String,
    weight: Option<f64>,
    #[serde(default)]
    eligibility: RawEligibility,
    subdomains: Vec<RawSubdomain>,
}
```

Rename raw vendor types and axes to `RawPlatformGroup`, `RawPlatform`, and `platform_scopes`. Remove `RawHierarchicalDomain`, `RawCompositionalDomain`, `axes.domains`, `vendor_groups`, `vendor_scopes`, and the hierarchical sampler.

- [ ] **Step 4: Write failing eligibility-inheritance tests**

```rust
#[test]
fn domain_eligibility_replaces_category_axis_and_missing_axis_inherits() {
    let catalog = TaxonomyCatalog::from_yaml(V2_INHERITANCE_FIXTURE, None).unwrap();
    let resolved = catalog.resolved_eligibility("workplace", "email").unwrap();
    assert_eq!(resolved.platform_groups, vec!["microsoft_365", "google_workspace"]);
    assert_eq!(resolved.environments, vec!["saas_tenant", "hybrid"]);
}

#[test]
fn category_cannot_mix_weighted_and_unweighted_domains() {
    let error = TaxonomyCatalog::from_yaml(V2_MIXED_WEIGHT_FIXTURE, None).unwrap_err();
    assert!(error.to_string().contains("mix weighted and unweighted domains"));
}
```

- [ ] **Step 5: Implement validation, inheritance, and fixed sampling order**

For every constrainable axis, resolve domain value, otherwise category value, otherwise all enabled global values. Reject explicit empty lists, duplicates, unknown references, unsatisfied platform cardinality, mixed domain-weight modes, invalid complete weights, unreachable domains, and missing subdomains.

Sample in the exact spec order: category, domain, subdomain, task family, environment, platform scope, platforms, incident mechanism, evidence condition, evidence bundle, action risk, difficulty, presentation, then language in the caller.

- [ ] **Step 6: Add universal task-v2 schema tests**

Create a fixture containing `category`, `domain`, `subdomain`, and all universal coordinates. Assert valid records pass and v1 `vendor_scope/vendors` fields fail with `additionalProperties: false`.

Run: `cargo test schema::tests taxonomy::tests -- --nocapture`

Expected before the v2 schema is embedded: FAIL because the current schema is NetOps-specific.

- [ ] **Step 7: Complete the v2 schema and verify GREEN**

Run: `cargo test schema::tests taxonomy::tests -- --nocapture`

Expected: all v2 parsing, validation, inheritance, cardinality, weighting, seeded sampling, and schema tests pass.

- [ ] **Step 8: Commit the universal loader**

```bash
git add src/taxonomy.rs src/schema.rs schemas/task-v2.schema.json tests/fixtures/taxonomy
git commit -m "feat: unify compositional taxonomy schema"
```

---

### Task B: Migrate Enterprise NetOps to Taxonomy v2

**Files:**
- Modify: `docs/netops-taxonomy.yaml`
- Rename: `prompts/netops-taskgen-system-v1.txt` to `prompts/netops-taskgen-system-v2.txt`
- Modify: `prompts/netops-taskgen-system-v2.txt`
- Modify: `schemas/netops-task-v1.schema.json` only to remove it after references move to `schemas/task-v2.schema.json`
- Test: `src/taxonomy.rs`

**Interfaces:**
- Consumes: Task A's v2 loader.
- Produces: `scogo-enterprise-netops-v2` with one category, 25 weighted domains, 531 subdomains, generalized platform coordinates, and unchanged operational scope.

- [ ] **Step 1: Write the failing NetOps migration-count test**

```rust
#[test]
fn embedded_netops_v2_preserves_all_subjects() {
    let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();
    assert_eq!(catalog.id(), "scogo-enterprise-netops-v2");
    assert_eq!(catalog.category_count(), 1);
    assert_eq!(catalog.domain_count(), 25);
    assert_eq!(catalog.subdomain_count(), 531);
    assert_eq!(catalog.platform_group_count(), 14);
}
```

- [ ] **Step 2: Run the migration test and verify RED**

Run: `cargo test taxonomy::tests::embedded_netops_v2_preserves_all_subjects -- --nocapture`

Expected: FAIL because the file is still v1 with `axes.domains` and vendor fields.

- [ ] **Step 3: Convert NetOps YAML without losing taxonomy content**

Move the current 25 domains under:

```yaml
categories:
  - id: enterprise_netops
    label: Enterprise NetOps
    weight: 1.0
    domains: [...]
```

Rename `vendor_groups/vendors/vendor_scopes/vendor_neutral/single_vendor/multi_vendor` to `platform_groups/platforms/platform_scopes/platform_neutral/single_platform/multi_platform`. Move each domain's allow-lists under `eligibility`. Retain every domain weight, label, subdomain, platform weight, environment constraint, incident constraint, evidence constraint, task-family bound, and difficulty distribution.

- [ ] **Step 4: Update the NetOps generation prompt to universal coordinates**

Require category/domain/subdomain plus task, environment, platform, incident, evidence, risk, difficulty, and presentation coordinates. Replace v1 field names and keep the enterprise-versus-telecom boundary unchanged.

- [ ] **Step 5: Verify NetOps v2 GREEN**

Run: `cargo test taxonomy::tests::embedded_netops_v2 -- --nocapture`

Expected: counts, weights, all references, platform cardinality, and seeded sampling snapshots pass.

- [ ] **Step 6: Commit the NetOps migration**

```bash
git add docs/netops-taxonomy.yaml prompts/netops-taskgen-system-v2.txt schemas
git commit -m "feat: migrate netops taxonomy to v2"
```

---

### Task C: Convert IT Ops to Compositional Taxonomy v2

**Files:**
- Modify: `docs/it-ops-taxonomy.yaml`
- Create: `prompts/itops-taskgen-system-v2.txt`
- Create: `docs/it-ops-taxonomy-v4-migration.json`
- Test: `src/taxonomy.rs`
- Test: `tests/itops_taxonomy_migration.rs`

**Interfaces:**
- Consumes: Task A's v2 loader and universal axes.
- Produces: embedded `scogo-itops-v4`, platform groups, category/domain eligibility, complete coordinates, and a lossless migration report.

- [ ] **Step 1: Write failing lossless-migration tests**

```rust
#[test]
fn itops_v4_is_compositional_and_lossless() {
    let catalog = TaxonomyCatalog::embedded_itops().unwrap();
    assert_eq!(catalog.id(), "scogo-itops-v4");
    assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
    assert_eq!(catalog.category_count(), 14);
    assert_eq!(catalog.domain_count(), 129);
    assert_eq!(catalog.subdomain_count(), 884);
}

#[test]
fn migration_report_has_no_missing_subjects() {
    let report: MigrationReport = serde_json::from_str(include_str!("../docs/it-ops-taxonomy-v4-migration.json")).unwrap();
    assert_eq!(report.source_counts, Counts { categories: 14, domains: 129, subdomains: 884 });
    assert_eq!(report.source_counts, report.target_counts);
    assert!(report.missing_domains.is_empty());
    assert!(report.missing_subdomains.is_empty());
    assert!(report.duplicate_target_ids.is_empty());
}
```

- [ ] **Step 2: Run IT Ops migration tests and verify RED**

Run: `cargo test --test itops_taxonomy_migration -- --nocapture`

Expected: FAIL because IT Ops is v1 hierarchy-only and no migration report exists.

- [ ] **Step 3: Convert all IT Ops domains to stable IDs**

Retain the 14 category blocks, weights, labels, pillar/blurb/source metadata, domain labels, and all subdomain IDs. Add a stable snake-case `id` to each domain and leave domain `weight` absent so sampling remains uniform inside the category.

- [ ] **Step 4: Add the shared axes and IT Ops platform catalogue**

Add every axis and initial option listed in the unified taxonomy spec. Add weighted groups for public cloud, Microsoft 365, Google Workspace, collaboration, ITSM/CMDB, endpoint management, endpoint OS, identity, SecOps, network security, networking, observability, IaC/automation, Kubernetes, virtualization/private cloud, server OS, storage/backup, databases/data, DevOps delivery, enterprise applications, AI/agent platforms, OEM products, and open source.

Every product line currently represented in the OEM category must remain reachable as a platform.

- [ ] **Step 5: Add category eligibility and domain overrides**

Populate all 14 category defaults. Add domain overrides wherever category defaults would permit an invalid platform, environment, task family, evidence bundle, or action risk. At minimum, explicitly constrain email, collaboration, print/workplace devices, endpoint management, identity, cloud/FinOps, networking, observability, databases, storage/backup, DevOps delivery, enterprise applications, AI agents, and every OEM product family.

- [ ] **Step 6: Write the IT Ops v2 generation prompt**

Require operational hypothesis/evidence/tool behavior, missing-state abstention, current pricing retrieval for FinOps, read-only defaults, approval-gated changes, rollback/verification, and platform-authentic syntax only when a platform is selected. Prohibit certification trivia, hidden answers, and fabricated tool results.

- [ ] **Step 7: Generate and verify the lossless migration report**

The test helper compares a checked-in v3 subject snapshot fixture with v4. Write `docs/it-ops-taxonomy-v4-migration.json` containing exact source/target counts and empty loss/duplicate arrays.

- [ ] **Step 8: Add exhaustive reachability and seeded-coordinate tests**

For each category/domain/subdomain, use deterministic direct sampling helpers to prove that it resolves a non-empty eligible set on every axis and that every platform scope can be satisfied. Add snapshots for representative ITSM, M365/Google Workspace, cloud FinOps, endpoint, database, storage/backup, OEM, and agentic tasks.

- [ ] **Step 9: Verify IT Ops v2 GREEN**

Run: `cargo test --test itops_taxonomy_migration taxonomy::tests::embedded_itops_v4 -- --nocapture`

Expected: `14/129/884`, lossless report, eligibility, reachability, platform cardinality, and seeded snapshots all pass.

- [ ] **Step 10: Commit the IT Ops conversion**

```bash
git add docs/it-ops-taxonomy.yaml docs/it-ops-taxonomy-v4-migration.json prompts/itops-taskgen-system-v2.txt tests/itops_taxonomy_migration.rs src/taxonomy.rs
git commit -m "feat: make itops taxonomy compositional"
```

---

### Task D: Migrate Canonical Trajectory Schemas and ATIF Mapping

**Files:**
- Modify: `schemas/netops-teacher-trajectory-audit-v1.schema.json`
- Modify: `schemas/netops-teacher-trajectory-sft-v1.schema.json`
- Modify: `src/atif.rs`
- Modify: `tests/fixtures/canonical/valid-task.json`
- Modify: `tests/fixtures/canonical/valid-audit.json`
- Modify: `tests/fixtures/canonical/valid-sft.json`
- Modify: `tests/fixtures/atif-v1.7/valid-scogo-roundtrip.json`
- Test: `src/atif.rs`
- Test: `src/schema.rs`

**Interfaces:**
- Consumes: Task A's universal coordinates and existing ATIF v1.7 conversion contract.
- Produces: canonical audit/SFT records that preserve v2 coordinates through ATIF export/import without changing ATIF itself.

- [ ] **Step 1: Write failing coordinate round-trip tests**

Update the canonical fixture to use `scogo.taskgen.task.v2`, `category_id`, `platform_scope`, and `platforms`. Assert:

```rust
#[test]
fn atif_roundtrip_preserves_universal_platform_coordinates() {
    let canonical = canonical_v2_fixture();
    let atif = export_value(canonical.clone()).unwrap();
    let imported = import_value(atif).unwrap();
    let coordinates = &imported["task"]["coordinates"];
    assert_eq!(coordinates["category_id"], "enterprise_netops");
    assert_eq!(coordinates["platform_scope"], "multi_platform");
    assert_eq!(coordinates["platforms"], json!(["cisco_ios_xe", "juniper_junos"]));
}
```

- [ ] **Step 2: Run schema and ATIF tests and verify RED**

Run: `cargo test atif::tests schema::tests -- --nocapture`

Expected: v2 fixtures fail because canonical schemas and ATIF fallback coordinates still require v1 vendor fields.

- [ ] **Step 3: Update canonical coordinate definitions**

Replace `vendor_scope/vendors` with `platform_scope/platforms`, require `category_id`, and accept both taxonomy IDs through non-empty strings rather than a NetOps-v1 constant. Keep canonical audit and SFT schema version identifiers stable because this is an intentional breaking development-branch change and no released compatibility is required.

- [ ] **Step 4: Update ATIF import fallback coordinates**

For unverified external ATIF imports, emit:

```json
{
  "taxonomy_id": "external_atif_unverified",
  "category_id": "external_atif_unverified",
  "task_family": "external_atif_unverified",
  "environment": "external_atif_unverified",
  "platform_scope": "platform_neutral",
  "platforms": [],
  "incident_mechanism": "external_atif_unverified",
  "evidence_condition": "external_atif_unverified",
  "evidence_bundle": "external_atif_unverified",
  "action_risk": "external_atif_unverified",
  "presentation": "external_atif_unverified"
}
```

Preserve Scogo coordinates in the guarded ATIF context extension exactly as before, with v2 field names.

- [ ] **Step 5: Verify ATIF and schemas GREEN**

Run: `cargo test atif::tests schema::tests -- --nocapture`

Expected: canonical JSON/JSONL and ATIF v1.7 round trips pass with universal coordinates and external imports remain explicitly unverified.

- [ ] **Step 6: Commit schema and ATIF migration**

```bash
git add schemas/netops-teacher-trajectory-audit-v1.schema.json schemas/netops-teacher-trajectory-sft-v1.schema.json src/atif.rs tests/fixtures
git commit -m "feat: carry universal coordinates through atif"
```

---

### Task 1: Provider and Credential Resolution

**Files:**
- Create: `src/provider.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Test: `src/provider.rs`

**Interfaces:**
- Produces: `CredentialPool`, `ProviderConfig`, `ProviderOverrides`, `normalize_api_base`, and `resolve_review_provider`.
- Consumes: generation CLI endpoint, model, API key/keyfile, and the corresponding optional review values.

- [ ] **Step 1: Write failing provider-resolution tests**

Add unit tests that express the trust-boundary behavior:

```rust
#[test]
fn same_endpoint_inherits_generation_credentials() {
    let generation = ProviderConfig::for_test(
        "https://api.example/v1/",
        "generator",
        vec![SecretString::new("gen-key")],
    );
    let review = resolve_review_provider(&generation, ProviderOverrides::default()).unwrap();
    assert_eq!(review.api_base.as_str(), "https://api.example/v1");
    assert_eq!(review.model, "generator");
    assert_eq!(review.credentials.len(), 1);
}

#[test]
fn different_endpoint_requires_explicit_review_credentials() {
    let generation = ProviderConfig::for_test(
        "https://generator.example/v1",
        "generator",
        vec![SecretString::new("gen-key")],
    );
    let error = resolve_review_provider(
        &generation,
        ProviderOverrides {
            api_base: Some("https://reviewer.example/v1".into()),
            model: Some("reviewer".into()),
            credentials: None,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("explicit review credentials"));
}

#[test]
fn secret_debug_output_is_redacted() {
    let secret = SecretString::new("never-print-me");
    assert!(!format!("{secret:?}").contains("never-print-me"));
}
```

- [ ] **Step 2: Run the provider tests and verify RED**

Run: `cargo test provider::tests -- --nocapture`

Expected: compilation fails because `src/provider.rs` and the required types do not exist.

- [ ] **Step 3: Implement provider types and resolution**

Implement these public contracts:

```rust
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self;
    pub fn expose(&self) -> &str;
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

#[derive(Clone)]
pub struct CredentialPool {
    values: std::sync::Arc<Vec<SecretString>>,
    next: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl CredentialPool {
    pub fn new(values: Vec<SecretString>) -> anyhow::Result<Self>;
    pub fn next(&self) -> SecretString;
    pub fn len(&self) -> usize;
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub api_base: url::Url,
    pub model: String,
    pub credentials: CredentialPool,
}

#[derive(Default)]
pub struct ProviderOverrides {
    pub api_base: Option<String>,
    pub model: Option<String>,
    pub credentials: Option<CredentialPool>,
}

pub fn normalize_api_base(raw: &str) -> anyhow::Result<url::Url>;
pub fn resolve_review_provider(
    generation: &ProviderConfig,
    review: ProviderOverrides,
) -> anyhow::Result<ProviderConfig>;
pub fn load_credential_pool(
    keyfile: Option<&std::path::Path>,
    key: Option<String>,
    label: &str,
) -> anyhow::Result<CredentialPool>;
```

Use `url::Url`; strip only trailing path slashes. Derive endpoint equality from scheme, host, effective port, and normalized path. Reject an empty pool and conflicting single-key/keyfile CLI forms. Add `url = "2"` to dependencies.

- [ ] **Step 4: Run provider tests and verify GREEN**

Run: `cargo test provider::tests -- --nocapture`

Expected: all provider tests pass.

- [ ] **Step 5: Commit the provider boundary**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/provider.rs
git commit -m "feat: isolate provider credentials"
```

---

### Task 2: Reviewer Schema, Prompts, and Taxonomy Defaults

**Files:**
- Create: `src/review.rs`
- Create: `schemas/prompt-review-v1.schema.json`
- Create: `prompts/itops-prompt-review-system-v2.txt`
- Create: `prompts/netops-prompt-review-system-v2.txt`
- Modify: `src/schema.rs`
- Modify: `src/taxonomy.rs`
- Modify: `docs/it-ops-taxonomy.yaml`
- Modify: `docs/netops-taxonomy.yaml`
- Test: `src/review.rs`
- Test: `src/taxonomy.rs`

**Interfaces:**
- Consumes: `ProviderConfig`, `TaskEntry`, and sampled taxonomy coordinates.
- Produces: `ReviewDecision`, `ReviewEnvelope`, `ReviewReason`, `ReviewClient`, and taxonomy `default_review_system_prompt_path()`.

- [ ] **Step 1: Write failing reviewer-contract tests**

```rust
#[test]
fn accepted_review_has_no_reasons_or_retry_guidance() {
    let json = r#"{
      "schema_version":"scogo.taskgen.prompt-review.v1",
      "verdict":"accept",
      "reason_codes":[],
      "summary":"The prompt is operationally coherent.",
      "retry_guidance":""
    }"#;
    let decision = ReviewDecision::parse_and_validate(json).unwrap();
    assert_eq!(decision.verdict, ReviewVerdict::Accept);
}

#[test]
fn rejected_review_requires_reason_and_guidance() {
    let json = r#"{
      "schema_version":"scogo.taskgen.prompt-review.v1",
      "verdict":"reject",
      "reason_codes":[],
      "summary":"Bad prompt.",
      "retry_guidance":""
    }"#;
    assert!(ReviewDecision::parse_and_validate(json).is_err());
}

#[test]
fn taxonomy_resolves_review_prompt_relative_to_yaml() {
    let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml")).unwrap();
    assert_eq!(
        catalog.default_review_system_prompt_path().unwrap(),
        PathBuf::from("docs/../prompts/netops-prompt-review-system-v2.txt")
    );
}
```

- [ ] **Step 2: Run reviewer tests and verify RED**

Run: `cargo test review::tests taxonomy::tests -- --nocapture`

Expected: compilation fails because the review module, schema, and taxonomy accessor are missing.

- [ ] **Step 3: Add the exact review schema and Rust types**

Implement:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict { Accept, Reject }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    TechnicalInaccuracy,
    InventedPlatformFeature,
    InvalidCommandOrSyntax,
    ProtocolOrArchitectureError,
    UnsupportedCausality,
    NumericalOrTemporalInconsistency,
    InternalContradiction,
    CoordinateMismatch,
    InsufficientOrInvalidEvidence,
    NotOperational,
    UnsafeOrUnapprovedChange,
    HiddenAnswerOrSolutionLeakage,
    ScopeViolation,
    AmbiguousOrUnanswerable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    pub schema_version: String,
    pub verdict: ReviewVerdict,
    pub reason_codes: Vec<ReviewReason>,
    pub summary: String,
    pub retry_guidance: String,
}

impl ReviewDecision {
    pub fn parse_and_validate(raw: &str) -> anyhow::Result<Self>;
}
```

Validate with the embedded JSON Schema and explicit accept/reject cross-field rules. The model supplies only `ReviewDecision`; Taskgen supplies envelope metadata.

- [ ] **Step 4: Add taxonomy-specific reviewer prompts and defaults**

Add to `RawDefaults`:

```rust
review_system_prompt_file: Option<PathBuf>,
```

Add:

```rust
pub fn default_review_system_prompt_path(&self) -> Option<PathBuf>;
```

Add `review_system_prompt_file` to both v2 YAML files and validate configured generation/reviewer prompt files during command preflight.

- [ ] **Step 5: Implement the review API request**

Implement:

```rust
pub struct ReviewRequest<'a> {
    pub entry: &'a TaskEntry,
    pub taxonomy_id: &'a str,
    pub taxonomy_kind: TaxonomyKind,
    pub system_prompt: &'a str,
    pub retry_context: Option<&'a str>,
}

pub struct ReviewResult {
    pub decision: ReviewDecision,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait::async_trait]
pub trait CandidateReviewer: Send + Sync {
    async fn review(&self, request: ReviewRequest<'_>) -> anyhow::Result<ReviewResult>;
}

pub struct ReviewClient {
    provider: ProviderConfig,
    client: reqwest::Client,
    max_output_tokens: u64,
}
```

Reuse the existing OpenAI-compatible response extraction rules, request strict JSON when supported, and still validate parsed output independently. Add `async-trait = "0.1"`.

- [ ] **Step 6: Run reviewer and taxonomy tests and verify GREEN**

Run: `cargo test review::tests taxonomy::tests -- --nocapture`

Expected: all review and taxonomy tests pass.

- [ ] **Step 7: Commit reviewer contracts**

```bash
git add Cargo.toml Cargo.lock src/review.rs src/schema.rs src/taxonomy.rs schemas/prompt-review-v1.schema.json prompts/itops-prompt-review-system-v2.txt prompts/netops-prompt-review-system-v2.txt docs/it-ops-taxonomy.yaml docs/netops-taxonomy.yaml
git commit -m "feat: add prompt quality reviewer"
```

---

### Task 3: Native Exact and Lexical Dedup

**Files:**
- Create: `src/dedup.rs`
- Create: `tests/fixtures/dedup/input.jsonl`
- Create: `tests/fixtures/dedup/expected-lexical.jsonl`
- Create: `tests/fixtures/dedup/expected-dropped.jsonl`
- Modify: `src/main.rs`
- Test: `src/dedup.rs`

**Interfaces:**
- Consumes: `TaskEntry` or arbitrary JSON records with a configurable prompt field.
- Produces: `DedupConfig`, `DedupIndex`, `DuplicateMatch`, `DedupReason`, and `run_dedup_command`.

- [ ] **Step 1: Write failing normalization and lexical tests**

```rust
#[test]
fn exact_normalization_preserves_word_boundaries() {
    assert_eq!(normalize_prompt("  VLAN\n10  DOWN "), "vlan 10 down");
    assert_ne!(exact_key("ab c"), exact_key("a bc"));
}

#[test]
fn lexical_comparison_is_bucketed() {
    let mut index = DedupIndex::lexical(DedupConfig::default());
    index.insert(fixture("en", "network", "acl", "check the same five word incident template now"), None).unwrap();
    let other_domain = fixture("en", "security", "acl", "check the same five word incident template now");
    assert!(index.find_duplicate(&other_domain, None).unwrap().is_none());
}

#[test]
fn jaccard_threshold_is_inclusive() {
    assert!(is_duplicate_score(0.80, 0.80));
    assert!(!is_duplicate_score(0.7999, 0.80));
}
```

- [ ] **Step 2: Run dedup unit tests and verify RED**

Run: `cargo test dedup::tests -- --nocapture`

Expected: compilation fails because `src/dedup.rs` does not exist.

- [ ] **Step 3: Implement exact and bucketed lexical dedup**

Implement:

```rust
#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
pub enum DedupMode { Lexical, Semantic }

#[derive(Debug, Clone)]
pub struct DedupConfig {
    pub mode: DedupMode,
    pub prompt_field: String,
    pub ngram: usize,
    pub jaccard_threshold: f32,
    pub semantic_threshold: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupReason { Exact, Jaccard, Semantic }

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateMatch {
    pub reason: DedupReason,
    pub accepted_sha256: String,
    pub score: Option<f32>,
    pub threshold: Option<f32>,
}

pub struct DedupCandidate<'a> {
    pub prompt: &'a str,
    pub language: Option<&'a str>,
    pub domain: &'a str,
    pub subdomain: &'a str,
}

pub struct DedupIndex { /* exact hashes and bucketed accepted features */ }

impl DedupIndex {
    pub fn new(config: DedupConfig, embedder: Option<Arc<dyn PromptEmbedder>>) -> anyhow::Result<Self>;
    pub fn find_duplicate(
        &self,
        candidate: &DedupCandidate<'_>,
        embedding: Option<&[f32]>,
    ) -> anyhow::Result<Option<DuplicateMatch>>;
    pub fn insert(
        &mut self,
        candidate: DedupCandidate<'_>,
        embedding: Option<Vec<f32>>,
    ) -> anyhow::Result<String>;
}
```

Port the Python whitespace-preserving exact hash, bucket key, short-input behavior, unique n-gram sets, and Jaccard survivor order exactly.

- [ ] **Step 4: Add standalone-command fixture tests**

The fixture must include invalid JSON, a global exact clone, a same-bucket lexical near-duplicate, and identical text in a different bucket. Assert kept rows, dropped reasons, match hashes, and report counts.

Run: `cargo test dedup::tests::standalone_lexical_fixture -- --nocapture`

Expected before the command implementation: FAIL because no outputs are produced.

- [ ] **Step 5: Implement `taskgen dedup` lexical mode**

Add a `Dedup` top-level Clap subcommand with atomic kept/dropped/report destinations. Reject missing string prompt fields, same output/dropped paths, invalid thresholds, zero n-gram size, and existing destinations without `--overwrite`.

- [ ] **Step 6: Run dedup tests and verify GREEN**

Run: `cargo test dedup::tests -- --nocapture`

Expected: all exact, lexical, and standalone fixture tests pass.

- [ ] **Step 7: Commit native lexical dedup**

```bash
git add src/main.rs src/dedup.rs tests/fixtures/dedup
git commit -m "feat: port lexical dedup to rust"
```

---

### Task 4: Local Semantic Dedup

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/dedup.rs`
- Create: `tests/fixtures/dedup/semantic-cases.json`
- Test: `src/dedup.rs`

**Interfaces:**
- Consumes: `DedupConfig` and candidate prompts.
- Produces: `PromptEmbedder`, `FastEmbedder`, local model selection, and cosine duplicate matches.

- [ ] **Step 1: Write failing semantic-index tests with a deterministic real embedder implementation**

Use a tiny test embedder that maps fixture strings to fixed vectors; this tests production cosine/index behavior without downloading a model:

```rust
#[async_trait::async_trait]
trait PromptEmbedder: Send + Sync {
    fn model_id(&self) -> &str;
    async fn embed(&self, prompt: &str) -> anyhow::Result<Vec<f32>>;
}

#[tokio::test]
async fn semantic_duplicate_is_rejected_at_inclusive_threshold() {
    let embedder = Arc::new(FixedEmbedder::new([
        ("first", vec![1.0, 0.0]),
        ("paraphrase", vec![0.9, 0.4358899]),
    ]));
    let mut index = DedupIndex::new(DedupConfig::semantic_at(0.9), Some(embedder.clone())).unwrap();
    let first = candidate("first");
    index.insert(first, Some(embedder.embed("first").await.unwrap())).unwrap();
    let vector = embedder.embed("paraphrase").await.unwrap();
    let hit = index.find_duplicate(&candidate("paraphrase"), Some(&vector)).unwrap().unwrap();
    assert!(matches!(hit.reason, DedupReason::Semantic));
}
```

- [ ] **Step 2: Run semantic tests and verify RED**

Run: `cargo test dedup::tests::semantic -- --nocapture`

Expected: compilation fails because `PromptEmbedder` and semantic index support are missing.

- [ ] **Step 3: Implement semantic index and local FastEmbed adapter**

Add FastEmbed using the current compatible crate release. Implement:

```rust
pub enum SemanticModel {
    AllMiniLmL6V2,
    MultilingualE5Small,
}

pub struct FastEmbedder {
    model_id: String,
    model: tokio::sync::Mutex<fastembed::TextEmbedding>,
}

impl FastEmbedder {
    pub fn initialize(
        model: SemanticModel,
        cache_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self>;
}
```

Run blocking ONNX work through `tokio::task::spawn_blocking`. Preflight model initialization before creating run artifacts. Store accepted embeddings only within their bucket and compare exact cosine scores.

- [ ] **Step 4: Run semantic tests and verify GREEN**

Run: `cargo test dedup::tests::semantic -- --nocapture`

Expected: semantic index tests pass without network access.

- [ ] **Step 5: Run a cache-backed FastEmbed smoke test**

Mark the model-loading smoke test `#[ignore]` so ordinary unit tests remain offline. Run it explicitly with a temporary or configured cache:

Run: `cargo test dedup::tests::fastembed_smoke -- --ignored --nocapture`

Expected: one non-empty 384-dimensional embedding and finite self-cosine near `1.0`.

- [ ] **Step 6: Commit semantic dedup**

```bash
git add Cargo.toml Cargo.lock src/dedup.rs tests/fixtures/dedup/semantic-cases.json
git commit -m "feat: add local semantic dedup"
```

---

### Task 5: Working Artifacts and Atomic Publication

**Files:**
- Create: `src/artifacts.rs`
- Create: `schemas/task-rejection-v1.schema.json`
- Modify: `src/schema.rs`
- Modify: `Cargo.toml`
- Test: `src/artifacts.rs`

**Interfaces:**
- Consumes: accepted `TaskEntry`, `ReviewEnvelope`, `RejectionEvent`, and `RunReport`.
- Produces: `RunArtifacts::create`, journal append methods, `publish_new`, `publish_append`, and `retain_incomplete`.

- [ ] **Step 1: Write failing atomic-publication tests**

```rust
#[test]
fn incomplete_run_does_not_create_requested_output() {
    let temp = tempfile::tempdir().unwrap();
    let final_path = temp.path().join("tasks.jsonl");
    let mut artifacts = RunArtifacts::create(&final_path, false, false).unwrap();
    artifacts.append_accepted(b"{\"prompt\":\"one\"}\n").unwrap();
    let retained = artifacts.retain_incomplete("attempts_exhausted").unwrap();
    assert!(!final_path.exists());
    assert!(retained.exists());
}

#[test]
fn append_preserves_original_until_exact_success() {
    let temp = tempfile::tempdir().unwrap();
    let final_path = temp.path().join("tasks.jsonl");
    std::fs::write(&final_path, "{\"prompt\":\"old\"}\n").unwrap();
    let mut artifacts = RunArtifacts::create(&final_path, true, false).unwrap();
    artifacts.append_accepted(b"{\"prompt\":\"new\"}\n").unwrap();
    assert_eq!(std::fs::read_to_string(&final_path).unwrap(), "{\"prompt\":\"old\"}\n");
}
```

- [ ] **Step 2: Run artifact tests and verify RED**

Run: `cargo test artifacts::tests -- --nocapture`

Expected: compilation fails because `RunArtifacts` does not exist.

- [ ] **Step 3: Implement journals and final publication**

Implement:

```rust
pub struct RunArtifacts {
    pub run_id: uuid::Uuid,
    working_dir: tempfile::TempDir,
    requested_output: PathBuf,
    accepted: BufWriter<File>,
    reviews: BufWriter<File>,
    rejected: BufWriter<File>,
}

impl RunArtifacts {
    pub fn create(output: &Path, append: bool, overwrite: bool) -> anyhow::Result<Self>;
    pub fn append_accepted(&mut self, serialized: &[u8]) -> anyhow::Result<()>;
    pub fn append_review(&mut self, value: &ReviewEnvelope) -> anyhow::Result<()>;
    pub fn append_rejection(&mut self, value: &RejectionEvent) -> anyhow::Result<()>;
    pub fn publish(self, report: RunReport, expected_new: usize) -> anyhow::Result<PublishedArtifacts>;
    pub fn retain_incomplete(self, reason: &str) -> anyhow::Result<PathBuf>;
}
```

Use `tempfile` and `uuid`. Flush and `sync_all` journals, validate count and schemas, materialize append replacements inside the working directory, write sidecars atomically, and rename the final dataset last.

- [ ] **Step 4: Add rejection-schema tests**

Test allowed gates, required candidate hash, optional dedup match, embedded quality review, and rejection of unknown properties.

Run: `cargo test artifacts::tests schema::tests -- --nocapture`

Expected before schema integration: FAIL on rejection validation.

- [ ] **Step 5: Complete schema integration and verify GREEN**

Run: `cargo test artifacts::tests schema::tests -- --nocapture`

Expected: all artifact and schema tests pass.

- [ ] **Step 6: Commit atomic artifacts**

```bash
git add Cargo.toml Cargo.lock src/artifacts.rs src/schema.rs schemas/task-rejection-v1.schema.json
git commit -m "feat: publish exact generation atomically"
```

---

### Task 6: Exact-Count Acceptance Scheduler

**Files:**
- Create: `src/acceptance.rs`
- Modify: `src/main.rs`
- Test: `src/acceptance.rs`

**Interfaces:**
- Consumes: pre-sampled `Slot`, `CandidateGenerator`, `CandidateReviewer`, shared `DedupIndex`, and `RunArtifacts`.
- Produces: `AcceptanceConfig`, `AcceptanceRun`, exact accepted records, rejection events, and retry feedback.

- [ ] **Step 1: Write failing scripted acceptance-loop tests**

Define scripted test doubles implementing production traits. The first scenario returns, in order: invalid candidate, reviewer rejection, exact duplicate, lexical duplicate, semantic duplicate, then enough unique accepts.

```rust
#[tokio::test]
async fn successful_run_replaces_every_rejection_and_returns_exact_count() {
    let slots = vec![slot(0), slot(1), slot(2)];
    let generator = ScriptedGenerator::new(scripted_candidates());
    let reviewer = ScriptedReviewer::new(scripted_reviews());
    let result = run_acceptance(
        slots,
        AcceptanceConfig { workers: 2, max_attempts_per_slot: 20 },
        &generator,
        &reviewer,
        test_dedup_index(),
        test_artifacts(),
    )
    .await
    .unwrap();
    assert_eq!(result.accepted_new, 3);
    assert!(result.rejected_total >= 5);
    assert_eq!(result.accepted_coordinates, result.sampled_coordinates);
}

#[tokio::test]
async fn exhausted_slot_is_nonzero_and_does_not_publish() {
    let error = run_acceptance(
        vec![slot(0)],
        AcceptanceConfig { workers: 1, max_attempts_per_slot: 2 },
        &AlwaysInvalidGenerator,
        &AlwaysAcceptReviewer,
        test_dedup_index(),
        test_artifacts(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("slot 0 exhausted"));
    assert!(!requested_output().exists());
}
```

- [ ] **Step 2: Run acceptance tests and verify RED**

Run: `cargo test acceptance::tests -- --nocapture`

Expected: compilation fails because the scheduler and traits do not exist.

- [ ] **Step 3: Implement generator and scheduler interfaces**

```rust
pub struct Slot {
    pub index: usize,
    pub sample: SampledTask,
    pub language: Option<String>,
}

pub struct GeneratedCandidate {
    pub entry: TaskEntry,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait::async_trait]
pub trait CandidateGenerator: Send + Sync {
    async fn generate(
        &self,
        slot: &Slot,
        attempt: usize,
        retry_guidance: Option<&str>,
    ) -> anyhow::Result<GeneratedCandidate>;
}

pub struct AcceptanceConfig {
    pub workers: usize,
    pub max_attempts_per_slot: usize,
}

pub struct AcceptanceRun {
    pub accepted_new: usize,
    pub rejected_total: usize,
    pub generation_usage: Usage,
    pub review_usage: Usage,
}
```

Schedule at most one in-flight attempt per slot. Requeue the same slot after candidate rejection. Count progress only after final commit. Bound all queues by slot count and worker count.

- [ ] **Step 4: Implement gate order and serialized final commit**

Implement validation, dedup precheck, review, and final dedup recheck. Compute embedding once per candidate. Keep reviewer guidance only on that slot. Use an async mutex around final dedup check plus journal insert so two workers cannot accept a race duplicate.

- [ ] **Step 5: Add concurrent duplicate-race regression test**

Use a barrier so two workers finish review together with paraphrases above threshold. Assert one is accepted, one is rejected at `final_dedup`, and the rejected slot later accepts a unique replacement.

Run: `cargo test acceptance::tests::concurrent_duplicate -- --nocapture`

Expected before the serialized recheck: FAIL with two accepted duplicates.

- [ ] **Step 6: Run all acceptance tests and verify GREEN**

Run: `cargo test acceptance::tests -- --nocapture`

Expected: all acceptance, exhaustion, feedback-isolation, and race tests pass.

- [ ] **Step 7: Commit the exact-count scheduler**

```bash
git add src/main.rs src/acceptance.rs
git commit -m "feat: generate exact accepted task counts"
```

---

### Task 7: CLI Integration and Run Reporting

**Files:**
- Modify: `src/main.rs`
- Modify: `src/provider.rs`
- Modify: `src/review.rs`
- Modify: `src/dedup.rs`
- Modify: `src/acceptance.rs`
- Modify: `src/artifacts.rs`
- Test: `src/main.rs`
- Test: `tests/generate_acceptance.rs`

**Interfaces:**
- Consumes: all modules from Tasks 1-6.
- Produces: final `taskgen generate` CLI, safe run report, and local fake-provider integration coverage.

- [ ] **Step 1: Write failing CLI parsing tests**

Cover:

```rust
#[test]
fn parses_different_review_provider() {
    let cli = Cli::try_parse_from([
        "taskgen", "generate",
        "--api-base", "https://generator.example/v1",
        "--api-key", "gen",
        "--model", "generator",
        "--review-api-base", "https://reviewer.example/v1",
        "--review-api-key", "review",
        "--review-model", "reviewer",
        "--count", "10",
    ]).unwrap();
    assert_generate(cli);
}

#[test]
fn old_optional_dedup_flag_is_rejected() {
    assert!(Cli::try_parse_from(["taskgen", "generate", "--dedup"]).is_err());
}
```

- [ ] **Step 2: Run CLI tests and verify RED**

Run: `cargo test main_tests -- --nocapture`

Expected: parse tests fail because review and replacement-dedup flags are absent.

- [ ] **Step 3: Replace the old generation arguments and post-hoc dedup block**

Add every flag from the spec, including review provider/prompt/pricing, max attempts, dedup mode/threshold/model/cache, and generation `--overwrite`. Remove `--dedup`, `--dedup-threshold`, and the post-generation trigram rewrite.

- [ ] **Step 4: Wire `run_generate` through preflight and acceptance**

Order:

```text
parse CLI
load/validate taxonomy and both prompts
resolve generation and review providers
initialize semantic model
load existing append records into validation/dedup indexes
pre-sample N slots
create working artifacts
run acceptance scheduler
validate exact count and final records
publish sidecars and dataset
print accepted/rejected/token/cost summary
```

Use a generator adapter around the existing OpenAI-compatible completion logic. Preserve proxy, free-model, Qwen, GPT/o-series, timeout, and cancellation behavior unless contradicted by the new success contract.

- [ ] **Step 5: Write local fake-provider integration tests**

Use `wiremock` to expose `/v1/chat/completions`. Route generation and review requests by model name and system-prompt marker. Script rejected and duplicate candidates before accepted replacements. Run the compiled command and assert:

- exit code zero;
- requested `-c` line count exactly matches;
- every final row has an accepted review envelope;
- rejection counts match the script;
- final standalone dedup keeps every row;
- fake provider received no cross-endpoint credential.

Add `assert_cmd`, `predicates`, and `wiremock` as dev dependencies.

- [ ] **Step 6: Run integration test and verify RED, then GREEN**

Run before completing wiring: `cargo test --test generate_acceptance -- --nocapture`

Expected RED: command either exits early or writes fewer than requested records.

Run after wiring: `cargo test --test generate_acceptance -- --nocapture`

Expected GREEN: all exact-count, provider-boundary, append, and failure-publication scenarios pass.

- [ ] **Step 7: Add and test safe run reporting**

Serialize endpoint origins, model IDs, token/cost totals, thresholds, coordinate distributions, output hashes, and terminal status. Add a regression test with sentinel generation/review keys and assert they do not occur in output, sidecars, reports, stdout, or stderr.

Run: `cargo test --test generate_acceptance secret -- --nocapture`

Expected: sentinel keys are absent.

- [ ] **Step 8: Commit CLI integration**

```bash
git add Cargo.toml Cargo.lock src tests/generate_acceptance.rs
git commit -m "feat: enforce reviewed unique generation"
```

---

### Task 8: Documentation, Full Verification, and ARM64 Release Gates

**Files:**
- Modify: `README.md`
- Modify: `docs/netops-data-contract.md`
- Modify: `.github/workflows/release.yml` only if FastEmbed requires explicit build configuration
- Test: repository commands

**Interfaces:**
- Consumes: final CLI behavior.
- Produces: operator documentation and release evidence.

- [ ] **Step 1: Update operator documentation**

Document accepted-count semantics, same-provider inheritance, different-provider review configuration, no cross-provider key inheritance, reviewer prompt precedence, local embedding cache preparation, standalone Rust dedup, audit sidecars, append, incomplete working directories, budgets, and the preserved IT Ops hierarchy inside the universal compositional v2 schema.

- [ ] **Step 2: Remove stale manual-dedup guidance**

Search:

```bash
rg -n "--dedup|dedup_jsonl.py|trigram|0\.6|post-generation" README.md docs src tests
```

Expected: only migration/history context explicitly describing removal remains.

- [ ] **Step 3: Run formatting and complete tests**

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: zero failures and zero warnings.

- [ ] **Step 4: Run release builds**

```bash
cargo build --release
cargo build --release --target aarch64-apple-darwin
```

Run Linux ARM64 on the native CI runner if unavailable locally:

```bash
cargo build --release --target aarch64-unknown-linux-gnu
```

Expected: release binary builds with semantic dedup included on both release targets.

- [ ] **Step 5: Revalidate both taxonomies**

```bash
./target/release/taskgen taxonomy validate --taxonomy docs/it-ops-taxonomy.yaml
./target/release/taskgen taxonomy validate --taxonomy docs/netops-taxonomy.yaml
```

Expected: IT Ops reports compositional `14/129/884`; NetOps reports compositional `1/25/531`.

- [ ] **Step 6: Commit documentation and release adjustments**

```bash
git add README.md docs/netops-data-contract.md .github/workflows/release.yml
git commit -m "docs: explain reviewed exact-count generation"
```

---

### Task 9: TokenRouter `-c 10` Live Canary

**Files:**
- No tracked source files
- Create outside Git tracking: run-specific output and sidecars under a temporary directory

**Interfaces:**
- Consumes: user-supplied TokenRouter credential and release binary.
- Produces: fresh evidence for exact count, review acceptance, dedup stability, and secret hygiene.

- [ ] **Step 1: Create a private temporary output directory**

Use `mktemp -d` and confirm the path is outside the repository. Do not put the API key in a script, shell history file, command transcript, output name, or environment file.

- [ ] **Step 2: Run same-model/same-provider generation**

Pass the credential through the process environment and invoke:

```bash
./target/release/taskgen generate \
  --taxonomy docs/netops-taxonomy.yaml \
  --api-base https://api.tokenrouter.com/v1 \
  --model qwen/qwen3.8-max-free \
  --count 10 \
  --workers 5 \
  --max-attempts-per-slot 20 \
  --output "$TEMP_OUTPUT/tasks.jsonl"
```

The omitted `--review-model`, `--review-api-base`, and review key prove same-model/same-provider inheritance.

- [ ] **Step 3: Validate exact count and all records**

```bash
wc -l "$TEMP_OUTPUT/tasks.jsonl"
jq -e -s 'length == 10 and all(.[]; (.prompt | type == "string") and (.prompt | length > 0))' "$TEMP_OUTPUT/tasks.jsonl"
jq -e -s 'length >= 10 and ([.[] | select(.review.verdict == "accept")] | length) == 10' "$TEMP_OUTPUT/tasks.reviews.jsonl"
jq -e '.requested_new == 10 and .accepted_new == 10 and .status == "complete"' "$TEMP_OUTPUT/tasks.run.json"
```

Expected: exactly 10 final rows, 10 accepted reviews, and a complete run report.

- [ ] **Step 4: Re-run native dedup against the final file**

```bash
./target/release/taskgen dedup \
  --input "$TEMP_OUTPUT/tasks.jsonl" \
  --output "$TEMP_OUTPUT/recheck.jsonl" \
  --dropped "$TEMP_OUTPUT/recheck.dropped.jsonl" \
  --report "$TEMP_OUTPUT/recheck.json" \
  --overwrite
```

Expected: 10 kept and zero dropped under the same default thresholds.

- [ ] **Step 5: Inspect quality and rejection evidence**

Read all 10 prompts, every accepted review summary, and every rejected event. Verify taxonomy-coordinate fidelity, plausible vendor/protocol syntax, operational evidence requirements, read-only defaults, and approval-gated changes. Record any false accept/reject as a failing fixture before changing implementation or prompts.

- [ ] **Step 6: Run secret scan**

Search the repository, temporary outputs, reports, and captured logs for the credential sentinel without printing matching content. The scan must report zero matching files.

- [ ] **Step 7: Report evidence**

Report the exact command shape with the credential redacted, output line counts, accepted/rejected counts, dedup recheck counts, token usage, reviewer inheritance behavior, and any unresolved quality observations. Do not report success unless every command above completed with exit code zero.
