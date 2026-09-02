use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
        if !root.is_dir() {
            bail!(
                "review reference directory is not a directory: {}",
                root.display()
            );
        }
        let mut paths = Vec::new();
        collect_supported_files(root, &mut paths)?;
        paths.sort();
        let mut documents = Vec::with_capacity(paths.len());
        for path in paths {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read review reference: {}", path.display()))?;
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
        if path.is_dir() {
            collect_supported_files(&path, paths)?;
        } else if path
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
}
