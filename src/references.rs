use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) fn contains_credential(bytes: &[u8]) -> bool {
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

#[derive(Debug, Clone)]
struct ReferenceDocument {
    reference_id: String,
    text: String,
    terms: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReferenceExcerpt {
    pub reference_id: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceStore {
    documents: Vec<ReferenceDocument>,
}

impl ReferenceStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load(root: &Path) -> Result<Self> {
        let root_metadata = fs::symlink_metadata(root).with_context(|| {
            format!(
                "failed to inspect review reference directory: {}",
                root.display()
            )
        })?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            bail!(
                "review reference directory must be a real directory: {}",
                root.display()
            );
        }
        let mut paths = Vec::new();
        collect_supported_files(root, &mut paths)?;
        paths.sort();
        let mut documents = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read review reference: {}", path.display()))?;
            if contains_credential(&bytes) {
                bail!(
                    "review reference contains credential-like content: {}",
                    path.display()
                );
            }
            let text = String::from_utf8(bytes)
                .with_context(|| format!("review reference is not UTF-8: {}", path.display()))?;
            let reference_id = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            documents.push(ReferenceDocument {
                reference_id,
                terms: terms(&text),
                text,
            });
        }
        Ok(Self { documents })
    }

    pub(crate) fn from_documents(rows: Vec<(String, String)>) -> Self {
        Self {
            documents: rows
                .into_iter()
                .map(|(reference_id, text)| ReferenceDocument {
                    terms: terms(&text),
                    reference_id,
                    text,
                })
                .collect(),
        }
    }

    pub fn retrieve(
        &self,
        query: &str,
        max_results: usize,
        max_excerpt_chars: usize,
    ) -> Vec<ReferenceExcerpt> {
        if max_results == 0 || max_excerpt_chars == 0 {
            return Vec::new();
        }
        let query_terms = terms(query);
        let mut ranked: Vec<(usize, &ReferenceDocument)> = self
            .documents
            .iter()
            .filter_map(|document| {
                let score = query_terms.intersection(&document.terms).count();
                (score > 0).then_some((score, document))
            })
            .collect();
        ranked.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.reference_id.cmp(&right.reference_id))
        });
        ranked
            .into_iter()
            .take(max_results)
            .map(|(_, document)| ReferenceExcerpt {
                reference_id: document.reference_id.clone(),
                excerpt: document.text.chars().take(max_excerpt_chars).collect(),
            })
            .collect()
    }
}

fn collect_supported_files(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory).with_context(|| {
        format!(
            "failed to read reference directory: {}",
            directory.display()
        )
    })? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect review reference: {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "review reference corpus forbids symlinks: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_supported_files(&path, paths)?;
        } else if metadata.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
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

fn terms(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_store_retrieves_relevant_local_excerpt() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("fortios.md"),
            "FortiOS VDOMs provide separate virtual firewall domains and routing tables.",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("unrelated.txt"),
            "Wireless channel planning reference.",
        )
        .unwrap();

        let store = ReferenceStore::load(temp.path()).unwrap();
        let hits = store.retrieve("FortiOS VDOM routing", 3, 800);

        assert_eq!(hits.len(), 1);
        assert!(hits[0].reference_id.contains("fortios.md"));
        assert!(hits[0].excerpt.contains("VDOMs"));
    }

    #[cfg(unix)]
    #[test]
    fn reference_store_rejects_symlinked_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("references");
        let outside = temp.path().join("outside.md");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, "private reference text").unwrap();
        symlink(&outside, root.join("linked.md")).unwrap();

        let error = ReferenceStore::load(&root).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
    }

    #[test]
    fn reference_store_rejects_credential_like_content() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("secret.md"),
            "temporary token sk-abcdefghijklmnopqrstuvwxyz",
        )
        .unwrap();

        let error = ReferenceStore::load(temp.path()).unwrap_err();
        assert!(error.to_string().contains("credential"), "{error:#}");
    }
}
