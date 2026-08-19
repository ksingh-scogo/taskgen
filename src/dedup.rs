use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SemanticModel {
    AllMiniLmL6V2,
    MultilingualE5Small,
}

impl SemanticModel {
    pub fn model_id(self) -> &'static str {
        match self {
            Self::AllMiniLmL6V2 => "sentence-transformers/all-MiniLM-L6-v2",
            Self::MultilingualE5Small => "intfloat/multilingual-e5-small",
        }
    }

    fn fastembed_model(self) -> fastembed::EmbeddingModel {
        match self {
            Self::AllMiniLmL6V2 => fastembed::EmbeddingModel::AllMiniLML6V2,
            Self::MultilingualE5Small => fastembed::EmbeddingModel::MultilingualE5Small,
        }
    }
}

pub struct FastEmbedder {
    model_id: String,
    model: Arc<Mutex<fastembed::TextEmbedding>>,
}

impl FastEmbedder {
    pub fn initialize(model: SemanticModel, cache_dir: Option<PathBuf>) -> Result<Self> {
        let mut options = fastembed::TextInitOptions::new(model.fastembed_model())
            .with_show_download_progress(true);
        if let Some(cache_dir) = cache_dir {
            options = options.with_cache_dir(cache_dir);
        }
        let embedding = fastembed::TextEmbedding::try_new(options)
            .context("failed to initialize local semantic embedding model")?;
        Ok(Self {
            model_id: model.model_id().to_string(),
            model: Arc::new(Mutex::new(embedding)),
        })
    }
}

#[async_trait]
impl PromptEmbedder for FastEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn embed(&self, prompt: &str) -> Result<Vec<f32>> {
        let model = self.model.clone();
        let prompt = prompt.to_string();
        tokio::task::spawn_blocking(move || {
            let mut model = model
                .lock()
                .map_err(|_| anyhow::anyhow!("semantic embedding model lock poisoned"))?;
            let mut embeddings = model
                .embed(vec![prompt], None)
                .context("local semantic embedding failed")?;
            embeddings
                .pop()
                .context("local semantic embedding returned no vector")
        })
        .await
        .context("local semantic embedding task failed")?
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DedupMode {
    Lexical,
    Semantic,
}

#[derive(Debug, Clone)]
pub struct DedupConfig {
    pub mode: DedupMode,
    pub prompt_field: String,
    pub ngram: usize,
    pub jaccard_threshold: f32,
    pub semantic_threshold: f32,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            mode: DedupMode::Semantic,
            prompt_field: "prompt".into(),
            ngram: 5,
            jaccard_threshold: 0.80,
            semantic_threshold: 0.90,
        }
    }
}

impl DedupConfig {
    pub fn lexical() -> Self {
        Self {
            mode: DedupMode::Lexical,
            ..Self::default()
        }
    }

