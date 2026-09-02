use std::collections::VecDeque;
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) fn source_task_id(task: &Value) -> Result<String> {
    if !task.is_object() {
        bail!("source task must be a JSON object");
    }
    let identity = json!({
        "schema_version": task.get("schema_version").cloned().unwrap_or(Value::Null),
        "prompt": task.get("prompt").cloned().unwrap_or(Value::Null),
        "category": task.get("category").cloned().unwrap_or(Value::Null),
        "domain": task.get("domain").cloned().unwrap_or(Value::Null),
        "subdomain": task.get("subdomain").cloned().unwrap_or(Value::Null),
        "difficulty": task.get("difficulty").cloned().unwrap_or(Value::Null),
        "coordinates": task.get("coordinates").cloned().unwrap_or_else(|| json!({})),
    });
    Ok(format!(
        "task_{:x}",
        Sha256::digest(serde_json::to_vec(&identity)?)
    ))
}

pub(crate) fn source_population_sha256(tasks: &[Value]) -> Result<String> {
    let mut rows = tasks
        .iter()
        .map(|task| {
            Ok(json!({
                "source_task_id": source_task_id(task)?,
                "source_task": task,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by(|left, right| {
        left["source_task_id"]
            .as_str()
            .cmp(&right["source_task_id"].as_str())
    });
    let population = json!({
        "schema_version": "scogo.private-hf-source-population.v1",
        "rows": rows,
    });
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&population)?)
    ))
}

const MAX_SOURCE_ROWS: usize = 100_000;
const MAX_SOURCE_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct SourcePopulation {
    rows: Vec<SourceRow>,
    tasks: Vec<Value>,
    canonical_jsonl: Vec<u8>,
    population_sha256: String,
    held: Option<HeldFile>,
}

#[derive(Debug)]
struct EvidenceArtifact {
    logical_name: String,
    relative_file: String,
    held: Option<HeldFile>,
    bytes: Vec<u8>,
    sha256: String,
}

#[derive(Debug)]
struct PreparedRun {
    run_id: String,
    target: usize,
    work_dir: PathBuf,
    final_run_dir: PathBuf,
    source_repo_id: String,
    source_revision: String,
    source_file: String,
    source_selection: String,
    source: SourcePopulation,
    eligible_rows: Vec<SourceRow>,
    excluded_ids: Vec<String>,
    exclusion_authority: EvidenceArtifact,
    historical_reservation: Option<EvidenceArtifact>,
    prior_releases: Vec<PriorRelease>,
    taxonomy_held: Option<HeldFile>,
    reference_snapshot: ReferenceSnapshot,
    config: Value,
    config_sha256: String,
}

impl PreparedRun {
    fn assert_inputs_unchanged(&self) -> Result<()> {
        if let Some(held) = &self.source.held {
            held.assert_current()?;
        }
        if let Some(held) = &self.taxonomy_held {
            held.assert_current()?;
        }
        self.reference_snapshot.assert_current()?;
        let source_by_id = self
            .source
            .rows
            .iter()
            .map(|row| (row.task_id.clone(), row.task.clone()))
            .collect::<BTreeMap<_, _>>();
        for release in &self.prior_releases {
            if release.release.release_id != release.run_id
                || selected_task_sha256(&source_by_id, &release.selected_ids)?
                    != release.selected_task_sha256
            {
                bail!("typed prior release evidence changed after validation");
            }
            for artifact in release.artifacts.values() {
                artifact
                    .held
                    .as_ref()
                    .context("prior evidence lost its held file")?
                    .assert_current()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseSetView {
    schema_version: String,
    release_id: String,
    source_run_id: String,
    source_artifacts: BTreeMap<String, String>,
    source_receipt_sha256: Option<String>,
    source_manifest_sha256: Option<String>,
    source_manifest_bytes: Option<usize>,
    scheduler_contract: Option<String>,
    work_run_sha256: Option<String>,
    artifacts: Vec<ReleaseArtifactView>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyReceipt {
    schema_version: String,
    repo_id: String,
    repo_type: String,
    private: bool,
    revision: String,
    source_file: String,
    selection: String,
    rows: usize,
    subset_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentReceipt {
    schema_version: String,
    repo_id: String,
    repo_type: String,
    private: bool,
    revision: String,
    source_file: String,
    selection: String,
    rows: usize,
    subset_sha256: String,
    selected_source_task_ids: Vec<String>,
    source_file_rows: usize,
    source_file_sha256: String,
    source_population_sha256: String,
    excluded_source_task_ids: Vec<String>,
    exclusion_authority_sha256: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifactView {
    path: String,
    sha256: String,
    bytes: usize,
    rows: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct TaskgenManifestView {
    schema_version: String,
    status: String,
    run_id: String,
    artifacts: BTreeMap<String, ManifestArtifactView>,
}

#[derive(Debug, Deserialize)]
struct ManifestArtifactView {
    file: String,
    bytes: Option<usize>,
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalSourceTask {
    schema_version: String,
    source_task_id: String,
    split_group_id: String,
    split: String,
    prompt: String,
    domain: Option<String>,
    subdomain: Option<String>,
    difficulty: Option<String>,
    #[serde(default)]
    coordinates: BTreeMap<String, Value>,
    source_schema_version: String,
    source_task: Value,
    source_review: Option<Value>,
}

#[derive(Debug)]
enum PriorMode {
    Current,
    PinnedExternalLegacy,
}

#[derive(Debug)]
struct PriorRelease {
    run_id: String,
    release: ReleaseSetView,
    selected_ids: Vec<String>,
    selected_task_sha256: String,
    artifacts: BTreeMap<String, EvidenceArtifact>,
    authority_entry: Value,
    origin_evidence: Value,
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn contains_credential(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [("hf_", 8), ("sk-", 8), ("bearer ", 20)]
        .into_iter()
        .any(|(prefix, minimum)| {
            text.match_indices(prefix).any(|(index, _)| {
                if prefix == "sk-"
                    && index > 0
                    && text.as_bytes()[index - 1].is_ascii_alphanumeric()
                {
                    return false;
                }
                text[index + prefix.len()..]
                    .bytes()
                    .take_while(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    })
                    .count()
                    >= minimum
            })
        })
}

fn read_evidence(
    path: &Path,
    logical_name: &str,
    relative_file: String,
) -> Result<EvidenceArtifact> {
    let (held, bytes) = HeldFile::capture(path, 512 * 1024 * 1024)
        .with_context(|| format!("failed to read {logical_name}: {}", path.display()))?;
    if bytes.is_empty() {
        bail!("{logical_name} is empty");
    }
    if contains_credential(&bytes) {
        bail!("{logical_name} contains credential-like content");
    }
    Ok(EvidenceArtifact {
        logical_name: logical_name.to_string(),
        relative_file,
        held: Some(held),
        sha256: sha256_bytes(&bytes),
        bytes,
    })
}

fn parse_mappings(values: &[String], label: &str) -> Result<BTreeMap<String, String>> {
    let mut mappings = BTreeMap::new();
    for value in values {
        let (name, mapped) = value
            .split_once('=')
            .with_context(|| format!("invalid {label} mapping {value:?}"))?;
        if name.is_empty()
            || mapped.is_empty()
            || mappings.insert(name.into(), mapped.into()).is_some()
        {
            bail!("invalid or duplicate {label} mapping for {name:?}");
        }
    }
    Ok(mappings)
}

fn jsonl_rows(bytes: &[u8], label: &str) -> Result<Vec<Value>> {
    bytes
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.iter().all(u8::is_ascii_whitespace))
        .map(|(index, line)| {
            serde_json::from_slice(line)
                .with_context(|| format!("invalid {label} JSONL row {}", index + 1))
        })
        .collect()
}

fn selected_task_sha256(
    source_by_id: &BTreeMap<String, Value>,
    selected_ids: &[String],
) -> Result<String> {
    let mut bytes = Vec::new();
    for task_id in selected_ids {
        serde_json::to_writer(
            &mut bytes,
            source_by_id
                .get(task_id)
                .with_context(|| format!("prior task {task_id} is outside source population"))?,
        )?;
        bytes.push(b'\n');
    }
    Ok(sha256_bytes(&bytes))
}

fn validate_receipt_source(
    receipt: &LegacyReceipt,
    args: &crate::ReviewArgs,
    task_bytes: Option<&[u8]>,
    task_count: usize,
) -> Result<()> {
    if receipt.schema_version != "scogo.private-hf-subset-receipt.v1"
        || receipt.repo_id != args.source_repo_id.as_deref().unwrap_or_default()
        || receipt.repo_type != "dataset"
        || !receipt.private
        || receipt.revision != args.source_revision.as_deref().unwrap_or_default()
        || receipt.source_file != args.source_file.as_deref().unwrap_or_default()
        || receipt.rows != task_count
        || task_bytes.is_some_and(|bytes| receipt.subset_sha256 != sha256_bytes(bytes))
    {
        bail!("prior source receipt does not match exact source/task evidence");
    }
    validate_component(&receipt.selection, "prior source selection")
}

fn validate_review(review: &Value, expected_index: Option<usize>) -> Result<()> {
    if review["schema_version"] != "scogo.taskgen.review-record.v3"
        || review["final_disposition"] != "accepted"
        || review.get("candidate_id").and_then(Value::as_str).is_none()
        || review.get("sequence").and_then(Value::as_u64).is_none()
        || expected_index.is_some_and(|index| {
            review.get("sequence").and_then(Value::as_u64) != Some((index + 1) as u64)
        })
    {
        bail!("prior Taskgen accepted review is malformed");
    }
    let decision = crate::review::ReviewDecision::parse_and_validate(&serde_json::to_string(
        &review["decision"],
    )?)?;
    match decision.outcome {
        crate::review::ReviewOutcome::Accept => {
            if review
                .get("adjudication")
                .is_some_and(|value| !value.is_null())
            {
                bail!("prior accepted review unexpectedly contains adjudication");
            }
        }
        crate::review::ReviewOutcome::NeedsVerification => {
            let adjudication = review
                .get("adjudication")
                .and_then(|value| value.get("decision"))
                .context("prior verified review is missing adjudication")?;
            let decision = crate::review::AdjudicationDecision::parse_and_validate(
                &serde_json::to_string(adjudication)?,
            )?;
            if decision.outcome != crate::review::AdjudicationOutcome::Accept {
                bail!("prior accepted review has rejected adjudication");
            }
        }
        crate::review::ReviewOutcome::Revise | crate::review::ReviewOutcome::Reject => {
            bail!("prior accepted review has a non-accepted decision");
        }
    }
    Ok(())
}

fn validate_canonical_tasks(
    artifact: &EvidenceArtifact,
    source_by_id: &BTreeMap<String, Value>,
    task_review_by_id: Option<&BTreeMap<String, (Value, Value)>>,
) -> Result<(Vec<String>, String)> {
    let rows = jsonl_rows(&artifact.bytes, "prior canonical tasks")?;
    let mut ids = Vec::new();
    for row in rows {
        let canonical: CanonicalSourceTask = serde_json::from_value(row)?;
        let task_id = source_task_id(&canonical.source_task)?;
        if canonical.schema_version != "scogo.data-factory.source-task.v1"
            || canonical.source_task_id != task_id
            || canonical.split_group_id != task_id
            || !matches!(
                canonical.split.as_str(),
                "train" | "validation" | "evaluation"
            )
            || canonical.prompt != canonical.source_task["prompt"].as_str().unwrap_or_default()
            || canonical.domain.as_deref() != canonical.source_task["domain"].as_str()
            || canonical.subdomain.as_deref() != canonical.source_task["subdomain"].as_str()
            || canonical.difficulty.as_deref()
                != canonical.source_task["difficulty"]
                    .as_i64()
                    .map(|value| value.to_string())
                    .as_deref()
            || canonical.coordinates
                != serde_json::from_value(canonical.source_task["coordinates"].clone())?
            || canonical.source_schema_version != "scogo.taskgen.task.v2"
            || source_by_id.get(&task_id) != Some(&canonical.source_task)
        {
            bail!("prior canonical task {task_id} is not bound to the source population");
        }
        let review = canonical
            .source_review
            .as_ref()
            .context("prior canonical task is missing source review")?;
        if let Some(taskgen) = task_review_by_id {
            let (task, taskgen_review) = taskgen
                .get(&task_id)
                .context("prior canonical task is absent from Taskgen evidence")?;
            if task != &canonical.source_task || taskgen_review != review {
                bail!("prior canonical task/review differs from Taskgen evidence");
            }
        }
        validate_review(review, None)?;
        ids.push(task_id);
    }
    if ids.is_empty() || ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("prior canonical task IDs must be non-empty, unique, and sorted");
    }
    Ok((ids.clone(), selected_task_sha256(source_by_id, &ids)?))
}

fn release_descriptor<'a>(
    release: &'a ReleaseSetView,
    path: &str,
) -> Result<&'a ReleaseArtifactView> {
    let matches = release
        .artifacts
        .iter()
        .filter(|artifact| artifact.path == path)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!("prior release must declare {path} exactly once");
    }
    Ok(matches[0])
}

fn evidence_artifact<'a>(
    artifacts: &'a BTreeMap<String, EvidenceArtifact>,
    prefix: &str,
    run_id: &str,
) -> Result<&'a EvidenceArtifact> {
    artifacts
        .get(&format!("{prefix}.{run_id}"))
        .with_context(|| format!("missing {prefix}.{run_id}"))
}

fn evidence_parent(artifact: &EvidenceArtifact) -> &Path {
    artifact
        .held
        .as_ref()
        .and_then(|held| held.path.parent())
        .unwrap_or_else(|| Path::new("."))
}

fn load_prior_releases(
    args: &crate::ReviewArgs,
    source: &SourcePopulation,
) -> Result<Vec<PriorRelease>> {
    let pins = parse_mappings(&args.prior_release_pin, "--prior-release-pin")?;
    let paths = parse_mappings(&args.prior_evidence, "--prior-evidence")?;
    if pins.is_empty() {
        if !paths.is_empty() {
            bail!("prior evidence requires an owner release-set pin");
        }
        return Ok(Vec::new());
    }
    let source_by_id = source
        .rows
        .iter()
        .map(|row| (row.task_id.clone(), row.task.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut expected_names = HashSet::new();
    let mut releases = Vec::new();
    for (run_id, owner_pin) in pins {
        validate_component(&run_id, "prior release ID")?;
        if owner_pin.len() != 64
            || !owner_pin
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("prior release pin must be a lowercase SHA-256");
        }
        let current_name = format!("prior_source_receipt.{run_id}");
        let legacy_name = format!("prior_legacy_source_receipt.{run_id}");
        let mode = match (
            paths.contains_key(&current_name),
            paths.contains_key(&legacy_name),
        ) {
            (true, false) => PriorMode::Current,
            (false, true) => PriorMode::PinnedExternalLegacy,
            _ => bail!("prior release {run_id} must declare exactly one receipt mode"),
        };
        let prefixes: &[&str] = match mode {
            PriorMode::Current => &[
                "prior_release_set",
                "prior_canonical_tasks",
                "prior_source_receipt",
            ],
            PriorMode::PinnedExternalLegacy => &[
                "prior_release_set",
                "prior_canonical_tasks",
                "prior_legacy_source_receipt",
                "prior_taskgen_run",
                "prior_taskgen_tasks",
                "prior_taskgen_reviews",
            ],
        };
        let mut artifacts = BTreeMap::new();
        for prefix in prefixes {
            let name = format!("{prefix}.{run_id}");
            expected_names.insert(name.clone());
            let path = paths
                .get(&name)
                .with_context(|| format!("missing mapping {name}"))?;
            artifacts.insert(
                name.clone(),
                read_evidence(Path::new(path), &name, format!("prior_evidence/{name}"))?,
            );
        }
        let release_artifact = evidence_artifact(&artifacts, "prior_release_set", &run_id)?;
        if release_artifact.sha256 != owner_pin {
            bail!("prior release-set bytes do not match owner pin for {run_id}");
        }
        let release: ReleaseSetView = serde_json::from_slice(&release_artifact.bytes)?;
        if release.schema_version != "scogo.data-factory.release-set.v1"
            || release.release_id != run_id
        {
            bail!("prior release set identity/schema mismatch for {run_id}");
        }
        let canonical_artifact = evidence_artifact(&artifacts, "prior_canonical_tasks", &run_id)?;
        let descriptor = release_descriptor(&release, "canonical/tasks.jsonl")?;
        if descriptor.sha256 != canonical_artifact.sha256
            || descriptor.bytes != canonical_artifact.bytes.len()
        {
            bail!("prior canonical task descriptor mismatch for {run_id}");
        }
        let (selected_ids, selected_digest, authority_entry, origin_evidence) = match mode {
            PriorMode::PinnedExternalLegacy => {
                if release.scheduler_contract.is_some()
                    || release.source_receipt_sha256.is_some()
                    || release.source_manifest_sha256.is_some()
                    || release.source_manifest_bytes.is_some()
                    || release.source_artifacts.contains_key("source_receipt")
                {
                    bail!("legacy prior release unexpectedly declares current source binding");
                }
                let taskgen_run = evidence_artifact(&artifacts, "prior_taskgen_run", &run_id)?;
                let taskgen_tasks = evidence_artifact(&artifacts, "prior_taskgen_tasks", &run_id)?;
                let taskgen_reviews =
                    evidence_artifact(&artifacts, "prior_taskgen_reviews", &run_id)?;
                let manifest: TaskgenManifestView = serde_json::from_slice(&taskgen_run.bytes)?;
                if manifest.schema_version != "scogo.taskgen.run.v3"
                    || manifest.status != "success"
                    || release.source_run_id != manifest.run_id
                {
                    bail!("legacy Taskgen run does not match prior release");
                }
                for (name, artifact, file) in [
                    ("tasks", taskgen_tasks, "tasks.jsonl"),
                    ("reviews", taskgen_reviews, "reviews.jsonl"),
                ] {
                    let declared = manifest
                        .artifacts
                        .get(name)
                        .context("prior Taskgen manifest is incomplete")?;
                    if declared.file != file
                        || declared.bytes != Some(artifact.bytes.len())
                        || declared.sha256.as_deref() != Some(&artifact.sha256)
                        || release.source_artifacts.get(name) != Some(&artifact.sha256)
                    {
                        bail!("prior Taskgen {name} descriptor mismatch");
                    }
                }
                if manifest
                    .artifacts
                    .get("run")
                    .map(|artifact| artifact.file.as_str())
                    != Some("run.json")
                    || release.source_artifacts.get("run") != Some(&taskgen_run.sha256)
                {
                    bail!("prior Taskgen run descriptor mismatch");
                }
                let tasks = jsonl_rows(&taskgen_tasks.bytes, "prior Taskgen tasks")?;
                let reviews = jsonl_rows(&taskgen_reviews.bytes, "prior Taskgen reviews")?;
                let accepted = reviews
                    .into_iter()
                    .filter(|review| review["final_disposition"] == "accepted")
                    .collect::<Vec<_>>();
                if accepted.len() != tasks.len() {
                    bail!("prior Taskgen accepted review count does not match tasks");
                }
                let mut by_id = BTreeMap::new();
                for (index, (task, review)) in tasks.iter().zip(&accepted).enumerate() {
                    crate::schema::validate_instance(crate::schema::SchemaKind::Task, task)?;
                    validate_review(review, Some(index))?;
                    let task_id = source_task_id(task)?;
                    if source_by_id.get(&task_id) != Some(task)
                        || by_id
                            .insert(task_id, (task.clone(), review.clone()))
                            .is_some()
                    {
                        bail!("prior Taskgen tasks are duplicate or outside the current source");
                    }
                }
                let (ids, selected_digest) =
                    validate_canonical_tasks(canonical_artifact, &source_by_id, Some(&by_id))?;
                if descriptor.rows != Some(ids.len())
                    || ids.iter().cloned().collect::<HashSet<_>>()
                        != by_id.keys().cloned().collect()
                {
                    bail!("prior canonical/Taskgen task populations differ");
                }
                let receipt_artifact =
                    evidence_artifact(&artifacts, "prior_legacy_source_receipt", &run_id)?;
                let receipt: LegacyReceipt = serde_json::from_slice(&receipt_artifact.bytes)?;
                validate_receipt_source(&receipt, args, Some(&taskgen_tasks.bytes), tasks.len())?;
                let entry = json!({
                    "evidence_mode":"pinned_external_legacy","run_id":run_id,
                    "release_set_sha256":release_artifact.sha256,"canonical_tasks_sha256":canonical_artifact.sha256,
                    "legacy_source_receipt_sha256":receipt_artifact.sha256,"taskgen_run_sha256":taskgen_run.sha256,
                    "taskgen_tasks_sha256":taskgen_tasks.sha256,"taskgen_reviews_sha256":taskgen_reviews.sha256,
                    "selected_source_task_ids":ids,
                });
                let origin = json!({
                    "run_id":run_id,
                    "workspace_path":evidence_parent(taskgen_run),
                    "release_path":evidence_parent(release_artifact),
                    "release_set_sha256":release_artifact.sha256,"workspace_snapshot_sha256":taskgen_run.sha256,
                    "workspace_source_task_sha256":taskgen_tasks.sha256,"selected_source_task_ids":ids,
                    "selected_task_sha256":selected_digest,
                });
                (ids, selected_digest, entry, origin)
            }
            PriorMode::Current => {
                let receipt_artifact =
                    evidence_artifact(&artifacts, "prior_source_receipt", &run_id)?;
                let receipt: CurrentReceipt = serde_json::from_slice(&receipt_artifact.bytes)?;
                let legacy = LegacyReceipt {
                    schema_version: receipt.schema_version.clone(),
                    repo_id: receipt.repo_id.clone(),
                    repo_type: receipt.repo_type.clone(),
                    private: receipt.private,
                    revision: receipt.revision.clone(),
                    source_file: receipt.source_file.clone(),
                    selection: receipt.selection.clone(),
                    rows: receipt.rows,
                    subset_sha256: receipt.subset_sha256.clone(),
                };
                validate_receipt_source(&legacy, args, None, receipt.rows)?;
                let (ids, selected_digest) =
                    validate_canonical_tasks(canonical_artifact, &source_by_id, None)?;
                if ids.iter().cloned().collect::<HashSet<_>>()
                    != receipt.selected_source_task_ids.iter().cloned().collect()
                    || selected_task_sha256(&source_by_id, &receipt.selected_source_task_ids)?
                        != receipt.subset_sha256
                    || receipt.source_file_rows != source.tasks.len()
                    || receipt.source_file_sha256 != sha256_bytes(&source.canonical_jsonl)
                    || receipt.source_population_sha256 != source.population_sha256
                    || !receipt
                        .excluded_source_task_ids
                        .iter()
                        .all(|id| source_by_id.contains_key(id))
                    || receipt.exclusion_authority_sha256.len() != 64
                    || release.source_receipt_sha256.as_deref() != Some(&receipt_artifact.sha256)
                    || release.source_artifacts.get("source_receipt")
                        != Some(&receipt_artifact.sha256)
                    || release.source_artifacts.get("tasks") != Some(&receipt.subset_sha256)
                {
                    bail!("current prior receipt/release does not match source population");
                }
                let entry = json!({
                    "evidence_mode":"current","run_id":run_id,"release_set_sha256":release_artifact.sha256,
                    "source_receipt_sha256":receipt_artifact.sha256,"canonical_tasks_sha256":canonical_artifact.sha256,
                    "selected_source_task_ids":ids,
                });
                let origin = json!({
                    "run_id":run_id,"workspace_path":evidence_parent(release_artifact),
                    "release_path":evidence_parent(release_artifact),
                    "release_set_sha256":release_artifact.sha256,
                    "workspace_snapshot_sha256":release.work_run_sha256.clone().unwrap_or_else(|| release_artifact.sha256.clone()),
                    "workspace_source_task_sha256":selected_digest,"selected_source_task_ids":ids,
                    "selected_task_sha256":selected_digest,
                });
                (ids, selected_digest, entry, origin)
            }
        };
        if descriptor.rows != Some(selected_ids.len()) {
            bail!("prior canonical task row descriptor mismatch for {run_id}");
        }
        releases.push(PriorRelease {
            run_id,
            release,
            selected_ids,
            selected_task_sha256: selected_digest,
            artifacts,
            authority_entry,
            origin_evidence,
        });
    }
    if paths.keys().cloned().collect::<HashSet<_>>() != expected_names {
        bail!("prior evidence mappings contain missing or unpinned artifacts");
    }
    releases.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    Ok(releases)
}

fn derive_history(
    run_id: &str,
    source: &SourcePopulation,
    releases: &[PriorRelease],
) -> Result<(Vec<String>, EvidenceArtifact, Option<EvidenceArtifact>)> {
    let mut excluded_ids = releases
        .iter()
        .flat_map(|release| release.selected_ids.iter().cloned())
        .collect::<Vec<_>>();
    let occurrences = excluded_ids.len();
    excluded_ids.sort();
    excluded_ids.dedup();
    if occurrences != excluded_ids.len() {
        bail!("prior completed releases overlap each other");
    }
    let source_by_id = source
        .rows
        .iter()
        .map(|row| (row.task_id.clone(), row.task.clone()))
        .collect::<BTreeMap<_, _>>();
    let historical = if excluded_ids.is_empty() {
        None
    } else {
        let namespace = "taskgen-phase-b-exclusions";
        let selected_digest = selected_task_sha256(&source_by_id, &excluded_ids)?;
        let origin_run_ids = releases
            .iter()
            .map(|release| &release.run_id)
            .collect::<Vec<_>>();
        let reservation_run_id = format!(
            "historical-import-{}",
            sha256_bytes(&serde_json::to_vec(&json!({
                "source_run_id":run_id,"namespace":namespace,"origin_run_ids":origin_run_ids,
                "selected_source_task_ids":excluded_ids,"selected_task_sha256":selected_digest,
            }))?)
        );
        let source_artifacts = json!({"tasks":selected_digest});
        let identity = json!({
            "run_id":reservation_run_id,"campaign_id":"historical-import","source_run_id":run_id,
            "source_artifacts":source_artifacts,"source_receipt_sha256":null,
            "source_manifest_sha256":null,"source_manifest_bytes":null,
            "source_population_sha256":source.population_sha256,
            "source_population_rows":source.tasks.len(),"exclusion_evidence_sha256":null,
            "namespace":namespace,"strategy":"historical_import","seed":null,
            "origin_run_ids":origin_run_ids,"origin_evidence":releases.iter().map(|release| &release.origin_evidence).collect::<Vec<_>>(),
            "requested_count":excluded_ids.len(),"selected_source_task_ids":excluded_ids,
            "selected_task_sha256":selected_digest,
        });
        let reservation_id = format!(
            "reservation_{}",
            sha256_bytes(&serde_json::to_vec(&identity)?)
        );
        let now = chrono::Utc::now().to_rfc3339();
        let mut reservation = identity;
        let object = reservation
            .as_object_mut()
            .context("reservation identity is not an object")?;
        object.insert(
            "schema_version".into(),
            json!("scogo.data-factory.task-reservation.v1"),
        );
        object.insert("reservation_id".into(), json!(reservation_id));
        object.insert("status".into(), json!("completed"));
        object.insert("created_at".into(), json!(now));
        object.insert("updated_at".into(), json!(now));
        object.insert("work_started_at".into(), Value::Null);
        object.insert("completed_release_id".into(), Value::Null);
        object.insert("completed_release_path".into(), Value::Null);
        object.insert("completed_release_set_sha256".into(), Value::Null);
        object.insert("release_reason".into(), Value::Null);
        let bytes = serde_json::to_vec(&reservation)?;
        Some(EvidenceArtifact {
            logical_name: "historical_import_reservation".into(),
            relative_file: "historical_import_reservation.json".into(),
            held: None,
            sha256: sha256_bytes(&bytes),
            bytes,
        })
    };
    let authority = json!({
        "schema_version":"scogo.data-factory.source-exclusion-authority.v1",
        "excluded_source_task_ids":excluded_ids,
        "historical_import_reservation_sha256":historical.as_ref().map(|artifact| &artifact.sha256),
        "prior_completed_releases":releases.iter().map(|release| &release.authority_entry).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&authority)?;
    Ok((
        excluded_ids,
        EvidenceArtifact {
            logical_name: "exclusion_authority".into(),
            relative_file: "source_exclusion_authority.json".into(),
            held: None,
            sha256: sha256_bytes(&bytes),
            bytes,
        },
        historical,
    ))
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        bail!("{label} must match [a-z0-9][a-z0-9._-]*");
    }
    Ok(())
}

fn validate_source_metadata(
    run_id: &str,
    repo_id: &str,
    revision: &str,
    source_file: &str,
    selection: &str,
) -> Result<()> {
    validate_component(run_id, "Phase-B run ID")?;
    validate_component(selection, "Phase-B source selection")?;
    if selection.len() > 128 {
        bail!("Phase-B source selection exceeds 128 bytes");
    }
    if [repo_id, source_file, selection]
        .iter()
        .any(|value| contains_credential(value.as_bytes()))
        || ["authorization", "bearer", "token", "header"]
            .iter()
            .any(|marker| selection.to_ascii_lowercase().contains(marker))
    {
        bail!("Phase-B source metadata contains credential-like content");
    }
    let repo_parts = repo_id.split('/').collect::<Vec<_>>();
    if repo_parts.len() != 2 || repo_parts.iter().any(|part| part.trim().is_empty()) {
        bail!("Phase-B source repo ID must be owner/name");
    }
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("Phase-B source revision must be a 40-character lowercase commit SHA");
    }
    let path = Path::new(source_file);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::CurDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("Phase-B source file must be a safe relative path");
    }
    Ok(())
}

fn canonical_target(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("failed to canonicalize {}", path.display()));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("path has no existing ancestor")?
                .to_os_string(),
        );
        ancestor = ancestor.parent().context("path has no existing ancestor")?;
    }
    let mut canonical = std::fs::canonicalize(ancestor)?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn validate_path_isolation(
    work: &Path,
    final_run: &Path,
    inputs: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    if work == final_run || work.starts_with(final_run) || final_run.starts_with(work) {
        bail!("Phase-B work and final run paths must not alias or nest");
    }
    let mut seen = HashSet::new();
    for input in inputs {
        let input = canonical_target(&input)?;
        if !seen.insert(input.clone()) {
            bail!("Phase-B input/evidence paths must not alias");
        }
        if input.starts_with(work)
            || input.starts_with(final_run)
            || work.starts_with(&input)
            || final_run.starts_with(&input)
        {
            bail!("Phase-B input/evidence paths must not nest work or final paths");
        }
    }
    Ok(())
}

