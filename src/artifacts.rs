use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

const ARTIFACT_FLUSH_INTERVAL: usize = 256;

#[derive(Debug, Clone)]
pub struct PublishedPaths {
    pub run_dir: PathBuf,
    pub output: PathBuf,
    pub partial: PathBuf,
    pub candidates: PathBuf,
    pub reviews: PathBuf,
    pub rejected: PathBuf,
    pub run: PathBuf,
    pub log: PathBuf,
}

impl PublishedPaths {
    pub fn for_run_dir(run_dir: &Path) -> Self {
        Self {
            run_dir: run_dir.to_path_buf(),
            output: run_dir.join("tasks.jsonl"),
            partial: run_dir.join("accepted.partial.jsonl"),
            candidates: run_dir.join("candidates.jsonl"),
            reviews: run_dir.join("reviews.jsonl"),
            rejected: run_dir.join("rejected.jsonl"),
            run: run_dir.join("run.json"),
            log: run_dir.join("run.log"),
        }
    }
}

pub fn automatic_run_dir(root: &Path, timestamp: &str, taxonomy_id: &str, run_id: &str) -> PathBuf {
    root.join(format!(
        "{}-{}-{}",
        run_directory_slug(timestamp),
        run_directory_slug(taxonomy_id),
        run_directory_slug(run_id)
    ))
}

pub fn automatic_generation_run_dir(
    root: &Path,
    taxonomy_id: &str,
    generation_model: &str,
    count: usize,
    local_start_time: &str,
) -> Result<PathBuf> {
    let base_name = format!(
        "{}+{}+c{}+{}",
        run_directory_slug(taxonomy_id),
        run_directory_slug(generation_model),
        count,
        run_directory_slug(local_start_time)
    );
    let base = root.join(&base_name);
    if !base.exists() {
        return Ok(base);
    }
    for collision in 2..=9999 {
        let candidate = root.join(format!("{base_name}+{collision:02}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "could not allocate an automatic run directory under {}",
        root.display()
    )
}

pub fn default_generation_runs_root(current_directory: &Path) -> PathBuf {
    let taskgen_path = current_directory.join("taskgen");
    if taskgen_path.is_file() {
        current_directory.join("runs")
    } else {
        taskgen_path.join("runs")
    }
}

fn reject_symlinked_ancestors(path: &Path, label: &str) -> Result<()> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "{label} path or ancestor is a symlink: {}",
                    current.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect {label} path: {}", current.display())
                });
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

fn run_directory_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_') {
            slug.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "unknown".to_string()
    } else {
        slug
    }
}

#[derive(Debug)]
pub struct RunArtifacts {
    published: PublishedPaths,
    accepted_path: PathBuf,
    accepted: BufWriter<File>,
    candidates: BufWriter<File>,
    reviews: BufWriter<File>,
    rejected: BufWriter<File>,
    accepted_since_flush: usize,
    candidates_since_flush: usize,
    reviews_since_flush: usize,
    rejected_since_flush: usize,
}

impl RunArtifacts {
    pub fn create<T: Serialize>(
        run_dir: &Path,
        append_from: Option<&Path>,
        initial_report: &T,
    ) -> Result<Self> {
        // Generate/review run paths are trusted local inputs. Reject ordinary
        // symlink and hard-link hazards before opening them; bounded Phase-B
        // inputs use descriptor-backed HeldFile/HeldDirectory checks.
        reject_symlinked_ancestors(run_dir, "run directory")?;
        if let Some(source) = append_from {
            reject_symlinked_ancestors(source, "append source")?;
            let metadata = fs::symlink_metadata(source).with_context(|| {
                format!("failed to inspect append source: {}", source.display())
            })?;
            if metadata.file_type().is_symlink() {
                bail!("append source must not be a symlink: {}", source.display());
            }
            if !metadata.is_file() {
                bail!("append source must be a regular file: {}", source.display());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    bail!(
                        "append source must be a single-link regular file: {}",
                        source.display()
                    );
                }
            }
        }
        if run_dir.exists() {
            let metadata = fs::symlink_metadata(run_dir).with_context(|| {
                format!("failed to inspect run directory: {}", run_dir.display())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "run directory must be a real directory: {}",
                    run_dir.display()
                );
            }
            let mut entries = fs::read_dir(run_dir).with_context(|| {
                format!("failed to inspect run directory: {}", run_dir.display())
            })?;
            if entries.next().transpose()?.is_some() {
                bail!("run directory is not empty: {}", run_dir.display());
            }
        } else {
            fs::create_dir_all(run_dir).with_context(|| {
                format!("failed to create run directory: {}", run_dir.display())
            })?;
            let metadata = fs::symlink_metadata(run_dir).with_context(|| {
                format!("failed to inspect run directory: {}", run_dir.display())
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "run directory must be a real directory: {}",
                    run_dir.display()
                );
            }
        }

