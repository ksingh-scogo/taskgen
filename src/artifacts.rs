use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct PublishedPaths {
    pub output: PathBuf,
    pub reviews: PathBuf,
    pub rejected: PathBuf,
    pub run: PathBuf,
}

impl PublishedPaths {
    pub fn for_output(output: &Path) -> Result<Self> {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let stem = output
            .file_stem()
            .and_then(|value| value.to_str())
            .context("output path must have a UTF-8 file stem")?;
        Ok(Self {
            output: output.to_path_buf(),
            reviews: parent.join(format!("{stem}.reviews.jsonl")),
            rejected: parent.join(format!("{stem}.rejected.jsonl")),
            run: parent.join(format!("{stem}.run.json")),
        })
    }
}

pub struct RunArtifacts {
    published: PublishedPaths,
    work_dir: PathBuf,
    accepted_path: PathBuf,
    reviews_path: PathBuf,
    rejected_path: PathBuf,
    run_path: PathBuf,
    accepted: BufWriter<File>,
    reviews: BufWriter<File>,
    rejected: BufWriter<File>,
}

impl RunArtifacts {
    pub fn create(output: &Path, append: bool, overwrite: bool) -> Result<Self> {
        if output.exists() && !append && !overwrite {
            bail!(
                "output already exists: {} (use --overwrite or --append)",
                output.display()
            );
        }
        if append && !output.exists() {
            bail!(
                "cannot append because output does not exist: {}",
                output.display()
            );
        }
        let published = PublishedPaths::for_output(output)?;
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory: {}", parent.display()))?;
        let stem = output.file_stem().and_then(|value| value.to_str()).unwrap();
        let nonce: u64 = rand::random();
        let work_dir = parent.join(format!(
            ".{stem}.taskgen-{}-{nonce:016x}",
            std::process::id()
        ));
        fs::create_dir(&work_dir).with_context(|| {
            format!(
                "failed to create run work directory: {}",
                work_dir.display()
            )
        })?;

        let accepted_path = work_dir.join("accepted.jsonl");
        let reviews_path = work_dir.join("reviews.jsonl");
        let rejected_path = work_dir.join("rejected.jsonl");
        let run_path = work_dir.join("run.json");
        if append {
            fs::copy(output, &accepted_path).with_context(|| {
                format!(
                    "failed to stage existing output for append: {}",
                    output.display()
                )
            })?;
        }
        let accepted = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&accepted_path)?,
        );
        let reviews = BufWriter::new(File::create(&reviews_path)?);
        let rejected = BufWriter::new(File::create(&rejected_path)?);

        Ok(Self {
            published,
            work_dir,
            accepted_path,
            reviews_path,
            rejected_path,
            run_path,
            accepted,
            reviews,
            rejected,
        })
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn accepted_path(&self) -> &Path {
        &self.accepted_path
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
        self.accepted.flush()?;
        self.reviews.flush()?;
        self.rejected.flush()?;
        let mut run = BufWriter::new(File::create(&self.run_path)?);
        serde_json::to_writer_pretty(&mut run, report)?;
        run.write_all(b"\n")?;
        run.flush()?;
        drop(run);
        drop(self.accepted);
        drop(self.reviews);
        drop(self.rejected);

        fs::rename(&self.reviews_path, &self.published.reviews)?;
        fs::rename(&self.rejected_path, &self.published.rejected)?;
        fs::rename(&self.run_path, &self.published.run)?;
        fs::rename(&self.accepted_path, &self.published.output)?;
        fs::remove_dir(&self.work_dir)?;
        Ok(self.published)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn incomplete_run_never_touches_final_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("tasks.jsonl");
        let work_dir = {
            let mut artifacts = RunArtifacts::create(&output, false, false).unwrap();
            artifacts
                .write_accepted_line(r#"{"prompt":"partial"}"#)
                .unwrap();
            artifacts.work_dir().to_path_buf()
        };
        assert!(!output.exists());
        assert!(work_dir.exists());
    }

    #[test]
    fn publish_writes_sidecars_then_complete_dataset() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("tasks.jsonl");
        let mut artifacts = RunArtifacts::create(&output, false, false).unwrap();
        artifacts
            .write_accepted_line(r#"{"prompt":"accepted"}"#)
            .unwrap();
        artifacts
            .write_review(&json!({"verdict":"accept"}))
            .unwrap();
        artifacts
            .write_rejection(&json!({"reason":"duplicate"}))
            .unwrap();
        let paths = artifacts.publish(&json!({"accepted":1})).unwrap();
        assert_eq!(fs::read_to_string(paths.output).unwrap().lines().count(), 1);
        assert!(paths.reviews.exists());
        assert!(paths.rejected.exists());
        assert!(paths.run.exists());
    }

    #[test]
    fn append_stages_existing_records_without_modifying_final_until_publish() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("tasks.jsonl");
        fs::write(&output, "old\n").unwrap();
        let mut artifacts = RunArtifacts::create(&output, true, false).unwrap();
        artifacts.write_accepted_line("new").unwrap();
        assert_eq!(fs::read_to_string(&output).unwrap(), "old\n");
        artifacts.publish(&json!({"accepted":1})).unwrap();
        assert_eq!(fs::read_to_string(&output).unwrap(), "old\nnew\n");
    }
}