fn isolated_run_paths(args: &crate::ReviewArgs) -> Result<(PathBuf, PathBuf)> {
    let work = canonical_target(
        args.work_dir
            .as_deref()
            .context("Phase-B work dir is required")?,
    )?;
    let final_run = canonical_target(
        args.final_run_dir
            .as_deref()
            .context("Phase-B final run dir is required")?,
    )?;
    let evidence = parse_mappings(&args.prior_evidence, "--prior-evidence")?
        .into_values()
        .map(PathBuf::from);
    validate_path_isolation(
        &work,
        &final_run,
        [args.input.clone(), args.taxonomy.clone()]
            .into_iter()
            .chain(args.review_reference_dir.iter().cloned())
            .chain(evidence),
    )?;
    Ok((work, final_run))
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<(FileIdentity, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        },
        metadata.nlink(),
    ))
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> Result<File> {
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?;
    Ok(File::from(fd))
}

#[cfg(not(unix))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

#[cfg(not(unix))]
fn file_identity(file: &File) -> Result<(FileIdentity, u64)> {
    let metadata = file.metadata()?;
    Ok((
        FileIdentity {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        },
        1,
    ))
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path) -> Result<File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("held input path is a symlink");
    }
    Ok(File::open(path)?)
}

#[derive(Debug)]
struct HeldFile {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
}