        let published = PublishedPaths::for_run_dir(run_dir);
        write_json_atomic(&published.run, initial_report)?;
        if let Some(source) = append_from {
            fs::copy(source, &published.partial).with_context(|| {
                format!(
                    "failed to stage existing append dataset: {}",
                    source.display()
                )
            })?;
        } else {
            File::create(&published.partial)?;
        }
        let accepted = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&published.partial)?,
        );
        let candidates = BufWriter::new(File::create(&published.candidates)?);
        let reviews = BufWriter::new(File::create(&published.reviews)?);
        let rejected = BufWriter::new(File::create(&published.rejected)?);

        Ok(Self {
            accepted_path: published.partial.clone(),
            published,
            accepted,
            candidates,
            reviews,
            rejected,
            accepted_since_flush: 0,
            candidates_since_flush: 0,
            reviews_since_flush: 0,
            rejected_since_flush: 0,
        })
    }

    pub fn accepted_path(&self) -> &Path {
        &self.accepted_path
    }

    pub fn paths(&self) -> &PublishedPaths {
        &self.published
    }

    pub fn write_accepted_line(&mut self, line: &str) -> Result<()> {
        writeln!(self.accepted, "{}", line.trim_end())?;
        self.accepted_since_flush += 1;
        if self.accepted_since_flush >= ARTIFACT_FLUSH_INTERVAL {
            self.accepted.flush()?;
            self.accepted_since_flush = 0;
        }
        Ok(())
    }

    pub fn write_candidate<T: Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.candidates, value)?;
        self.candidates.write_all(b"\n")?;
        self.candidates_since_flush += 1;
        if self.candidates_since_flush >= ARTIFACT_FLUSH_INTERVAL {
            self.candidates.flush()?;
            self.candidates_since_flush = 0;
        }
        Ok(())
    }

    pub fn write_review<T: Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.reviews, value)?;
        self.reviews.write_all(b"\n")?;
        self.reviews_since_flush += 1;
        if self.reviews_since_flush >= ARTIFACT_FLUSH_INTERVAL {
            self.reviews.flush()?;
            self.reviews_since_flush = 0;
        }
        Ok(())
    }

    pub fn write_rejection<T: Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.rejected, value)?;
        self.rejected.write_all(b"\n")?;
        self.rejected_since_flush += 1;
        if self.rejected_since_flush >= ARTIFACT_FLUSH_INTERVAL {
            self.rejected.flush()?;
            self.rejected_since_flush = 0;
        }
        Ok(())
    }

    /// Flush buffered records so operators can tail a live run directory.
    ///
    /// This intentionally does not call `sync_all`; the terminal flush still
    /// provides the durability barrier before publication. Live progress needs
    /// visibility without turning every model response into four fsyncs.
    pub fn flush_visible(&mut self) -> Result<()> {
        self.accepted.flush()?;
        self.candidates.flush()?;
        self.reviews.flush()?;
        self.rejected.flush()?;
        self.accepted_since_flush = 0;
        self.candidates_since_flush = 0;
        self.reviews_since_flush = 0;
        self.rejected_since_flush = 0;
        Ok(())
    }

    pub fn publish<T: Serialize>(mut self, report: &T) -> Result<PublishedPaths> {
        self.flush()?;
        let report_temporary = write_json_temporary(&self.published.run, report)?;
        if let Err(error) = fs::rename(&self.accepted_path, &self.published.output) {
            let _ = fs::remove_file(&report_temporary);
            return Err(error.into());
        }
        if let Err(sync_error) = sync_directory(&self.published.run_dir) {
            let rollback_errors = rollback_publication(&self.published, None);
            let _ = fs::remove_file(&report_temporary);
            return publication_error(
                "failed to sync published tasks",
                sync_error,
                rollback_errors,
            );
        }

        let running_report_backup = self.published.run_dir.join(".run.json.running");
        if let Err(backup_error) = copy_and_sync(&self.published.run, &running_report_backup) {
            let rollback_errors = rollback_publication(&self.published, None);
            let _ = fs::remove_file(&report_temporary);
            let _ = fs::remove_file(&running_report_backup);
            return publication_error(
                "failed to preserve the running report",
                backup_error,
                rollback_errors,
            );
        }
        if let Err(report_error) = fs::rename(&report_temporary, &self.published.run) {
            let rollback_errors = rollback_publication(&self.published, None);
            let _ = fs::remove_file(&report_temporary);
            let _ = fs::remove_file(&running_report_backup);
            return publication_error(
                "failed to publish run report",
                report_error,
                rollback_errors,
            );
        }
        if let Err(sync_error) = sync_directory(&self.published.run_dir) {
            let rollback_errors =
                rollback_publication(&self.published, Some(&running_report_backup));
            let _ = fs::remove_file(&report_temporary);
            return publication_error(
                "failed to durably commit published tasks and report",
                sync_error,
                rollback_errors,
            );
        }
        let _ = fs::remove_file(&running_report_backup);
        let _ = sync_directory(&self.published.run_dir);
        Ok(self.published)
    }

    pub fn finish_incomplete<T: Serialize>(mut self, report: &T) -> Result<PublishedPaths> {
        self.flush()?;
        write_json_atomic(&self.published.run, report)?;
        Ok(self.published)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.flush_visible()?;
        self.accepted.get_ref().sync_all()?;
        self.candidates.get_ref().sync_all()?;
        self.reviews.get_ref().sync_all()?;
        self.rejected.get_ref().sync_all()?;
        Ok(())
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = write_json_temporary(path, value)?;
    fs::rename(&temporary, path)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

fn write_json_temporary<T: Serialize>(path: &Path, value: &T) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(".run.json.tmp");
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(File::create(&temporary)?);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(temporary)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open artifact directory: {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync artifact directory: {}", path.display()))
}

fn copy_and_sync(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    File::open(destination)
        .with_context(|| format!("failed to open report backup: {}", destination.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync report backup: {}", destination.display()))?;
    sync_directory(destination.parent().unwrap_or_else(|| Path::new(".")))
}

fn rollback_publication(
    paths: &PublishedPaths,
    running_report_backup: Option<&Path>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if paths.output.exists()
        && let Err(error) = fs::rename(&paths.output, &paths.partial)
    {
        errors.push(format!("failed to restore partial tasks: {error}"));
    }
    if let Some(backup) = running_report_backup
        && backup.exists()
        && let Err(error) = fs::rename(backup, &paths.run)
    {
        errors.push(format!("failed to restore running report: {error}"));
    }
    if let Err(error) = sync_directory(&paths.run_dir) {
        errors.push(format!("failed to sync publication rollback: {error:#}"));
    }
    errors
}

fn publication_error(
    operation: &str,
    error: impl std::fmt::Display,
    rollback_errors: Vec<String>,
) -> Result<PublishedPaths> {
    if rollback_errors.is_empty() {
        bail!("{operation}: {error}");
    }
    bail!(
        "{operation}: {error}; rollback errors: {}",
        rollback_errors.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use serde_json::json;

    struct FailingReport;

    impl Serialize for FailingReport {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("intentional report failure"))
        }
    }

    #[test]
    fn automatic_run_directory_is_timestamped_and_taxonomy_specific() {
        let root = Path::new("taskgen-runs");
        let path = automatic_run_dir(
            root,
            "20260820T143501Z",
            "scogo-enterprise-netops-v2",
            "a81f9c2d",
        );
        assert_eq!(
            path,
            PathBuf::from("taskgen-runs/20260820T143501Z-scogo-enterprise-netops-v2-a81f9c2d")
        );
    }

    #[test]
    fn generation_run_directory_contains_taxonomy_model_count_and_local_start_time() {
        let path = automatic_generation_run_dir(
            Path::new("taskgen/runs"),
            "scogo-enterprise-netops-v2",
            "cx/gpt-5.6-luna-xhigh",
            100,
            "250826-14-35",
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from(
                "taskgen/runs/scogo-enterprise-netops-v2+cx-gpt-5.6-luna-xhigh+c100+250826-14-35"
            )
        );
    }

    #[test]
    fn generation_run_directory_adds_a_suffix_on_same_minute_collision() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("taskgen/runs");
        let first =
            automatic_generation_run_dir(&root, "scogo-itops-v4", "model/name", 2, "250826-14-35")
                .unwrap();
        fs::create_dir_all(&first).unwrap();
        let second =
            automatic_generation_run_dir(&root, "scogo-itops-v4", "model/name", 2, "250826-14-35")
                .unwrap();
        assert_eq!(
            second.file_name().unwrap(),
            "scogo-itops-v4+model-name+c2+250826-14-35+02"
        );
    }

    #[test]
    fn generation_runs_root_avoids_the_repository_binary_name_collision() {
        let temporary = tempfile::tempdir().unwrap();
        assert_eq!(
            default_generation_runs_root(temporary.path()),
            temporary.path().join("taskgen/runs")
        );
        fs::write(temporary.path().join("taskgen"), b"local binary").unwrap();
        assert_eq!(
            default_generation_runs_root(temporary.path()),
            temporary.path().join("runs")
        );
    }

    #[test]
    fn report_failure_never_publishes_final_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("report-failure");
        let initial = json!({"status":"running"});
        let mut run = RunArtifacts::create(&run_dir, None, &initial).unwrap();
        run.write_accepted_line(r#"{"prompt":"valid staged record"}"#)
            .unwrap();

        assert!(run.publish(&FailingReport).is_err());
        assert!(!run_dir.join("tasks.jsonl").exists());
        assert!(run_dir.join("accepted.partial.jsonl").exists());
        assert!(!run_dir.join(".run.json.tmp").exists());
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(report["status"], "running");
    }

    #[test]
    fn incomplete_run_retains_partial_data_and_terminal_report() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("run-001");
        let mut artifacts =
            RunArtifacts::create(&run_dir, None, &json!({"status":"running"})).unwrap();
        artifacts
            .write_accepted_line(r#"{"prompt":"partial"}"#)
            .unwrap();
        artifacts
            .finish_incomplete(&json!({"status":"failed","terminal_error":"quota"}))
            .unwrap();

        let paths = PublishedPaths::for_run_dir(&run_dir);
        assert!(!paths.output.exists());
        assert_eq!(
            fs::read_to_string(paths.partial).unwrap(),
            "{\"prompt\":\"partial\"}\n"
        );
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(paths.run).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert_eq!(report["terminal_error"], "quota");
    }

    #[test]
    fn successful_run_publishes_every_artifact_inside_one_directory() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("run-002");
        let mut artifacts =
            RunArtifacts::create(&run_dir, None, &json!({"status":"running"})).unwrap();
        artifacts
            .write_accepted_line(r#"{"prompt":"accepted"}"#)
            .unwrap();
        artifacts
            .write_candidate(&json!({"sequence":1,"prompt":"accepted"}))
            .unwrap();
        artifacts
            .write_review(&json!({"verdict":"accept"}))
            .unwrap();
        artifacts
            .write_rejection(&json!({"reason":"duplicate"}))
            .unwrap();
        let paths = artifacts
            .publish(&json!({"status":"success","accepted":1}))
            .unwrap();
        assert_eq!(paths.run_dir, run_dir);
        assert_eq!(
            fs::read_to_string(&paths.output).unwrap().lines().count(),
            1
        );
        assert!(paths.reviews.exists());
        assert!(paths.candidates.exists());
        assert!(paths.rejected.exists());
        assert!(paths.run.exists());
        assert!(!paths.partial.exists());
        for path in [
            paths.output,
            paths.candidates,
            paths.reviews,
            paths.rejected,
            paths.run,
        ] {
            assert_eq!(path.parent(), Some(run_dir.as_path()));
        }
    }

    #[test]
    fn batched_artifact_writes_are_fully_flushed_at_completion() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("batched-run");
        let mut artifacts =
            RunArtifacts::create(&run_dir, None, &json!({"status":"running"})).unwrap();
        for index in 0..(ARTIFACT_FLUSH_INTERVAL + 17) {
            artifacts
                .write_accepted_line(&format!(r#"{{"prompt":"accepted-{index}"}}"#))
                .unwrap();
            artifacts.write_review(&json!({"index":index})).unwrap();
            artifacts.write_rejection(&json!({"index":index})).unwrap();
        }

        let paths = artifacts
            .finish_incomplete(&json!({"status":"failed"}))
            .unwrap();
        let expected = ARTIFACT_FLUSH_INTERVAL + 17;
        assert_eq!(
            fs::read_to_string(paths.partial).unwrap().lines().count(),
            expected
        );
        assert_eq!(
            fs::read_to_string(paths.reviews).unwrap().lines().count(),
            expected
        );
        assert_eq!(
            fs::read_to_string(paths.rejected).unwrap().lines().count(),
            expected
        );
    }

    #[test]
    fn append_from_stages_existing_records_without_modifying_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("existing.jsonl");
        let run_dir = temp.path().join("run-003");
        fs::write(&source, "old\n").unwrap();
        let mut artifacts =
            RunArtifacts::create(&run_dir, Some(&source), &json!({"status":"running"})).unwrap();
        artifacts.write_accepted_line("new").unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "old\n");
        let paths = artifacts
            .publish(&json!({"status":"success","accepted":1}))
            .unwrap();
        assert_eq!(fs::read_to_string(paths.output).unwrap(), "old\nnew\n");
    }

    #[test]
    fn non_empty_run_directory_is_never_reused() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("occupied");
        fs::create_dir(&run_dir).unwrap();
        fs::write(run_dir.join("keep.txt"), "user data").unwrap();

        let error = RunArtifacts::create(&run_dir, None, &json!({"status":"running"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not empty"), "{error}");
        assert_eq!(
            fs::read_to_string(run_dir.join("keep.txt")).unwrap(),
            "user data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_run_directory_is_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let run_dir = temp.path().join("run");
        fs::create_dir(&target).unwrap();
        symlink(&target, &run_dir).unwrap();

        let error = RunArtifacts::create(&run_dir, None, &json!({"status":"running"}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("symlink") || error.contains("real directory"),
            "{error}"
        );
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn append_source_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("existing.jsonl");
        let source = temp.path().join("source.jsonl");
        fs::write(&target, "old\n").unwrap();
        symlink(&target, &source).unwrap();

        let error = RunArtifacts::create(
            &temp.path().join("run"),
            Some(&source),
            &json!({"status":"running"}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn append_source_hardlink_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("existing.jsonl");
        let source = temp.path().join("source.jsonl");
        fs::write(&target, "old\n").unwrap();
        fs::hard_link(&target, &source).unwrap();

        let error = RunArtifacts::create(
            &temp.path().join("run"),
            Some(&source),
            &json!({"status":"running"}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("single-link"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_directory_rejects_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_parent = temp.path().join("real-parent");
        let linked_parent = temp.path().join("linked-parent");
        let run_dir = linked_parent.join("run");
        std::fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        let error = RunArtifacts::create(&run_dir, None, &json!({"status":"running"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("ancestor"), "{error}");
        assert!(std::fs::read_dir(&real_parent).unwrap().next().is_none());
    }
}