    pub fn semantic_at(threshold: f32) -> Self {
        Self {
            semantic_threshold: threshold,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.ngram == 0 {
            bail!("dedup ngram must be positive");
        }
        if !(0.0..=1.0).contains(&self.jaccard_threshold)
            || !(0.0..=1.0).contains(&self.semantic_threshold)
        {
            bail!("dedup thresholds must be in [0,1]");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileDedupOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub dropped: PathBuf,
    pub report: Option<PathBuf>,
    pub overwrite: bool,
    pub config: DedupConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDedupStats {
    pub schema_version: String,
    pub input_records: usize,
    pub kept_records: usize,
    pub dropped_records: usize,
    pub invalid_records: usize,
    pub exact_duplicates: usize,
    pub jaccard_duplicates: usize,
    pub semantic_duplicates: usize,
}

pub async fn dedup_jsonl(
    options: FileDedupOptions,
    embedder: Option<Arc<dyn PromptEmbedder>>,
) -> Result<FileDedupStats> {
    options.config.validate()?;
    for destination in [&options.output, &options.dropped] {
        if destination.exists() && !options.overwrite {
            bail!(
                "dedup destination already exists: {} (use --overwrite)",
                destination.display()
            );
        }
    }
    if let Some(report) = &options.report
        && report.exists()
        && !options.overwrite
    {
        bail!(
            "dedup destination already exists: {} (use --overwrite)",
            report.display()
        );
    }

    let contents = fs::read_to_string(&options.input)
        .with_context(|| format!("failed to read dedup input: {}", options.input.display()))?;
    let mut index = DedupIndex::new(options.config.clone(), embedder.clone())?;
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut stats = FileDedupStats {
        schema_version: "scogo.taskgen.dedup-report.v1".into(),
        input_records: 0,
        kept_records: 0,
        dropped_records: 0,
        invalid_records: 0,
        exact_duplicates: 0,
        jaccard_duplicates: 0,
        semantic_duplicates: 0,
    };

    for (line_index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        stats.input_records += 1;
        let mut value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                stats.invalid_records += 1;
                dropped.push(json!({
                    "raw": line,
                    "_dedup": {
                        "reason": "invalid_record",
                        "line": line_index + 1,
                        "detail": error.to_string()
                    }
                }));
                continue;
            }
        };
        let Some(prompt) = value
            .get(&options.config.prompt_field)
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            stats.invalid_records += 1;
            add_dedup_metadata(
                &mut value,
                json!({
                    "reason": "invalid_record",
                    "line": line_index + 1,
                    "detail": format!("missing string field '{}'", options.config.prompt_field)
                }),
            );
            dropped.push(value);
            continue;
        };
        let language = value
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("en")
            .to_string();
        let domain = value
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or("_missing")
            .to_string();
        let subdomain = value
            .get("subdomain")
            .and_then(Value::as_str)
            .unwrap_or("_missing")
            .to_string();
        let embedding = match &embedder {
            Some(embedder) => Some(embedder.embed(&prompt).await?),
            None => None,
        };
        let candidate = DedupCandidate {
            prompt: &prompt,
            language: Some(&language),
            domain: &domain,
            subdomain: &subdomain,
        };
        if let Some(hit) = index.find_duplicate(&candidate, embedding.as_deref())? {
            match hit.reason {
                DedupReason::Exact => stats.exact_duplicates += 1,
                DedupReason::Jaccard => stats.jaccard_duplicates += 1,
                DedupReason::Semantic => stats.semantic_duplicates += 1,
            }
            add_dedup_metadata(
                &mut value,
                json!({
                    "reason": hit.reason,
                    "accepted_sha256": hit.accepted_sha256,
                    "score": hit.score,
                    "threshold": hit.threshold,
                    "bucket": format!("{}|{}|{}", language, domain, subdomain)
                }),
            );
            dropped.push(value);
        } else {
            index.insert(candidate, embedding)?;
            kept.push(value);
        }
    }
    stats.kept_records = kept.len();
    stats.dropped_records = dropped.len();
    write_jsonl_atomic(&options.output, &kept)?;
    write_jsonl_atomic(&options.dropped, &dropped)?;
    if let Some(report) = &options.report {
        write_json_atomic(report, &stats)?;
    }
    Ok(stats)
}

fn add_dedup_metadata(value: &mut Value, metadata: Value) {
    match value.as_object_mut() {
        Some(object) => {
            object.insert("_dedup".into(), metadata);
        }
        None => {
            *value = json!({"record": value.take(), "_dedup": metadata});
        }
    }
}

fn atomic_temp_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("dedup destination must have a UTF-8 filename")?;
    Ok(parent.join(format!(
        ".{name}.taskgen-{}-{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    )))
}

fn write_jsonl_atomic(destination: &Path, values: &[Value]) -> Result<()> {
    let temporary = atomic_temp_path(destination)?;
    let result = (|| -> Result<()> {
        let mut data = Vec::new();
        for value in values {
            serde_json::to_writer(&mut data, value)?;
            data.push(b'\n');
        }
        fs::write(&temporary, data)?;
        fs::rename(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json_atomic<T: Serialize>(destination: &Path, value: &T) -> Result<()> {
    let temporary = atomic_temp_path(destination)?;
    let result = (|| -> Result<()> {
        let mut data = serde_json::to_vec_pretty(value)?;
        data.push(b'\n');
        fs::write(&temporary, data)?;
        fs::rename(&temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DedupReason {
    Exact,
    Jaccard,
    Semantic,
}

#[derive(Debug, Clone, Serialize)]
pub struct DuplicateMatch {
    pub reason: DedupReason,
    pub accepted_sha256: String,
    pub score: Option<f32>,
    pub threshold: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
pub struct DedupCandidate<'a> {
    pub prompt: &'a str,
    pub language: Option<&'a str>,
    pub domain: &'a str,
    pub subdomain: &'a str,
}

#[async_trait]
pub trait PromptEmbedder: Send + Sync {
    fn model_id(&self) -> &str;
    async fn embed(&self, prompt: &str) -> Result<Vec<f32>>;
}

struct AcceptedFeatures {
    prompt_sha256: String,
    grams: HashSet<String>,
    embedding: Option<Vec<f32>>,
}

pub struct DedupIndex {
    config: DedupConfig,
    embedder: Option<Arc<dyn PromptEmbedder>>,
    exact: HashMap<String, String>,
    buckets: HashMap<String, Vec<AcceptedFeatures>>,
}

impl DedupIndex {
    pub fn new(config: DedupConfig, embedder: Option<Arc<dyn PromptEmbedder>>) -> Result<Self> {
        config.validate()?;
        if config.mode == DedupMode::Semantic && embedder.is_none() {
            bail!("semantic dedup requires a local prompt embedder");
        }
        Ok(Self {
            config,
            embedder,
            exact: HashMap::new(),
            buckets: HashMap::new(),
        })
    }

    pub fn embedder(&self) -> Option<Arc<dyn PromptEmbedder>> {
        self.embedder.clone()
    }

    pub fn find_duplicate(
        &self,
        candidate: &DedupCandidate<'_>,
        embedding: Option<&[f32]>,
    ) -> Result<Option<DuplicateMatch>> {
        let exact = exact_key(candidate.prompt);
        if let Some(accepted_sha256) = self.exact.get(&exact) {
            return Ok(Some(DuplicateMatch {
                reason: DedupReason::Exact,
                accepted_sha256: accepted_sha256.clone(),
                score: Some(1.0),
                threshold: Some(1.0),
            }));
        }
        let bucket = bucket_key(candidate);
        let grams = word_ngrams(candidate.prompt, self.config.ngram);
        let Some(accepted) = self.buckets.get(&bucket) else {
            return Ok(None);
        };
        for existing in accepted {
            let score = jaccard(&existing.grams, &grams);
            if is_duplicate_score(score, self.config.jaccard_threshold) {
                return Ok(Some(DuplicateMatch {
                    reason: DedupReason::Jaccard,
                    accepted_sha256: existing.prompt_sha256.clone(),
                    score: Some(score),
                    threshold: Some(self.config.jaccard_threshold),
                }));
            }
        }
        if self.config.mode == DedupMode::Semantic {
            let vector = embedding.context("semantic dedup candidate is missing an embedding")?;
            for existing in accepted {
                let Some(existing_vector) = existing.embedding.as_deref() else {
                    bail!("semantic dedup index contains an entry without an embedding");
                };
                let score = cosine_similarity(existing_vector, vector)?;
                if is_duplicate_score(score, self.config.semantic_threshold) {
                    return Ok(Some(DuplicateMatch {
                        reason: DedupReason::Semantic,
                        accepted_sha256: existing.prompt_sha256.clone(),
                        score: Some(score),
                        threshold: Some(self.config.semantic_threshold),
                    }));
                }
            }
        }
        Ok(None)
    }

    pub fn insert(
        &mut self,
        candidate: DedupCandidate<'_>,
        embedding: Option<Vec<f32>>,
    ) -> Result<String> {
        if self.config.mode == DedupMode::Semantic && embedding.is_none() {
            bail!("semantic dedup candidate is missing an embedding");
        }
        let prompt_sha256 = prompt_sha256(candidate.prompt);
        self.exact
            .insert(exact_key(candidate.prompt), prompt_sha256.clone());
        self.buckets
            .entry(bucket_key(&candidate))
            .or_default()
            .push(AcceptedFeatures {
                prompt_sha256: prompt_sha256.clone(),
                grams: word_ngrams(candidate.prompt, self.config.ngram),
                embedding,
            });
        Ok(prompt_sha256)
    }
}

pub fn normalize_prompt(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn exact_key(text: &str) -> String {
    sha256_hex(normalize_prompt(text).as_bytes())
}

pub fn prompt_sha256(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bucket_key(candidate: &DedupCandidate<'_>) -> String {
    let language = candidate.language.unwrap_or("en").trim().to_lowercase();
    let language = if language.is_empty() { "en" } else { &language };
    let domain = if candidate.domain.trim().is_empty() {
        "_missing"
    } else {
        candidate.domain.trim()
    };
    let subdomain = if candidate.subdomain.trim().is_empty() {
        "_missing"
    } else {
        candidate.subdomain.trim()
    };
    format!("{language}|{domain}|{subdomain}")
}

fn word_ngrams(text: &str, n: usize) -> HashSet<String> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let (tokens, separator): (Vec<String>, &str) = if words.len() <= 1 {
        (
            lower
                .chars()
                .filter(|character| !character.is_whitespace())
                .map(|character| character.to_string())
                .collect(),
            "",
        )
    } else {
        (words.into_iter().map(str::to_string).collect(), " ")
    };
    if tokens.is_empty() {
        return HashSet::new();
    }
    if tokens.len() < n {
        return HashSet::from([tokens.join(separator)]);
    }
    tokens
        .windows(n)
        .map(|window| window.join(separator))
        .collect()
}

fn jaccard(left: &HashSet<String>, right: &HashSet<String>) -> f32 {
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f32;
    let union = left.union(right).count() as f32;
    intersection / union
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32> {
    if left.is_empty() || left.len() != right.len() {
        bail!("embedding dimensions must be equal and non-empty");
    }
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm: f32 = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm: f32 = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        bail!("embedding norm must be non-zero");
    }
    Ok(dot / (left_norm * right_norm))
}

pub fn is_duplicate_score(score: f32, threshold: f32) -> bool {
    score >= threshold
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn candidate<'a>(prompt: &'a str, domain: &'a str) -> DedupCandidate<'a> {
        DedupCandidate {
            prompt,
            language: Some("en"),
            domain,
            subdomain: "acl",
        }
    }

    #[test]
    fn exact_normalization_preserves_word_boundaries() {
        assert_eq!(normalize_prompt("  VLAN\n10  DOWN "), "vlan 10 down");
        assert_ne!(exact_key("ab c"), exact_key("a bc"));
    }

    #[test]
    fn lexical_comparison_is_bucketed_by_language_domain_and_subdomain() {
        let mut index = DedupIndex::new(DedupConfig::lexical(), None).unwrap();
        let original = candidate(
            "check the same five word incident template right now",
            "network",
        );
        index.insert(original, None).unwrap();
        let other_domain = candidate(
            "check the same five word incident template right now",
            "security",
        );
        let duplicate = index.find_duplicate(&other_domain, None).unwrap();
        assert!(matches!(duplicate.unwrap().reason, DedupReason::Exact));
    }

    #[test]
    fn lexical_near_duplicates_are_not_compared_across_buckets() {
        let mut index = DedupIndex::new(DedupConfig::lexical(), None).unwrap();
        let original = candidate(
            "check the same five word incident template right now",
            "network",
        );
        index.insert(original, None).unwrap();
        let other_domain = candidate(
            "check the same five word incident template again now",
            "security",
        );
        assert!(index.find_duplicate(&other_domain, None).unwrap().is_none());
    }

    #[test]
    fn score_threshold_is_inclusive() {
        assert!(is_duplicate_score(0.80, 0.80));
        assert!(!is_duplicate_score(0.7999, 0.80));
    }

    struct FixedEmbedder {
        values: HashMap<String, Vec<f32>>,
    }

    #[async_trait::async_trait]
    impl PromptEmbedder for FixedEmbedder {
        fn model_id(&self) -> &str {
            "fixed-test"
        }

        async fn embed(&self, prompt: &str) -> anyhow::Result<Vec<f32>> {
            Ok(self.values[prompt].clone())
        }
    }

    #[tokio::test]
    async fn semantic_duplicate_is_rejected_at_inclusive_threshold() {
        let embedder = std::sync::Arc::new(FixedEmbedder {
            values: HashMap::from([
                ("first".to_string(), vec![1.0, 0.0]),
                ("paraphrase".to_string(), vec![0.9, 0.4358899]),
            ]),
        });
        let mut index =
            DedupIndex::new(DedupConfig::semantic_at(0.9), Some(embedder.clone())).unwrap();
        let first_vector = embedder.embed("first").await.unwrap();
        index
            .insert(candidate("first", "network"), Some(first_vector))
            .unwrap();
        let paraphrase_vector = embedder.embed("paraphrase").await.unwrap();
        let hit = index
            .find_duplicate(
                &candidate("paraphrase", "network"),
                Some(&paraphrase_vector),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(hit.reason, DedupReason::Semantic));
        assert!(hit.score.unwrap() >= 0.9);
    }

    #[tokio::test]
    #[ignore = "downloads or loads the configured local ONNX model"]
    async fn fastembed_smoke_returns_finite_embedding() {
        let embedder = FastEmbedder::initialize(SemanticModel::AllMiniLmL6V2, None).unwrap();
        let vector = embedder
            .embed("Investigate a BGP route leak")
            .await
            .unwrap();
        assert_eq!(vector.len(), 384);
        assert!(vector.iter().all(|value| value.is_finite()));
        assert!(cosine_similarity(&vector, &vector).unwrap() > 0.999);
    }

    #[tokio::test]
    async fn standalone_lexical_dedup_writes_kept_dropped_and_report() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input.jsonl");
        let output = temp.path().join("kept.jsonl");
        let dropped = temp.path().join("dropped.jsonl");
        let report = temp.path().join("report.json");
        fs::write(
            &input,
            concat!(
                r#"{"prompt":"Investigate BGP route leak now","domain":"routing","subdomain":"bgp"}"#,
                "\n",
                r#"{"prompt":" investigate  bgp route LEAK now ","domain":"routing","subdomain":"bgp"}"#,
                "\n",
                r#"{"prompt":"Check OSPF adjacency reset","domain":"routing","subdomain":"ospf"}"#,
                "\n"
            ),
        )
        .unwrap();
        let stats = dedup_jsonl(
            FileDedupOptions {
                input,
                output: output.clone(),
                dropped: dropped.clone(),
                report: Some(report.clone()),
                overwrite: false,
                config: DedupConfig::lexical(),
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(stats.input_records, 3);
        assert_eq!(stats.kept_records, 2);
        assert_eq!(stats.exact_duplicates, 1);
        assert_eq!(fs::read_to_string(output).unwrap().lines().count(), 2);
        assert!(
            fs::read_to_string(dropped)
                .unwrap()
                .contains("accepted_sha256")
        );
        assert!(
            fs::read_to_string(report)
                .unwrap()
                .contains("dropped_records")
        );
    }
}