impl HeldFile {
    fn capture(path: &Path, max_bytes: usize) -> Result<(Self, Vec<u8>)> {
        let file = open_nofollow(path)
            .with_context(|| format!("failed no-follow open: {}", path.display()))?;
        let (identity, links) = file_identity(&file)?;
        if links != 1 || identity_len(&identity) > max_bytes as u64 {
            bail!("held input must be a bounded single-link regular file");
        }
        let mut bytes = Vec::new();
        file.try_clone()?
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            bail!("held input exceeds byte limit");
        }
        let held = Self {
            path: path.to_path_buf(),
            file,
            identity,
        };
        held.assert_current()?;
        Ok((held, bytes))
    }

    fn assert_current(&self) -> Result<()> {
        let (held_identity, held_links) = file_identity(&self.file)?;
        let reopened = open_nofollow(&self.path)?;
        let (path_identity, path_links) = file_identity(&reopened)?;
        if held_links != 1
            || path_links != 1
            || held_identity != self.identity
            || path_identity != self.identity
        {
            bail!("held input changed or was replaced");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct HeldDirectory {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
}

impl HeldDirectory {
    #[cfg(unix)]
    fn capture(path: &Path) -> Result<Self> {
        let fd = rustix::fs::openat(
            rustix::fs::CWD,
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(fd);
        let (identity, _) = file_identity(&file)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            identity,
        })
    }

    #[cfg(not(unix))]
    fn capture(path: &Path) -> Result<Self> {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            bail!("held directory path is a symlink");
        }
        let file = File::open(path)?;
        let (identity, _) = file_identity(&file)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            identity,
        })
    }

    fn assert_current(&self) -> Result<()> {
        let (held, _) = file_identity(&self.file)?;
        let reopened = Self::capture(&self.path)?;
        if held != self.identity || reopened.identity != self.identity {
            bail!("held directory changed or was replaced");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn read_file(&self, relative: &Path, max_bytes: usize) -> Result<(HeldFile, Vec<u8>)> {
        use std::os::unix::ffi::OsStrExt;
        let mut directory = self.file.try_clone()?;
        let components = relative.components().collect::<Vec<_>>();
        if components.is_empty() {
            bail!("empty held relative path");
        }
        for component in &components[..components.len() - 1] {
            let std::path::Component::Normal(name) = component else {
                bail!("unsafe held relative path");
            };
            let fd = rustix::fs::openat(
                &directory,
                name.as_bytes(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )?;
            directory = File::from(fd);
        }
        let std::path::Component::Normal(name) = components[components.len() - 1] else {
            bail!("unsafe held relative path");
        };
        let fd = rustix::fs::openat(
            &directory,
            name.as_bytes(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(fd);
        let (identity, links) = file_identity(&file)?;
        if links != 1 || identity_len(&identity) > max_bytes as u64 {
            bail!("held artifact is not a bounded single-link file");
        }
        let mut bytes = Vec::new();
        file.try_clone()?
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            bail!("held artifact exceeds byte limit");
        }
        Ok((
            HeldFile {
                path: self.path.join(relative),
                file,
                identity,
            },
            bytes,
        ))
    }

    #[cfg(not(unix))]
    fn read_file(&self, relative: &Path, max_bytes: usize) -> Result<(HeldFile, Vec<u8>)> {
        HeldFile::capture(&self.path.join(relative), max_bytes)
    }
}

#[cfg(unix)]
fn identity_len(identity: &FileIdentity) -> u64 {
    identity.len
}

#[cfg(not(unix))]
fn identity_len(identity: &FileIdentity) -> u64 {
    identity.len
}

#[derive(Debug)]
struct WorkLock {
    _file: File,
}

impl WorkLock {
    fn acquire(work_dir: &Path) -> Result<Self> {
        let work_dir = canonical_target(work_dir)?;
        let parent = work_dir
            .parent()
            .context("Phase-B work path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let name = work_dir
            .file_name()
            .and_then(|value| value.to_str())
            .context("Phase-B work path needs a UTF-8 name")?;
        let path = parent.join(format!(".{name}.phase-b.lock"));
        #[cfg(unix)]
        let file = File::from(rustix::fs::openat(
            rustix::fs::CWD,
            &path,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_bits_truncate(0o600),
        )?);
        #[cfg(not(unix))]
        let file = {
            if std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                bail!("Phase-B work lock path is a symlink");
            }
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)?
        };
        if file_identity(&file)?.1 != 1 {
            bail!("Phase-B work lock must be a single-link file");
        }
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| anyhow::anyhow!("Phase-B work is already active"))?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
struct ReferenceSnapshot {
    held: Vec<HeldFile>,
    digest: String,
    store: Arc<crate::references::ReferenceStore>,
}

impl ReferenceSnapshot {
    fn capture(root: Option<&Path>) -> Result<Self> {
        fn visit(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
            let mut entries = std::fs::read_dir(directory)
                .with_context(|| {
                    format!(
                        "failed to read reference directory: {}",
                        directory.display()
                    )
                })?
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() {
                    bail!(
                        "Phase-B reference corpus forbids symlinks: {}",
                        path.display()
                    );
                }
                if metadata.is_dir() {
                    visit(&path, paths)?;
                } else if metadata.is_file()
                    && path
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|extension| {
                            matches!(
                                extension.to_ascii_lowercase().as_str(),
                                "md" | "txt" | "json" | "yaml" | "yml"
                            )
                        })
                {
                    paths.push(path);
                }
            }
            Ok(())
        }
        let mut paths = Vec::new();
        if let Some(root) = root {
            if !root.is_dir() || std::fs::symlink_metadata(root)?.file_type().is_symlink() {
                bail!(
                    "Phase-B review reference path is not a safe directory: {}",
                    root.display()
                );
            }
            visit(root, &mut paths)?;
            paths.sort();
        }
        let mut held = Vec::new();
        let mut documents = Vec::new();
        let mut digest_rows = Vec::new();
        for path in paths {
            let (file, bytes) = HeldFile::capture(&path, 64 * 1024 * 1024)?;
            let relative = path
                .strip_prefix(root.context("reference root disappeared")?)?
                .to_string_lossy()
                .replace('\\', "/");
            let text = String::from_utf8(bytes.clone())
                .with_context(|| format!("reference is not UTF-8: {}", path.display()))?;
            digest_rows.push(json!({"file":relative,"sha256":sha256_bytes(&bytes)}));
            documents.push((relative, text));
            held.push(file);
        }
        Ok(Self {
            digest: sha256_bytes(&serde_json::to_vec(&digest_rows)?),
            store: Arc::new(crate::references::ReferenceStore::from_documents(documents)),
            held,
        })
    }

    fn assert_current(&self) -> Result<()> {
        for file in &self.held {
            file.assert_current()?;
        }
        Ok(())
    }
}

fn prepare_run(
    args: &crate::ReviewArgs,
    taxonomy: &crate::taxonomy::TaxonomyCatalog,
    review_prompt: &str,
    taxonomy_snapshot: Option<(HeldFile, Vec<u8>)>,
) -> Result<PreparedRun> {
    let target = args
        .accepted_target
        .context("Phase-B accepted target is required")?;
    let run_id = args.run_id.clone().context("Phase-B run ID is required")?;
    let (work_dir, final_run_dir) = isolated_run_paths(args)?;
    let source_repo_id = args
        .source_repo_id
        .clone()
        .context("Phase-B source repo ID is required")?;
    let source_revision = args
        .source_revision
        .clone()
        .context("Phase-B source revision is required")?;
    let source_file = args
        .source_file
        .clone()
        .context("Phase-B source file is required")?;
    let source_selection = args
        .source_selection
        .clone()
        .context("Phase-B source selection is required")?;
    validate_source_metadata(
        &run_id,
        &source_repo_id,
        &source_revision,
        &source_file,
        &source_selection,
    )?;

    let source = load_source_population(&args.input, taxonomy)?;
    let prior_releases = load_prior_releases(args, &source)?;
    let (excluded_ids, exclusion_authority, historical_reservation) =
        derive_history(&run_id, &source, &prior_releases)?;
    let excluded = excluded_ids.iter().collect::<HashSet<_>>();
    let eligible_rows = source
        .rows
        .iter()
        .filter(|row| !excluded.contains(&row.task_id))
        .cloned()
        .collect::<Vec<_>>();
    if target > eligible_rows.len() {
        bail!(
            "Phase-B accepted target {target} exceeds {} eligible bounded source rows",
            eligible_rows.len()
        );
    }

    let (taxonomy_held, taxonomy_bytes) = match taxonomy_snapshot {
        Some(snapshot) => snapshot,
        None => HeldFile::capture(&args.taxonomy, 16 * 1024 * 1024)?,
    };
    let reference_snapshot = ReferenceSnapshot::capture(args.review_reference_dir.as_deref())?;
    let prior_digests = prior_releases
        .iter()
        .flat_map(|release| release.artifacts.values())
        .map(|artifact| (artifact.logical_name.clone(), json!(artifact.sha256)))
        .collect::<serde_json::Map<_, _>>();
    let config = json!({
        "schema_version":"scogo.taskgen.phase-b-config.v1",
        "run_id":run_id,
        "accepted_target":target,
        "paths":{"work_dir":work_dir,"final_run_dir":final_run_dir},
        "source":{
            "repo_id":source_repo_id,
            "repo_type":"dataset",
            "private":true,
            "revision":source_revision,
            "source_file":source_file,
            "selection":source_selection,
            "rows":source.tasks.len(),
            "source_file_sha256":sha256_bytes(&source.canonical_jsonl),
            "source_population_sha256":source.population_sha256,
        },
        "evidence":{
            "exclusion_authority_sha256":exclusion_authority.sha256,
            "historical_import_reservation_sha256":historical_reservation.as_ref().map(|artifact| &artifact.sha256),
            "prior":prior_digests,
        },
        "taxonomy":{
            "id":taxonomy.id(),
            "sha256":sha256_bytes(&taxonomy_bytes),
        },
        "review":{
            "model":args.model,
            "endpoint":crate::safe_requested_api_base(&args.api_base),
            "prompt_sha256":sha256_bytes(review_prompt.as_bytes()),
            "max_output_tokens":args.max_output_tokens,
            "workers":args.review_workers,
            "requests_per_minute":args.review_requests_per_minute,
            "reference_sha256":reference_snapshot.digest,
        },
        "adjudication":{
            "model":args.adjudication_model.as_deref().unwrap_or(&args.model),
            "endpoint":crate::safe_requested_api_base(args.adjudication_api_base.as_deref().unwrap_or(&args.api_base)),
        }
    });
    let config_sha256 = sha256_bytes(&serde_json::to_vec(&config)?);
    Ok(PreparedRun {
        run_id,
        target,
        work_dir,
        final_run_dir,
        source_repo_id,
        source_revision,
        source_file,
        source_selection,
        source,
        eligible_rows,
        excluded_ids,
        exclusion_authority,
        historical_reservation,
        prior_releases,
        taxonomy_held: Some(taxonomy_held),
        reference_snapshot,
        config,
        config_sha256,
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn initialize_work_files(prepared: &PreparedRun) -> Result<StageJournal> {
    let config_path = prepared.work_dir.join("config.json");
    let config_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&config_path)?;
    let mut writer = std::io::BufWriter::new(config_file);
    serde_json::to_writer_pretty(
        &mut writer,
        &json!({
            "schema_version":"scogo.taskgen.phase-b-work.v1",
            "config_sha256":prepared.config_sha256,
            "config":prepared.config,
        }),
    )?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let journal = StageJournal::create(
        &prepared.work_dir.join("stage.journal.jsonl"),
        &prepared.config_sha256,
    )?;
    sync_directory(&prepared.work_dir)?;
    Ok(journal)
}

fn open_work(prepared: &PreparedRun, resume: bool) -> Result<StageJournal> {
    if prepared.config["paths"]["work_dir"] != json!(canonical_target(&prepared.work_dir)?)
        || prepared.config["paths"]["final_run_dir"]
            != json!(canonical_target(&prepared.final_run_dir)?)
    {
        bail!("Phase-B immutable config changed; resume refused before provider setup");
    }
    let config_path = prepared.work_dir.join("config.json");
    let journal_path = prepared.work_dir.join("stage.journal.jsonl");
    if resume {
        if !prepared.work_dir.is_dir() {
            bail!("Phase-B resume work directory does not exist");
        }
        if !config_path.exists() {
            if std::fs::read_dir(&prepared.work_dir)?.next().is_none() {
                return initialize_work_files(prepared);
            }
            bail!("Phase-B initialization is incomplete and work directory is not empty");
        }
        let stored: Value = serde_json::from_slice(&std::fs::read(&config_path)?)
            .context("Phase-B work config is invalid JSON")?;
        if stored.get("schema_version").and_then(Value::as_str)
            != Some("scogo.taskgen.phase-b-work.v1")
            || stored.get("config_sha256").and_then(Value::as_str)
                != Some(prepared.config_sha256.as_str())
            || stored.get("config") != Some(&prepared.config)
        {
            bail!("Phase-B immutable config changed; resume refused before provider setup");
        }
        if !journal_path.exists() {
            let names = std::fs::read_dir(&prepared.work_dir)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<Vec<_>>>()?;
            if names == [std::ffi::OsString::from("config.json")] {
                return StageJournal::create(&journal_path, &prepared.config_sha256);
            }
            bail!("Phase-B journal is missing from a non-initial work directory");
        }
        return StageJournal::resume(&journal_path, &prepared.config_sha256)
            .map(|(journal, _)| journal);
    }
    if prepared.work_dir.exists() || prepared.final_run_dir.exists() {
        bail!("Phase-B fresh run requires absent work and final directories");
    }
    let parent = prepared.work_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    std::fs::create_dir(&prepared.work_dir)?;
    initialize_work_files(prepared)
}

fn load_source_population(
    path: &Path,
    taxonomy: &crate::taxonomy::TaxonomyCatalog,
) -> Result<SourcePopulation> {
    let (held, source_bytes) = HeldFile::capture(path, 1024 * 1024 * 1024)
        .with_context(|| format!("failed to open Phase-B source: {}", path.display()))?;
    let mut reader = BufReader::new(source_bytes.as_slice());
    let mut line = Vec::new();
    let mut rows = Vec::new();
    let mut tasks = Vec::new();
    let mut canonical_jsonl = Vec::new();
    let mut task_ids = HashSet::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        if line.len() > MAX_SOURCE_LINE_BYTES {
            bail!("Phase-B source line {line_number} exceeds the bounded line limit");
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if tasks.len() >= MAX_SOURCE_ROWS {
            bail!("Phase-B source exceeds the bounded row limit of {MAX_SOURCE_ROWS}");
        }
        let value: Value = serde_json::from_slice(&line).with_context(|| {
            format!(
                "invalid Phase-B source JSON at {}:{line_number}",
                path.display()
            )
        })?;
        if contains_credential(&line) {
            bail!("Phase-B source row {line_number} contains credential-like content");
        }
        crate::schema::validate_instance(crate::schema::SchemaKind::Task, &value)
            .with_context(|| format!("schema-invalid Phase-B source row {line_number}"))?;
        let entry: crate::TaskEntry = serde_json::from_value(value)?;
        taxonomy.validate_task_coordinates(
            &entry.category,
            &entry.domain,
            &entry.subdomain,
            entry
                .coordinates
                .as_ref()
                .context("Phase-B source row is missing coordinates")?,
        )?;
        let task = serde_json::to_value(&entry)?;
        let task_id = source_task_id(&task)?;
        if !task_ids.insert(task_id.clone()) {
            bail!("Phase-B source contains duplicate task ID {task_id}");
        }
        serde_json::to_writer(&mut canonical_jsonl, &task)?;
        canonical_jsonl.push(b'\n');
        let deterministic = crate::deterministic_candidate_checks(&entry);
        rows.push(SourceRow {
            source_index: tasks.len(),
            task_id,
            task: task.clone(),
            deterministic_hard_failures: deterministic.hard_failures,
        });
        tasks.push(task);
    }
    if tasks.is_empty() {
        bail!("Phase-B source population is empty");
    }
    let population_sha256 = source_population_sha256(&tasks)?;
    Ok(SourcePopulation {
        rows,
        tasks,
        canonical_jsonl,
        population_sha256,
        held: Some(held),
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JournalStage {
    Admitted,
    ReviewCompleted,
    Accepted,
    Rejected,
    ProviderPaused,
    SealPrepared,
    Sealed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntryBody {
    schema_version: String,
    sequence: u64,
    previous_sha256: Option<String>,
    config_sha256: String,
    task_id: String,
    source_index: usize,
    stage: JournalStage,
    payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    #[serde(flatten)]
    body: JournalEntryBody,
    event_sha256: String,
}

#[derive(Debug, Clone, Default)]
struct JournalRowState {
    source_index: usize,
    task: Option<Value>,
    review: Option<ReviewStageResult>,
    terminal: Option<JournalStage>,
    terminal_payload: Option<TerminalPayload>,
}

#[derive(Debug, Clone, Default)]
struct JournalSnapshot {
    rows: BTreeMap<String, JournalRowState>,
    accepted: usize,
    rejected: usize,
    seal_prepared_manifest_sha256: Option<String>,
    seal_temporary_name: Option<String>,
    sealed_manifest_sha256: Option<String>,
}

impl JournalSnapshot {
    fn apply(&mut self, body: &JournalEntryBody) -> Result<()> {
        if body.stage == JournalStage::SealPrepared {
            let payload: SealPreparedPayload = serde_json::from_value(body.payload.clone())?;
            if body.task_id != "__run__" || self.seal_prepared_manifest_sha256.is_some() {
                bail!("journal seal-prepared conflict");
            }
            self.seal_prepared_manifest_sha256 = Some(payload.manifest_sha256);
            self.seal_temporary_name = Some(payload.temporary_name);
            return Ok(());
        }
        if body.stage == JournalStage::Sealed {
            let payload: SealedPayload = serde_json::from_value(body.payload.clone())?;
            if body.task_id != "__run__"
                || self.sealed_manifest_sha256.is_some()
                || Some(payload.manifest_sha256.as_str())
                    != self.seal_prepared_manifest_sha256.as_deref()
            {
                bail!("terminal journal conflict for sealed run");
            }
            self.sealed_manifest_sha256 = Some(payload.manifest_sha256);
            return Ok(());
        }

        match body.stage {
            JournalStage::Admitted => {
                let payload: AdmissionPayload = serde_json::from_value(body.payload.clone())?;
                if self.rows.contains_key(&body.task_id) {
                    bail!("duplicate journal admission for {}", body.task_id);
                }
                if source_task_id(&payload.task)? != body.task_id {
                    bail!("journal admission task does not match its task ID");
                }
                self.rows.insert(
                    body.task_id.clone(),
                    JournalRowState {
                        source_index: body.source_index,
                        task: Some(payload.task),
                        ..JournalRowState::default()
                    },
                );
            }
            JournalStage::ReviewCompleted => {
                let review: ReviewStageResult = serde_json::from_value(body.payload.clone())?;
                let row = self
                    .rows
                    .get_mut(&body.task_id)
                    .context("review event precedes admission")?;
                if row.source_index != body.source_index
                    || row.review.is_some()
                    || row.terminal.is_some()
                {
                    bail!("journal review conflict for {}", body.task_id);
                }
                review.validate_completed(&body.task_id, body.source_index)?;
                row.review = Some(review);
            }
            JournalStage::Accepted | JournalStage::Rejected => {
                let payload: TerminalPayload = serde_json::from_value(body.payload.clone())?;
                let row = self
                    .rows
                    .get_mut(&body.task_id)
                    .context("terminal event precedes admission")?;
                if row.source_index != body.source_index || row.terminal.is_some() {
                    bail!("terminal journal conflict for {}", body.task_id);
                }
                let accepted = body.stage == JournalStage::Accepted;
                match (&row.review, &payload.review) {
                    (Some(completed), Some(terminal)) => terminal.validate_terminal(
                        completed,
                        &body.task_id,
                        body.source_index,
                        accepted,
                    )?,
                    (None, None) if !accepted && payload.rejection.is_some() => {}
                    _ => bail!("terminal journal payload does not match completed review"),
                }
                row.terminal = Some(body.stage);
                row.terminal_payload = Some(payload);
                if accepted {
                    self.accepted += 1;
                } else {
                    self.rejected += 1;
                }
            }
            JournalStage::ProviderPaused => {
                let payload: PausePayload = serde_json::from_value(body.payload.clone())?;
                if payload.reason.trim().is_empty() {
                    bail!("provider pause reason is empty");
                }
                let row = self
                    .rows
                    .get(&body.task_id)
                    .context("provider pause precedes admission")?;
                if row.source_index != body.source_index || row.terminal.is_some() {
                    bail!(
                        "provider pause conflicts with terminal row {}",
                        body.task_id
                    );
                }
            }
            JournalStage::SealPrepared | JournalStage::Sealed => unreachable!(),
        }
        Ok(())
    }

    fn pending(&self) -> usize {
        self.rows
            .values()
            .filter(|row| row.terminal.is_none())
            .count()
    }
}

#[derive(Debug, Clone)]
struct SourceRow {
    source_index: usize,
    task_id: String,
    task: Value,
    deterministic_hard_failures: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReviewStageOutcome {
    Accept,
    Reject,
    NeedsVerification,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ReviewStageResult {
    outcome: ReviewStageOutcome,
    record: Value,
}

impl ReviewStageResult {
    fn validate_completed(&self, task_id: &str, source_index: usize) -> Result<()> {
        if self.record["schema_version"] != "scogo.taskgen.review-record.v3"
            || self.record["candidate_id"] != task_id
            || self.record["sequence"] != source_index + 1
            || self.record["final_disposition"] != "pending"
            || self
                .record
                .get("review_model")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || self
                .record
                .get("review_input_tokens")
                .and_then(Value::as_u64)
                .is_none()
            || self
                .record
                .get("review_output_tokens")
                .and_then(Value::as_u64)
                .is_none()
            || self
                .record
                .get("adjudication")
                .is_some_and(|value| !value.is_null())
        {
            bail!("durable completed review does not match candidate/source index");
        }
        serde_json::from_value::<crate::review::ReviewNormalization>(
            self.record["decision_normalization"].clone(),
        )?;
        serde_json::from_value::<Vec<crate::references::ReferenceExcerpt>>(
            self.record["references"].clone(),
        )?;
        let decision = crate::review::ReviewDecision::parse_and_validate(&serde_json::to_string(
            &self.record["decision"],
        )?)?;
        let expected = match decision.outcome {
            crate::review::ReviewOutcome::Accept => ReviewStageOutcome::Accept,
            crate::review::ReviewOutcome::NeedsVerification => {
                ReviewStageOutcome::NeedsVerification
            }
            crate::review::ReviewOutcome::Revise | crate::review::ReviewOutcome::Reject => {
                ReviewStageOutcome::Reject
            }
        };
        if self.outcome != expected {
            bail!("durable review outcome disagrees with validated decision");
        }
        Ok(())
    }

    fn validate_terminal(
        &self,
        completed: &Self,
        task_id: &str,
        source_index: usize,
        accepted: bool,
    ) -> Result<()> {
        completed.validate_completed(task_id, source_index)?;
        let mut normalized = self.clone();
        normalized.record["final_disposition"] = json!("pending");
        normalized.record["adjudication"] = Value::Null;
        if &normalized != completed
            || self.record["final_disposition"]
                != json!(if accepted { "accepted" } else { "rejected" })
        {
            bail!("terminal review differs from its durable completed review");
        }
        match self.outcome {
            ReviewStageOutcome::Accept if accepted && self.record["adjudication"].is_null() => {}
            ReviewStageOutcome::Reject if !accepted && self.record["adjudication"].is_null() => {}
            ReviewStageOutcome::NeedsVerification => {
                let result = &self.record["adjudication"];
                if result
                    .get("model")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                    || result.get("input_tokens").and_then(Value::as_u64).is_none()
                    || result
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .is_none()
                {
                    bail!("terminal adjudication result is incomplete");
                }
                let adjudication = result
                    .get("decision")
                    .context("terminal verified review is missing adjudication decision")?;
                let decision = crate::review::AdjudicationDecision::parse_and_validate(
                    &serde_json::to_string(adjudication)?,
                )?;
                if accepted != (decision.outcome == crate::review::AdjudicationOutcome::Accept) {
                    bail!("terminal disposition disagrees with adjudication");
                }
                let review_decision: crate::review::ReviewDecision =
                    serde_json::from_value(completed.record["decision"].clone())?;
                let requested = review_decision
                    .claims_requiring_verification
                    .iter()
                    .map(|claim| claim.claim_id.as_str())
                    .collect::<HashSet<_>>();
                let returned = decision
                    .claims
                    .iter()
                    .map(|claim| claim.claim_id.as_str())
                    .collect::<HashSet<_>>();
                if requested != returned {
                    bail!("terminal adjudication claim IDs differ from completed review");
                }
            }
            _ => bail!("terminal disposition disagrees with review decision"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionPayload {
    task: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalPayload {
    review: Option<ReviewStageResult>,
    rejection: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PausePayload {
    reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealPreparedPayload {
    manifest_sha256: String,
    temporary_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedPayload {
    manifest_sha256: String,
}

#[derive(Debug, Clone)]
struct AdjudicationStageResult {
    accepted: bool,
    adjudication: Value,
}

#[async_trait::async_trait]
trait PhaseBReviewer: Send + Sync {
    async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult>;
    async fn adjudicate(
        &self,
        row: &SourceRow,
        review: &ReviewStageResult,
    ) -> Result<AdjudicationStageResult>;
}

#[derive(Clone)]
struct NetworkReviewer {
    taxonomy_id: String,
    taxonomy_kind: String,
    review_provider: crate::provider::ProviderConfig,
    adjudication_provider: crate::provider::ProviderConfig,
    client: reqwest::Client,
    system_prompt: String,
    max_output_tokens: u64,
    review_telemetry: Arc<crate::telemetry::RequestTelemetry>,
    adjudication_telemetry: Arc<crate::telemetry::RequestTelemetry>,
    references: Arc<crate::references::ReferenceStore>,
    rate_limiter: Option<Arc<crate::review::ReviewRateLimiter>>,
}

#[async_trait::async_trait]
impl PhaseBReviewer for NetworkReviewer {
    async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
        use crate::review::CandidateReviewer;

        let entry: crate::TaskEntry = serde_json::from_value(row.task.clone())?;
        let checks = crate::deterministic_candidate_checks(&entry);
        let client = crate::review::ReviewClient::new(
            self.review_provider.clone(),
            self.client.clone(),
            self.max_output_tokens,
            self.review_telemetry.clone(),
            self.rate_limiter.clone(),
        )?;
        let result = client
            .review(crate::review::ReviewRequest {
                candidate: row.task.clone(),
                taxonomy_id: self.taxonomy_id.clone(),
                taxonomy_kind: self.taxonomy_kind.clone(),
                system_prompt: self.system_prompt.clone(),
                deterministic_checks: Some(serde_json::to_value(checks)?),
            })
            .await?;
        let mut references = BTreeMap::new();
        if result.decision.outcome == crate::review::ReviewOutcome::NeedsVerification {
            for claim in &result.decision.claims_requiring_verification {
                for excerpt in self.references.retrieve(&claim.reference_query, 3, 1200) {
                    references
                        .entry(excerpt.reference_id.clone())
                        .or_insert(excerpt);
                }
            }
        }
        let outcome = match result.decision.outcome {
            crate::review::ReviewOutcome::Accept => ReviewStageOutcome::Accept,
            crate::review::ReviewOutcome::NeedsVerification => {
                ReviewStageOutcome::NeedsVerification
            }
            crate::review::ReviewOutcome::Revise | crate::review::ReviewOutcome::Reject => {
                ReviewStageOutcome::Reject
            }
        };
        Ok(ReviewStageResult {
            outcome,
            record: json!({
                "schema_version":"scogo.taskgen.review-record.v3",
                "candidate_id":row.task_id,
                "sequence":row.source_index + 1,
                "review_model":result.model,
                "review_input_tokens":result.input_tokens,
                "review_output_tokens":result.output_tokens,
                "decision_normalization":result.normalization,
                "decision":result.decision,
                "references":references.into_values().collect::<Vec<_>>(),
                "adjudication":null,
                "final_disposition":"pending",
            }),
        })
    }

    async fn adjudicate(
        &self,
        row: &SourceRow,
        review: &ReviewStageResult,
    ) -> Result<AdjudicationStageResult> {
        use crate::review::CandidateAdjudicator;

        let decision = serde_json::from_value(review.record["decision"].clone())?;
        let references = serde_json::from_value(review.record["references"].clone())?;
        let client = crate::review::AdjudicationClient::new(
            self.adjudication_provider.clone(),
            self.client.clone(),
            1024,
            self.adjudication_telemetry.clone(),
            self.rate_limiter.clone(),
        )?;
        let result = client
            .adjudicate(crate::review::AdjudicationRequest {
                candidate: row.task.clone(),
                review: decision,
                references,
                system_prompt: include_str!("../prompts/prompt-adjudication-system-v1.txt")
                    .to_string(),
            })
            .await?;
        Ok(AdjudicationStageResult {
            accepted: result.decision.outcome == crate::review::AdjudicationOutcome::Accept,
            adjudication: serde_json::to_value(result)?,
        })
    }
}

#[derive(Debug)]
struct AdmissionResult {
    snapshot: JournalSnapshot,
    admitted: usize,
    max_accepted_plus_in_flight: usize,
}

#[derive(Debug)]
enum PendingJob {
    Review(SourceRow),
    Adjudicate(SourceRow, ReviewStageResult),
}

#[derive(Debug)]
enum JobOutput {
    Reviewed(ReviewStageResult),
    Adjudicated(AdjudicationStageResult),
}

#[derive(Debug)]
struct CompletedJob {
    row: SourceRow,
    saved_review: Option<ReviewStageResult>,
    result: Result<JobOutput>,
}

type ActiveJob = Pin<Box<dyn Future<Output = CompletedJob> + Send>>;

fn start_job(
    active: &mut FuturesUnordered<ActiveJob>,
    reviewer: Arc<dyn PhaseBReviewer>,
    job: PendingJob,
) {
    active.push(Box::pin(async move {
        match job {
            PendingJob::Review(row) => CompletedJob {
                result: reviewer.review(&row).await.map(JobOutput::Reviewed),
                row,
                saved_review: None,
            },
            PendingJob::Adjudicate(row, review) => CompletedJob {
                result: reviewer
                    .adjudicate(&row, &review)
                    .await
                    .map(JobOutput::Adjudicated),
                row,
                saved_review: Some(review),
            },
        }
    }));
}

async fn run_admission(
    rows: Vec<SourceRow>,
    target: usize,
    workers: usize,
    journal: StageJournal,
    reviewer: Arc<dyn PhaseBReviewer>,
) -> Result<AdmissionResult> {
    if target == 0 || workers == 0 {
        bail!("Phase-B target and review workers must be positive");
    }
    let rows_by_id = rows
        .iter()
        .map(|row| (row.task_id.clone(), row.clone()))
        .collect::<BTreeMap<_, _>>();
    if rows_by_id.len() != rows.len() {
        bail!("Phase-B source rows contain duplicate task IDs");
    }
    for (task_id, state) in &journal.snapshot.rows {
        let source = rows_by_id
            .get(task_id)
            .with_context(|| format!("journal task {task_id} is absent from source population"))?;
        if source.source_index != state.source_index || state.task.as_ref() != Some(&source.task) {
            bail!("journal source index conflicts for {task_id}");
        }
    }
    if journal.snapshot.accepted > target
        || journal.snapshot.accepted + journal.snapshot.pending() > target
    {
        bail!("journal violates accepted + pending <= target");
    }

    let mut journal = journal;
    let mut pending = journal
        .snapshot
        .rows
        .iter()
        .filter(|(_, state)| state.terminal.is_none())
        .map(|(task_id, state)| {
            let row = rows_by_id[task_id].clone();
            Ok((row, state.review.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    pending.sort_by_key(|(row, _)| row.source_index);
    let mut ready = VecDeque::new();
    for (row, review) in pending {
        match review {
            Some(review) if review.outcome == ReviewStageOutcome::Accept => {
                finalize_row(&mut journal, &row, review, true)?;
            }
            Some(review) if review.outcome == ReviewStageOutcome::Reject => {
                finalize_row(&mut journal, &row, review, false)?;
            }
            Some(review) => ready.push_back(PendingJob::Adjudicate(row, review)),
            None if !row.deterministic_hard_failures.is_empty() => {
                journal.append(&row.task_id,row.source_index,JournalStage::Rejected,json!({
                    "review":null,
                    "rejection":rejection_record(&row,"deterministic_validation",Some(&row.deterministic_hard_failures))
                }))?;
            }
            None => ready.push_back(PendingJob::Review(row)),
        }
    }
    let mut unused = rows
        .into_iter()
        .filter(|row| !journal.snapshot.rows.contains_key(&row.task_id))
        .collect::<VecDeque<_>>();
    let mut active = FuturesUnordered::<ActiveJob>::new();
    let mut stop_reason = None;
    let mut max_occupancy = journal.snapshot.accepted;
    loop {
        while stop_reason.is_none() && active.len() < workers {
            let job = if let Some(job) = ready.pop_front() {
                Some(job)
            } else if journal.snapshot.accepted + journal.snapshot.pending() < target {
                let mut admitted = None;
                while let Some(row) = unused.pop_front() {
                    journal.append(
                        &row.task_id,
                        row.source_index,
                        JournalStage::Admitted,
                        json!({"task":row.task}),
                    )?;
                    if row.deterministic_hard_failures.is_empty() {
                        admitted = Some(PendingJob::Review(row));
                        break;
                    }
                    journal.append(&row.task_id,row.source_index,JournalStage::Rejected,json!({
                        "rejection":rejection_record(&row,"deterministic_validation",Some(&row.deterministic_hard_failures))
                    }))?;
                }
                admitted
            } else {
                None
            };
            let Some(job) = job else { break };
            start_job(&mut active, reviewer.clone(), job);
            max_occupancy = max_occupancy.max(journal.snapshot.accepted + active.len());
            if journal.snapshot.accepted + active.len() > target {
                bail!("Phase-B admission invariant violated");
            }
        }
        if active.is_empty() {
            if let Some(reason) = stop_reason {
                bail!("Phase-B provider/transport exhaustion paused the run: {reason}");
            }
            if journal.snapshot.accepted == target && journal.snapshot.pending() == 0 {
                break;
            }
            bail!("Phase-B source population exhausted before exact acceptance");
        }
        let completed = active
            .next()
            .await
            .context("active Phase-B job set ended")?;
        match completed.result {
            Err(error) => {
                let reason = error.to_string();
                journal.append(
                    &completed.row.task_id,
                    completed.row.source_index,
                    JournalStage::ProviderPaused,
                    json!({"reason":reason}),
                )?;
                stop_reason.get_or_insert(reason);
            }
            Ok(JobOutput::Reviewed(review)) => {
                journal.append(
                    &completed.row.task_id,
                    completed.row.source_index,
                    JournalStage::ReviewCompleted,
                    serde_json::to_value(&review)?,
                )?;
                match review.outcome {
                    ReviewStageOutcome::Accept => {
                        finalize_row(&mut journal, &completed.row, review, true)?;
                    }
                    ReviewStageOutcome::Reject => {
                        finalize_row(&mut journal, &completed.row, review, false)?;
                    }
                    ReviewStageOutcome::NeedsVerification if stop_reason.is_none() => {
                        ready.push_back(PendingJob::Adjudicate(completed.row, review));
                    }
                    ReviewStageOutcome::NeedsVerification => {}
                }
            }
            Ok(JobOutput::Adjudicated(adjudication)) => {
                let mut review = completed
                    .saved_review
                    .context("adjudication completed without its durable review")?;
                review.record["adjudication"] = adjudication.adjudication;
                finalize_row(&mut journal, &completed.row, review, adjudication.accepted)?;
            }
        }
    }
    Ok(AdmissionResult {
        admitted: journal.snapshot.rows.len(),
        snapshot: journal.snapshot,
        max_accepted_plus_in_flight: max_occupancy,
    })
}

fn rejection_record(row: &SourceRow, stage: &str, hard_failures: Option<&[String]>) -> Value {
    json!({"schema_version":"scogo.taskgen.rejection.v2","candidate_id":row.task_id,
        "stage":stage,"hard_failures":hard_failures,"candidate":row.task})
}

fn finalize_row(
    journal: &mut StageJournal,
    row: &SourceRow,
    mut review: ReviewStageResult,
    accepted: bool,
) -> Result<bool> {
    review.record["final_disposition"] = json!(if accepted { "accepted" } else { "rejected" });
    journal.append(
        &row.task_id,
        row.source_index,
        if accepted {
            JournalStage::Accepted
        } else {
            JournalStage::Rejected
        },
        json!({"review":review,
            "rejection":(!accepted).then(|| rejection_record(row,"model_review_v3",None))}),
    )?;
    Ok(accepted)
}

#[derive(Debug)]
struct StageJournal {
    writer: File,
    config_sha256: String,
    next_sequence: u64,
    previous_sha256: Option<String>,
    snapshot: JournalSnapshot,
}

impl StageJournal {
    fn create(path: &Path, config_sha256: &str) -> Result<Self> {
        let writer = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to create stage journal: {}", path.display()))?;
        sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
        Ok(Self {
            writer,
            config_sha256: config_sha256.to_string(),
            next_sequence: 0,
            previous_sha256: None,
            snapshot: JournalSnapshot::default(),
        })
    }

    fn resume(path: &Path, config_sha256: &str) -> Result<(Self, JournalSnapshot)> {
        let mut bytes = std::fs::read(path)
            .with_context(|| format!("failed to read stage journal: {}", path.display()))?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            let committed = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |position| position + 1);
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(committed as u64)?;
            file.sync_all()?;
            bytes.truncate(committed);
        }

        let mut snapshot = JournalSnapshot::default();
        let mut previous_sha256: Option<String> = None;
        let mut next_sequence = 0u64;
        for (line_index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: JournalEntry = serde_json::from_slice(line).with_context(|| {
                format!("invalid committed journal entry at line {}", line_index + 1)
            })?;
            if entry.body.schema_version != "scogo.taskgen.phase-b-journal.v1"
                || entry.body.sequence != next_sequence
                || entry.body.previous_sha256 != previous_sha256
                || entry.body.config_sha256 != config_sha256
            {
                bail!(
                    "stage journal chain/config conflict at line {}",
                    line_index + 1
                );
            }
            let actual = format!("{:x}", Sha256::digest(serde_json::to_vec(&entry.body)?));
            if entry.event_sha256 != actual {
                bail!("stage journal digest conflict at line {}", line_index + 1);
            }
            snapshot.apply(&entry.body)?;
            previous_sha256 = Some(actual);
            next_sequence += 1;
        }
        let writer = OpenOptions::new().append(true).open(path)?;
        let journal = Self {
            writer,
            config_sha256: config_sha256.to_string(),
            next_sequence,
            previous_sha256,
            snapshot: snapshot.clone(),
        };
        Ok((journal, snapshot))
    }

    fn append(
        &mut self,
        task_id: &str,
        source_index: usize,
        stage: JournalStage,
        payload: Value,
    ) -> Result<()> {
        let body = JournalEntryBody {
            schema_version: "scogo.taskgen.phase-b-journal.v1".to_string(),
            sequence: self.next_sequence,
            previous_sha256: self.previous_sha256.clone(),
            config_sha256: self.config_sha256.clone(),
            task_id: task_id.to_string(),
            source_index,
            stage,
            payload,
        };
        let mut prospective = self.snapshot.clone();
        prospective.apply(&body)?;
        let event_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&body)?));
        let entry = JournalEntry {
            body,
            event_sha256: event_sha256.clone(),
        };
        serde_json::to_writer(&mut self.writer, &entry)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.writer.sync_all()?;
        self.snapshot = prospective;
        self.previous_sha256 = Some(event_sha256);
        self.next_sequence += 1;
        Ok(())
    }
}

fn jsonl(values: impl IntoIterator<Item = Value>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, &value)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

fn descriptor(file: &str, bytes: &[u8]) -> Value {
    json!({"file":file,"bytes":bytes.len(),"sha256":sha256_bytes(bytes)})
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "redox",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
))]
fn atomic_rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn atomic_rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)?;
    Ok(())
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "redox",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "windows"
)))]
fn atomic_rename_noreplace(_source: &Path, _destination: &Path) -> Result<()> {
    bail!("atomic no-replace Phase-B publication is unsupported on this platform")
}

fn seal_run<F, G>(
    prepared: &PreparedRun,
    snapshot: &JournalSnapshot,
    journal: &mut StageJournal,
    before_manifest: F,
    after_rename: G,
) -> Result<String>
where
    F: FnOnce() -> Result<()>,
    G: FnOnce() -> Result<()>,
{
    prepared.assert_inputs_unchanged()?;
    if snapshot.accepted != prepared.target || snapshot.pending() != 0 {
        bail!("Phase-B seal requires exactly the accepted target and no pending rows");
    }
    if prepared.final_run_dir.exists() {
        bail!("Phase-B final run already exists; refusing overwrite");
    }
    let parent = prepared
        .final_run_dir
        .parent()
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let final_name = prepared
        .final_run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("Phase-B final run directory needs a UTF-8 name")?;
    let temporary_name = format!(".{final_name}.prepared-{}", &prepared.config_sha256[..16]);
    let temporary = parent.join(&temporary_name);
    std::fs::create_dir(&temporary)?;
    let result = (|| -> Result<String> {
        let source_by_id = prepared
            .eligible_rows
            .iter()
            .map(|row| (row.task_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        let mut states = snapshot.rows.iter().collect::<Vec<_>>();
        states.sort_by_key(|(_, state)| state.source_index);
        let mut tasks = Vec::new();
        let mut candidates = Vec::new();
        let mut reviews = Vec::new();
        let mut rejected = Vec::new();
        let mut selected_ids = Vec::new();
        for (task_id, state) in states {
            let row = source_by_id.get(task_id.as_str()).with_context(|| {
                format!("journal task {task_id} is absent from eligible source")
            })?;
            candidates.push(json!({
                "schema_version":"scogo.taskgen.candidate.v1",
                "candidate_id":task_id,
                "sequence":state.source_index + 1,
                "candidate":row.task,
            }));
            let payload = state
                .terminal_payload
                .as_ref()
                .context("Phase-B terminal row is missing its payload")?;
            if let Some(review) = &payload.review {
                reviews.push(review.record.clone());
            }
            if let Some(rejection) = &payload.rejection {
                rejected.push(rejection.clone());
            }
            if state.terminal == Some(JournalStage::Accepted) {
                tasks.push(row.task.clone());
                selected_ids.push(task_id.clone());
            }
        }
        let tasks_bytes = jsonl(tasks)?;
        let candidates_bytes = jsonl(candidates)?;
        let reviews_bytes = jsonl(reviews)?;
        let rejected_bytes = jsonl(rejected)?;
        let mut artifacts = serde_json::Map::new();
        for (name, file, bytes) in [
            ("tasks", "tasks.jsonl", tasks_bytes.as_slice()),
            ("reviews", "reviews.jsonl", reviews_bytes.as_slice()),
            (
                "candidates",
                "candidates.jsonl",
                candidates_bytes.as_slice(),
            ),
            ("rejected", "rejected.jsonl", rejected_bytes.as_slice()),
            (
                "source_population",
                "source_population.jsonl",
                prepared.source.canonical_jsonl.as_slice(),
            ),
            (
                "exclusion_authority",
                prepared.exclusion_authority.relative_file.as_str(),
                prepared.exclusion_authority.bytes.as_slice(),
            ),
        ] {
            write_synced(&temporary.join(file), bytes)?;
            artifacts.insert(name.to_string(), descriptor(file, bytes));
        }
        if let Some(evidence) = &prepared.historical_reservation {
            write_synced(&temporary.join(&evidence.relative_file), &evidence.bytes)?;
            artifacts.insert(
                "historical_import_reservation".to_string(),
                descriptor(&evidence.relative_file, &evidence.bytes),
            );
        }
        for release in &prepared.prior_releases {
            for evidence in release.artifacts.values() {
                write_synced(&temporary.join(&evidence.relative_file), &evidence.bytes)?;
                artifacts.insert(
                    evidence.logical_name.clone(),
                    descriptor(&evidence.relative_file, &evidence.bytes),
                );
            }
        }
        let receipt = json!({
            "schema_version":"scogo.private-hf-subset-receipt.v1",
            "repo_id":prepared.source_repo_id,
            "repo_type":"dataset",
            "private":true,
            "revision":prepared.source_revision,
            "source_file":prepared.source_file,
            "selection":prepared.source_selection,
            "rows":prepared.target,
            "subset_sha256":sha256_bytes(&tasks_bytes),
            "selected_source_task_ids":selected_ids,
            "source_file_rows":prepared.source.rows.len(),
            "source_file_sha256":sha256_bytes(&prepared.source.canonical_jsonl),
            "source_population_sha256":prepared.source.population_sha256,
            "excluded_source_task_ids":prepared.excluded_ids,
            "exclusion_authority_sha256":prepared.exclusion_authority.sha256,
        });
        let mut receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
        receipt_bytes.push(b'\n');
        write_synced(&temporary.join("source_receipt.json"), &receipt_bytes)?;
        artifacts.insert(
            "source_receipt".to_string(),
            descriptor("source_receipt.json", &receipt_bytes),
        );
        artifacts.insert("run".to_string(), json!({"file":"run.json"}));
        sync_directory(&temporary)?;
        before_manifest()?;
        let now = chrono::Utc::now().to_rfc3339();
        let manifest = json!({
            "schema_version":"scogo.taskgen.run.v3",
            "command_version":env!("CARGO_PKG_VERSION"),
            "run_id":prepared.run_id,
            "status":"success",
            "started_at":now,
            "completed_at":now,
            "input_records":prepared.source.rows.len(),
            "reviewed_records":snapshot.rows.values().filter(|row| row.review.is_some()).count(),
            "accepted_records":prepared.target,
            "rejected_records":snapshot.rejected,
            "phase_b":{
                "config_sha256":prepared.config_sha256,
                "accepted_target":prepared.target,
                "source_population_sha256":prepared.source.population_sha256,
                "admitted_records":snapshot.rows.len(),
            },
            "review":prepared.config["review"],
            "adjudication":prepared.config["adjudication"],
            "artifacts":artifacts,
        });
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        manifest_bytes.push(b'\n');
        write_synced(&temporary.join("run.json"), &manifest_bytes)?;
        sync_directory(&temporary)?;
        let manifest_sha256 = sha256_bytes(&manifest_bytes);
        journal.append(
            "__run__",
            0,
            JournalStage::SealPrepared,
            json!({"manifest_sha256":manifest_sha256,"temporary_name":temporary_name}),
        )?;
        prepared.assert_inputs_unchanged()?;
        atomic_rename_noreplace(&temporary, &prepared.final_run_dir)?;
        sync_directory(parent)?;
        after_rename()?;
        journal.append(
            "__run__",
            0,
            JournalStage::Sealed,
            json!({"manifest_sha256":manifest_sha256}),
        )?;
        Ok(manifest_sha256)
    })();
    match result {
        Ok(digest) => Ok(digest),
        Err(error) => {
            if journal.snapshot.seal_prepared_manifest_sha256.is_none() && temporary.exists() {
                let _ = std::fs::remove_dir_all(&temporary);
                let _ = sync_directory(parent);
            }
            Err(error)
        }
    }
}

fn verify_run_directory(
    prepared: &PreparedRun,
    run_dir: &Path,
    expected_manifest: Option<&str>,
) -> Result<String> {
    let directory = HeldDirectory::capture(run_dir)?;
    let (manifest_file, manifest_bytes) =
        directory.read_file(Path::new("run.json"), 16 * 1024 * 1024)?;
    let mut held_files = vec![manifest_file];
    let mut payloads = BTreeMap::new();
    let manifest_sha256 = sha256_bytes(&manifest_bytes);
    if expected_manifest.is_some_and(|expected| expected != manifest_sha256) {
        bail!("sealed Phase-B manifest digest changed");
    }
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .context("sealed Phase-B manifest is invalid JSON")?;
    if manifest["schema_version"] != "scogo.taskgen.run.v3"
        || manifest["status"] != "success"
        || manifest["run_id"] != prepared.run_id
        || manifest["phase_b"]["config_sha256"] != prepared.config_sha256
        || manifest["accepted_records"] != prepared.target
    {
        bail!("sealed Phase-B manifest conflicts with immutable config");
    }
    let artifacts = manifest["artifacts"]
        .as_object()
        .context("sealed Phase-B manifest has no artifact map")?;
    let mut expected_names = [
        "tasks",
        "reviews",
        "candidates",
        "rejected",
        "source_population",
        "source_receipt",
        "exclusion_authority",
        "run",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<HashSet<_>>();
    if prepared.historical_reservation.is_some() {
        expected_names.insert("historical_import_reservation".to_string());
    }
    expected_names.extend(
        prepared
            .prior_releases
            .iter()
            .flat_map(|release| release.artifacts.keys().cloned()),
    );
    if artifacts.keys().cloned().collect::<HashSet<_>>() != expected_names {
        bail!("sealed Phase-B artifact set is incomplete or contains extras");
    }
    let mut declared_files = HashSet::new();
    for (name, value) in artifacts {
        let file = value
            .get("file")
            .and_then(Value::as_str)
            .with_context(|| format!("artifact {name} has no file"))?;
        let relative = Path::new(file);
        if relative.is_absolute()
            || relative.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::CurDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
            || !declared_files.insert(file.to_string())
        {
            bail!("artifact {name} has an unsafe or duplicate path");
        }
        if name == "run" {
            if file != "run.json" || value.as_object().is_none_or(|row| row.len() != 1) {
                bail!("run artifact descriptor must contain only run.json");
            }
            continue;
        }
        let (held, bytes) = directory.read_file(relative, 1024 * 1024 * 1024)?;
        if value.get("bytes").and_then(Value::as_u64) != Some(bytes.len() as u64)
            || value.get("sha256").and_then(Value::as_str) != Some(&sha256_bytes(&bytes))
        {
            bail!("artifact digest/size mismatch for {name}");
        }
        held_files.push(held);
        payloads.insert(name.clone(), bytes);
    }
    let tasks = std::str::from_utf8(&payloads["tasks"])?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let reviews = std::str::from_utf8(&payloads["reviews"])?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let selected_ids = tasks
        .iter()
        .map(source_task_id)
        .collect::<Result<Vec<_>>>()?;
    if tasks.len() != prepared.target
        || reviews
            .iter()
            .filter(|review| review["final_disposition"] == "accepted")
            .count()
            != prepared.target
    {
        bail!("sealed Phase-B task/review count is inconsistent");
    }
    let receipt: CurrentReceipt = serde_json::from_slice(&payloads["source_receipt"])?;
    if receipt.schema_version != "scogo.private-hf-subset-receipt.v1"
        || receipt.repo_id != prepared.source_repo_id
        || receipt.repo_type != "dataset"
        || !receipt.private
        || receipt.revision != prepared.source_revision
        || receipt.source_file != prepared.source_file
        || receipt.selection != prepared.source_selection
        || receipt.rows != prepared.target
        || receipt.selected_source_task_ids != selected_ids
        || receipt.excluded_source_task_ids != prepared.excluded_ids
        || receipt.subset_sha256 != sha256_bytes(&payloads["tasks"])
        || receipt.source_population_sha256 != prepared.source.population_sha256
        || receipt.source_file_rows != prepared.source.rows.len()
        || receipt.source_file_sha256 != sha256_bytes(&prepared.source.canonical_jsonl)
        || receipt.exclusion_authority_sha256 != prepared.exclusion_authority.sha256
        || payloads["source_population"] != prepared.source.canonical_jsonl
        || payloads["exclusion_authority"] != prepared.exclusion_authority.bytes
    {
        bail!("sealed Phase-B receipt does not match source selection");
    }
    if payloads.get("historical_import_reservation")
        != prepared
            .historical_reservation
            .as_ref()
            .map(|artifact| &artifact.bytes)
    {
        bail!("sealed Phase-B historical reservation differs from derived evidence");
    }
    for release in &prepared.prior_releases {
        for artifact in release.artifacts.values() {
            if payloads.get(&artifact.logical_name) != Some(&artifact.bytes) {
                bail!("sealed Phase-B prior evidence differs from held validated bytes");
            }
        }
    }
    for held in &held_files {
        held.assert_current()?;
    }
    directory.assert_current()?;
    Ok(manifest_sha256)
}

fn verify_sealed_run(prepared: &PreparedRun, expected_manifest: Option<&str>) -> Result<String> {
    verify_run_directory(prepared, &prepared.final_run_dir, expected_manifest)
}

fn finish_prepared_seal(prepared: &PreparedRun, journal: &mut StageJournal) -> Result<String> {
    prepared.assert_inputs_unchanged()?;
    let digest = journal
        .snapshot
        .seal_prepared_manifest_sha256
        .clone()
        .context("Phase-B finalization has no seal-prepared digest")?;
    let temporary_name = journal
        .snapshot
        .seal_temporary_name
        .as_deref()
        .context("Phase-B finalization has no prepared directory")?;
    let parent = prepared
        .final_run_dir
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(temporary_name);
    if prepared.final_run_dir.exists() {
        verify_sealed_run(prepared, Some(&digest))?;
    } else {
        verify_run_directory(prepared, &temporary, Some(&digest))?;
        atomic_rename_noreplace(&temporary, &prepared.final_run_dir)?;
        sync_directory(parent)?;
    }
    if journal.snapshot.sealed_manifest_sha256.is_none() {
        journal.append(
            "__run__",
            0,
            JournalStage::Sealed,
            json!({"manifest_sha256":digest}),
        )?;
    }
    Ok(digest)
}

pub(crate) async fn run(args: crate::ReviewArgs) -> Result<()> {
    if args.gold_labels.is_some() {
        bail!("bounded Phase-B review does not support --gold-labels");
    }
    let (work_dir, _) = isolated_run_paths(&args)?;
    let _work_lock = WorkLock::acquire(&work_dir)?;
    let taxonomy_snapshot = HeldFile::capture(&args.taxonomy, 16 * 1024 * 1024)?;
    let taxonomy_text =
        std::str::from_utf8(&taxonomy_snapshot.1).context("Phase-B taxonomy is not UTF-8")?;
    let taxonomy =
        crate::taxonomy::TaxonomyCatalog::from_yaml(taxonomy_text, Some(&args.taxonomy))?;
    let system_prompt = if let Some(prompt) = &args.system_prompt {
        prompt.clone()
    } else if let Some(path) = &args.system_prompt_file {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read review prompt: {}", path.display()))?
    } else if let Some(path) = taxonomy.default_review_system_prompt_path() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read taxonomy review prompt: {}", path.display()))?
    } else {
        include_str!("../prompts/itops-prompt-review-system-v3.txt").to_string()
    };
    let prepared = prepare_run(&args, &taxonomy, &system_prompt, Some(taxonomy_snapshot))?;
    prepared.assert_inputs_unchanged()?;
    let mut journal = open_work(&prepared, args.resume)?;
    if journal.snapshot.seal_prepared_manifest_sha256.is_some() {
        finish_prepared_seal(&prepared, &mut journal)?;
        println!(
            "Verified existing bounded Phase-B review run: {}",
            prepared.final_run_dir.display()
        );
        return Ok(());
    }
    if prepared.final_run_dir.exists() {
        bail!("Phase-B final run exists without a matching seal-prepared journal anchor");
    }

    let credentials = crate::provider::load_credential_pool(
        args.keyfile.as_deref(),
        args.api_key.clone(),
        "review",
    )?;
    let review_provider = crate::provider::ProviderConfig {
        api_base: crate::provider::normalize_api_base(&args.api_base)?,
        model: args.model.clone(),
        credentials,
    };
    let adjudication_credentials =
        if args.adjudication_keyfile.is_some() || args.adjudication_api_key.is_some() {
            Some(crate::provider::load_credential_pool(
                args.adjudication_keyfile.as_deref(),
                args.adjudication_api_key.clone(),
                "adjudication",
            )?)
        } else {
            None
        };
    let adjudication_provider = crate::provider::resolve_review_provider(
        &review_provider,
        crate::provider::ProviderOverrides {
            api_base: args.adjudication_api_base.clone(),
            model: args.adjudication_model.clone(),
            credentials: adjudication_credentials,
        },
    )?;
    let client = crate::taskgen_http_client_builder(
        std::time::Duration::from_secs(120),
        std::time::Duration::from_secs(15),
        args.review_workers,
    )
    .build()?;
    let reviewer = NetworkReviewer {
        taxonomy_id: taxonomy.id().to_string(),
        taxonomy_kind: format!("{:?}", taxonomy.kind()).to_ascii_lowercase(),
        review_provider: review_provider.clone(),
        adjudication_provider,
        client,
        system_prompt,
        max_output_tokens: crate::review_max_output_tokens(
            &review_provider.model,
            args.max_output_tokens,
        ),
        review_telemetry: Arc::new(crate::telemetry::RequestTelemetry::default()),
        adjudication_telemetry: Arc::new(crate::telemetry::RequestTelemetry::default()),
        references: prepared.reference_snapshot.store.clone(),
        rate_limiter: crate::review::ReviewRateLimiter::from_requests_per_minute(
            args.review_requests_per_minute,
        )?,
    };
    let result = run_admission(
        prepared.eligible_rows.clone(),
        prepared.target,
        args.review_workers,
        journal,
        Arc::new(reviewer),
    )
    .await?;
    let (mut journal, _) = StageJournal::resume(
        &prepared.work_dir.join("stage.journal.jsonl"),
        &prepared.config_sha256,
    )?;
    let manifest_sha256 = seal_run(
        &prepared,
        &result.snapshot,
        &mut journal,
        || Ok(()),
        || Ok(()),
    )?;
    verify_sealed_run(&prepared, Some(&manifest_sha256))?;
    println!(
        "Reviewed bounded source population: exactly {} accepted from {} admitted (max accepted + in-flight {}) -> {}",
        prepared.target,
        result.admitted,
        result.max_accepted_plus_in_flight,
        prepared.final_run_dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use clap::Parser;

    use super::*;

    fn golden_task() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/canonical/valid-task.json")).unwrap()
    }

    fn test_source_row(source_index: usize) -> SourceRow {
        let mut task = golden_task();
        task["prompt"] = json!(format!("Phase-B test prompt {source_index}"));
        SourceRow {
            source_index,
            task_id: source_task_id(&task).unwrap(),
            task,
            deterministic_hard_failures: vec![],
        }
    }

    fn test_review(row: &SourceRow, outcome: ReviewStageOutcome) -> ReviewStageResult {
        let decision = match outcome {
            ReviewStageOutcome::Accept => serde_json::from_str(include_str!(
                "../tests/fixtures/canonical/valid-review-v3.json"
            ))
            .unwrap(),
            ReviewStageOutcome::Reject => json!({
                "schema_version":"scogo.taskgen.prompt-review.v3","outcome":"reject",
                "checks":{
                    "coordinate_realization":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "internal_consistency":{"status":"fail","rationale":"conflict","evidence_paths":["$.candidate.prompt"]},
                    "operational_quality":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "safety":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "technical_authenticity":{"status":"pass","rationale":"ok","evidence_paths":[]}},
                "hard_failures":["internal_contradiction"],"claims_requiring_verification":[],
                "summary":"conflict","retry_guidance":"replace"
            }),
            ReviewStageOutcome::NeedsVerification => json!({
                "schema_version":"scogo.taskgen.prompt-review.v3","outcome":"needs_verification",
                "checks":{
                    "coordinate_realization":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "internal_consistency":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "operational_quality":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "safety":{"status":"pass","rationale":"ok","evidence_paths":[]},
                    "technical_authenticity":{"status":"unknown","rationale":"verify","evidence_paths":["$.candidate.prompt"]}},
                "hard_failures":[],"claims_requiring_verification":[{
                    "claim_id":"claim-1","claim":"verify","candidate_evidence_paths":["$.candidate.prompt"],
                    "reference_query":"verify"}],"summary":"verify","retry_guidance":""
            }),
        };
        ReviewStageResult {
            outcome,
            record: json!({
                "schema_version":"scogo.taskgen.review-record.v3","candidate_id":row.task_id,
                "sequence":row.source_index + 1,"review_model":"test-reviewer",
                "review_input_tokens":1,"review_output_tokens":1,
                "decision_normalization":{"summary_truncated":false,"retry_guidance_truncated":false,
                    "hard_failure_aliases_normalized":0,"claim_ids_repaired":0,"response_format":"json_schema"},
                "decision":decision,"references":[],"adjudication":null,"final_disposition":"pending"
            }),
        }
    }

    #[test]
    fn task_ids_and_population_digest_match_python_golden_values() {
        let first = golden_task();
        let mut second = first.clone();
        second["prompt"] = serde_json::json!("Unicode café 路由 incident");

        assert_eq!(
            source_task_id(&first).unwrap(),
            "task_cc3e0bec7b87ec223ae0ef01f4d4235ff26f8b5111d6590c39c8c7db13a88a7f"
        );
        assert_eq!(
            source_task_id(&second).unwrap(),
            "task_8a45fd82c66ff7d94244f25acc28ec84d57d96bb85ebf50f777a3c74bf1ddf34"
        );
        assert_eq!(
            source_population_sha256(&[first, second]).unwrap(),
            "f01f0b22eac765cec916eb7f1b92973ddf53b3472705967445a53c87f0392a76"
        );
    }

    #[test]
    fn credential_scan_requires_a_token_prefix_boundary() {
        assert!(!contains_credential(
            b"scogo.data-factory.task-reservation.v1"
        ));
        let shaped_secret = ["credential=sk-", "abcdefghijklmnopqrstuvwxyz"].concat();
        assert!(contains_credential(shaped_secret.as_bytes()));
    }

    #[test]
    fn source_population_rejects_duplicate_data_factory_task_ids() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        let task = golden_task();
        std::fs::write(&source, format!("{task}\n{task}\n")).unwrap();
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();

        let error = load_source_population(&source, &taxonomy).unwrap_err();
        assert!(error.to_string().contains("duplicate task ID"), "{error:#}");
    }

    #[test]
    fn immutable_resume_rejects_changed_source_before_provider_setup() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        let work = temporary.path().join("work");
        let final_run = temporary.path().join("final");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let parse = |resume: bool| {
            let mut argv = vec![
                "taskgen".to_string(),
                "review".into(),
                "--input".into(),
                source.display().to_string(),
                "--taxonomy".into(),
                "docs/netops-taxonomy.yaml".into(),
                "--accepted-target".into(),
                "1".into(),
                "--run-id".into(),
                "phase-b-test".into(),
                "--work-dir".into(),
                work.display().to_string(),
                "--final-run-dir".into(),
                final_run.display().to_string(),
                "--source-repo-id".into(),
                "ScogoAI/netops-prompt-seed".into(),
                "--source-revision".into(),
                "0123456789abcdef0123456789abcdef01234567".into(),
                "--source-file".into(),
                "part-3/tasks.jsonl".into(),
                "--source-selection".into(),
                "unused-phase-b-test".into(),
            ];
            if resume {
                argv.push("--resume".into());
            }
            let cli = crate::Cli::try_parse_from(argv).unwrap();
            let crate::Command::Review(args) = cli.command else {
                panic!("expected review command")
            };
            *args
        };
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let first = prepare_run(&parse(false), &taxonomy, "review prompt", None).unwrap();
        let _journal = open_work(&first, false).unwrap();

        let mut changed = golden_task();
        changed["prompt"] = serde_json::json!("Changed source prompt");
        std::fs::write(&source, format!("{changed}\n")).unwrap();
        let changed = prepare_run(&parse(true), &taxonomy, "review prompt", None).unwrap();
        let error = open_work(&changed, true).unwrap_err();
        assert!(
            error.to_string().contains("immutable config changed"),
            "{error:#}"
        );

        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let mut changed_destination =
            prepare_run(&parse(true), &taxonomy, "review prompt", None).unwrap();
        changed_destination.final_run_dir = temporary.path().join("different-final");
        let error = open_work(&changed_destination, true).unwrap_err();
        assert!(
            error.to_string().contains("immutable config changed"),
            "{error:#}"
        );
    }

    #[test]
    fn resume_recovers_an_empty_work_directory_from_initialization_crash() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        let work = temporary.path().join("work");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let cli = crate::Cli::try_parse_from(vec![
            "taskgen".to_string(),
            "review".into(),
            "--input".into(),
            source.display().to_string(),
            "--taxonomy".into(),
            "docs/netops-taxonomy.yaml".into(),
            "--accepted-target".into(),
            "1".into(),
            "--run-id".into(),
            "init-crash".into(),
            "--work-dir".into(),
            work.display().to_string(),
            "--final-run-dir".into(),
            temporary.path().join("final").display().to_string(),
            "--source-repo-id".into(),
            "ScogoAI/netops-prompt-seed".into(),
            "--source-revision".into(),
            "0123456789abcdef0123456789abcdef01234567".into(),
            "--source-file".into(),
            "part-3/tasks.jsonl".into(),
            "--source-selection".into(),
            "init-crash".into(),
            "--resume".into(),
        ])
        .unwrap();
        let crate::Command::Review(args) = cli.command else {
            panic!("expected review")
        };
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        std::fs::create_dir(&prepared.work_dir).unwrap();

        let journal = open_work(&prepared, true).unwrap();
        assert_eq!(journal.snapshot.rows.len(), 0);
        assert!(prepared.work_dir.join("config.json").is_file());
        assert!(prepared.work_dir.join("stage.journal.jsonl").is_file());
        drop(journal);
        std::fs::remove_file(prepared.work_dir.join("stage.journal.jsonl")).unwrap();
        let journal = open_work(&prepared, true).unwrap();
        assert_eq!(journal.snapshot.rows.len(), 0);
    }

    #[tokio::test]
    async fn pinned_legacy_evidence_requires_all_six_exact_payloads() {
        let temporary = tempfile::tempdir().unwrap();
        let mut prior_tasks = [golden_task(), golden_task()];
        prior_tasks[0]["prompt"] = json!("Previously accepted source prompt one");
        prior_tasks[1]["prompt"] = json!("Previously accepted source prompt two");
        let mut current_task = golden_task();
        current_task["prompt"] = json!("New unused source prompt");
        let prior_ids = prior_tasks
            .iter()
            .map(|task| source_task_id(task).unwrap())
            .collect::<Vec<_>>();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(
            &source,
            format!("{}\n{}\n{current_task}\n", prior_tasks[0], prior_tasks[1]),
        )
        .unwrap();
        let decision: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/canonical/valid-review-v3.json"
        ))
        .unwrap();
        let reviews = prior_ids
            .iter()
            .enumerate()
            .map(|(index, task_id)| {
                json!({
                    "schema_version":"scogo.taskgen.review-record.v3",
                    "candidate_id":task_id,
                    "sequence":index + 1,
                    "review_model":"legacy-reviewer",
                    "review_input_tokens":10,
                    "review_output_tokens":5,
                    "decision_normalization":{
                        "summary_truncated":false,"retry_guidance_truncated":false,
                        "hard_failure_aliases_normalized":0,"claim_ids_repaired":0,
                        "response_format":"json_schema"
                    },
                    "decision":decision,
                    "references":[],"adjudication":null,"final_disposition":"accepted"
                })
            })
            .collect::<Vec<_>>();
        let taskgen_tasks_path = temporary.path().join("prior-taskgen-tasks.jsonl");
        let taskgen_reviews_path = temporary.path().join("prior-taskgen-reviews.jsonl");
        std::fs::write(
            &taskgen_tasks_path,
            jsonl(prior_tasks.iter().cloned()).unwrap(),
        )
        .unwrap();
        std::fs::write(&taskgen_reviews_path, jsonl(reviews.clone()).unwrap()).unwrap();
        let taskgen_manifest_path = temporary.path().join("prior-taskgen-run.json");
        let taskgen_manifest = json!({
            "schema_version":"scogo.taskgen.run.v3","run_id":"legacy-taskgen-run","status":"success",
            "artifacts":{
                "tasks":descriptor("tasks.jsonl",&std::fs::read(&taskgen_tasks_path).unwrap()),
                "reviews":descriptor("reviews.jsonl",&std::fs::read(&taskgen_reviews_path).unwrap()),
                "run":{"file":"run.json"}
            }
        });
        std::fs::write(
            &taskgen_manifest_path,
            serde_json::to_vec(&taskgen_manifest).unwrap(),
        )
        .unwrap();
        let legacy_receipt_path = temporary.path().join("legacy-source-receipt.json");
        std::fs::write(
            &legacy_receipt_path,
            serde_json::to_vec(&json!({
                "schema_version":"scogo.private-hf-subset-receipt.v1",
                "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
                "revision":"0123456789abcdef0123456789abcdef01234567",
                "source_file":"part-3/tasks.jsonl","selection":"legacy-first-two",
                "rows":2,"subset_sha256":sha256_bytes(&std::fs::read(&taskgen_tasks_path).unwrap())
            }))
            .unwrap(),
        )
        .unwrap();
        let mut canonical_pairs = prior_tasks.iter().cloned().zip(reviews).collect::<Vec<_>>();
        canonical_pairs.sort_by_key(|(task, _)| source_task_id(task).unwrap());
        let canonical = canonical_pairs.iter().map(|(task, review)| {
            let task_id = source_task_id(task).unwrap();
            json!({
                "schema_version":"scogo.data-factory.source-task.v1","source_task_id":task_id,
                "split_group_id":task_id,"split":"train","prompt":task["prompt"],
                "domain":task["domain"],"subdomain":task["subdomain"],
                "difficulty":task["difficulty"].to_string(),"coordinates":task["coordinates"],
                "source_schema_version":"scogo.taskgen.task.v2","source_task":task,"source_review":review
            })
        }).collect::<Vec<_>>();
        let canonical_path = temporary.path().join("prior-canonical-tasks.jsonl");
        std::fs::write(&canonical_path, jsonl(canonical).unwrap()).unwrap();
        let release_path = temporary.path().join("prior-release-set.json");
        let taskgen_digests = (
            sha256_bytes(&std::fs::read(&taskgen_manifest_path).unwrap()),
            sha256_bytes(&std::fs::read(&taskgen_tasks_path).unwrap()),
            sha256_bytes(&std::fs::read(&taskgen_reviews_path).unwrap()),
        );
        let canonical_bytes = std::fs::read(&canonical_path).unwrap();
        std::fs::write(&release_path, serde_json::to_vec(&json!({
            "schema_version":"scogo.data-factory.release-set.v1","release_id":"legacy-release",
            "campaign_id":"legacy-campaign","campaign_sha256":"a".repeat(64),
            "source_run_id":"legacy-taskgen-run",
            "source_artifacts":{"run":taskgen_digests.0,"tasks":taskgen_digests.1,"reviews":taskgen_digests.2},
            "rubric_version":"scogo.itops-rubric.v1","providers":{},"claim_status":"development_only",
            "split_counts":{"train":2,"validation":0,"evaluation":0},"projection_counts":{},
            "artifacts":[{"path":"canonical/tasks.jsonl","sha256":sha256_bytes(&canonical_bytes),
                "bytes":canonical_bytes.len(),"rows":2}],"created_at":"2026-09-01T00:00:00Z"
        })).unwrap()).unwrap();
        let release_pin = sha256_bytes(&std::fs::read(&release_path).unwrap());
        let evidence = [
            ("prior_release_set.legacy-release", release_path),
            ("prior_canonical_tasks.legacy-release", canonical_path),
            (
                "prior_legacy_source_receipt.legacy-release",
                legacy_receipt_path,
            ),
            ("prior_taskgen_run.legacy-release", taskgen_manifest_path),
            ("prior_taskgen_tasks.legacy-release", taskgen_tasks_path),
            ("prior_taskgen_reviews.legacy-release", taskgen_reviews_path),
        ];
        let mut argv = vec![
            "taskgen".to_string(),
            "review".into(),
            "--input".into(),
            source.display().to_string(),
            "--taxonomy".into(),
            "docs/netops-taxonomy.yaml".into(),
            "--accepted-target".into(),
            "1".into(),
            "--run-id".into(),
            "legacy-evidence-test".into(),
            "--work-dir".into(),
            temporary.path().join("work").display().to_string(),
            "--final-run-dir".into(),
            temporary.path().join("final").display().to_string(),
            "--source-repo-id".into(),
            "ScogoAI/netops-prompt-seed".into(),
            "--source-revision".into(),
            "0123456789abcdef0123456789abcdef01234567".into(),
            "--source-file".into(),
            "part-3/tasks.jsonl".into(),
            "--source-selection".into(),
            "unused-legacy-test".into(),
            "--prior-release-pin".into(),
            format!("legacy-release={release_pin}"),
        ];
        for (name, path) in &evidence {
            argv.extend([
                "--prior-evidence".into(),
                format!("{name}={}", path.display()),
            ]);
        }
        let cli = crate::Cli::try_parse_from(argv).unwrap();
        let crate::Command::Review(args) = cli.command else {
            panic!("expected review")
        };
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        assert_eq!(prepared.prior_releases[0].artifacts.len(), 6);
        assert_eq!(prepared.eligible_rows.len(), 1);
        assert_eq!(prepared.excluded_ids.len(), 2);
        assert!(prepared.historical_reservation.is_some());

        for (_, path) in evidence {
            let original = std::fs::read(&path).unwrap();
            std::fs::write(&path, b"tampered\n").unwrap();
            assert!(prepare_run(&args, &taxonomy, "review prompt", None).is_err());
            std::fs::write(path, original).unwrap();
        }

        #[derive(Clone)]
        struct Accept;
        #[async_trait::async_trait]
        impl PhaseBReviewer for Accept {
            async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
                Ok(test_review(row, ReviewStageOutcome::Accept))
            }
            async fn adjudicate(
                &self,
                _row: &SourceRow,
                _review: &ReviewStageResult,
            ) -> Result<AdjudicationStageResult> {
                panic!("adjudication is not expected")
            }
        }
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        let result = run_admission(
            prepared.eligible_rows.clone(),
            1,
            1,
            open_work(&prepared, false).unwrap(),
            Arc::new(Accept),
        )
        .await
        .unwrap();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        seal_run(
            &prepared,
            &result.snapshot,
            &mut journal,
            || Ok(()),
            || Ok(()),
        )
        .unwrap();
        let data_factory = Path::new(
            "/Users/ksingh/git/scogo/work/experiments/scogo-data-factory/.worktree/data-factory-phase-b-100-smoke",
        );
        if data_factory.join(".venv/bin/python").is_file() {
            let status = std::process::Command::new(data_factory.join(".venv/bin/python"))
                .env("PYTHONPATH", data_factory.join("src"))
                .arg("-c")
                .arg("from pathlib import Path; import sys; from scogo_ai_data_factory.taskgen import load_taskgen_run; p=Path(sys.argv[1]); r=load_taskgen_run(p, source_receipt=p/'source_receipt.json', require_source_receipt=True); assert len(r.tasks)==1; assert r.exclusion_authority.prior_completed_releases[0].evidence_mode=='pinned_external_legacy'")
                .arg(std::fs::canonicalize(&prepared.final_run_dir).unwrap())
                .status()
                .unwrap();
            assert!(
                status.success(),
                "Data Factory rejected derived legacy evidence"
            );
        }
    }

    #[test]
    fn current_prior_evidence_derives_history_from_three_cross_bound_artifacts() {
        let temporary = tempfile::tempdir().unwrap();
        let mut prior_task = golden_task();
        prior_task["prompt"] = json!("Current-mode prior task");
        let mut new_task = golden_task();
        new_task["prompt"] = json!("Current-mode unused task");
        let prior_id = source_task_id(&prior_task).unwrap();
        let source = temporary.path().join("source.jsonl");
        let source_bytes = jsonl([prior_task.clone(), new_task]).unwrap();
        std::fs::write(&source, &source_bytes).unwrap();
        let decision: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/canonical/valid-review-v3.json"
        ))
        .unwrap();
        let review = json!({
            "schema_version":"scogo.taskgen.review-record.v3","candidate_id":"prior-current",
            "sequence":1,"decision":decision,"adjudication":null,"final_disposition":"accepted"
        });
        let canonical = json!({
            "schema_version":"scogo.data-factory.source-task.v1","source_task_id":prior_id,
            "split_group_id":prior_id,"split":"train","prompt":prior_task["prompt"],
            "domain":prior_task["domain"],"subdomain":prior_task["subdomain"],
            "difficulty":prior_task["difficulty"].to_string(),"coordinates":prior_task["coordinates"],
            "source_schema_version":"scogo.taskgen.task.v2","source_task":prior_task,"source_review":review
        });
        let canonical_path = temporary.path().join("current-canonical.jsonl");
        let canonical_bytes = jsonl([canonical]).unwrap();
        std::fs::write(&canonical_path, &canonical_bytes).unwrap();
        let selected_bytes = jsonl([prior_task]).unwrap();
        let receipt_path = temporary.path().join("current-receipt.json");
        std::fs::write(&receipt_path, serde_json::to_vec(&json!({
            "schema_version":"scogo.private-hf-subset-receipt.v1",
            "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
            "revision":"0123456789abcdef0123456789abcdef01234567",
            "source_file":"part-3/tasks.jsonl","selection":"prior-current","rows":1,
            "subset_sha256":sha256_bytes(&selected_bytes),"selected_source_task_ids":[prior_id],
            "source_file_rows":2,"source_file_sha256":sha256_bytes(&source_bytes),
            "source_population_sha256":source_population_sha256(&jsonl_rows(&source_bytes,"source").unwrap()).unwrap(),
            "excluded_source_task_ids":[],"exclusion_authority_sha256":"e".repeat(64)
        })).unwrap()).unwrap();
        let receipt_digest = sha256_bytes(&std::fs::read(&receipt_path).unwrap());
        let release_path = temporary.path().join("current-release.json");
        std::fs::write(&release_path, serde_json::to_vec(&json!({
            "schema_version":"scogo.data-factory.release-set.v1","release_id":"current-release",
            "source_run_id":"prior-source","source_artifacts":{
                "tasks":sha256_bytes(&selected_bytes),"source_receipt":receipt_digest,"run":"f".repeat(64)},
            "source_receipt_sha256":receipt_digest,"source_manifest_sha256":"f".repeat(64),
            "source_manifest_bytes":1,"artifacts":[{"path":"canonical/tasks.jsonl",
                "sha256":sha256_bytes(&canonical_bytes),"bytes":canonical_bytes.len(),"rows":1}]
        })).unwrap()).unwrap();
        let release_pin = sha256_bytes(&std::fs::read(&release_path).unwrap());
        let argv = vec![
            "taskgen".to_string(),
            "review".into(),
            "--input".into(),
            source.display().to_string(),
            "--taxonomy".into(),
            "docs/netops-taxonomy.yaml".into(),
            "--accepted-target".into(),
            "1".into(),
            "--run-id".into(),
            "current-evidence-test".into(),
            "--work-dir".into(),
            temporary.path().join("work").display().to_string(),
            "--final-run-dir".into(),
            temporary.path().join("final").display().to_string(),
            "--source-repo-id".into(),
            "ScogoAI/netops-prompt-seed".into(),
            "--source-revision".into(),
            "0123456789abcdef0123456789abcdef01234567".into(),
            "--source-file".into(),
            "part-3/tasks.jsonl".into(),
            "--source-selection".into(),
            "unused-current".into(),
            "--prior-release-pin".into(),
            format!("current-release={release_pin}"),
            "--prior-evidence".into(),
            format!(
                "prior_release_set.current-release={}",
                release_path.display()
            ),
            "--prior-evidence".into(),
            format!(
                "prior_canonical_tasks.current-release={}",
                canonical_path.display()
            ),
            "--prior-evidence".into(),
            format!(
                "prior_source_receipt.current-release={}",
                receipt_path.display()
            ),
        ];
        let cli = crate::Cli::try_parse_from(argv).unwrap();
        let crate::Command::Review(args) = cli.command else {
            panic!("expected review")
        };
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        assert_eq!(prepared.excluded_ids, vec![prior_id]);
        assert_eq!(prepared.prior_releases.len(), 1);
        assert_eq!(
            prepared.prior_releases[0].authority_entry["evidence_mode"],
            "current"
        );
    }

    #[test]
    fn journal_recovers_a_torn_tail_and_rejects_terminal_conflicts() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stage.journal.jsonl");
        let row = test_source_row(0);
        let mut journal = StageJournal::create(&path, "config-a").unwrap();
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::Admitted,
                json!({"task":row.task}),
            )
            .unwrap();
        let completed = test_review(&row, ReviewStageOutcome::Accept);
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::ReviewCompleted,
                serde_json::to_value(&completed).unwrap(),
            )
            .unwrap();
        let mut terminal = completed;
        terminal.record["final_disposition"] = json!("accepted");
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::Accepted,
                json!({"review":terminal,"rejection":null}),
            )
            .unwrap();
        drop(journal);

        let complete_len = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"sequence":2,"stage":"rej"#)
            .unwrap();

        let (mut resumed, snapshot) = StageJournal::resume(&path, "config-a").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), complete_len);
        assert_eq!(snapshot.accepted, 1);
        let conflict = resumed.append(&row.task_id, 0, JournalStage::Rejected, json!({}));
        assert!(
            conflict
                .unwrap_err()
                .to_string()
                .contains("terminal journal conflict")
        );
    }

    #[test]
    fn journal_never_accepts_without_a_durable_review() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stage.journal.jsonl");
        let row = test_source_row(0);
        let mut journal = StageJournal::create(&path, "config-a").unwrap();
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::Admitted,
                json!({"task":row.task}),
            )
            .unwrap();
        let error = journal
            .append(
                &row.task_id,
                0,
                JournalStage::Accepted,
                json!({"review":null,"rejection":null}),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match completed review")
        );
    }

    #[test]
    fn journal_rejects_review_candidate_and_policy_rebinding() {
        let temporary = tempfile::tempdir().unwrap();
        let row = test_source_row(0);
        let mut journal =
            StageJournal::create(&temporary.path().join("stage.journal.jsonl"), "config-a")
                .unwrap();
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::Admitted,
                json!({"task":row.task}),
            )
            .unwrap();
        let mut rebound = test_review(&row, ReviewStageOutcome::Accept);
        rebound.record["candidate_id"] = json!("different-candidate");
        assert!(
            journal
                .append(
                    &row.task_id,
                    0,
                    JournalStage::ReviewCompleted,
                    serde_json::to_value(rebound).unwrap(),
                )
                .is_err()
        );
    }

    #[test]
    fn work_lock_is_exclusive_nonblocking_and_released_on_drop() {
        let temporary = tempfile::tempdir().unwrap();
        let work = temporary.path().join("work/run");
        let first = WorkLock::acquire(&work).unwrap();
        let error = WorkLock::acquire(&work).unwrap_err();
        assert!(error.to_string().contains("already active"), "{error:#}");
        assert!(
            !work.exists(),
            "losing concurrent attempt must not create journal state"
        );
        drop(first);
        WorkLock::acquire(&work).unwrap();
    }

    #[test]
    fn path_isolation_rejects_nested_roots_and_input_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        let work = temporary.path().join("work");
        let nested_final = work.join("final");
        assert!(validate_path_isolation(&work, &nested_final, []).is_err());

        let input = temporary.path().join("input.jsonl");
        std::fs::write(&input, b"{}\n").unwrap();
        assert!(
            validate_path_isolation(
                &work,
                &temporary.path().join("final"),
                [input.clone(), input]
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn held_input_rejects_symlink_and_multi_link_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.json");
        let alias = temporary.path().join("alias.json");
        let symlink = temporary.path().join("symlink.json");
        std::fs::write(&source, b"{}\n").unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        std::os::unix::fs::symlink(&source, &symlink).unwrap();

        assert!(HeldFile::capture(&source, 1024).is_err());
        assert!(HeldFile::capture(&symlink, 1024).is_err());
    }

    #[test]
    fn held_directory_detects_path_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let run = temporary.path().join("run");
        let moved = temporary.path().join("moved");
        std::fs::create_dir(&run).unwrap();
        let held = HeldDirectory::capture(&run).unwrap();
        std::fs::rename(&run, &moved).unwrap();
        std::fs::create_dir(&run).unwrap();
        assert!(held.assert_current().is_err());
    }

    #[test]
    fn reference_hash_and_provider_store_share_one_held_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let reference = temporary.path().join("routing.md");
        std::fs::write(&reference, "original BGP evidence").unwrap();
        let snapshot = ReferenceSnapshot::capture(Some(temporary.path())).unwrap();
        let original_digest = snapshot.digest.clone();
        std::fs::write(&reference, "changed BGP evidence").unwrap();

        assert!(snapshot.assert_current().is_err());
        assert_eq!(snapshot.digest, original_digest);
        assert!(
            snapshot.store.retrieve("BGP evidence", 1, 100)[0]
                .excerpt
                .contains("original")
        );
    }

    #[derive(Clone)]
    struct PatternReviewer {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PhaseBReviewer for PatternReviewer {
        async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let outcome = if matches!(row.source_index, 0 | 2) {
                ReviewStageOutcome::Reject
            } else {
                ReviewStageOutcome::Accept
            };
            Ok(test_review(row, outcome))
        }

        async fn adjudicate(
            &self,
            _row: &SourceRow,
            _review: &ReviewStageResult,
        ) -> Result<AdjudicationStageResult> {
            panic!("adjudication is not expected")
        }
    }

    #[tokio::test]
    async fn mixed_accept_reject_top_up_hits_exact_target_without_overshoot() {
        let temporary = tempfile::tempdir().unwrap();
        let journal_path = temporary.path().join("stage.journal.jsonl");
        let journal = StageJournal::create(&journal_path, "config-a").unwrap();
        let rows = (0..6).map(test_source_row).collect::<Vec<_>>();
        let calls = Arc::new(AtomicUsize::new(0));

        let result = run_admission(
            rows,
            3,
            2,
            journal,
            Arc::new(PatternReviewer {
                calls: calls.clone(),
            }),
        )
        .await
        .unwrap();

        assert_eq!(result.snapshot.accepted, 3);
        assert_eq!(result.snapshot.rejected, 2);
        assert_eq!(result.snapshot.pending(), 0);
        assert_eq!(result.admitted, 5);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(result.max_accepted_plus_in_flight <= 3);
    }

    #[derive(Clone)]
    struct StopOnFailureReviewer {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PhaseBReviewer for StopOnFailureReviewer {
        async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                bail!("provider unavailable")
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(test_review(row, ReviewStageOutcome::Reject))
        }

        async fn adjudicate(
            &self,
            _row: &SourceRow,
            _review: &ReviewStageResult,
        ) -> Result<AdjudicationStageResult> {
            panic!("adjudication is not expected")
        }
    }

    #[tokio::test]
    async fn first_provider_failure_stops_all_replacement_admission() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stage.journal.jsonl");
        let rows = (0..5).map(test_source_row).collect();
        let calls = Arc::new(AtomicUsize::new(0));
        let result = run_admission(
            rows,
            3,
            2,
            StageJournal::create(&path, "config-a").unwrap(),
            Arc::new(StopOnFailureReviewer {
                calls: calls.clone(),
            }),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("paused"));
        let (_, snapshot) = StageJournal::resume(&path, "config-a").unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rejected, 1);
        assert_eq!(snapshot.pending(), 1);
    }

    #[derive(Clone)]
    struct VerificationReviewer {
        review_calls: Arc<AtomicUsize>,
        adjudication_calls: Arc<AtomicUsize>,
        fail_adjudication: bool,
    }

    #[async_trait::async_trait]
    impl PhaseBReviewer for VerificationReviewer {
        async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
            self.review_calls.fetch_add(1, Ordering::SeqCst);
            Ok(test_review(row, ReviewStageOutcome::NeedsVerification))
        }

        async fn adjudicate(
            &self,
            _row: &SourceRow,
            _review: &ReviewStageResult,
        ) -> Result<AdjudicationStageResult> {
            self.adjudication_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_adjudication {
                bail!("temporary upstream exhaustion")
            }
            Ok(AdjudicationStageResult {
                accepted: true,
                adjudication: json!({
                    "decision":serde_json::from_str::<Value>(include_str!(
                        "../tests/fixtures/canonical/valid-adjudication-v1.json"
                    )).unwrap(),
                    "model":"test-adjudicator","input_tokens":1,"output_tokens":1
                }),
            })
        }
    }

    #[tokio::test]
    async fn interrupted_needs_verification_resumes_at_adjudication() {
        let temporary = tempfile::tempdir().unwrap();
        let journal_path = temporary.path().join("stage.journal.jsonl");
        let row = test_source_row(0);
        let review_calls = Arc::new(AtomicUsize::new(0));
        let adjudication_calls = Arc::new(AtomicUsize::new(0));
        let first = run_admission(
            vec![row.clone()],
            1,
            1,
            StageJournal::create(&journal_path, "config-a").unwrap(),
            Arc::new(VerificationReviewer {
                review_calls: review_calls.clone(),
                adjudication_calls: adjudication_calls.clone(),
                fail_adjudication: true,
            }),
        )
        .await;
        assert!(first.unwrap_err().to_string().contains("paused"));
        assert_eq!(review_calls.load(Ordering::SeqCst), 1);
        assert_eq!(adjudication_calls.load(Ordering::SeqCst), 1);

        let (journal, snapshot) = StageJournal::resume(&journal_path, "config-a").unwrap();
        assert_eq!(snapshot.pending(), 1);
        assert_eq!(snapshot.rejected, 0);
        let resumed = run_admission(
            vec![row],
            1,
            1,
            journal,
            Arc::new(VerificationReviewer {
                review_calls: review_calls.clone(),
                adjudication_calls: adjudication_calls.clone(),
                fail_adjudication: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(resumed.snapshot.accepted, 1);
        assert_eq!(resumed.snapshot.pending(), 0);
        assert_eq!(review_calls.load(Ordering::SeqCst), 1);
        assert_eq!(adjudication_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn resume_materializes_saved_accept_without_adjudication_or_review_call() {
        #[derive(Clone)]
        struct NoCalls;
        #[async_trait::async_trait]
        impl PhaseBReviewer for NoCalls {
            async fn review(&self, _row: &SourceRow) -> Result<ReviewStageResult> {
                panic!("saved accept must not repeat review")
            }
            async fn adjudicate(
                &self,
                _row: &SourceRow,
                _review: &ReviewStageResult,
            ) -> Result<AdjudicationStageResult> {
                panic!("saved accept must not adjudicate")
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stage.journal.jsonl");
        let row = test_source_row(0);
        let mut journal = StageJournal::create(&path, "config-a").unwrap();
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::Admitted,
                json!({"task":row.task}),
            )
            .unwrap();
        journal
            .append(
                &row.task_id,
                0,
                JournalStage::ReviewCompleted,
                serde_json::to_value(test_review(&row, ReviewStageOutcome::Accept)).unwrap(),
            )
            .unwrap();
        let result = run_admission(vec![row], 1, 1, journal, Arc::new(NoCalls))
            .await
            .unwrap();
        assert_eq!(result.snapshot.accepted, 1);
        assert_eq!(result.snapshot.pending(), 0);
    }

    #[tokio::test]
    async fn resume_materializes_saved_and_deterministic_rejects_without_adjudication() {
        #[derive(Clone)]
        struct AcceptNext(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl PhaseBReviewer for AcceptNext {
            async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(test_review(row, ReviewStageOutcome::Accept))
            }
            async fn adjudicate(
                &self,
                _row: &SourceRow,
                _review: &ReviewStageResult,
            ) -> Result<AdjudicationStageResult> {
                panic!("saved/deterministic reject must not adjudicate")
            }
        }
        for saved_model_reject in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("stage.journal.jsonl");
            let mut rejected = test_source_row(0);
            if !saved_model_reject {
                rejected.deterministic_hard_failures = vec!["deterministic failure".into()];
            }
            let accepted = test_source_row(1);
            let mut journal = StageJournal::create(&path, "config-a").unwrap();
            journal
                .append(
                    &rejected.task_id,
                    0,
                    JournalStage::Admitted,
                    json!({"task":rejected.task}),
                )
                .unwrap();
            if saved_model_reject {
                journal
                    .append(
                        &rejected.task_id,
                        0,
                        JournalStage::ReviewCompleted,
                        serde_json::to_value(test_review(&rejected, ReviewStageOutcome::Reject))
                            .unwrap(),
                    )
                    .unwrap();
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let result = run_admission(
                vec![rejected, accepted],
                1,
                1,
                journal,
                Arc::new(AcceptNext(calls.clone())),
            )
            .await
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(result.snapshot.accepted, 1);
            assert_eq!(result.snapshot.rejected, 1);
        }
    }

    fn seal_fixture() -> (tempfile::TempDir, PreparedRun, JournalSnapshot) {
        let temporary = tempfile::tempdir().unwrap();
        let task = golden_task();
        let task_id = source_task_id(&task).unwrap();
        let source_jsonl = format!("{task}\n").into_bytes();
        let authority_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version":"scogo.data-factory.source-exclusion-authority.v1",
            "excluded_source_task_ids":[],
            "historical_import_reservation_sha256":null,
            "prior_completed_releases":[]
        }))
        .unwrap();
        let config = serde_json::json!({"run_id":"phase-b-seal-test"});
        let prepared = PreparedRun {
            run_id: "phase-b-seal-test".into(),
            target: 1,
            work_dir: temporary.path().join("work"),
            final_run_dir: temporary.path().join("final"),
            source_repo_id: "ScogoAI/netops-prompt-seed".into(),
            source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
            source_file: "part-3/tasks.jsonl".into(),
            source_selection: "unused-phase-b-test".into(),
            source: SourcePopulation {
                rows: vec![],
                tasks: vec![task.clone()],
                canonical_jsonl: source_jsonl,
                population_sha256: source_population_sha256(std::slice::from_ref(&task)).unwrap(),
                held: None,
            },
            eligible_rows: vec![SourceRow {
                source_index: 0,
                task_id: task_id.clone(),
                task,
                deterministic_hard_failures: Vec::new(),
            }],
            excluded_ids: vec![],
            exclusion_authority: EvidenceArtifact {
                logical_name: "exclusion_authority".into(),
                relative_file: "source_exclusion_authority.json".into(),
                held: None,
                sha256: sha256_bytes(&authority_bytes),
                bytes: authority_bytes,
            },
            historical_reservation: None,
            prior_releases: vec![],
            taxonomy_held: None,
            reference_snapshot: ReferenceSnapshot::capture(None).unwrap(),
            config_sha256: sha256_bytes(&serde_json::to_vec(&config).unwrap()),
            config,
        };
        std::fs::create_dir(&prepared.work_dir).unwrap();
        let mut journal = StageJournal::create(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        let row = prepared.eligible_rows[0].clone();
        journal
            .append(
                &task_id,
                0,
                JournalStage::Admitted,
                json!({"task":row.task}),
            )
            .unwrap();
        let completed = test_review(&row, ReviewStageOutcome::Accept);
        journal
            .append(
                &task_id,
                0,
                JournalStage::ReviewCompleted,
                serde_json::to_value(&completed).unwrap(),
            )
            .unwrap();
        let mut terminal = completed;
        terminal.record["final_disposition"] = json!("accepted");
        journal
            .append(
                &task_id,
                0,
                JournalStage::Accepted,
                json!({"review":terminal,"rejection":null}),
            )
            .unwrap();
        let snapshot = journal.snapshot.clone();
        (temporary, prepared, snapshot)
    }

    #[test]
    fn seal_fault_leaves_no_partial_final_run() {
        let (temporary, prepared, snapshot) = seal_fixture();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        let error = seal_run(
            &prepared,
            &snapshot,
            &mut journal,
            || bail!("injected seal fault"),
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected seal fault"));
        assert!(!prepared.final_run_dir.exists());
        assert!(std::fs::read_dir(temporary.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("sealing")
        }));
    }

    #[test]
    fn post_rename_resume_requires_and_uses_seal_prepared_anchor() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        let error = seal_run(
            &prepared,
            &snapshot,
            &mut journal,
            || Ok(()),
            || bail!("simulated crash after rename"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("simulated crash"));
        assert!(prepared.final_run_dir.exists());
        assert!(journal.snapshot.seal_prepared_manifest_sha256.is_some());
        assert!(journal.snapshot.sealed_manifest_sha256.is_none());

        finish_prepared_seal(&prepared, &mut journal).unwrap();
        assert_eq!(
            journal.snapshot.sealed_manifest_sha256,
            journal.snapshot.seal_prepared_manifest_sha256
        );
    }

    #[test]
    fn seal_never_overwrites_an_existing_final_directory() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        std::fs::create_dir(&prepared.final_run_dir).unwrap();
        std::fs::write(prepared.final_run_dir.join("owner-marker"), b"keep").unwrap();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();

        assert!(seal_run(&prepared, &snapshot, &mut journal, || Ok(()), || Ok(())).is_err());
        assert_eq!(
            std::fs::read(prepared.final_run_dir.join("owner-marker")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn sealed_run_verification_detects_every_artifact_tamper() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        let manifest_sha256 =
            seal_run(&prepared, &snapshot, &mut journal, || Ok(()), || Ok(())).unwrap();
        verify_sealed_run(&prepared, Some(&manifest_sha256)).unwrap();
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(prepared.final_run_dir.join("run.json")).unwrap(),
        )
        .unwrap();
        for descriptor in manifest["artifacts"].as_object().unwrap().values() {
            let file = descriptor["file"].as_str().unwrap();
            if file == "run.json" {
                continue;
            }
            let path = prepared.final_run_dir.join(file);
            let original = std::fs::read(&path).unwrap();
            std::fs::write(&path, b"tampered\n").unwrap();
            assert!(
                verify_sealed_run(&prepared, Some(&manifest_sha256)).is_err(),
                "tamper went undetected for {file}"
            );
            std::fs::write(path, original).unwrap();
        }
        let manifest_path = prepared.final_run_dir.join("run.json");
        let original_manifest = std::fs::read(&manifest_path).unwrap();
        std::fs::write(&manifest_path, b"{}\n").unwrap();
        assert!(verify_sealed_run(&prepared, Some(&manifest_sha256)).is_err());
        std::fs::write(&manifest_path, original_manifest).unwrap();
        assert_eq!(
            verify_sealed_run(&prepared, Some(&manifest_sha256)).unwrap(),
            manifest_sha256
        );
    }
}
