use std::collections::VecDeque;
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use futures::stream::{self, StreamExt};
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
}

#[derive(Debug)]
struct EvidenceArtifact {
    logical_name: String,
    relative_file: String,
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
    prior_evidence: Vec<EvidenceArtifact>,
    config: Value,
    config_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExclusionAuthority {
    schema_version: String,
    excluded_source_task_ids: Vec<String>,
    historical_import_reservation_sha256: Option<String>,
    prior_completed_releases: Vec<PriorEvidence>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorEvidence {
    #[serde(default = "current_evidence_mode")]
    evidence_mode: String,
    run_id: String,
    release_set_sha256: String,
    source_receipt_sha256: Option<String>,
    canonical_tasks_sha256: String,
    legacy_source_receipt_sha256: Option<String>,
    taskgen_run_sha256: Option<String>,
    taskgen_tasks_sha256: Option<String>,
    taskgen_reviews_sha256: Option<String>,
    selected_source_task_ids: Vec<String>,
}

fn current_evidence_mode() -> String {
    "current".to_string()
}

#[derive(Debug, Deserialize)]
struct HistoricalReservationView {
    schema_version: String,
    strategy: String,
    status: String,
    origin_run_ids: Vec<String>,
    selected_source_task_ids: Vec<String>,
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
    let bytes = std::fs::read(path)
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
        sha256: sha256_bytes(&bytes),
        bytes,
    })
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

fn reference_digest(root: Option<&Path>) -> Result<String> {
    fn visit(root: &Path, directory: &Path, rows: &mut Vec<Value>) -> Result<()> {
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
                visit(root, &path, rows)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                rows.push(json!({
                    "file":relative,
                    "sha256":sha256_bytes(&std::fs::read(&path)?),
                }));
            }
        }
        Ok(())
    }
    let mut rows = Vec::new();
    if let Some(root) = root {
        if !root.is_dir() {
            bail!(
                "Phase-B review reference path is not a directory: {}",
                root.display()
            );
        }
        visit(root, root, &mut rows)?;
    }
    Ok(sha256_bytes(&serde_json::to_vec(&rows)?))
}

