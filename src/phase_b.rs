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
    raw_jsonl: Vec<u8>,
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
    source_selection: String,
    source: SourcePopulation,
    eligible_rows: Vec<SourceRow>,
    source_plan: SourcePlan,
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
        self.source_plan.assert_current()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorCompletedRelease {
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceAuthority {
    schema_version: String,
    authority_id: String,
    repo_id: String,
    repo_type: String,
    private: bool,
    revision: String,
    source_file: String,
    source_file_rows: usize,
    source_file_sha256: String,
    source_population_sha256: String,
    excluded_source_task_ids: Vec<String>,
    prior_completed_releases: Vec<PriorCompletedRelease>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalReceipt {
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

#[derive(Debug)]
struct SourcePlan {
    path: PathBuf,
    directory: HeldDirectory,
    nested_directories: Vec<HeldDirectory>,
    authority: SourceAuthority,
    authority_artifact: EvidenceArtifact,
    artifacts: BTreeMap<String, EvidenceArtifact>,
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

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn prior_files(prior: &PriorCompletedRelease) -> Result<Vec<(String, String, String)>> {
    validate_component(&prior.run_id, "source-plan prior run ID")?;
    let mut files = vec![
        (
            format!("prior_release_set.{}", prior.run_id),
            format!("prior-releases/{}/release_set.json", prior.run_id),
            prior.release_set_sha256.clone(),
        ),
        (
            format!("prior_canonical_tasks.{}", prior.run_id),
            format!("prior-releases/{}/canonical_tasks.jsonl", prior.run_id),
            prior.canonical_tasks_sha256.clone(),
        ),
    ];
    match prior.evidence_mode.as_str() {
        "current"
            if prior.source_receipt_sha256.is_some()
                && prior.legacy_source_receipt_sha256.is_none()
                && prior.taskgen_run_sha256.is_none()
                && prior.taskgen_tasks_sha256.is_none()
                && prior.taskgen_reviews_sha256.is_none() =>
        {
            files.push((
                format!("prior_source_receipt.{}", prior.run_id),
                format!("prior-releases/{}/source_receipt.json", prior.run_id),
                prior.source_receipt_sha256.clone().unwrap_or_default(),
            ));
        }
        "pinned_external_legacy"
            if prior.source_receipt_sha256.is_none()
                && prior.legacy_source_receipt_sha256.is_some()
                && prior.taskgen_run_sha256.is_some()
                && prior.taskgen_tasks_sha256.is_some()
                && prior.taskgen_reviews_sha256.is_some() =>
        {
            for (prefix, filename, digest) in [
                (
                    "prior_legacy_source_receipt",
                    "legacy_source_receipt.json",
                    prior.legacy_source_receipt_sha256.as_ref(),
                ),
                (
                    "prior_taskgen_run",
                    "taskgen_run.json",
                    prior.taskgen_run_sha256.as_ref(),
                ),
                (
                    "prior_taskgen_tasks",
                    "taskgen_tasks.jsonl",
                    prior.taskgen_tasks_sha256.as_ref(),
                ),
                (
                    "prior_taskgen_reviews",
                    "taskgen_reviews.jsonl",
                    prior.taskgen_reviews_sha256.as_ref(),
                ),
            ] {
                files.push((
                    format!("{prefix}.{}", prior.run_id),
                    format!("prior-releases/{}/{filename}", prior.run_id),
                    digest.cloned().unwrap_or_default(),
                ));
            }
        }
        _ => bail!("source-plan prior evidence mode is incomplete"),
    }
    if files.iter().any(|(_, _, digest)| !valid_sha256(digest)) {
        bail!("source-plan prior evidence contains an invalid digest");
    }
    if prior.selected_source_task_ids.is_empty()
        || prior.selected_source_task_ids
            != prior
                .selected_source_task_ids
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
    {
        bail!("source-plan prior selected task IDs must be sorted and unique");
    }
    Ok(files)
}

fn hold_plan_tree(
    root: &Path,
    directory: &Path,
    files: &mut HashSet<String>,
    directories: &mut HashSet<String>,
    held: &mut Vec<HeldDirectory>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("source-plan tree contains a symlink");
        }
        if metadata.is_dir() {
            directories.insert(
                path.strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            held.push(HeldDirectory::capture(&path)?);
            hold_plan_tree(root, &path, files, directories, held)?;
        } else if metadata.is_file() {
            files.insert(
                path.strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        } else {
            bail!("source-plan tree contains a non-file entry");
        }
    }
    Ok(())
}

impl SourcePlan {
    fn load(args: &crate::ReviewArgs, source: &SourcePopulation, target: usize) -> Result<Self> {
        let requested_path = args
            .source_plan_dir
            .as_deref()
            .context("Phase-B source-plan dir is required")?;
        let requested_entry = requested_path.components().collect::<PathBuf>();
        if std::fs::symlink_metadata(&requested_entry)?
            .file_type()
            .is_symlink()
        {
            bail!("Phase-B source-plan directory must not be a symlink");
        }
        let path = canonical_target(requested_path)?;
        let pin = args
            .source_plan_sha256
            .as_deref()
            .context("Phase-B source-plan SHA-256 is required")?;
        if !valid_sha256(pin) {
            bail!("Phase-B source-plan SHA-256 must be lowercase hex64");
        }
        let directory = HeldDirectory::capture(&path)?;
        let (authority_file, authority_bytes) = directory.read_file(
            Path::new("source_exclusion_authority.json"),
            16 * 1024 * 1024,
        )?;
        if sha256_bytes(&authority_bytes) != pin || contains_credential(&authority_bytes) {
            bail!("source-plan authority pin or credential scan failed");
        }
        let raw: Value = serde_json::from_slice(&authority_bytes)?;
        let mut canonical_authority = serde_json::to_vec(&raw)?;
        canonical_authority.push(b'\n');
        if canonical_authority != authority_bytes {
            bail!("source-plan authority is not exact canonical JSON");
        }
        let authority: SourceAuthority = serde_json::from_value(raw.clone())?;
        let mut identity = raw
            .as_object()
            .context("source-plan authority must be an object")?
            .clone();
        let claimed_id = identity.remove("authority_id");
        let derived_id = format!(
            "authority_{}",
            sha256_bytes(&serde_json::to_vec(&identity)?)
        );
        if authority.schema_version != "scogo.data-factory.source-exclusion-authority.v2"
            || claimed_id.as_ref().and_then(Value::as_str) != Some(&derived_id)
            || authority.authority_id != derived_id
            || authority.repo_type != "dataset"
            || !authority.private
            || authority.source_file_rows != source.rows.len()
            || authority.source_file_sha256 != sha256_bytes(&source.raw_jsonl)
            || authority.source_population_sha256 != source.population_sha256
        {
            bail!("source-plan authority does not match exact raw source");
        }
        validate_source_metadata(
            args.run_id.as_deref().unwrap_or_default(),
            &authority.repo_id,
            &authority.revision,
            &authority.source_file,
            args.source_selection.as_deref().unwrap_or_default(),
        )?;
        let source_ids = source
            .rows
            .iter()
            .map(|row| row.task_id.as_str())
            .collect::<HashSet<_>>();
        if authority.excluded_source_task_ids
            != authority
                .excluded_source_task_ids
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
            || authority
                .excluded_source_task_ids
                .iter()
                .any(|task_id| !source_ids.contains(task_id.as_str()))
            || target > source.rows.len() - authority.excluded_source_task_ids.len()
        {
            bail!("source-plan exclusions or accepted target are invalid");
        }
        let prior_run_ids = authority
            .prior_completed_releases
            .iter()
            .map(|prior| prior.run_id.as_str())
            .collect::<Vec<_>>();
        if prior_run_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            bail!("source-plan prior releases must be sorted and unique");
        }
        let mut prior_union = HashSet::new();
        let mut expected_files = HashSet::from(["source_exclusion_authority.json".to_string()]);
        let mut expected_directories = HashSet::new();
        let mut specifications = Vec::new();
        for prior in &authority.prior_completed_releases {
            expected_directories.insert("prior-releases".to_string());
            expected_directories.insert(format!("prior-releases/{}", prior.run_id));
            for task_id in &prior.selected_source_task_ids {
                if !prior_union.insert(task_id.clone()) {
                    bail!("source-plan prior release selections overlap");
                }
            }
            for specification in prior_files(prior)? {
                expected_files.insert(specification.1.clone());
                specifications.push(specification);
            }
        }
        if prior_union
            != authority
                .excluded_source_task_ids
                .iter()
                .cloned()
                .collect::<HashSet<_>>()
        {
            bail!("source-plan prior union does not match exclusions");
        }
        let mut actual_files = HashSet::new();
        let mut actual_directories = HashSet::new();
        let mut nested_directories = Vec::new();
        hold_plan_tree(
            &path,
            &path,
            &mut actual_files,
            &mut actual_directories,
            &mut nested_directories,
        )?;
        if actual_files != expected_files || actual_directories != expected_directories {
            bail!("source-plan tree inventory differs from authority");
        }
        let mut artifacts = BTreeMap::new();
        for (logical_name, relative_file, digest) in specifications {
            let (held, bytes) =
                directory.read_file(Path::new(&relative_file), 1024 * 1024 * 1024)?;
            if sha256_bytes(&bytes) != digest {
                bail!("source-plan opaque prior artifact digest mismatch");
            }
            artifacts.insert(
                logical_name.clone(),
                EvidenceArtifact {
                    logical_name,
                    relative_file,
                    held: Some(held),
                    sha256: digest,
                    bytes,
                },
            );
        }
        let plan = Self {
            path,
            directory,
            nested_directories,
            authority,
            authority_artifact: EvidenceArtifact {
                logical_name: "exclusion_authority".into(),
                relative_file: "source_exclusion_authority.json".into(),
                held: Some(authority_file),
                sha256: pin.to_string(),
                bytes: authority_bytes,
            },
            artifacts,
        };
        plan.assert_current()?;
        Ok(plan)
    }

    fn assert_current(&self) -> Result<()> {
        self.directory.assert_current()?;
        for directory in &self.nested_directories {
            directory.assert_current()?;
        }
        self.authority_artifact
            .held
            .as_ref()
            .context("source-plan authority lost held file")?
            .assert_current()?;
        for artifact in self.artifacts.values() {
            artifact
                .held
                .as_ref()
                .context("source-plan prior artifact lost held file")?
                .assert_current()?;
        }
        Ok(())
    }
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
    if repo_id.trim() != repo_id
        || repo_parts.len() != 2
        || repo_parts
            .iter()
            .any(|part| part.is_empty() || part.trim() != *part)
    {
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
    let components = path.components().collect::<Vec<_>>();
    let normalized = components
        .iter()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    if source_file.trim() != source_file
        || source_file.contains('\\')
        || normalized.is_empty()
        || normalized.iter().any(|part| part.trim() != *part)
        || normalized.join("/") != source_file
        || path.is_absolute()
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
    validate_path_isolation(
        &work,
        &final_run,
        [args.input.clone(), args.taxonomy.clone()]
            .into_iter()
            .chain(args.review_reference_dir.iter().cloned())
            .chain(args.source_plan_dir.iter().cloned()),
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
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
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
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("held input path is not a regular file");
    }
    Ok(File::open(path)?)
}

fn require_regular_file(file: &File) -> Result<()> {
    if !file.metadata()?.is_file() {
        bail!("held input is not a regular file");
    }
    Ok(())
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
        require_regular_file(&file)?;
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
        require_regular_file(&self.file)?;
        let (held_identity, held_links) = file_identity(&self.file)?;
        let reopened = open_nofollow(&self.path)?;
        require_regular_file(&reopened)?;
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
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        let file = File::from(fd);
        require_regular_file(&file)?;
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

    #[cfg(unix)]
    fn inventory(&self) -> Result<(HashSet<String>, HashSet<String>, Vec<HeldDirectory>)> {
        fn visit(
            directory: &File,
            root: &Path,
            relative: &Path,
            files: &mut HashSet<String>,
            directories: &mut HashSet<String>,
            held: &mut Vec<HeldDirectory>,
        ) -> Result<()> {
            let mut entries = rustix::fs::Dir::read_from(directory)?;
            while let Some(entry) = entries.read() {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                use std::os::unix::ffi::OsStrExt;
                let child_relative = relative.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
                let child_name = child_relative
                    .to_str()
                    .context("held directory contains a non-UTF-8 entry")?
                    .replace('\\', "/");
                let stat =
                    rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
                match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                    rustix::fs::FileType::RegularFile => {
                        files.insert(child_name);
                    }
                    rustix::fs::FileType::Directory => {
                        directories.insert(child_name);
                        let fd = rustix::fs::openat(
                            directory,
                            name,
                            rustix::fs::OFlags::RDONLY
                                | rustix::fs::OFlags::DIRECTORY
                                | rustix::fs::OFlags::CLOEXEC
                                | rustix::fs::OFlags::NOFOLLOW,
                            rustix::fs::Mode::empty(),
                        )?;
                        let file = File::from(fd);
                        let (identity, _) = file_identity(&file)?;
                        let child = HeldDirectory {
                            path: root.join(&child_relative),
                            file,
                            identity,
                        };
                        visit(&child.file, root, &child_relative, files, directories, held)?;
                        held.push(child);
                    }
                    _ => bail!("held directory tree contains a non-regular entry"),
                }
            }
            Ok(())
        }

        let mut files = HashSet::new();
        let mut directories = HashSet::new();
        let mut held = Vec::new();
        visit(
            &self.file,
            &self.path,
            Path::new(""),
            &mut files,
            &mut directories,
            &mut held,
        )?;
        Ok((files, directories, held))
    }

    #[cfg(not(unix))]
    fn inventory(&self) -> Result<(HashSet<String>, HashSet<String>, Vec<HeldDirectory>)> {
        let mut files = HashSet::new();
        let mut directories = HashSet::new();
        let mut held = Vec::new();
        hold_plan_tree(
            &self.path,
            &self.path,
            &mut files,
            &mut directories,
            &mut held,
        )?;
        Ok((files, directories, held))
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
    let review_api_base = crate::provider::normalize_api_base(&args.api_base)?;
    let adjudication_api_base = match args.adjudication_api_base.as_deref() {
        Some(value) => crate::provider::normalize_api_base(value)?,
        None => review_api_base.clone(),
    };
    let target = args
        .accepted_target
        .context("Phase-B accepted target is required")?;
    let run_id = args.run_id.clone().context("Phase-B run ID is required")?;
    let (work_dir, final_run_dir) = isolated_run_paths(args)?;
    let source_selection = args
        .source_selection
        .clone()
        .context("Phase-B source selection is required")?;
    let source = load_source_population(&args.input, taxonomy)?;
    let source_plan = SourcePlan::load(args, &source, target)?;
    let excluded = source_plan
        .authority
        .excluded_source_task_ids
        .iter()
        .collect::<HashSet<_>>();
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
    let prior_digests = source_plan
        .artifacts
        .values()
        .map(|artifact| (artifact.logical_name.clone(), json!(artifact.sha256)))
        .collect::<serde_json::Map<_, _>>();
    let config = json!({
        "schema_version":"scogo.taskgen.phase-b-config.v1",
        "run_id":run_id,
        "accepted_target":target,
        "paths":{"work_dir":work_dir,"final_run_dir":final_run_dir,"source_plan_dir":source_plan.path},
        "source":{
            "repo_id":source_plan.authority.repo_id,
            "repo_type":"dataset",
            "private":true,
            "revision":source_plan.authority.revision,
            "source_file":source_plan.authority.source_file,
            "selection":source_selection,
            "rows":source.tasks.len(),
            "source_file_sha256":sha256_bytes(&source.raw_jsonl),
            "source_population_sha256":source.population_sha256,
        },
        "evidence":{
            "source_plan_sha256":source_plan.authority_artifact.sha256,
            "authority_id":source_plan.authority.authority_id,
            "prior":prior_digests,
        },
        "taxonomy":{
            "id":taxonomy.id(),
            "sha256":sha256_bytes(&taxonomy_bytes),
        },
        "review":{
            "model":args.model,
            "endpoint":crate::safe_api_base(&review_api_base),
            "prompt_sha256":sha256_bytes(review_prompt.as_bytes()),
            "max_output_tokens":args.max_output_tokens,
            "workers":args.review_workers,
            "requests_per_minute":args.review_requests_per_minute,
            "reference_sha256":reference_snapshot.digest,
        },
        "adjudication":{
            "model":args.adjudication_model.as_deref().unwrap_or(&args.model),
            "endpoint":crate::safe_api_base(&adjudication_api_base),
        }
    });
    let config_sha256 = sha256_bytes(&serde_json::to_vec(&config)?);
    Ok(PreparedRun {
        run_id,
        target,
        work_dir,
        final_run_dir,
        source_selection,
        source,
        eligible_rows,
        source_plan,
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

fn path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sibling_work_directory(prepared: &PreparedRun, state: &str) -> Result<PathBuf> {
    let parent = prepared
        .work_dir
        .parent()
        .context("Phase-B work path has no parent")?;
    let name = prepared
        .work_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("Phase-B work path needs a UTF-8 name")?;
    Ok(parent.join(format!(".{name}.{state}-{}", &prepared.config_sha256[..16])))
}

fn remove_unanchored_directory(path: &Path, parent: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("unanchored Phase-B path is not a directory");
    }
    HeldDirectory::capture(path)?.assert_current()?;
    std::fs::remove_dir_all(path)?;
    sync_directory(parent)?;
    Ok(())
}

fn initialize_work_files(prepared: &PreparedRun) -> Result<StageJournal> {
    let parent = prepared.work_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let initializing = sibling_work_directory(prepared, "initializing")?;
    remove_unanchored_directory(&initializing, parent)?;
    std::fs::create_dir(&initializing)?;
    sync_directory(parent)?;
    let result = (|| -> Result<StageJournal> {
        let mut config_bytes = serde_json::to_vec_pretty(&json!({
            "schema_version":"scogo.taskgen.phase-b-work.v1",
            "config_sha256":prepared.config_sha256,
            "config":prepared.config,
        }))?;
        config_bytes.push(b'\n');
        write_synced(&initializing.join("config.json"), &config_bytes)?;
        let journal = StageJournal::create(
            &initializing.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )?;
        sync_directory(&initializing)?;
        sync_directory(parent)?;
        atomic_rename_noreplace(&initializing, &prepared.work_dir)?;
        sync_directory(parent)?;
        Ok(journal)
    })();
    if result.is_err() && initializing.exists() {
        let _ = remove_unanchored_directory(&initializing, parent);
    }
    result
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
            if path_entry_exists(&sibling_work_directory(prepared, "initializing")?)? {
                return initialize_work_files(prepared);
            }
            bail!("Phase-B resume work directory does not exist");
        }
        if !config_path.exists() {
            if std::fs::read_dir(&prepared.work_dir)?.next().is_none() {
                std::fs::remove_dir(&prepared.work_dir)?;
                sync_directory(prepared.work_dir.parent().unwrap_or_else(|| Path::new(".")))?;
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
    if path_entry_exists(&prepared.work_dir)? || path_entry_exists(&prepared.final_run_dir)? {
        bail!("Phase-B fresh run requires absent work and final directories");
    }
    let parent = prepared.work_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
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
        let entry: crate::TaskEntry = serde_json::from_value(value.clone())?;
        taxonomy.validate_task_coordinates(
            &entry.category,
            &entry.domain,
            &entry.subdomain,
            entry
                .coordinates
                .as_ref()
                .context("Phase-B source row is missing coordinates")?,
        )?;
        let task = value;
        let task_id = source_task_id(&task)?;
        if !task_ids.insert(task_id.clone()) {
            bail!("Phase-B source contains duplicate task ID {task_id}");
        }
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
        raw_jsonl: source_bytes,
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
    if path_entry_exists(&prepared.final_run_dir)? {
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
    if journal.snapshot.seal_prepared_manifest_sha256.is_some() {
        bail!("Phase-B seal is already anchored");
    }
    remove_unanchored_directory(&temporary, parent)?;
    std::fs::create_dir(&temporary)?;
    sync_directory(parent)?;
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
                prepared.source.raw_jsonl.as_slice(),
            ),
            (
                "exclusion_authority",
                prepared
                    .source_plan
                    .authority_artifact
                    .relative_file
                    .as_str(),
                prepared.source_plan.authority_artifact.bytes.as_slice(),
            ),
        ] {
            write_synced(&temporary.join(file), bytes)?;
            artifacts.insert(name.to_string(), descriptor(file, bytes));
        }
        for evidence in prepared.source_plan.artifacts.values() {
            write_synced(&temporary.join(&evidence.relative_file), &evidence.bytes)?;
            artifacts.insert(
                evidence.logical_name.clone(),
                descriptor(&evidence.relative_file, &evidence.bytes),
            );
        }
        let receipt = json!({
            "schema_version":"scogo.private-hf-subset-receipt.v1",
            "repo_id":prepared.source_plan.authority.repo_id,
            "repo_type":"dataset",
            "private":true,
            "revision":prepared.source_plan.authority.revision,
            "source_file":prepared.source_plan.authority.source_file,
            "selection":prepared.source_selection,
            "rows":prepared.target,
            "subset_sha256":sha256_bytes(&tasks_bytes),
            "selected_source_task_ids":selected_ids,
            "source_file_rows":prepared.source.rows.len(),
            "source_file_sha256":sha256_bytes(&prepared.source.raw_jsonl),
            "source_population_sha256":prepared.source.population_sha256,
            "excluded_source_task_ids":prepared.source_plan.authority.excluded_source_task_ids,
            "exclusion_authority_sha256":prepared.source_plan.authority_artifact.sha256,
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
        sync_directory(parent)?;
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
    expected_names.extend(prepared.source_plan.artifacts.keys().cloned());
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
    let mut declared_directories = HashSet::new();
    for file in &declared_files {
        let mut parent = Path::new(file).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            declared_directories.insert(path.to_string_lossy().replace('\\', "/"));
            parent = path.parent();
        }
    }
    let (actual_files, actual_directories, held_directories) = directory.inventory()?;
    if actual_files != declared_files || actual_directories != declared_directories {
        bail!("sealed Phase-B directory tree differs from its manifest");
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
    let receipt: FinalReceipt = serde_json::from_slice(&payloads["source_receipt"])?;
    if receipt.schema_version != "scogo.private-hf-subset-receipt.v1"
        || receipt.repo_id != prepared.source_plan.authority.repo_id
        || receipt.repo_type != "dataset"
        || !receipt.private
        || receipt.revision != prepared.source_plan.authority.revision
        || receipt.source_file != prepared.source_plan.authority.source_file
        || receipt.selection != prepared.source_selection
        || receipt.rows != prepared.target
        || receipt.selected_source_task_ids != selected_ids
        || receipt.excluded_source_task_ids
            != prepared.source_plan.authority.excluded_source_task_ids
        || receipt.subset_sha256 != sha256_bytes(&payloads["tasks"])
        || receipt.source_population_sha256 != prepared.source.population_sha256
        || receipt.source_file_rows != prepared.source.rows.len()
        || receipt.source_file_sha256 != sha256_bytes(&prepared.source.raw_jsonl)
        || receipt.exclusion_authority_sha256 != prepared.source_plan.authority_artifact.sha256
        || payloads["source_population"] != prepared.source.raw_jsonl
        || payloads["exclusion_authority"] != prepared.source_plan.authority_artifact.bytes
    {
        bail!("sealed Phase-B receipt does not match source selection");
    }
    for artifact in prepared.source_plan.artifacts.values() {
        if payloads.get(&artifact.logical_name) != Some(&artifact.bytes) {
            bail!("sealed Phase-B prior evidence differs from held source-plan bytes");
        }
    }
    for held in &held_files {
        held.assert_current()?;
    }
    for held in &held_directories {
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
    if path_entry_exists(&prepared.final_run_dir)? {
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
    if args.preflight_only {
        if path_entry_exists(&prepared.work_dir)? || path_entry_exists(&prepared.final_run_dir)? {
            bail!("Phase-B fresh preflight requires absent work and final destinations");
        }
        println!(
            "{}",
            serde_json::to_string(&json!({
                "schema_version":"scogo.taskgen.phase-b-preflight.v1",
                "status":"ready",
                "authority_id":prepared.source_plan.authority.authority_id,
                "source_plan_sha256":prepared.source_plan.authority_artifact.sha256,
                "source_rows":prepared.source.rows.len(),
                "excluded_rows":prepared.source_plan.authority.excluded_source_task_ids.len(),
                "eligible_rows":prepared.eligible_rows.len(),
                "accepted_target":prepared.target,
            }))?
        );
        return Ok(());
    }
    let _work_lock = WorkLock::acquire(&work_dir)?;
    let mut journal = open_work(&prepared, args.resume)?;
    if journal.snapshot.seal_prepared_manifest_sha256.is_some() {
        finish_prepared_seal(&prepared, &mut journal)?;
        println!(
            "Verified existing bounded Phase-B review run: {}",
            prepared.final_run_dir.display()
        );
        return Ok(());
    }
    if path_entry_exists(&prepared.final_run_dir)? {
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

    fn write_empty_plan(root: &Path, source: &Path) -> (PathBuf, String) {
        let source_bytes = std::fs::read(source).unwrap();
        let tasks = source_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect::<Vec<Value>>();
        let body = json!({
            "schema_version":"scogo.data-factory.source-exclusion-authority.v2",
            "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
            "revision":"0123456789abcdef0123456789abcdef01234567",
            "source_file":"part-3/tasks.jsonl","source_file_rows":tasks.len(),
            "source_file_sha256":sha256_bytes(&source_bytes),
            "source_population_sha256":source_population_sha256(&tasks).unwrap(),
            "excluded_source_task_ids":[],"prior_completed_releases":[]
        });
        let mut authority = body.as_object().unwrap().clone();
        authority.insert(
            "authority_id".into(),
            json!(format!(
                "authority_{}",
                sha256_bytes(&serde_json::to_vec(&body).unwrap())
            )),
        );
        let mut bytes = serde_json::to_vec(&authority).unwrap();
        bytes.push(b'\n');
        let plan = root.join("plan");
        std::fs::create_dir(&plan).unwrap();
        std::fs::write(plan.join("source_exclusion_authority.json"), &bytes).unwrap();
        (plan, sha256_bytes(&bytes))
    }

    fn write_current_compatible_plan(root: &Path) -> (PathBuf, PathBuf, String) {
        let mut prior = golden_task();
        prior["prompt"] = json!("prior compatible current-plan task");
        let mut unused = golden_task();
        unused["prompt"] = json!("unused compatible current-plan task");
        let prior_id = source_task_id(&prior).unwrap();
        let source = root.join("current-source.jsonl");
        let source_bytes = jsonl([prior.clone(), unused]).unwrap();
        std::fs::write(&source, &source_bytes).unwrap();
        let mut review = test_review(
            &SourceRow {
                source_index: 0,
                task_id: prior_id.clone(),
                task: prior.clone(),
                deterministic_hard_failures: vec![],
            },
            ReviewStageOutcome::Accept,
        );
        review.record["final_disposition"] = json!("accepted");
        let canonical = json!({
            "schema_version":"scogo.data-factory.source-task.v1","source_task_id":prior_id,
            "split_group_id":prior_id,"split":"train","prompt":prior["prompt"],
            "domain":prior["domain"],"subdomain":prior["subdomain"],
            "difficulty":prior["difficulty"].to_string(),"coordinates":prior["coordinates"],
            "source_schema_version":"scogo.taskgen.task.v2","source_task":prior,
            "source_review":review.record
        });
        let canonical_bytes = jsonl([canonical]).unwrap();
        let selected_bytes = jsonl([prior.clone()]).unwrap();
        let plan = root.join("current-plan");
        let prior_dir = plan.join("prior-releases/current-release");
        std::fs::create_dir_all(&prior_dir).unwrap();
        std::fs::write(prior_dir.join("canonical_tasks.jsonl"), &canonical_bytes).unwrap();
        let receipt = json!({
            "schema_version":"scogo.private-hf-subset-receipt.v1",
            "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
            "revision":"0123456789abcdef0123456789abcdef01234567",
            "source_file":"part-3/tasks.jsonl","selection":"prior-current","rows":1,
            "subset_sha256":sha256_bytes(&selected_bytes),"selected_source_task_ids":[prior_id],
            "source_file_rows":2,"source_file_sha256":sha256_bytes(&source_bytes),
            "source_population_sha256":source_population_sha256(&[prior.clone(),
                serde_json::from_slice(source_bytes.split(|byte| *byte == b'\n').nth(1).unwrap()).unwrap()]).unwrap(),
            "excluded_source_task_ids":[],"exclusion_authority_sha256":"e".repeat(64)
        });
        let mut receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        receipt_bytes.push(b'\n');
        std::fs::write(prior_dir.join("source_receipt.json"), &receipt_bytes).unwrap();
        let receipt_sha = sha256_bytes(&receipt_bytes);
        let release = json!({
            "schema_version":"scogo.data-factory.release-set.v1","release_id":"current-release",
            "campaign_id":"current-campaign","campaign_sha256":"a".repeat(64),
            "source_run_id":"prior-source","source_artifacts":{
                "tasks":sha256_bytes(&selected_bytes),"source_receipt":receipt_sha,"run":"f".repeat(64)},
            "source_receipt_sha256":receipt_sha,"source_manifest_sha256":"f".repeat(64),
            "source_manifest_bytes":1,"rubric_version":"scogo.itops-rubric.v1","providers":{},
            "claim_status":"development_only","split_counts":{"train":1,"validation":0,"evaluation":0},
            "projection_counts":{},"artifacts":[{"path":"canonical/tasks.jsonl",
                "sha256":sha256_bytes(&canonical_bytes),"bytes":canonical_bytes.len(),"rows":1}],
            "created_at":"2026-09-01T00:00:00Z"
        });
        let mut release_bytes = serde_json::to_vec(&release).unwrap();
        release_bytes.push(b'\n');
        std::fs::write(prior_dir.join("release_set.json"), &release_bytes).unwrap();
        let tasks = source_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect::<Vec<Value>>();
        let body = json!({
            "schema_version":"scogo.data-factory.source-exclusion-authority.v2",
            "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
            "revision":"0123456789abcdef0123456789abcdef01234567","source_file":"part-3/tasks.jsonl",
            "source_file_rows":2,"source_file_sha256":sha256_bytes(&source_bytes),
            "source_population_sha256":source_population_sha256(&tasks).unwrap(),
            "excluded_source_task_ids":[prior_id],"prior_completed_releases":[{
                "evidence_mode":"current","run_id":"current-release",
                "release_set_sha256":sha256_bytes(&release_bytes),"source_receipt_sha256":receipt_sha,
                "canonical_tasks_sha256":sha256_bytes(&canonical_bytes),
                "legacy_source_receipt_sha256":null,"taskgen_run_sha256":null,
                "taskgen_tasks_sha256":null,"taskgen_reviews_sha256":null,
                "selected_source_task_ids":[prior_id]
            }]
        });
        let mut authority = body.as_object().unwrap().clone();
        authority.insert(
            "authority_id".into(),
            json!(format!(
                "authority_{}",
                sha256_bytes(&serde_json::to_vec(&body).unwrap())
            )),
        );
        let mut authority_bytes = serde_json::to_vec(&authority).unwrap();
        authority_bytes.push(b'\n');
        std::fs::write(
            plan.join("source_exclusion_authority.json"),
            &authority_bytes,
        )
        .unwrap();
        (source, plan, sha256_bytes(&authority_bytes))
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

    fn phase_b_args(source: &Path, plan: &Path, pin: &str, root: &Path) -> crate::ReviewArgs {
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
            "source-plan-test".into(),
            "--work-dir".into(),
            root.join("work").display().to_string(),
            "--final-run-dir".into(),
            root.join("final").display().to_string(),
            "--source-plan-dir".into(),
            plan.display().to_string(),
            "--source-plan-sha256".into(),
            pin.into(),
            "--source-selection".into(),
            "source-plan-test".into(),
        ])
        .unwrap();
        let crate::Command::Review(args) = cli.command else {
            panic!("expected review")
        };
        *args
    }

    #[test]
    fn source_plan_rejects_pin_id_unknown_field_tree_and_symlink_tamper() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let source_population = load_source_population(&source, &taxonomy).unwrap();
        let args = phase_b_args(&source, &plan, &pin, temporary.path());
        SourcePlan::load(&args, &source_population, 1).unwrap();
        let mut wrong_pin = args.clone();
        wrong_pin.source_plan_sha256 = Some("0".repeat(64));
        assert!(SourcePlan::load(&wrong_pin, &source_population, 1).is_err());

        let authority_path = plan.join("source_exclusion_authority.json");
        let original = std::fs::read(&authority_path).unwrap();
        let mut authority: Value = serde_json::from_slice(&original).unwrap();
        authority["unexpected"] = json!(true);
        let mut identity = authority.as_object().unwrap().clone();
        identity.remove("authority_id");
        authority["authority_id"] = json!(format!(
            "authority_{}",
            sha256_bytes(&serde_json::to_vec(&identity).unwrap())
        ));
        let mut bytes = serde_json::to_vec(&authority).unwrap();
        bytes.push(b'\n');
        std::fs::write(&authority_path, &bytes).unwrap();
        let unknown_args = phase_b_args(&source, &plan, &sha256_bytes(&bytes), temporary.path());
        assert!(SourcePlan::load(&unknown_args, &source_population, 1).is_err());
        std::fs::write(&authority_path, &original).unwrap();

        std::fs::write(plan.join("extra"), b"extra").unwrap();
        assert!(SourcePlan::load(&args, &source_population, 1).is_err());
        std::fs::remove_file(plan.join("extra")).unwrap();
        std::fs::create_dir(plan.join("extra-dir")).unwrap();
        assert!(SourcePlan::load(&args, &source_population, 1).is_err());
        std::fs::remove_dir(plan.join("extra-dir")).unwrap();
        #[cfg(unix)]
        {
            let target = plan.join("authority-target");
            std::fs::rename(&authority_path, &target).unwrap();
            std::os::unix::fs::symlink(&target, &authority_path).unwrap();
            assert!(SourcePlan::load(&args, &source_population, 1).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_plan_rejects_top_level_symlink_and_inexact_source_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let population = load_source_population(&source, &taxonomy).unwrap();

        let plan_link = temporary.path().join("plan-link");
        std::os::unix::fs::symlink(&plan, &plan_link).unwrap();
        for linked_path in [
            plan_link.clone(),
            PathBuf::from(format!("{}/", plan_link.display())),
        ] {
            let linked_args = phase_b_args(&source, &linked_path, &pin, temporary.path());
            assert!(SourcePlan::load(&linked_args, &population, 1).is_err());
        }

        let authority_path = plan.join("source_exclusion_authority.json");
        let original: Value =
            serde_json::from_slice(&std::fs::read(&authority_path).unwrap()).unwrap();
        for (field, value) in [
            ("repo_id", " ScogoAI/netops-prompt-seed"),
            ("repo_id", "ScogoAI /netops-prompt-seed"),
            ("repo_id", "ScogoAI/"),
            ("source_file", ""),
            ("source_file", "part-3/tasks.jsonl "),
            ("source_file", "part-3 /tasks.jsonl"),
            ("source_file", "part-3//tasks.jsonl"),
        ] {
            let mut changed = original.clone();
            changed[field] = json!(value);
            let mut identity = changed.as_object().unwrap().clone();
            identity.remove("authority_id");
            changed["authority_id"] = json!(format!(
                "authority_{}",
                sha256_bytes(&serde_json::to_vec(&identity).unwrap())
            ));
            let mut bytes = serde_json::to_vec(&changed).unwrap();
            bytes.push(b'\n');
            std::fs::write(&authority_path, &bytes).unwrap();
            let args = phase_b_args(&source, &plan, &sha256_bytes(&bytes), temporary.path());
            assert!(
                SourcePlan::load(&args, &population, 1).is_err(),
                "accepted inexact {field}={value:?}"
            );
        }
    }

    #[tokio::test]
    async fn preflight_only_creates_no_work_lock_or_provider_state() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);
        let mut args = phase_b_args(&source, &plan, &pin, temporary.path());
        args.preflight_only = true;

        run(args).await.unwrap();

        assert!(!temporary.path().join("work").exists());
        assert!(!temporary.path().join("final").exists());
        assert!(!temporary.path().join(".work.phase-b.lock").exists());
    }

    #[tokio::test]
    async fn preflight_rejects_invalid_provider_urls_without_side_effects() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);

        for adjudication in [false, true] {
            let mut args = phase_b_args(&source, &plan, &pin, temporary.path());
            args.preflight_only = true;
            if adjudication {
                args.adjudication_api_base = Some("not a URL".into());
            } else {
                args.api_base = "file:///tmp/not-http".into();
            }
            let error = run(args).await.unwrap_err();
            assert!(error.to_string().contains("API base URL"), "{error:#}");
            assert!(!temporary.path().join("work").exists());
            assert!(!temporary.path().join("final").exists());
            assert!(!temporary.path().join(".work.phase-b.lock").exists());
        }
    }

    #[tokio::test]
    async fn fresh_preflight_rejects_existing_work_or_final_destination() {
        for existing_work in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source.jsonl");
            std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
            let (plan, pin) = write_empty_plan(temporary.path(), &source);
            let mut args = phase_b_args(&source, &plan, &pin, temporary.path());
            args.preflight_only = true;
            let occupied = if existing_work {
                temporary.path().join("work")
            } else {
                temporary.path().join("final")
            };
            std::fs::create_dir(&occupied).unwrap();

            let error = run(args).await.unwrap_err();
            assert!(
                error.to_string().contains("absent work and final"),
                "{error:#}"
            );
            assert!(occupied.is_dir());
            assert!(!temporary.path().join(".work.phase-b.lock").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fresh_preflight_rejects_destination_symlink_but_allows_intermediate_symlink() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);

        let destination_link = temporary.path().join("work-link");
        std::os::unix::fs::symlink(temporary.path().join("missing"), &destination_link).unwrap();
        let mut occupied_args = phase_b_args(&source, &plan, &pin, temporary.path());
        occupied_args.preflight_only = true;
        occupied_args.work_dir = Some(destination_link);
        assert!(run(occupied_args).await.is_err());
        assert!(!temporary.path().join(".work-link.phase-b.lock").exists());

        let real_parent = temporary.path().join("real-parent");
        let parent_link = temporary.path().join("parent-link");
        std::fs::create_dir(&real_parent).unwrap();
        std::os::unix::fs::symlink(&real_parent, &parent_link).unwrap();
        let mut intermediate_args = phase_b_args(&source, &plan, &pin, temporary.path());
        intermediate_args.preflight_only = true;
        intermediate_args.work_dir = Some(parent_link.join("work"));
        intermediate_args.final_run_dir = Some(parent_link.join("final"));
        run(intermediate_args).await.unwrap();
        assert!(!real_parent.join("work").exists());
        assert!(!real_parent.join("final").exists());
        assert!(!real_parent.join(".work.phase-b.lock").exists());
    }

    #[tokio::test]
    async fn actual_legacy_source_plan_seals_for_data_factory_consumer() {
        let plan = Path::new("/private/tmp/scogo-source-plan-actual-BapSactX/plan");
        if !plan.is_dir() {
            return;
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
        let source = Path::new(
            "/Users/ksingh/git/scogo/work/experiments/taskgen/dataset/scogoai-enterprise-netops/part-2/tasks.jsonl",
        );
        let temporary = tempfile::tempdir().unwrap();
        let args = phase_b_args(
            source,
            plan,
            "b7576f4a2eba8a844c567cc065bbccc55f884b782848d245fafecd71daca2a94",
            temporary.path(),
        );
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        assert_eq!(prepared.source.rows.len(), 4_010);
        assert_eq!(prepared.eligible_rows.len(), 4_000);
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
        assert_eq!(
            std::fs::read(prepared.final_run_dir.join("source_population.jsonl")).unwrap(),
            std::fs::read(source).unwrap()
        );
        let data_factory = Path::new(
            "/Users/ksingh/git/scogo/work/experiments/scogo-data-factory/.worktree/data-factory-phase-b-100-smoke",
        );
        if data_factory.join(".venv/bin/python").is_file() {
            let status = std::process::Command::new(data_factory.join(".venv/bin/python"))
                .env("PYTHONPATH", data_factory.join("src"))
                .arg("-c")
                .arg("from pathlib import Path; import sys; from scogo_ai_data_factory.taskgen import load_taskgen_run; p=Path(sys.argv[1]); r=load_taskgen_run(p, source_receipt=p/'source_receipt.json', require_source_receipt=True); assert len(r.tasks)==1; assert len(r.exclusion_authority.excluded_source_task_ids)==10")
                .arg(std::fs::canonicalize(&prepared.final_run_dir).unwrap())
                .status()
                .unwrap();
            assert!(
                status.success(),
                "Data Factory rejected actual legacy source plan seal"
            );
        }
    }

    #[tokio::test]
    async fn current_source_plan_seals_for_data_factory_consumer() {
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
        let temporary = tempfile::tempdir().unwrap();
        let (source, plan, pin) = write_current_compatible_plan(temporary.path());
        let args = phase_b_args(&source, &plan, &pin, temporary.path());
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
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
                .arg("from pathlib import Path; import sys; from scogo_ai_data_factory.taskgen import load_taskgen_run; p=Path(sys.argv[1]); r=load_taskgen_run(p, source_receipt=p/'source_receipt.json', require_source_receipt=True); assert len(r.tasks)==1; assert r.exclusion_authority.prior_completed_releases[0].evidence_mode=='current'")
                .arg(std::fs::canonicalize(&prepared.final_run_dir).unwrap())
                .status()
                .unwrap();
            assert!(
                status.success(),
                "Data Factory rejected current source-plan seal"
            );
        }
        std::fs::write(
            plan.join("prior-releases/current-release/source_receipt.json"),
            b"tampered\n",
        )
        .unwrap();
        assert!(SourcePlan::load(&args, &prepared.source, 1).is_err());
    }

    #[test]
    fn immutable_resume_rejects_changed_source_before_provider_setup() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        let work = temporary.path().join("work");
        let final_run = temporary.path().join("final");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, plan_sha256) = write_empty_plan(temporary.path(), &source);
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
                "--source-plan-dir".into(),
                plan.display().to_string(),
                "--source-plan-sha256".into(),
                plan_sha256.clone(),
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
        let error = prepare_run(&parse(true), &taxonomy, "review prompt", None).unwrap_err();
        assert!(error.to_string().contains("raw source"), "{error:#}");

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
        let (plan, plan_sha256) = write_empty_plan(temporary.path(), &source);
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
            "--source-plan-dir".into(),
            plan.display().to_string(),
            "--source-plan-sha256".into(),
            plan_sha256,
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

    #[test]
    fn resume_rebuilds_an_unpublished_atomic_work_initialization() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.jsonl");
        std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
        let (plan, pin) = write_empty_plan(temporary.path(), &source);
        let args = phase_b_args(&source, &plan, &pin, temporary.path());
        let taxonomy =
            crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
        let work_name = prepared.work_dir.file_name().unwrap().to_string_lossy();
        let initializing = prepared.work_dir.parent().unwrap().join(format!(
            ".{work_name}.initializing-{}",
            &prepared.config_sha256[..16]
        ));
        std::fs::create_dir(&initializing).unwrap();
        std::fs::write(initializing.join("config.json"), b"{\"torn\":").unwrap();

        let journal = open_work(&prepared, true).unwrap();
        assert_eq!(journal.snapshot.rows.len(), 0);
        assert!(!initializing.exists());
        let stored: Value =
            serde_json::from_slice(&std::fs::read(prepared.work_dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(stored["config_sha256"], prepared.config_sha256);
        assert!(prepared.work_dir.join("stage.journal.jsonl").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_run_rejects_dangling_work_or_final_leaf_before_work_initialization() {
        for dangling_work in [true, false] {
            let temporary = tempfile::tempdir().unwrap();
            let source = temporary.path().join("source.jsonl");
            std::fs::write(&source, format!("{}\n", golden_task())).unwrap();
            let (plan, pin) = write_empty_plan(temporary.path(), &source);
            let args = phase_b_args(&source, &plan, &pin, temporary.path());
            let taxonomy =
                crate::taxonomy::TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
                    .unwrap();
            let prepared = prepare_run(&args, &taxonomy, "review prompt", None).unwrap();
            let occupied = if dangling_work {
                &prepared.work_dir
            } else {
                &prepared.final_run_dir
            };
            std::os::unix::fs::symlink(temporary.path().join("missing"), occupied).unwrap();

            let error = open_work(&prepared, false).unwrap_err();
            assert!(
                error.to_string().contains("fresh run requires absent"),
                "{error:#}"
            );
            assert!(!prepared.work_dir.join("config.json").exists());
            assert!(
                !sibling_work_directory(&prepared, "initializing")
                    .unwrap()
                    .exists()
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn held_input_rejects_devices_and_fifos_before_reading() {
        assert!(HeldFile::capture(Path::new("/dev/null"), 1024).is_err());

        let temporary = tempfile::tempdir().unwrap();
        let fifo = temporary.path().join("evidence.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let writer_path = fifo.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if let Ok(fd) = rustix::fs::openat(
                rustix::fs::CWD,
                &writer_path,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            ) {
                let mut file = File::from(fd);
                let _ = file.write_all(b"x");
            }
        });
        let result = HeldFile::capture(&fifo, 1024);
        writer.join().unwrap();
        assert!(result.is_err());
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
        let authority_body = json!({
            "schema_version":"scogo.data-factory.source-exclusion-authority.v2",
            "repo_id":"ScogoAI/netops-prompt-seed","repo_type":"dataset","private":true,
            "revision":"0123456789abcdef0123456789abcdef01234567",
            "source_file":"part-3/tasks.jsonl","source_file_rows":1,
            "source_file_sha256":sha256_bytes(&source_jsonl),
            "source_population_sha256":source_population_sha256(std::slice::from_ref(&task)).unwrap(),
            "excluded_source_task_ids":[],
            "prior_completed_releases":[]
        });
        let mut authority = authority_body.as_object().unwrap().clone();
        authority.insert(
            "authority_id".into(),
            json!(format!(
                "authority_{}",
                sha256_bytes(&serde_json::to_vec(&authority_body).unwrap())
            )),
        );
        let mut authority_bytes = serde_json::to_vec(&authority).unwrap();
        authority_bytes.push(b'\n');
        let plan_path = temporary.path().join("plan");
        std::fs::create_dir(&plan_path).unwrap();
        std::fs::write(
            plan_path.join("source_exclusion_authority.json"),
            &authority_bytes,
        )
        .unwrap();
        let plan_directory = HeldDirectory::capture(&plan_path).unwrap();
        let (authority_file, _) = plan_directory
            .read_file(
                Path::new("source_exclusion_authority.json"),
                16 * 1024 * 1024,
            )
            .unwrap();
        let source_row = SourceRow {
            source_index: 0,
            task_id: task_id.clone(),
            task: task.clone(),
            deterministic_hard_failures: Vec::new(),
        };
        let config = serde_json::json!({"run_id":"phase-b-seal-test"});
        let prepared = PreparedRun {
            run_id: "phase-b-seal-test".into(),
            target: 1,
            work_dir: temporary.path().join("work"),
            final_run_dir: temporary.path().join("final"),
            source_selection: "unused-phase-b-test".into(),
            source: SourcePopulation {
                rows: vec![source_row.clone()],
                tasks: vec![task.clone()],
                raw_jsonl: source_jsonl,
                population_sha256: source_population_sha256(std::slice::from_ref(&task)).unwrap(),
                held: None,
            },
            eligible_rows: vec![source_row],
            source_plan: SourcePlan {
                path: plan_path,
                directory: plan_directory,
                nested_directories: vec![],
                authority: serde_json::from_slice(&authority_bytes).unwrap(),
                authority_artifact: EvidenceArtifact {
                    logical_name: "exclusion_authority".into(),
                    relative_file: "source_exclusion_authority.json".into(),
                    held: Some(authority_file),
                    sha256: sha256_bytes(&authority_bytes),
                    bytes: authority_bytes,
                },
                artifacts: BTreeMap::new(),
            },
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
    fn seal_rebuilds_an_unanchored_prepared_directory() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let final_name = prepared
            .final_run_dir
            .file_name()
            .unwrap()
            .to_string_lossy();
        let unanchored = prepared.final_run_dir.parent().unwrap().join(format!(
            ".{final_name}.prepared-{}",
            &prepared.config_sha256[..16]
        ));
        std::fs::create_dir(&unanchored).unwrap();
        std::fs::write(unanchored.join("partial"), b"crash-before-anchor").unwrap();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();

        let manifest_sha256 =
            seal_run(&prepared, &snapshot, &mut journal, || Ok(()), || Ok(())).unwrap();
        assert!(!unanchored.exists());
        assert_eq!(
            verify_sealed_run(&prepared, Some(&manifest_sha256)).unwrap(),
            manifest_sha256
        );
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

    #[cfg(unix)]
    #[test]
    fn prepared_resume_rejects_dangling_final_leaf_before_no_replace_rename() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        seal_run(
            &prepared,
            &snapshot,
            &mut journal,
            || Ok(()),
            || bail!("simulated post-rename crash"),
        )
        .unwrap_err();
        let prepared_directory = prepared
            .final_run_dir
            .parent()
            .unwrap()
            .join(journal.snapshot.seal_temporary_name.as_deref().unwrap());
        std::fs::rename(&prepared.final_run_dir, &prepared_directory).unwrap();
        std::os::unix::fs::symlink("missing-final", &prepared.final_run_dir).unwrap();

        let error = finish_prepared_seal(&prepared, &mut journal).unwrap_err();
        assert!(!error.to_string().contains("File exists"), "{error:#}");
        assert!(prepared_directory.is_dir());
        assert!(
            std::fs::symlink_metadata(&prepared.final_run_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(journal.snapshot.sealed_manifest_sha256.is_none());
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

    #[cfg(unix)]
    #[test]
    fn seal_rejects_dangling_final_leaf_before_prepared_state() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        std::os::unix::fs::symlink("missing-final", &prepared.final_run_dir).unwrap();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();

        let error = seal_run(&prepared, &snapshot, &mut journal, || Ok(()), || Ok(())).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
        assert!(journal.snapshot.seal_prepared_manifest_sha256.is_none());
        assert!(
            !prepared
                .final_run_dir
                .parent()
                .unwrap()
                .join(format!(
                    ".{}.prepared-{}",
                    prepared
                        .final_run_dir
                        .file_name()
                        .unwrap()
                        .to_string_lossy(),
                    &prepared.config_sha256[..16]
                ))
                .exists()
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

    #[test]
    fn sealed_run_verification_rejects_undeclared_tree_entries() {
        let (_temporary, prepared, snapshot) = seal_fixture();
        let (mut journal, _) = StageJournal::resume(
            &prepared.work_dir.join("stage.journal.jsonl"),
            &prepared.config_sha256,
        )
        .unwrap();
        let manifest_sha256 =
            seal_run(&prepared, &snapshot, &mut journal, || Ok(()), || Ok(())).unwrap();

        let extra_file = prepared.final_run_dir.join("undeclared.txt");
        std::fs::write(&extra_file, b"extra").unwrap();
        assert!(verify_sealed_run(&prepared, Some(&manifest_sha256)).is_err());
        std::fs::remove_file(extra_file).unwrap();

        let extra_directory = prepared.final_run_dir.join("undeclared");
        std::fs::create_dir(&extra_directory).unwrap();
        assert!(verify_sealed_run(&prepared, Some(&manifest_sha256)).is_err());
        std::fs::remove_dir(extra_directory).unwrap();

        #[cfg(unix)]
        {
            let extra_symlink = prepared.final_run_dir.join("undeclared-link");
            std::os::unix::fs::symlink("run.json", &extra_symlink).unwrap();
            assert!(verify_sealed_run(&prepared, Some(&manifest_sha256)).is_err());
            std::fs::remove_file(extra_symlink).unwrap();
        }
        assert_eq!(
            verify_sealed_run(&prepared, Some(&manifest_sha256)).unwrap(),
            manifest_sha256
        );
    }
}
