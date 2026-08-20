use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct PublishedPaths {
    pub run_dir: PathBuf,
    pub output: PathBuf,
    pub partial: PathBuf,
    pub reviews: PathBuf,
    pub rejected: PathBuf,
    pub run: PathBuf,
}

impl PublishedPaths {
    pub fn for_run_dir(run_dir: &Path) -> Self {
        Self {
            run_dir: run_dir.to_path_buf(),
            output: run_dir.join("tasks.jsonl"),
            partial: run_dir.join("accepted.partial.jsonl"),
            reviews: run_dir.join("reviews.jsonl"),
            rejected: run_dir.join("rejected.jsonl"),
            run: run_dir.join("run.json"),
        }
    }
}

pub fn automatic_run_dir(root: &Path, timestamp: &str, taxonomy_id: &str, run_id: &str) -> PathBuf {
    fn slug(value: &str) -> String {
        value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                    character
                } else {
                    '-'
                }
            })
            .collect()
    }

    root.join(format!(
        "{}-{}-{}",
        slug(timestamp),
        slug(taxonomy_id),
        slug(run_id)
    ))
}

#[derive(Debug)]
pub struct RunArtifacts {
    published: PublishedPaths,
    accepted_path: PathBuf,
    accepted: BufWriter<File>,
    reviews: BufWriter<File>,
    rejected: BufWriter<File>,
}

impl RunArtifacts {
    pub fn create<T: Serialize>(
        run_dir: &Path,
        append_from: Option<&Path>,
        initial_report: &T,
    ) -> Result<Self> {
        if run_dir.exists() {
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
        let reviews = BufWriter::new(File::create(&published.reviews)?);
        let rejected = BufWriter::new(File::create(&published.rejected)?);

        Ok(Self {
            accepted_path: published.partial.clone(),
            published,
            accepted,
            reviews,
            rejected,
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
        self.accepted.flush()?;
        Ok(())
    }

    pub fn write_review<T: Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.reviews, value)?;
        self.reviews.write_all(b"\n")?;
        self.reviews.flush()?;
        Ok(())
    }

    pub fn write_rejection<T: Serialize>(&mut self, value: &T) -> Result<()> {
        serde_json::to_writer(&mut self.rejected, value)?;
        self.rejected.write_all(b"\n")?;
        self.rejected.flush()?;
        Ok(())
    }

    pub fn publish<T: Serialize>(mut self, report: &T) -> Result<PublishedPaths> {
        self.flush()?;
        fs::rename(&self.accepted_path, &self.published.output)?;
        write_json_atomic(&self.published.run, report)?;
        Ok(self.published)
    }

    pub fn finish_incomplete<T: Serialize>(mut self, report: &T) -> Result<PublishedPaths> {
        self.flush()?;
        write_json_atomic(&self.published.run, report)?;
        Ok(self.published)
    }

    fn flush(&mut self) -> Result<()> {
        self.accepted.flush()?;
        self.reviews.flush()?;
        self.rejected.flush()?;
        Ok(())
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(".run.json.tmp");
    let mut writer = BufWriter::new(File::create(&temporary)?);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    drop(writer);
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(paths.rejected.exists());
        assert!(paths.run.exists());
        assert!(!paths.partial.exists());
        for path in [paths.output, paths.reviews, paths.rejected, paths.run] {
            assert_eq!(path.parent(), Some(run_dir.as_path()));
        }
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
}