fn prepare_run(
    args: &crate::ReviewArgs,
    taxonomy: &crate::taxonomy::TaxonomyCatalog,
    review_prompt: &str,
) -> Result<PreparedRun> {
    let target = args
        .accepted_target
        .context("Phase-B accepted target is required")?;
    let run_id = args.run_id.clone().context("Phase-B run ID is required")?;
    let work_dir = args
        .work_dir
        .clone()
        .context("Phase-B work dir is required")?;
    let final_run_dir = args
        .final_run_dir
        .clone()
        .context("Phase-B final run dir is required")?;
    if work_dir == final_run_dir {
        bail!("Phase-B work and final run directories must be separate");
    }
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
    let authority_path = args
        .source_exclusion_authority
        .as_deref()
        .context("Phase-B source exclusion authority is required")?;
    let exclusion_authority = read_evidence(
        authority_path,
        "source exclusion authority",
        "source_exclusion_authority.json".to_string(),
    )?;
    let authority: ExclusionAuthority = serde_json::from_slice(&exclusion_authority.bytes)
        .context("source exclusion authority is invalid JSON")?;
    if authority.schema_version != "scogo.data-factory.source-exclusion-authority.v1" {
        bail!("unsupported source exclusion authority schema");
    }
    let excluded_ids = authority.excluded_source_task_ids.clone();
    if excluded_ids.len() != excluded_ids.iter().collect::<HashSet<_>>().len() {
        bail!("source exclusion authority contains duplicate task IDs");
    }
    let population_ids = source
        .rows
        .iter()
        .map(|row| row.task_id.as_str())
        .collect::<HashSet<_>>();
    if excluded_ids
        .iter()
        .any(|task_id| !population_ids.contains(task_id.as_str()))
    {
        bail!("source exclusion authority contains IDs outside the source population");
    }
    let excluded_set = excluded_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let eligible_rows = source
        .rows
        .iter()
        .filter(|row| !excluded_set.contains(row.task_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if target > eligible_rows.len() {
        bail!(
            "Phase-B accepted target {target} exceeds {} eligible bounded source rows",
            eligible_rows.len()
        );
    }

    let mut expected_evidence = BTreeMap::<String, String>::new();
    let mut prior_ids = HashSet::new();
    let mut prior_run_ids = Vec::new();
    for evidence in &authority.prior_completed_releases {
        validate_component(&evidence.run_id, "prior completed release run ID")?;
        if !prior_run_ids.contains(&evidence.run_id) {
            prior_run_ids.push(evidence.run_id.clone());
        } else {
            bail!("source exclusion authority contains duplicate prior run IDs");
        }
        if evidence.selected_source_task_ids.len()
            != evidence
                .selected_source_task_ids
                .iter()
                .collect::<HashSet<_>>()
                .len()
        {
            bail!("prior completed release contains duplicate selected task IDs");
        }
        for task_id in &evidence.selected_source_task_ids {
            prior_ids.insert(task_id.clone());
        }
        let mut digests = vec![
            ("prior_release_set", Some(&evidence.release_set_sha256)),
            (
                "prior_canonical_tasks",
                Some(&evidence.canonical_tasks_sha256),
            ),
        ];
        match evidence.evidence_mode.as_str() {
            "current"
                if evidence.source_receipt_sha256.is_some()
                    && evidence.legacy_source_receipt_sha256.is_none()
                    && evidence.taskgen_run_sha256.is_none()
                    && evidence.taskgen_tasks_sha256.is_none()
                    && evidence.taskgen_reviews_sha256.is_none() =>
            {
                digests.push((
                    "prior_source_receipt",
                    evidence.source_receipt_sha256.as_ref(),
                ));
            }
            "pinned_external_legacy"
                if evidence.source_receipt_sha256.is_none()
                    && evidence.legacy_source_receipt_sha256.is_some()
                    && evidence.taskgen_run_sha256.is_some()
                    && evidence.taskgen_tasks_sha256.is_some()
                    && evidence.taskgen_reviews_sha256.is_some() =>
            {
                digests.extend([
                    (
                        "prior_legacy_source_receipt",
                        evidence.legacy_source_receipt_sha256.as_ref(),
                    ),
                    ("prior_taskgen_run", evidence.taskgen_run_sha256.as_ref()),
                    (
                        "prior_taskgen_tasks",
                        evidence.taskgen_tasks_sha256.as_ref(),
                    ),
                    (
                        "prior_taskgen_reviews",
                        evidence.taskgen_reviews_sha256.as_ref(),
                    ),
                ]);
            }
            mode => bail!("prior evidence mode {mode} has incomplete or conflicting digests"),
        }
        for (prefix, digest) in digests {
            let digest = digest.context("prior completed release is missing a required digest")?;
            expected_evidence.insert(format!("{prefix}.{}", evidence.run_id), digest.clone());
        }
    }
    if prior_ids != excluded_ids.iter().cloned().collect::<HashSet<_>>() {
        bail!("prior release evidence task IDs do not exactly authorize exclusions");
    }

    let mut supplied = BTreeMap::<String, PathBuf>::new();
    for mapping in &args.prior_evidence {
        let (name, path) = mapping
            .split_once('=')
            .with_context(|| format!("invalid --prior-evidence mapping {mapping:?}"))?;
        if supplied
            .insert(name.to_string(), PathBuf::from(path))
            .is_some()
        {
            bail!("duplicate --prior-evidence mapping for {name}");
        }
    }
    if supplied.keys().collect::<Vec<_>>() != expected_evidence.keys().collect::<Vec<_>>() {
        bail!("prior evidence mappings do not exactly match source exclusion authority");
    }
    let mut prior_evidence = Vec::new();
    for (name, expected_digest) in expected_evidence {
        let artifact = read_evidence(&supplied[&name], &name, format!("prior_evidence/{name}"))?;
        if artifact.sha256 != expected_digest {
            bail!("prior evidence digest mismatch for {name}");
        }
        prior_evidence.push(artifact);
    }

    let historical_reservation =
        match (&args.historical_import_reservation, excluded_ids.is_empty()) {
            (None, true) => None,
            (Some(_), true) => bail!("empty exclusions forbid historical reservation evidence"),
            (None, false) => bail!("source exclusions require historical reservation evidence"),
            (Some(path), false) => {
                let artifact = read_evidence(
                    path,
                    "historical import reservation",
                    "historical_import_reservation.json".to_string(),
                )?;
                let expected_digest = authority
                    .historical_import_reservation_sha256
                    .as_deref()
                    .context("source exclusion authority is missing reservation digest")?;
                if artifact.sha256 != expected_digest {
                    bail!("historical import reservation digest mismatch");
                }
                let reservation: HistoricalReservationView =
                    serde_json::from_slice(&artifact.bytes)
                        .context("historical import reservation is invalid JSON")?;
                if reservation.schema_version != "scogo.data-factory.task-reservation.v1"
                    || reservation.strategy != "historical_import"
                    || reservation.status != "completed"
                    || reservation.selected_source_task_ids != excluded_ids
                {
                    bail!("historical import reservation does not authorize exact exclusions");
                }
                let mut origins = reservation.origin_run_ids;
                origins.sort();
                prior_run_ids.sort();
                if origins != prior_run_ids {
                    bail!("historical import reservation origin runs do not match authority");
                }
                Some(artifact)
            }
        };
    if excluded_ids.is_empty() && authority.historical_import_reservation_sha256.is_some() {
        bail!("empty exclusions forbid historical reservation digest");
    }

    let taxonomy_bytes = std::fs::read(&args.taxonomy)
        .with_context(|| format!("failed to hash taxonomy: {}", args.taxonomy.display()))?;
    let prior_digests = prior_evidence
        .iter()
        .map(|artifact| {
            (
                artifact.logical_name.clone(),
                Value::String(artifact.sha256.clone()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let config = json!({
        "schema_version":"scogo.taskgen.phase-b-config.v1",
        "run_id":run_id,
        "accepted_target":target,
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
            "reference_sha256":reference_digest(args.review_reference_dir.as_deref())?,
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
        prior_evidence,
        config,
        config_sha256,
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn open_work(prepared: &PreparedRun, resume: bool) -> Result<StageJournal> {
    let config_path = prepared.work_dir.join("config.json");
    let journal_path = prepared.work_dir.join("stage.journal.jsonl");
    if resume {
        if !prepared.work_dir.is_dir() {
            bail!("Phase-B resume work directory does not exist");
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
        return StageJournal::resume(&journal_path, &prepared.config_sha256)
            .map(|(journal, _)| journal);
    }
    if prepared.work_dir.exists() || prepared.final_run_dir.exists() {
        bail!("Phase-B fresh run requires absent work and final directories");
    }
    let parent = prepared.work_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    std::fs::create_dir(&prepared.work_dir)?;
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
    sync_directory(&prepared.work_dir)?;
    StageJournal::create(&journal_path, &prepared.config_sha256)
}

fn load_source_population(
    path: &Path,
    taxonomy: &crate::taxonomy::TaxonomyCatalog,
) -> Result<SourcePopulation> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Phase-B source: {}", path.display()))?;
    let mut reader = BufReader::new(file);
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
    review: Option<Value>,
    terminal: Option<JournalStage>,
    terminal_payload: Option<Value>,
}

#[derive(Debug, Clone, Default)]
struct JournalSnapshot {
    rows: BTreeMap<String, JournalRowState>,
    accepted: usize,
    rejected: usize,
    sealed_manifest_sha256: Option<String>,
}

impl JournalSnapshot {
    fn apply(&mut self, body: &JournalEntryBody) -> Result<()> {
        if body.stage == JournalStage::Sealed {
            if body.task_id != "__run__" || self.sealed_manifest_sha256.is_some() {
                bail!("terminal journal conflict for sealed run");
            }
            let manifest = body
                .payload
                .get("manifest_sha256")
                .and_then(Value::as_str)
                .context("sealed journal event is missing manifest_sha256")?;
            self.sealed_manifest_sha256 = Some(manifest.to_string());
            return Ok(());
        }

        match body.stage {
            JournalStage::Admitted => {
                if self.rows.contains_key(&body.task_id) {
                    bail!("duplicate journal admission for {}", body.task_id);
                }
                self.rows.insert(
                    body.task_id.clone(),
                    JournalRowState {
                        source_index: body.source_index,
                        ..JournalRowState::default()
                    },
                );
            }
            JournalStage::ReviewCompleted => {
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
                row.review = Some(body.payload.clone());
            }
            JournalStage::Accepted | JournalStage::Rejected => {
                let row = self
                    .rows
                    .get_mut(&body.task_id)
                    .context("terminal event precedes admission")?;
                if row.source_index != body.source_index || row.terminal.is_some() {
                    bail!("terminal journal conflict for {}", body.task_id);
                }
                if body.stage == JournalStage::Accepted && row.review.is_none() {
                    bail!(
                        "journal acceptance without a completed review for {}",
                        body.task_id
                    );
                }
                if body.stage == JournalStage::Accepted
                    && body
                        .payload
                        .get("review_record")
                        .and_then(|value| value.get("final_disposition"))
                        .and_then(Value::as_str)
                        != Some("accepted")
                {
                    bail!("journal acceptance has no accepted review record");
                }
                row.terminal = Some(body.stage);
                row.terminal_payload = Some(body.payload.clone());
                if body.stage == JournalStage::Accepted {
                    self.accepted += 1;
                } else {
                    self.rejected += 1;
                }
            }
            JournalStage::ProviderPaused => {
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
            JournalStage::Sealed => unreachable!(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewStageResult {
    outcome: ReviewStageOutcome,
    record: Value,
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
        if source.source_index != state.source_index {
            bail!("journal source index conflicts for {task_id}");
        }
    }
    if journal.snapshot.accepted > target
        || journal.snapshot.accepted + journal.snapshot.pending() > target
    {
        bail!("journal violates accepted + pending <= target");
    }

    let mut pending = journal
        .snapshot
        .rows
        .iter()
        .filter(|(_, state)| state.terminal.is_none())
        .map(|(task_id, state)| {
            let row = rows_by_id[task_id].clone();
            let review = state
                .review
                .as_ref()
                .map(|payload| serde_json::from_value(payload.clone()))
                .transpose()?;
            Ok((row, review))
        })
        .collect::<Result<Vec<_>>>()?;
    pending.sort_by_key(|(row, _)| row.source_index);
    struct Queue {
        pending: VecDeque<(SourceRow, Option<ReviewStageResult>)>,
        unused: VecDeque<SourceRow>,
        journal: StageJournal,
    }
    let queue = Arc::new(Mutex::new(Queue {
        pending: VecDeque::from(pending),
        unused: rows
            .into_iter()
            .filter(|row| !journal.snapshot.rows.contains_key(&row.task_id))
            .collect(),
        journal,
    }));
    let accepted = Arc::new(AtomicUsize::new(
        queue.lock().unwrap().journal.snapshot.accepted,
    ));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_occupancy = Arc::new(AtomicUsize::new(accepted.load(Ordering::SeqCst)));
    let slots = target - accepted.load(Ordering::SeqCst);
    let results = stream::iter(0..slots)
        .map(|_| {
            let (queue, reviewer, accepted, in_flight, max_occupancy) = (
                queue.clone(), reviewer.clone(), accepted.clone(), in_flight.clone(), max_occupancy.clone(),
            );
            async move {
                loop {
                    let (row, saved_review) = loop {
                        let mut state = queue.lock().unwrap();
                        if let Some(work) = state.pending.pop_front() {
                            break work;
                        }
                        let row = state.unused.pop_front().context(
                            "Phase-B source population exhausted before exact acceptance",
                        )?;
                        state.journal.append(&row.task_id,row.source_index,JournalStage::Admitted,json!({}))?;
                        if row.deterministic_hard_failures.is_empty() {
                            break (row, None);
                        }
                        state.journal.append(&row.task_id,row.source_index,JournalStage::Rejected,json!({
                            "rejection":rejection_record(&row,"deterministic_validation",Some(&row.deterministic_hard_failures))
                        }))?;
                    };
                    in_flight.fetch_add(1, Ordering::SeqCst);
                    max_occupancy.fetch_max(
                        accepted.load(Ordering::SeqCst) + in_flight.load(Ordering::SeqCst),
                        Ordering::SeqCst,
                    );
                    let result = match saved_review.as_ref() {
                        Some(review) => reviewer.adjudicate(&row, review).await.map(|result| (None, result)),
                        None => reviewer.review(&row).await.map(|review| (Some(review), AdjudicationStageResult {
                            accepted:false, adjudication:Value::Null,
                        })),
                    };
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    let (review_result, adjudication) = match result {
                        Ok(value) => value,
                        Err(error) => {
                            let reason = error.to_string();
                            let mut state = queue.lock().unwrap();
                            state.journal.append(&row.task_id,row.source_index,JournalStage::ProviderPaused,json!({"reason":reason}))?;
                            bail!("Phase-B provider/transport exhaustion paused the run: {reason}");
                        }
                    };
                    let (mut review, newly_reviewed) = match (saved_review, review_result) {
                        (Some(review), None) => (review, false),
                        (None, Some(review)) => {
                            (review, true)
                        }
                        _ => bail!("Phase-B stage result mismatch"),
                    };
                    if newly_reviewed {
                        queue.lock().unwrap().journal.append(
                            &row.task_id,
                            row.source_index,
                            JournalStage::ReviewCompleted,
                            serde_json::to_value(&review)?,
                        )?;
                    }
                    if review.outcome == ReviewStageOutcome::NeedsVerification && adjudication.adjudication.is_null() {
                        in_flight.fetch_add(1, Ordering::SeqCst);
                        let adjudication = reviewer.adjudicate(&row, &review).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        let mut state = queue.lock().unwrap();
                        let adjudication = match adjudication {
                            Ok(value) => value,
                            Err(error) => {
                                let reason = error.to_string();
                                state.journal.append(&row.task_id,row.source_index,JournalStage::ProviderPaused,json!({"reason":reason}))?;
                                bail!("Phase-B provider/transport exhaustion paused the run: {reason}");
                            }
                        };
                        review.record["adjudication"] = adjudication.adjudication;
                        if finalize_row(&mut state.journal,&row,review,adjudication.accepted)? {
                            accepted.fetch_add(1,Ordering::SeqCst);
                            return Ok(());
                        }
                        continue;
                    }
                    let is_accepted = if review.outcome == ReviewStageOutcome::NeedsVerification {
                        review.record["adjudication"] = adjudication.adjudication;
                        adjudication.accepted
                    } else {
                        review.outcome == ReviewStageOutcome::Accept
                    };
                    if finalize_row(&mut queue.lock().unwrap().journal,&row,review,is_accepted)? {
                        accepted.fetch_add(1,Ordering::SeqCst);
                        return Ok(());
                    }
                }
            }
        })
        .buffer_unordered(workers)
        .collect::<Vec<Result<()>>>()
        .await;
    if let Some(error) = results.into_iter().find_map(Result::err) {
        return Err(error);
    }
    let state = Arc::try_unwrap(queue)
        .map_err(|_| anyhow::anyhow!("Phase-B queue still has owners"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("Phase-B queue mutex poisoned"))?;
    if state.journal.snapshot.accepted != target || state.journal.snapshot.pending() != 0 {
        bail!("Phase-B admission ended without an exact drained target");
    }
    Ok(AdmissionResult {
        admitted: state.journal.snapshot.rows.len(),
        snapshot: state.journal.snapshot,
        max_accepted_plus_in_flight: max_occupancy.load(Ordering::SeqCst),
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
        json!({"review_record":review.record,
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

fn seal_run<F>(
    prepared: &PreparedRun,
    snapshot: &JournalSnapshot,
    before_manifest: F,
) -> Result<String>
where
    F: FnOnce() -> Result<()>,
{
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
    let temporary = parent.join(format!(
        ".{final_name}.sealing-{}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
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
            if let Some(review) = payload.get("review_record") {
                reviews.push(review.clone());
            }
            if let Some(rejection) = payload.get("rejection").filter(|value| !value.is_null()) {
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
        for evidence in &prepared.prior_evidence {
            write_synced(&temporary.join(&evidence.relative_file), &evidence.bytes)?;
            artifacts.insert(
                evidence.logical_name.clone(),
                descriptor(&evidence.relative_file, &evidence.bytes),
            );
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
        atomic_rename_noreplace(&temporary, &prepared.final_run_dir)?;
        sync_directory(parent)?;
        Ok(sha256_bytes(&manifest_bytes))
    })();
    match result {
        Ok(digest) => Ok(digest),
        Err(error) => {
            if temporary.exists() {
                let _ = std::fs::remove_dir_all(&temporary);
                let _ = sync_directory(parent);
            }
            Err(error)
        }
    }
}

fn verify_sealed_run(prepared: &PreparedRun, expected_manifest: Option<&str>) -> Result<String> {
    if !prepared.final_run_dir.is_dir()
        || std::fs::symlink_metadata(&prepared.final_run_dir)?
            .file_type()
            .is_symlink()
    {
        bail!("Phase-B final run is not a regular directory");
    }
    let manifest_bytes = std::fs::read(prepared.final_run_dir.join("run.json"))?;
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
            .prior_evidence
            .iter()
            .map(|artifact| artifact.logical_name.clone()),
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
        let path = prepared.final_run_dir.join(relative);
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("artifact {name} is not a regular file");
        }
        if name == "run" {
            if file != "run.json" || value.as_object().is_none_or(|row| row.len() != 1) {
                bail!("run artifact descriptor must contain only run.json");
            }
            continue;
        }
        let bytes = std::fs::read(&path)?;
        if value.get("bytes").and_then(Value::as_u64) != Some(bytes.len() as u64)
            || value.get("sha256").and_then(Value::as_str) != Some(&sha256_bytes(&bytes))
        {
            bail!("artifact digest/size mismatch for {name}");
        }
    }
    fn collect_files(root: &Path, directory: &Path, files: &mut HashSet<String>) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("sealed Phase-B run contains a symlink");
            }
            if metadata.is_dir() {
                collect_files(root, &path, files)?;
            } else if metadata.is_file() {
                files.insert(
                    path.strip_prefix(root)?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        Ok(())
    }
    let mut actual_files = HashSet::new();
    collect_files(
        &prepared.final_run_dir,
        &prepared.final_run_dir,
        &mut actual_files,
    )?;
    if actual_files != declared_files {
        bail!("sealed Phase-B run contains undeclared files");
    }
    let tasks = std::fs::read_to_string(prepared.final_run_dir.join("tasks.jsonl"))?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let reviews = std::fs::read_to_string(prepared.final_run_dir.join("reviews.jsonl"))?
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
    let receipt: Value = serde_json::from_slice(&std::fs::read(
        prepared.final_run_dir.join("source_receipt.json"),
    )?)?;
    if receipt["selected_source_task_ids"] != json!(selected_ids)
        || receipt["excluded_source_task_ids"] != json!(prepared.excluded_ids)
        || receipt["subset_sha256"]
            != sha256_bytes(&std::fs::read(prepared.final_run_dir.join("tasks.jsonl"))?)
        || receipt["source_population_sha256"] != prepared.source.population_sha256
        || std::fs::read(prepared.final_run_dir.join("source_population.jsonl"))?
            != prepared.source.canonical_jsonl
    {
        bail!("sealed Phase-B receipt does not match source selection");
    }
    Ok(manifest_sha256)
}

pub(crate) async fn run(args: crate::ReviewArgs) -> Result<()> {
    let taxonomy = crate::taxonomy::TaxonomyCatalog::from_path(&args.taxonomy)?;
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
    let prepared = prepare_run(&args, &taxonomy, &system_prompt)?;
    let journal = open_work(&prepared, args.resume)?;
    if prepared.final_run_dir.exists() {
        if journal.snapshot.accepted != prepared.target || journal.snapshot.pending() != 0 {
            bail!("Phase-B final run exists but durable work is not drained");
        }
        let manifest_sha256 = verify_sealed_run(
            &prepared,
            journal.snapshot.sealed_manifest_sha256.as_deref(),
        )?;
        if journal.snapshot.sealed_manifest_sha256.is_none() {
            let mut journal = journal;
            journal.append(
                "__run__",
                0,
                JournalStage::Sealed,
                json!({"manifest_sha256":manifest_sha256}),
            )?;
        }
        println!(
            "Verified existing bounded Phase-B review run: {}",
            prepared.final_run_dir.display()
        );
        return Ok(());
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
        references: Arc::new(match args.review_reference_dir.as_deref() {
            Some(path) => crate::references::ReferenceStore::load(path)?,
            None => crate::references::ReferenceStore::empty(),
        }),
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
    let manifest_sha256 = seal_run(&prepared, &result.snapshot, || Ok(()))?;
    let (mut journal, _) = StageJournal::resume(
        &prepared.work_dir.join("stage.journal.jsonl"),
        &prepared.config_sha256,
    )?;
    journal.append(
        "__run__",
        0,
        JournalStage::Sealed,
        json!({"manifest_sha256":manifest_sha256}),
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
        let authority = temporary.path().join("authority.json");
        let work = temporary.path().join("work");
        let final_run = temporary.path().join("final");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        std::fs::write(
            &authority,
            serde_json::to_vec(&serde_json::json!({
                "schema_version":"scogo.data-factory.source-exclusion-authority.v1",
                "excluded_source_task_ids":[],
                "historical_import_reservation_sha256":null,
                "prior_completed_releases":[]
            }))
            .unwrap(),
        )
        .unwrap();
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
                "--source-exclusion-authority".into(),
                authority.display().to_string(),
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
        let first = prepare_run(&parse(false), &taxonomy, "review prompt").unwrap();
        let _journal = open_work(&first, false).unwrap();

        let mut changed = golden_task();
        changed["prompt"] = serde_json::json!("Changed source prompt");
        std::fs::write(&source, format!("{changed}\n")).unwrap();
        let changed = prepare_run(&parse(true), &taxonomy, "review prompt").unwrap();
        let error = open_work(&changed, true).unwrap_err();
        assert!(
            error.to_string().contains("immutable config changed"),
            "{error:#}"
        );
    }

    #[test]
    fn pinned_legacy_evidence_requires_all_six_exact_payloads() {
        let temporary = tempfile::tempdir().unwrap();
        let mut prior_task = golden_task();
        prior_task["prompt"] = json!("Previously accepted source prompt");
        let mut current_task = golden_task();
        current_task["prompt"] = json!("New unused source prompt");
        let prior_id = source_task_id(&prior_task).unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{prior_task}\n{current_task}\n")).unwrap();
        let evidence_names = [
            "prior_release_set.legacy-run",
            "prior_canonical_tasks.legacy-run",
            "prior_legacy_source_receipt.legacy-run",
            "prior_taskgen_run.legacy-run",
            "prior_taskgen_tasks.legacy-run",
            "prior_taskgen_reviews.legacy-run",
        ];
        let mut evidence_paths = Vec::new();
        let mut digests = BTreeMap::new();
        for (index, name) in evidence_names.iter().enumerate() {
            let path = temporary.path().join(format!("evidence-{index}"));
            let bytes = format!("{{\"evidence\":{index}}}\n").into_bytes();
            std::fs::write(&path, &bytes).unwrap();
            digests.insert(*name, sha256_bytes(&bytes));
            evidence_paths.push(path);
        }
        let historical = temporary.path().join("historical.json");
        let historical_bytes = serde_json::to_vec(&json!({
            "schema_version":"scogo.data-factory.task-reservation.v1",
            "strategy":"historical_import",
            "status":"completed",
            "origin_run_ids":["legacy-run"],
            "selected_source_task_ids":[prior_id]
        }))
        .unwrap();
        std::fs::write(&historical, &historical_bytes).unwrap();
        let authority = temporary.path().join("authority.json");
        std::fs::write(
            &authority,
            serde_json::to_vec(&json!({
                "schema_version":"scogo.data-factory.source-exclusion-authority.v1",
                "excluded_source_task_ids":[prior_id],
                "historical_import_reservation_sha256":sha256_bytes(&historical_bytes),
                "prior_completed_releases":[{
                    "evidence_mode":"pinned_external_legacy",
                    "run_id":"legacy-run",
                    "release_set_sha256":digests["prior_release_set.legacy-run"],
                    "canonical_tasks_sha256":digests["prior_canonical_tasks.legacy-run"],
                    "legacy_source_receipt_sha256":digests["prior_legacy_source_receipt.legacy-run"],
                    "taskgen_run_sha256":digests["prior_taskgen_run.legacy-run"],
                    "taskgen_tasks_sha256":digests["prior_taskgen_tasks.legacy-run"],
                    "taskgen_reviews_sha256":digests["prior_taskgen_reviews.legacy-run"],
                    "selected_source_task_ids":[prior_id]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
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
            "--source-exclusion-authority".into(),
            authority.display().to_string(),
            "--historical-import-reservation".into(),
            historical.display().to_string(),
        ];
        for (name, path) in evidence_names.iter().zip(&evidence_paths) {
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
        let prepared = prepare_run(&args, &taxonomy, "review prompt").unwrap();
        assert_eq!(prepared.prior_evidence.len(), 6);
        assert_eq!(prepared.eligible_rows.len(), 1);

        for path in evidence_paths {
            let original = std::fs::read(&path).unwrap();
            std::fs::write(&path, b"tampered\n").unwrap();
            assert!(prepare_run(&args, &taxonomy, "review prompt").is_err());
            std::fs::write(path, original).unwrap();
        }
    }

    #[test]
    fn journal_recovers_a_torn_tail_and_rejects_terminal_conflicts() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("stage.journal.jsonl");
        let mut journal = StageJournal::create(&path, "config-a").unwrap();
        journal
            .append("task_a", 0, JournalStage::Admitted, serde_json::json!({}))
            .unwrap();
        journal
            .append(
                "task_a",
                0,
                JournalStage::ReviewCompleted,
                serde_json::json!({
                    "outcome":"accept",
                    "record":{"schema_version":"scogo.taskgen.review-record.v3"}
                }),
            )
            .unwrap();
        journal
            .append(
                "task_a",
                0,
                JournalStage::Accepted,
                serde_json::json!({"review_record":{"final_disposition":"accepted"}}),
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
        let conflict = resumed.append("task_a", 0, JournalStage::Rejected, serde_json::json!({}));
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
        let mut journal = StageJournal::create(&path, "config-a").unwrap();
        journal
            .append("task_a", 0, JournalStage::Admitted, json!({}))
            .unwrap();
        let error = journal
            .append(
                "task_a",
                0,
                JournalStage::Accepted,
                json!({"review_record":{"final_disposition":"accepted"}}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("without a completed review"));
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
            Ok(ReviewStageResult {
                outcome,
                record: serde_json::json!({
                    "schema_version":"scogo.taskgen.review-record.v3",
                    "candidate_id":row.task_id,
                    "sequence":row.source_index + 1,
                    "final_disposition":"pending"
                }),
            })
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
        let rows = (0..6)
            .map(|source_index| SourceRow {
                source_index,
                task_id: format!("task_{source_index}"),
                task: serde_json::json!({"prompt":format!("prompt-{source_index}")}),
                deterministic_hard_failures: Vec::new(),
            })
            .collect::<Vec<_>>();
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
    struct VerificationReviewer {
        review_calls: Arc<AtomicUsize>,
        adjudication_calls: Arc<AtomicUsize>,
        fail_adjudication: bool,
    }

    #[async_trait::async_trait]
    impl PhaseBReviewer for VerificationReviewer {
        async fn review(&self, row: &SourceRow) -> Result<ReviewStageResult> {
            self.review_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ReviewStageResult {
                outcome: ReviewStageOutcome::NeedsVerification,
                record: serde_json::json!({
                    "schema_version":"scogo.taskgen.review-record.v3",
                    "candidate_id":row.task_id,
                    "decision":{"outcome":"needs_verification"},
                    "references":[],
                    "adjudication":null,
                    "final_disposition":"pending_verification"
                }),
            })
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
                adjudication: serde_json::json!({"outcome":"accept"}),
            })
        }
    }

    #[tokio::test]
    async fn interrupted_needs_verification_resumes_at_adjudication() {
        let temporary = tempfile::tempdir().unwrap();
        let journal_path = temporary.path().join("stage.journal.jsonl");
        let row = SourceRow {
            source_index: 0,
            task_id: "task_a".into(),
            task: serde_json::json!({"prompt":"verify me"}),
            deterministic_hard_failures: Vec::new(),
        };
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
                sha256: sha256_bytes(&authority_bytes),
                bytes: authority_bytes,
            },
            historical_reservation: None,
            prior_evidence: vec![],
            config_sha256: sha256_bytes(&serde_json::to_vec(&config).unwrap()),
            config,
        };
        std::fs::create_dir(&prepared.work_dir).unwrap();
        let mut journal = StageJournal::create(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        journal
            .append(&task_id, 0, JournalStage::Admitted, serde_json::json!({}))
            .unwrap();
        journal
            .append(
                &task_id,
                0,
                JournalStage::ReviewCompleted,
                serde_json::json!({
                    "outcome":"accept",
                    "record":{"schema_version":"scogo.taskgen.review-record.v3"}
                }),
            )
            .unwrap();
        journal
            .append(
                &task_id,
                0,
                JournalStage::Accepted,
                serde_json::json!({
                    "review_record":{
                        "schema_version":"scogo.taskgen.review-record.v3",
                        "candidate_id":task_id,
                        "final_disposition":"accepted"
                    }
                }),
            )
            .unwrap();
        let snapshot = journal.snapshot.clone();
        (temporary, prepared, snapshot)
    }

    #[test]
    fn seal_fault_leaves_no_partial_final_run() {
        let (temporary, prepared, snapshot) = seal_fixture();
        let error = seal_run(&prepared, &snapshot, || bail!("injected seal fault")).unwrap_err();
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
    fn seal_never_overwrites_an_existing_final_directory() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        std::fs::create_dir(&prepared.final_run_dir).unwrap();
        std::fs::write(prepared.final_run_dir.join("owner-marker"), b"keep").unwrap();

        assert!(seal_run(&prepared, &snapshot, || Ok(())).is_err());
        assert_eq!(
            std::fs::read(prepared.final_run_dir.join("owner-marker")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn sealed_run_verification_detects_every_artifact_tamper() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let manifest_sha256 = seal_run(&prepared, &snapshot, || Ok(())).unwrap();
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
