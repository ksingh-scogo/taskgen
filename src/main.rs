use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
#[cfg(test)]
use chrono::Local;
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use rand::SeedableRng;
use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod artifacts;
pub mod atif;
pub mod dedup;
pub mod provider;
pub mod review;
pub mod schema;
pub mod taxonomy;
pub mod telemetry;

const DEFAULT_SYSTEM_PROMPT: &str = r#"Write prompts as if a competent on-call asked a question at 2am — Slack, PagerDuty, a war-room thread, or an SSH session. They might be tired or frustrated, but they're competent. They state symptoms, what they already checked, and what they're afraid of. Don't be a child. Don't be a robot. Don't write a formal runbook. Be a human who forgot to be formal.
Good casual: "zabbix is paging every 30s on the same host, I already restarted the agent, still flapping—mute or real disk?", "canary's eating error budget and CAB's frozen till Monday, rollback or ride it?", "BGP to DR flapped twice, underlay looks fine, overlay SLA class is red"
Bad casual: "omg servers down pls help wtf" (too stupid)
CRITICAL: Difficulty affects both phrasing AND problem complexity/scope:
- Difficulty 1-3: Junior on-call, runbook exists, one failing check, clear answer. Simple phrasing. "host is down in monitoring, ping fails, what do I check first?" "backup job failed with exit 1, logs say disk full—free space and rerun?"
- Difficulty 4-5: Mid SRE with a real incident. Competing hypotheses, incomplete metrics, what they already tried. "latency spiked on the LB but origin p99 is fine and synthetics are green—health checks look ok, persistence or TLS offload?"
- Difficulty 6-7: Senior. SLO vs velocity, blast-radius, change-freeze tradeoffs. State them frankly. "error budget's blown, freeze is on, but the vuln is exploitable in the wild—emergency-change the WAF or wait for CAB?" Don't hide the complexity; phrase it casually but completely.
- Difficulty 8-10: Principal. Multi-system, unknown-unknown, no runbook, synthesis. State what's hard, what's uncertain, what you're trying to balance. Use casual language but preserve the full operational weight. Example: "packet loss only east-west between the k8s overlay and the storage VLAN during backup windows, no single dashboard shows both, and rolling the CNI would take down tenant traffic—MTU vs CRC vs noisy neighbor before we touch prod?" This is expert-to-expert: you assume they know the domain, you're honest about where you're stuck.
Write how you'd actually text a teammate who's good at this stuff. Use normal punctuation. The core task must be correct.
For difficulty 6+: Include genuine constraint conflicts—error budget, approval gate, blast radius, customer impact, change freeze—but phrase them as a competent person would, not as formal prose. Don't manufacture fake complexity. Do preserve real complexity operators would actually articulate.
Skip boilerplate. Don't include commands, code blocks, or versions unless the operator would realistically paste them. "postgres is lagging" is better than "PostgreSQL 15.4 on Ubuntu 22.04 with Patroni." But "pg 15 replica lag hit 40s after the vacuum, WAL keep is 2GB" is fine if it's real context.
Keep it short. Say the problem, not the life story. Output only the prompt itself. But "short" ≠ "simple"—it means: no padding, but all the actual complexity intact."#;

const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("nl", "Dutch"),
    ("zh", "Chinese"),
    ("ar", "Arabic"),
    ("ru", "Russian"),
];

fn difficulty_label(d: u8) -> &'static str {
    match d {
        1 => "Very Easy (junior on-call)",
        2 => "Easy (runbook exists)",
        3 => "Basic (one failing check)",
        4 => "Intermediate (mid SRE)",
        5 => "Standard (incomplete metrics)",
        6 => "Skilled (senior, SLO tradeoffs)",
        7 => "Proficient (blast-radius / freeze)",
        8 => "Advanced (principal, multi-system)",
        9 => "Expert (unknown-unknown)",
        10 => "Principal (no runbook, synthesis)",
        _ => "Unknown",
    }
}

fn language_instruction(language: Option<&str>) -> String {
    match language {
        Some(code) if code != "en" => {
            let lang_name = LANGUAGES
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, n)| *n)
                .unwrap_or("English");
            format!(
                "\n\nIMPORTANT: Write the entire task/prompt in {}. Do NOT use English.",
                lang_name
            )
        }
        _ => String::new(),
    }
}

fn task_user_message(sample: &taxonomy::SampledTask, language: Option<&str>) -> String {
    let lang_instruction = language_instruction(language);
    if let Some(coordinates) = &sample.coordinates {
        let platforms = if coordinates.platforms.is_empty() {
            "none; stay platform-neutral".to_string()
        } else {
            coordinates.platforms.join(", ")
        };
        format!(
            "Generate one task prompt using all of these mandatory constraints:\n\nTaxonomy: {}\nCategory: {}\nDomain: {} ({})\nSubdomain: {}\nTask family: {}\nEnvironment: {}\nPlatform scope: {}\nPlatforms: {}\nIncident mechanism: {}\nEvidence condition: {}\nEvidence bundle: {}\nAction risk: {}\nPresentation: {}\nDifficulty: {}/10 ({})\n\nMake every coordinate materially affect the scenario. Do not merely list these labels in the generated prompt. Output only the task prompt, nothing else.{}",
            sample.taxonomy_id,
            sample.category_id,
            sample.domain_id,
            sample.domain_label,
            sample.subdomain_id,
            coordinates.task_family,
            coordinates.environment,
            coordinates.platform_scope,
            platforms,
            coordinates.incident_mechanism,
            coordinates.evidence_condition,
            coordinates.evidence_bundle,
            coordinates.action_risk,
            coordinates.presentation,
            sample.difficulty,
            difficulty_label(sample.difficulty),
            lang_instruction
        )
    } else if sample.category_id == "oem" {
        let domain_display = format!("{}::{}", sample.category_id, sample.domain_label);
        format!(
            "Generate a task/prompt for the following:\n\nVendor/platform: {}\nProduct: {}\nDifficulty: {}/10 ({})\n\nThe subdomain is a product line, not a generic failure mode. The incident MUST be about the {} product \"{}\" specifically — use SKU, firmware, CLI, console, TAC, or license language an operator of that product would actually type. Do NOT write a generic capability ticket that could apply to any vendor.\n\nOutput only the task prompt, nothing else.{}",
            domain_display,
            sample.subdomain_id,
            sample.difficulty,
            difficulty_label(sample.difficulty),
            domain_display,
            sample.subdomain_id,
            lang_instruction
        )
    } else {
        let domain_display = format!("{}::{}", sample.category_id, sample.domain_label);
        format!(
            "Generate a task/prompt for the following:\n\nDomain: {}\nSubdomain: {}\nDifficulty: {}/10 ({})\n\nThe task MUST be directly and specifically about the subdomain \"{}\" within {}. Do NOT generate a generic {} task — the content must focus on {} specifically.\n\nOutput only the task prompt, nothing else.{}",
            domain_display,
            sample.subdomain_id,
            sample.difficulty,
            difficulty_label(sample.difficulty),
            sample.subdomain_id,
            domain_display,
            domain_display,
            sample.subdomain_id,
            lang_instruction
        )
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "taskgen",
    version,
    about = "SFT task generator for distillation datasets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Generate(Box<GenerateArgs>),
    Dedup(DedupArgs),
    Atif {
        #[command(subcommand)]
        command: AtifCommand,
    },
    Taxonomy {
        #[command(subcommand)]
        command: TaxonomyCommand,
    },
}

#[derive(ClapArgs, Debug)]
struct DedupArgs {
    #[arg(long)]
    input: PathBuf,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long)]
    dropped: Option<PathBuf>,

    #[arg(long)]
    report: Option<PathBuf>,

    #[arg(long, default_value = "prompt")]
    prompt_field: String,

    #[arg(long, value_enum, default_value_t = dedup::DedupMode::Semantic)]
    dedup_mode: dedup::DedupMode,

    #[arg(long, default_value_t = 0.80)]
    jaccard_threshold: f32,

    #[arg(long, default_value_t = 0.90)]
    semantic_threshold: f32,

    #[arg(long, default_value_t = 5)]
    dedup_ngram: usize,

    #[arg(long, value_enum)]
    semantic_model: Option<dedup::SemanticModel>,

    #[arg(long)]
    semantic_model_cache: Option<PathBuf>,

    #[arg(long)]
    overwrite: bool,
}

#[derive(Subcommand, Debug)]
enum AtifCommand {
    Export(AtifArgs),
    Import(AtifArgs),
}

#[derive(ClapArgs, Debug)]
struct AtifArgs {
    #[arg(long)]
    input: PathBuf,

    #[arg(long)]
    output: PathBuf,

    #[arg(long, value_enum)]
    container: Option<ContainerArg>,

    #[arg(long)]
    overwrite: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ContainerArg {
    Json,
    Jsonl,
}

impl From<ContainerArg> for atif::Container {
    fn from(value: ContainerArg) -> Self {
        match value {
            ContainerArg::Json => Self::Json,
            ContainerArg::Jsonl => Self::Jsonl,
        }
    }
}

#[derive(Subcommand, Debug)]
enum TaxonomyCommand {
    Validate {
        #[arg(long)]
        taxonomy: PathBuf,
    },
}

fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("'{value}' is not a positive integer"))?;
    if parsed == 0 {
        return Err("value must be greater than zero".into());
    }
    Ok(parsed)
}

#[derive(ClapArgs, Debug, Clone)]
struct GenerateArgs {
    #[arg(long, default_value = "https://api.openai.com/v1")]
    api_base: String,

    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    #[arg(short, long, default_value = "gpt-4o-mini")]
    model: String,

    #[arg(long, conflicts_with = "system_prompt_file")]
    system_prompt: Option<String>,

    #[arg(long, conflicts_with = "system_prompt")]
    system_prompt_file: Option<PathBuf>,

    #[arg(long)]
    taxonomy: Option<PathBuf>,

    #[arg(long)]
    seed: Option<u64>,

    /// Number of newly accepted records required for success.
    #[arg(short, long, default_value_t = 250, value_parser = parse_positive_usize)]
    count: usize,

    #[arg(long)]
    distribution: Option<String>,

    #[arg(long)]
    difficulty: Option<String>,

    #[arg(short, long, default_value_t = 0.9)]
    temperature: f64,

    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    max_output_tokens: Option<u64>,

    /// Maximum number of taxonomy-coordinate slots processed concurrently.
    #[arg(short, long, default_value_t = 5, value_parser = parse_positive_usize)]
    workers: usize,

    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    request_timeout_seconds: u64,

    /// Directory containing every artifact for this run. Generated automatically when omitted.
    #[arg(long)]
    run_dir: Option<PathBuf>,

    /// Existing task JSONL to copy into this new run before adding the requested records.
    #[arg(long)]
    append_from: Option<PathBuf>,

    #[arg(long)]
    proxies: Option<PathBuf>,

    #[arg(long)]
    rotating_proxy: bool,

    #[arg(long)]
    keyfile: Option<PathBuf>,

    #[arg(long)]
    review_model: Option<String>,

    #[arg(long)]
    review_api_base: Option<String>,

    #[arg(long, env = "TASKGEN_REVIEW_API_KEY", hide_env_values = true)]
    review_api_key: Option<String>,

    #[arg(long)]
    review_keyfile: Option<PathBuf>,

    #[arg(long, conflicts_with = "review_system_prompt_file")]
    review_system_prompt: Option<String>,

    #[arg(long, conflicts_with = "review_system_prompt")]
    review_system_prompt_file: Option<PathBuf>,

    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    review_max_output_tokens: Option<u64>,

    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    max_attempts_per_slot: u64,

    #[arg(long, value_enum, default_value_t = dedup::DedupMode::Semantic)]
    dedup_mode: dedup::DedupMode,

    #[arg(long, default_value_t = 0.80)]
    jaccard_threshold: f32,

    #[arg(long, default_value_t = 0.90)]
    semantic_threshold: f32,

    #[arg(long, default_value_t = 5)]
    dedup_ngram: usize,

    #[arg(long, value_enum)]
    semantic_model: Option<dedup::SemanticModel>,

    #[arg(long)]
    semantic_model_cache: Option<PathBuf>,

    #[arg(long)]
    free_models: bool,

    #[arg(long)]
    input_price: Option<f64>,

    #[arg(long)]
    output_price: Option<f64>,

    #[arg(long)]
    review_input_price: Option<f64>,

    #[arg(long)]
    review_output_price: Option<f64>,

    #[arg(long)]
    budget: Option<f64>,

    /// Generate tasks in multiple languages (en, de, fr, es, nl, zh, ar, ru)
    #[arg(long)]
    multilingual: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_version: Option<String>,
    prompt: String,
    category: String,
    domain: String,
    subdomain: String,
    difficulty: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordinates: Option<taxonomy::TaskCoordinates>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    taskgen_model: String,
    temperature: f64,
}

fn serialize_task_entry(entry: &TaskEntry) -> Result<String> {
    let value = serde_json::to_value(entry)?;
    if entry.schema_version.as_deref() == Some("scogo.taskgen.task.v2") {
        schema::validate_instance(schema::SchemaKind::Task, &value)?;
    }
    serde_json::to_string(&value).map_err(Into::into)
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// Omniroute/GPT-5 default to SSE unless this is explicit.
    stream: bool,
}

/// GPT-5 / o-series / luna reject `temperature` and `max_tokens` (OpenAI sampling rules).
fn restricted_sampling(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let name = m.rsplit('/').next().unwrap_or(&m);
    name.contains("gpt-5")
        || name.contains("gpt5")
        || name.contains("luna")
        || name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4")
}

fn chat_request(
    model: &str,
    messages: Vec<ChatMessage>,
    temperature: f64,
    max_out: u64,
) -> ChatRequest {
    let enable_thinking = model.to_ascii_lowercase().contains("qwen").then_some(false);
    let thinking_budget = enable_thinking.map(|_| 0);
    let reasoning_effort = enable_thinking.map(|_| "low".to_string());
    if restricted_sampling(model) {
        ChatRequest {
            model: model.to_string(),
            messages,
            temperature: None,
            max_tokens: None,
            max_completion_tokens: Some(max_out),
            enable_thinking,
            thinking_budget,
            reasoning_effort,
            stream: false,
        }
    } else {
        ChatRequest {
            model: model.to_string(),
            messages,
            temperature: Some(temperature),
            max_tokens: Some(max_out),
            max_completion_tokens: None,
            enable_thinking,
            thinking_budget,
            reasoning_effort,
            stream: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

fn truncate_body(s: &str, n: usize) -> String {
    let s = s.trim();
    let mut t: String = s.chars().take(n).collect();
    if t.len() < s.len() {
        t.push_str(&format!("… ({} bytes)", s.len()));
    }
    t.replace('\n', " ")
}

fn json_u64(v: &serde_json::Value, keys: &[&str]) -> u64 {
    for key in keys {
        match v.get(*key) {
            Some(serde_json::Value::Number(n)) => {
                if let Some(u) = n.as_u64() {
                    return u;
                }
                if let Some(f) = n.as_f64() {
                    return f.max(0.0) as u64;
                }
            }
            Some(serde_json::Value::String(s)) => {
                if let Ok(u) = s.parse::<u64>() {
                    return u;
                }
            }
            _ => {}
        }
    }
    0
}

fn content_from_value(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                } else if let Some(s) = p.as_str() {
                    out.push_str(s);
                }
            }
            let t = out.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

fn append_choice_text(buf: &mut String, v: &serde_json::Value) {
    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return;
    };
    let Some(c0) = choices.first() else { return };
    if let Some(delta) = c0.get("delta")
        && let Some(s) = delta.get("content").and_then(content_from_value)
    {
        buf.push_str(&s);
    }
    if let Some(msg) = c0.get("message")
        && let Some(s) = msg.get("content").and_then(content_from_value)
        && buf.is_empty()
    {
        buf.push_str(&s);
    }
    if buf.is_empty()
        && let Some(s) = c0.get("text").and_then(content_from_value)
    {
        buf.push_str(&s);
    }
}

fn finish_reason(value: &serde_json::Value) -> Option<&str> {
    value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(serde_json::Value::as_str)
}

fn validate_finish_reason(reason: Option<&str>) -> Result<()> {
    match reason {
        None | Some("stop") => Ok(()),
        Some("length") => bail!("completion truncated (finish_reason=length)"),
        Some(other) => bail!("completion did not finish normally (finish_reason={other})"),
    }
}

fn validate_generated_prompt(prompt: &str) -> Result<()> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        bail!("generated prompt is empty");
    }
    let lower = trimmed.to_ascii_lowercase();
    let leaked_planning = [
        "we need answer user's request",
        "we need to answer the user's request",
        "we need craft a prompt",
        "need output only final",
        "task enterprise_netops::",
    ];
    if leaked_planning
        .iter()
        .any(|marker| lower.starts_with(marker))
    {
        bail!("generated content contains model planning instead of a standalone prompt");
    }
    let word_count = trimmed.split_whitespace().count();
    if word_count > 800 {
        bail!("generated prompt exceeds the 800-word limit ({word_count} words)");
    }
    Ok(())
}

fn model_user_message(model: &str, message: &str) -> String {
    if model.to_ascii_lowercase().contains("qwen") {
        format!(
            "{message}\n\nKeep the final task prompt focused and under 800 words. Silently verify vendor authenticity and causal consistency. Output no analysis, planning, or constraint labels.\n/no_think"
        )
    } else {
        message.to_string()
    }
}

fn generation_max_output_tokens(model: &str, requested: Option<u64>) -> u64 {
    if let Some(requested) = requested {
        requested
    } else if model.to_ascii_lowercase().contains("qwen") {
        4096
    } else {
        2048
    }
}

fn parse_chat_payload(raw: &str) -> Result<(String, u64, u64)> {
    let t = raw.trim_start();
    if t.starts_with("data:") {
        let mut text = String::new();
        let mut usage = (0u64, 0u64);
        let mut final_reason = None;
        for line in t.lines() {
            let Some(payload) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(payload)
                .with_context(|| format!("bad SSE chunk: {}", truncate_body(payload, 200)))?;
            if let Some(err) = v.get("error")
                && !err.is_null()
            {
                bail!("API error payload: {}", err);
            }
            append_choice_text(&mut text, &v);
            if let Some(reason) = finish_reason(&v) {
                final_reason = Some(reason.to_string());
            }
            let u = extract_usage(&v);
            if u != (0, 0) {
                usage = u;
            }
        }
        validate_finish_reason(final_reason.as_deref())?;
        let text = text.trim().to_string();
        if text.is_empty() {
            bail!("no completion text in streamed API response");
        }
        return Ok((text, usage.0, usage.1));
    }

    let value: serde_json::Value = serde_json::from_str(raw)?;
    validate_finish_reason(finish_reason(&value))?;
    let mut text = String::new();
    append_choice_text(&mut text, &value);
    if text.trim().is_empty() {
        text = extract_completion(&value)?;
    }
    let usage = extract_usage(&value);
    Ok((text.trim().to_string(), usage.0, usage.1))
}

fn extract_completion(v: &serde_json::Value) -> Result<String> {
    if let Some(err) = v.get("error")
        && !err.is_null()
    {
        bail!("API error payload: {}", err);
    }
    validate_finish_reason(finish_reason(v))?;
    let mut buf = String::new();
    append_choice_text(&mut buf, v);
    if !buf.trim().is_empty() {
        return Ok(buf.trim().to_string());
    }
    if let Some(output) = v.get("output").and_then(|o| o.as_array()) {
        for item in output {
            if let Some(s) = item.get("content").and_then(content_from_value) {
                return Ok(s.trim().to_string());
            }
        }
    }
    bail!("no completion text in API response")
}

fn extract_usage(v: &serde_json::Value) -> (u64, u64) {
    let Some(u) = v.get("usage") else {
        return (0, 0);
    };
    (
        json_u64(u, &["prompt_tokens", "input_tokens"]),
        json_u64(u, &["completion_tokens", "output_tokens"]),
    )
}

struct AtomicStats {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    review_input_tokens: AtomicU64,
    review_output_tokens: AtomicU64,
    attempts: AtomicUsize,
    tasks: AtomicUsize,
    errors: AtomicUsize,
}

impl AtomicStats {
    fn new() -> Self {
        Self {
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            review_input_tokens: AtomicU64::new(0),
            review_output_tokens: AtomicU64::new(0),
            attempts: AtomicUsize::new(0),
            tasks: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
struct RunStats {
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tasks: usize,
    errors: usize,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct DatasetCounts {
    categories: HashMap<String, usize>,
    domains: HashMap<String, usize>,
    subdomains: HashMap<(String, String), usize>,
    difficulties: HashMap<u8, usize>,
    n: usize,
}

#[cfg(test)]
impl DatasetCounts {
    fn add(&mut self, domain: &str, subdomain: &str, difficulty: u8) {
        let cat = domain.split_once("::").map(|(c, _)| c).unwrap_or(domain);
        *self.categories.entry(cat.to_string()).or_insert(0) += 1;
        *self.domains.entry(domain.to_string()).or_insert(0) += 1;
        *self
            .subdomains
            .entry((domain.to_string(), subdomain.to_string()))
            .or_insert(0) += 1;
        *self.difficulties.entry(difficulty).or_insert(0) += 1;
        self.n += 1;
    }
}

#[cfg(test)]
fn sorted_count_rows(map: &HashMap<String, usize>) -> Vec<(&String, usize)> {
    let mut rows: Vec<_> = map.iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    rows
}

fn parse_distribution(input: &str) -> Result<HashMap<String, f64>> {
    let mut map = HashMap::new();
    for pair in input.split(',') {
        let pair = pair.trim();
        let (key, val) = pair
            .split_once('=')
            .context(format!("invalid distribution pair: {}", pair))?;
        let key = key.trim().to_lowercase();
        let val: f64 = val
            .trim()
            .parse()
            .context(format!("invalid weight: {}", val))?;
        if !val.is_finite() || val < 0.0 {
            bail!("distribution weight for '{key}' must be finite and non-negative");
        }
        map.insert(key, val);
    }
    let total: f64 = map.values().sum();
    if (total - 1.0).abs() > 0.000_001 {
        bail!("distribution weights must sum to 1.0, got {}", total);
    }
    Ok(map)
}

fn parse_difficulty(input: &str) -> Result<HashMap<u8, f64>> {
    let mut map = HashMap::new();
    for pair in input.split(',') {
        let pair = pair.trim();
        let (key, val) = pair
            .split_once('=')
            .context(format!("invalid difficulty pair: {}", pair))?;
        let key: String = key.trim().to_lowercase();
        let d: u8 = if let Some(stripped) = key.strip_prefix('d') {
            stripped
                .parse()
                .context(format!("invalid difficulty level: {}", key))?
        } else {
            key.parse()
                .context(format!("invalid difficulty level: {}", key))?
        };
        if !(1..=10).contains(&d) {
            bail!("difficulty must be 1-10, got {}", d);
        }
        let val: f64 = val
            .trim()
            .parse()
            .context(format!("invalid weight: {}", val))?;
        if !val.is_finite() || val < 0.0 {
            bail!("difficulty weight for '{d}' must be finite and non-negative");
        }
        map.insert(d, val);
    }
    let total: f64 = map.values().sum();
    if (total - 1.0).abs() > 0.000_001 {
        bail!("difficulty weights must sum to 1.0, got {}", total);
    }
    Ok(map)
}

fn resolve_system_prompt(
    args: &GenerateArgs,
    taxonomy: &taxonomy::TaxonomyCatalog,
) -> Result<String> {
    if let Some(prompt) = &args.system_prompt {
        return Ok(prompt.clone());
    }
    if let Some(path) = &args.system_prompt_file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("failed to read system prompt: {}", path.display()));
    }
    if let Some(path) = taxonomy.default_system_prompt_path() {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read taxonomy system prompt: {}", path.display()));
    }
    Ok(DEFAULT_SYSTEM_PROMPT.to_string())
}

fn parse_proxy_line(line: &str) -> Result<reqwest::Proxy> {
    let line = line.trim();
    let parts: Vec<&str> = line.split(':').collect();
    let proxy_url = match parts.len() {
        2 => format!("http://{}:{}", parts[0], parts[1]),
        4 => format!("http://{}:{}@{}:{}", parts[2], parts[3], parts[0], parts[1]),
        _ => bail!(
            "invalid proxy format '{}', expected host:port or host:port:user:pass",
            line
        ),
    };
    reqwest::Proxy::all(&proxy_url).context(format!("failed to create proxy from '{}'", line))
}

fn load_proxies(path: &PathBuf) -> Result<Vec<reqwest::Proxy>> {
    let file =
        File::open(path).context(format!("failed to open proxy file: {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut proxies = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.context("failed to read proxy file")?;
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        proxies.push(parse_proxy_line(&line).context(format!("proxy line {}", i + 1))?);
    }
    if proxies.is_empty() {
        bail!("proxy file is empty: {}", path.display());
    }
    Ok(proxies)
}

fn build_clients(proxies: &[reqwest::Proxy], timeout: std::time::Duration) -> Vec<reqwest::Client> {
    proxies
        .iter()
        .map(|p| {
            reqwest::Client::builder()
                .proxy(p.clone())
                .timeout(timeout)
                .build()
                .expect("failed to build client with proxy")
        })
        .collect()
}

const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
const MIN_FREE_MODEL_CTX: u64 = 16000;

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    name: String,
    architecture: ModelArchitecture,
    pricing: ModelPricing,
    top_provider: ModelProvider,
}

#[derive(Debug, Deserialize)]
struct ModelArchitecture {
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelPricing {
    prompt: String,
    completion: String,
}

#[derive(Debug, Deserialize)]
struct ModelProvider {
    context_length: Option<u64>,
}

async fn fetch_free_models(client: &reqwest::Client, api_key: &str) -> Result<Vec<String>> {
    let url = format!("{}/models", OPENROUTER_API_BASE);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
        .context("failed to fetch OpenRouter models")?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("OpenRouter models API error: {}", text);
    }

    let models: ModelsResponse = resp
        .json()
        .await
        .context("failed to parse models response")?;

    let mut free: Vec<(String, String, u64)> = models
        .data
        .into_iter()
        .filter(|m| {
            m.pricing.prompt == "0"
                && m.pricing.completion == "0"
                && m.architecture
                    .input_modalities
                    .contains(&"text".to_string())
                && m.architecture
                    .output_modalities
                    .contains(&"text".to_string())
                && m.id != "openrouter/free"
                && m.top_provider.context_length.unwrap_or(0) >= MIN_FREE_MODEL_CTX
        })
        .map(|m| {
            let ctx = m.top_provider.context_length.unwrap_or(0);
            (m.id, m.name, ctx)
        })
        .collect();

    // sort by context length descending so best models are first
    free.sort_by_key(|item| Reverse(item.2));

    if free.is_empty() {
        bail!(
            "no free models with >= {}k context available on OpenRouter right now",
            MIN_FREE_MODEL_CTX / 1000
        );
    }

    println!(
        "Found {} candidate free models, running health checks...",
        free.len()
    );

    // ping each model with a tiny request to verify it's actually online
    let mut verified: Vec<String> = Vec::new();
    for (id, name, ctx) in &free {
        print!("  testing {} ({}, {}k ctx)... ", id, name, ctx / 1000);
        match test_model(client, api_key, id).await {
            Ok(()) => {
                println!("ok");
                verified.push(id.clone());
            }
            Err(e) => {
                println!("offline ({})", e);
            }
        }
    }

    if verified.is_empty() {
        bail!("all free models are offline on OpenRouter right now");
    }

    println!("Using {} verified free models", verified.len());
    Ok(verified)
}

async fn test_model(client: &reqwest::Client, api_key: &str, model: &str) -> Result<()> {
    let body = chat_request(
        model,
        vec![ChatMessage {
            role: "user".into(),
            content: "Say hi.".into(),
        }],
        0.0,
        5,
    );

    let url = format!("{}/chat/completions", OPENROUTER_API_BASE);
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .context("request failed")?;

    let status = resp.status();

    // 429 means the model exists and is live, just rate limited — count as available
    if status.as_u16() == 429 {
        return Ok(());
    }

    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("{}: {}", status, &text[..text.len().min(100)]);
    }

    let raw = resp.text().await.unwrap_or_default();
    parse_chat_payload(&raw).context("bad response")?;
    Ok(())
}

enum ApiError {
    RateLimit(Option<u64>),
    Transient(reqwest::StatusCode, String),
    InvalidCompletion(String),
    Billing(String),
    Timeout,
    Other(anyhow::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::RateLimit(s) => write!(f, "rate limited (retry after {:?}s)", s),
            ApiError::Transient(status, body) => {
                write!(f, "transient API error {status}: {body}")
            }
            ApiError::InvalidCompletion(message) => {
                write!(f, "invalid model completion: {message}")
            }
            ApiError::Billing(msg) => write!(f, "billing error: {}", msg),
            ApiError::Timeout => write!(f, "request timed out"),
            ApiError::Other(e) => write!(f, "{}", e),
        }
    }
}

fn is_transient_http_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

fn is_billing_error(status: reqwest::StatusCode, body: &str) -> bool {
    if status.as_u16() == 402 {
        return true;
    }
    let lower = body.to_lowercase();
    lower.contains("insufficient_quota")
        || lower.contains("billing")
        || lower.contains("payment required")
        || lower.contains("exceeded your current quota")
        || lower.contains("account is not active")
        || lower.contains("insufficient_funds")
        || lower.contains("budget")
}

async fn api_request(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &ChatRequest,
) -> std::result::Result<(String, u64, u64), ApiError> {
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(body)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return Err(ApiError::Timeout);
            }
            return Err(ApiError::Other(e.into()));
        }
    };

    let status = resp.status();

    if status.as_u16() == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Err(ApiError::RateLimit(retry_after));
    }

    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        if is_billing_error(status, &text) {
            return Err(ApiError::Billing(text));
        }
        if is_transient_http_status(status) {
            return Err(ApiError::Transient(status, truncate_body(&text, 500)));
        }
        return Err(ApiError::Other(anyhow::anyhow!(
            "API error {}: {}",
            status,
            text
        )));
    }

    let raw = resp
        .text()
        .await
        .map_err(|e| ApiError::Other(anyhow::anyhow!("failed to read API response: {}", e)))?;
    parse_chat_payload(&raw).map_err(|error| {
        ApiError::InvalidCompletion(format!("{error} | body: {}", truncate_body(&raw, 500)))
    })
}

const MAX_RETRIES: u32 = 5;

struct GenerateTaskRequest<'a> {
    client: &'a reqwest::Client,
    api_base: &'a str,
    api_key: &'a str,
    model: &'a str,
    system_prompt: &'a str,
    sample: &'a taxonomy::SampledTask,
    temperature: f64,
    max_output_tokens: Option<u64>,
    language: Option<&'a str>,
    retry_guidance: Option<&'a str>,
    cancel: &'a AtomicBool,
    consecutive_timeouts: &'a AtomicUsize,
    progress: &'a ProgressBar,
    telemetry: &'a telemetry::RequestTelemetry,
}

async fn generate_task(
    request: GenerateTaskRequest<'_>,
) -> std::result::Result<(String, u64, u64), ApiError> {
    let GenerateTaskRequest {
        client,
        api_base,
        api_key,
        model,
        system_prompt,
        sample,
        temperature,
        max_output_tokens,
        language,
        retry_guidance,
        cancel,
        consecutive_timeouts,
        progress,
        telemetry,
    } = request;
    let mut task_message = task_user_message(sample, language);
    if let Some(guidance) = retry_guidance.filter(|value| !value.trim().is_empty()) {
        task_message.push_str(
            "\n\nThe previous candidate was rejected. Correct this issue in the replacement: ",
        );
        task_message.push_str(guidance.trim());
    }
    let user_msg = model_user_message(model, &task_message);
    let system = if model.to_ascii_lowercase().contains("qwen") {
        format!("{system_prompt}\n/no_think")
    } else {
        system_prompt.to_string()
    };

    let body = chat_request(
        model,
        vec![
            ChatMessage {
                role: "system".into(),
                content: system,
            },
            ChatMessage {
                role: "user".into(),
                content: user_msg,
            },
        ],
        temperature,
        generation_max_output_tokens(model, max_output_tokens),
    );

    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));

    let mut retries = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ApiError::Other(anyhow::anyhow!("cancelled")));
        }

        let request_started = std::time::Instant::now();
        match api_request(client, &url, api_key, &body).await {
            Ok(result) => {
                consecutive_timeouts.store(0, Ordering::Relaxed);
                if let Err(error) = validate_generated_prompt(&result.0) {
                    telemetry.record_error(
                        request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                    retries += 1;
                    if retries > MAX_RETRIES {
                        return Err(ApiError::InvalidCompletion(error.to_string()));
                    }
                    telemetry.record_retry();
                    progress.suspend(|| {
                        eprintln!(
                            "[CONTENT] invalid completion, retrying ({retries}/{MAX_RETRIES}): {error}"
                        );
                    });
                    continue;
                }
                telemetry.record_success(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                return Ok(result);
            }
            Err(ApiError::RateLimit(retry_after)) => {
                telemetry.record_rate_limit(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(ApiError::RateLimit(retry_after));
                }
                telemetry.record_retry();
                let wait = retry_after.unwrap_or_else(|| 2u64.pow(retries).min(60));
                progress.suspend(|| {
                    eprintln!(
                        "[RATE] 429 hit, waiting {}s (retry {}/{})",
                        wait, retries, MAX_RETRIES
                    );
                });
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
            }
            Err(ApiError::Timeout) => {
                telemetry.record_timeout(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                let count = consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= 5 {
                    progress.suspend(|| {
                        eprintln!(
                            "[FATAL] {} consecutive timeouts, shutting down gracefully...",
                            count
                        );
                    });
                    cancel.store(true, Ordering::Relaxed);
                    return Err(ApiError::Timeout);
                }
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(ApiError::Timeout);
                }
                telemetry.record_retry();
                let wait = 2u64.pow(retries).min(30);
                progress.suspend(|| {
                    eprintln!(
                        "[TIMEOUT] request timed out, waiting {}s (retry {}/{}, {} consecutive)",
                        wait, retries, MAX_RETRIES, count
                    );
                });
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
            }
            Err(ApiError::Transient(status, body)) => {
                telemetry.record_error(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64
                );
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(ApiError::Transient(status, body));
                }
                telemetry.record_retry();
                let wait = 2u64.pow(retries).min(30);
                progress.suspend(|| {
                    eprintln!(
                        "[TRANSIENT] {status}, waiting {wait}s (retry {retries}/{MAX_RETRIES})"
                    );
                });
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
            }
            Err(ApiError::InvalidCompletion(message)) => {
                telemetry.record_error(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64
                );
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(ApiError::InvalidCompletion(message));
                }
                telemetry.record_retry();
                progress.suspend(|| {
                    eprintln!(
                        "[CONTENT] invalid completion, retrying ({retries}/{MAX_RETRIES}): {message}"
                    );
                });
            }
            Err(ApiError::Billing(msg)) => {
                telemetry.record_error(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64
                );
                progress.suspend(|| {
                    eprintln!("[FATAL] billing error, shutting down gracefully: {}", msg);
                });
                cancel.store(true, Ordering::Relaxed);
                return Err(ApiError::Billing(msg));
            }
            Err(e) => {
                telemetry.record_error(
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64
                );
                return Err(e);
            }
        }
    }
}

fn count_existing_tasks(path: &PathBuf) -> usize {
    if !path.exists() {
        return 0;
    }
    let file = File::open(path).ok();
    match file {
        Some(f) => BufReader::new(f)
            .lines()
            .map_while(std::result::Result::ok)
            .filter(|l| !l.trim().is_empty())
            .count(),
        None => 0,
    }
}

#[cfg(test)]
fn share(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 * 100.0 / total as f64
    }
}

#[cfg(test)]
fn generate_readme(
    args: &GenerateArgs,
    stats: &RunStats,
    taxonomy_kind: taxonomy::TaxonomyKind,
    dist: &HashMap<String, f64>,
    diff_dist: &HashMap<u8, f64>,
    lang_counts: Option<&HashMap<String, usize>>,
    observed: &DatasetCounts,
) -> String {
    let input_cost = args
        .input_price
        .map(|p| p * stats.total_input_tokens as f64 / 1_000_000.0);
    let output_cost = args
        .output_price
        .map(|p| p * stats.total_output_tokens as f64 / 1_000_000.0);
    let total_cost = match (input_cost, output_cost) {
        (Some(i), Some(o)) => Some(i + o),
        _ => None,
    };

    let n = observed.n.max(stats.total_tasks);

    let mut md = String::new();

    md.push_str("# TaskGen Dataset\n\n");
    md.push_str("> Generated with **taskgen**\n\n");

    md.push_str("## Run Parameters\n\n");
    md.push_str("| Parameter | Value |\n|---|---|\n");
    md.push_str(&format!("| Model | `{}` |\n", args.model));
    md.push_str(&format!("| Temperature | `{}` |\n", args.temperature));
    md.push_str(&format!("| Total Tasks | {} |\n", n));
    md.push_str(&format!(
        "| Unique Domains | {} |\n",
        observed.domains.len()
    ));
    md.push_str(&format!(
        "| Unique Subdomains | {} |\n",
        observed.subdomains.len()
    ));
    md.push_str(&format!("| Concurrency | {} workers |\n", args.workers));
    md.push_str(&format!("| API Base | `{}` |\n", args.api_base));
    md.push_str(&format!(
        "| Taxonomy | `{}` |\n",
        args.taxonomy
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "embedded docs/it-ops-taxonomy.yaml".into())
    ));
    if let Some(seed) = args.seed {
        md.push_str(&format!("| Coordinate Seed | `{seed}` |\n"));
    }
    md.push_str(&format!(
        "| Generated | {} |\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    if let Some(b) = args.budget {
        md.push_str(&format!("| Budget Cap | ${:.4} |\n", b));
    }
    if args.multilingual {
        md.push_str("| Multilingual | Yes (en, de, fr, es, nl, zh, ar, ru) |\n");
    }
    md.push('\n');

    if let Some(counts) = lang_counts {
        md.push_str("## Language Distribution\n\n");
        md.push_str("| Language | Code | Tasks |\n|---|---|---|\n");
        let mut sorted: Vec<_> = counts.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (code, count) in &sorted {
            let name = LANGUAGES
                .iter()
                .find(|(c, _)| *c == code.as_str())
                .map(|(_, n)| *n)
                .unwrap_or("Unknown");
            md.push_str(&format!("| {} | `{}` | {} |\n", name, code, count));
        }
        md.push('\n');
    }

    let compositional = taxonomy_kind == taxonomy::TaxonomyKind::Compositional;
    md.push_str(if compositional {
        "## Sampling Domain Distribution\n\n"
    } else {
        "## Category Distribution\n\n"
    });
    md.push_str("Target sampling weights vs counts in this JSONL.\n\n");
    md.push_str(if compositional {
        "| Domain | Tasks | Share | Target |\n|---|---:|---:|---:|\n"
    } else {
        "| Category | Tasks | Share | Target |\n|---|---:|---:|---:|\n"
    });
    let mut sorted_cats: Vec<_> = dist.iter().collect();
    sorted_cats.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap().then_with(|| a.0.cmp(b.0)));
    for (cat, w) in &sorted_cats {
        let count = if compositional {
            observed
                .domains
                .get(&format!("enterprise_netops::{cat}"))
                .copied()
                .unwrap_or(0)
        } else {
            observed.categories.get(*cat).copied().unwrap_or(0)
        };
        md.push_str(&format!(
            "| {} | {} | {:.1}% | {:.1}% |\n",
            cat,
            count,
            share(count, n),
            **w * 100.0
        ));
    }
    if !compositional {
        for (cat, count) in sorted_count_rows(&observed.categories) {
            if dist.contains_key(cat) {
                continue;
            }
            md.push_str(&format!(
                "| {} | {} | {:.1}% | — |\n",
                cat,
                count,
                share(count, n)
            ));
        }
    }
    md.push('\n');

    md.push_str("## Domain Distribution\n\n");
    md.push_str("Actual `category::domain` values in this JSONL.\n\n");
    md.push_str("| Domain | Tasks | Share |\n|---|---:|---:|\n");
    if observed.domains.is_empty() {
        md.push_str("| — | 0 | 0.0% |\n");
    } else {
        for (domain, count) in sorted_count_rows(&observed.domains) {
            md.push_str(&format!(
                "| `{}` | {} | {:.1}% |\n",
                domain,
                count,
                share(count, n)
            ));
        }
    }
    md.push('\n');

    md.push_str("## Subdomain Distribution\n\n");
    md.push_str("Actual product lines / failure modes in this JSONL.\n\n");
    md.push_str("| Domain | Subdomain | Tasks | Share |\n|---|---|---:|---:|\n");
    if observed.subdomains.is_empty() {
        md.push_str("| — | — | 0 | 0.0% |\n");
    } else {
        let mut subs: Vec<_> = observed
            .subdomains
            .iter()
            .map(|((d, s), c)| (d, s, *c))
            .collect();
        subs.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.cmp(b.0))
                .then_with(|| a.1.cmp(b.1))
        });
        for (domain, subdomain, count) in subs {
            md.push_str(&format!(
                "| `{}` | `{}` | {} | {:.1}% |\n",
                domain,
                subdomain,
                count,
                share(count, n)
            ));
        }
    }
    md.push('\n');

    md.push_str("## Difficulty Distribution\n\n");
    md.push_str("| Level | Label | Tasks | Share | Target |\n|---|---|---:|---:|---:|\n");
    for d in 1..=10u8 {
        let count = observed.difficulties.get(&d).copied().unwrap_or(0);
        let target = diff_dist.get(&d).copied().unwrap_or(0.0) * 100.0;
        if count == 0 && target == 0.0 {
            continue;
        }
        md.push_str(&format!(
            "| {} | {} | {} | {:.1}% | {:.1}% |\n",
            d,
            difficulty_label(d),
            count,
            share(count, n),
            target
        ));
    }
    md.push('\n');

    md.push_str("## Token Usage & Cost\n\n");
    md.push_str("| Metric | Value |\n|---|---|\n");
    md.push_str(&format!(
        "| Input Tokens | {} |\n",
        stats.total_input_tokens
    ));
    md.push_str(&format!(
        "| Output Tokens | {} |\n",
        stats.total_output_tokens
    ));
    md.push_str(&format!(
        "| Total Tokens | {} |\n",
        stats.total_input_tokens + stats.total_output_tokens
    ));
    md.push_str(&format!("| Errors | {} |\n", stats.errors));
    if let Some(ic) = input_cost {
        md.push_str(&format!("| Input Cost | ${:.6} |\n", ic));
    }
    if let Some(oc) = output_cost {
        md.push_str(&format!("| Output Cost | ${:.6} |\n", oc));
    }
    if let Some(tc) = total_cost {
        md.push_str(&format!("| **Total Cost** | **${:.6}** |\n", tc));
    }
    if args.input_price.is_none() && args.output_price.is_none() {
        md.push_str(
            "| Cost | *Not calculated (use --input-price and --output-price per M tokens)* |\n",
        );
    }
    md.push('\n');

    md.push_str("## Output Format\n\n");
    md.push_str("Each line in the JSONL file contains:\n\n");
    md.push_str("```json\n");
    md.push_str("{\n");
    if compositional {
        md.push_str("  \"schema_version\": \"scogo.taskgen.task.v2\",\n");
    }
    md.push_str("  \"prompt\": \"...\",\n");
    if compositional {
        md.push_str("  \"domain\": \"enterprise_netops::layer3_routing\",\n");
        md.push_str("  \"subdomain\": \"bgp_route_leak\",\n");
    } else {
        md.push_str("  \"domain\": \"oem::Fortinet\",\n");
        md.push_str("  \"subdomain\": \"fortigate\",\n");
    }
    md.push_str("  \"difficulty\": 5,\n");
    if compositional {
        md.push_str("  \"coordinates\": { \"taxonomy_id\": \"scogo-enterprise-netops-v1\", \"...\": \"...\" },\n");
    }
    if args.multilingual {
        md.push_str("  \"language\": \"en\",\n");
    }
    md.push_str("  \"taskgen_model\": \"gpt-4o-mini\",\n");
    md.push_str("  \"temperature\": 0.9\n");
    md.push_str("}\n");
    md.push_str("```\n");

    md
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate(args) => run_generate(*args).await,
        Command::Dedup(args) => run_dedup(args).await,
        Command::Atif { command } => {
            let (direction, args) = match command {
                AtifCommand::Export(args) => (atif::ConversionDirection::Export, args),
                AtifCommand::Import(args) => (atif::ConversionDirection::Import, args),
            };
            let container = match args.container {
                Some(container) => container.into(),
                None => atif::infer_container(&args.input)?,
            };
            let stats = atif::convert_file(
                direction,
                &args.input,
                &args.output,
                container,
                args.overwrite,
            )?;
            println!("converted {} record(s)", stats.records);
            Ok(())
        }
        Command::Taxonomy {
            command: TaxonomyCommand::Validate { taxonomy },
        } => {
            let catalog = taxonomy::TaxonomyCatalog::from_path(&taxonomy)?;
            println!(
                "valid taxonomy: {} ({:?}, {} domains, {} subdomains)",
                catalog.id(),
                catalog.kind(),
                catalog.domain_count(),
                catalog.subdomain_count()
            );
            Ok(())
        }
    }
}

fn dedup_default_path(input: &std::path::Path, suffix: &str) -> Result<PathBuf> {
    let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .context("dedup input must have a UTF-8 file stem")?;
    Ok(parent.join(format!("{stem}.{suffix}.jsonl")))
}

async fn run_dedup(args: DedupArgs) -> Result<()> {
    let output = match args.output {
        Some(path) => path,
        None => dedup_default_path(&args.input, "dedup")?,
    };
    let dropped = match args.dropped {
        Some(path) => path,
        None => dedup_default_path(&args.input, "dropped")?,
    };
    let config = dedup::DedupConfig {
        mode: args.dedup_mode,
        prompt_field: args.prompt_field,
        ngram: args.dedup_ngram,
        jaccard_threshold: args.jaccard_threshold,
        semantic_threshold: args.semantic_threshold,
    };
    config.validate()?;
    let embedder: Option<Arc<dyn dedup::PromptEmbedder>> =
        if args.dedup_mode == dedup::DedupMode::Semantic {
            Some(Arc::new(dedup::FastEmbedder::initialize(
                args.semantic_model
                    .unwrap_or(dedup::SemanticModel::AllMiniLmL6V2),
                args.semantic_model_cache,
            )?))
        } else {
            None
        };
    let stats = dedup::dedup_jsonl(
        dedup::FileDedupOptions {
            input: args.input,
            output: output.clone(),
            dropped: dropped.clone(),
            report: args.report,
            overwrite: args.overwrite,
            config,
        },
        embedder,
    )
    .await?;
    println!(
        "dedup complete: {} kept, {} dropped -> {}",
        stats.kept_records,
        stats.dropped_records,
        output.display()
    );
    println!("dropped records written to {}", dropped.display());
    Ok(())
}

fn resolve_review_system_prompt(
    args: &GenerateArgs,
    taxonomy: &taxonomy::TaxonomyCatalog,
) -> Result<String> {
    if let Some(prompt) = &args.review_system_prompt {
        return Ok(prompt.clone());
    }
    if let Some(path) = &args.review_system_prompt_file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("failed to read review system prompt: {}", path.display()));
    }
    if let Some(path) = taxonomy.default_review_system_prompt_path() {
        return std::fs::read_to_string(&path).with_context(|| {
            format!(
                "failed to read taxonomy review system prompt: {}",
                path.display()
            )
        });
    }
    Ok(include_str!("../prompts/itops-prompt-review-system-v2.txt").to_string())
}

async fn seed_existing_dedup(
    path: &std::path::Path,
    index: &mut dedup::DedupIndex,
    embedder: Option<&Arc<dyn dedup::PromptEmbedder>>,
) -> Result<usize> {
    let file = File::open(path)
        .with_context(|| format!("failed to open existing append dataset: {}", path.display()))?;
    let mut count = 0usize;
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: TaskEntry = serde_json::from_str(&line).with_context(|| {
            format!(
                "invalid existing task at {}:{}",
                path.display(),
                line_index + 1
            )
        })?;
        let embedding = match embedder {
            Some(embedder) => Some(embedder.embed(&entry.prompt).await?),
            None => None,
        };
        index.insert(
            dedup::DedupCandidate {
                prompt: &entry.prompt,
                language: entry.language.as_deref(),
                domain: &entry.domain,
                subdomain: &entry.subdomain,
            },
            embedding,
        )?;
        count += 1;
    }
    Ok(count)
}

fn generation_cost(
    stats: &AtomicStats,
    input_price: Option<f64>,
    output_price: Option<f64>,
) -> f64 {
    match (input_price, output_price) {
        (Some(input), Some(output)) => {
            input * stats.input_tokens.load(Ordering::Relaxed) as f64 / 1_000_000.0
                + output * stats.output_tokens.load(Ordering::Relaxed) as f64 / 1_000_000.0
        }
        _ => 0.0,
    }
}

fn review_cost(stats: &AtomicStats, input_price: Option<f64>, output_price: Option<f64>) -> f64 {
    match (input_price, output_price) {
        (Some(input), Some(output)) => {
            input * stats.review_input_tokens.load(Ordering::Relaxed) as f64 / 1_000_000.0
                + output * stats.review_output_tokens.load(Ordering::Relaxed) as f64 / 1_000_000.0
        }
        _ => 0.0,
    }
}

struct GenerationReportContext<'a> {
    run_id: &'a str,
    started_at: chrono::DateTime<chrono::Utc>,
    args: &'a GenerateArgs,
    taxonomy: &'a taxonomy::TaxonomyCatalog,
    generation_provider: &'a provider::ProviderConfig,
    review_provider: &'a provider::ProviderConfig,
    semantic_model_id: &'a str,
    existing_records: usize,
    paths: &'a artifacts::PublishedPaths,
}

struct GenerationReportOutcome<'a> {
    status: &'a str,
    terminal_error: Option<&'a str>,
    elapsed: std::time::Duration,
    final_records: usize,
    stats: &'a AtomicStats,
    generation_requests: telemetry::RequestTelemetrySnapshot,
    review_requests: telemetry::RequestTelemetrySnapshot,
}

fn runtime_worker_threads() -> usize {
    std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|value| value.get())
                .unwrap_or(1)
        })
}

fn artifact_descriptor(path: &std::path::Path, published_name: &str) -> Result<serde_json::Value> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect run artifact: {}", path.display()))?;
    let mut file = File::open(path)
        .with_context(|| format!("failed to hash run artifact: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(serde_json::json!({
        "file": published_name,
        "bytes": metadata.len(),
        "sha256": format!("{:x}", hasher.finalize()),
    }))
}

fn rejection_summary(path: &std::path::Path) -> Result<serde_json::Value> {
    let mut by_stage: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_reason: std::collections::BTreeMap<String, u64> = Default::default();
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "invalid rejection event at {}:{}",
                path.display(),
                index + 1
            )
        })?;
        if let Some(stage) = value.get("stage").and_then(serde_json::Value::as_str) {
            *by_stage.entry(stage.to_string()).or_default() += 1;
        }
        if let Some(reasons) = value
            .get("decision")
            .and_then(|decision| decision.get("reason_codes"))
            .and_then(serde_json::Value::as_array)
        {
            for reason in reasons.iter().filter_map(serde_json::Value::as_str) {
                *by_reason.entry(reason.to_string()).or_default() += 1;
            }
        }
    }
    Ok(serde_json::json!({"by_stage": by_stage, "by_reason": by_reason}))
}

fn generation_run_report(
    context: &GenerationReportContext<'_>,
    outcome: GenerationReportOutcome<'_>,
) -> Result<serde_json::Value> {
    let completed_at = chrono::Utc::now();
    let duration_seconds = outcome.elapsed.as_secs_f64();
    let duration_minutes = duration_seconds / 60.0;
    let accepted = outcome.stats.tasks.load(Ordering::Relaxed);
    let candidate_attempts = outcome.stats.attempts.load(Ordering::Relaxed);
    let task_artifact = if outcome.status == "success" {
        artifact_descriptor(&context.paths.partial, "tasks.jsonl")?
    } else {
        artifact_descriptor(&context.paths.partial, "accepted.partial.jsonl")?
    };
    let logical_cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);

    Ok(serde_json::json!({
        "schema_version": "scogo.taskgen.run.v2",
        "command_version": env!("CARGO_PKG_VERSION"),
        "run_id": context.run_id,
        "status": outcome.status,
        "terminal_error": outcome.terminal_error,
        "started_at": context.started_at.to_rfc3339(),
        "completed_at": completed_at.to_rfc3339(),
        "duration_seconds": duration_seconds,
        "duration_minutes": duration_minutes,
        "run_directory": context.paths.run_dir,
        "taxonomy_id": context.taxonomy.id(),
        "taxonomy_kind": format!("{:?}", context.taxonomy.kind()).to_ascii_lowercase(),
        "requested_new_records": context.args.count,
        "accepted_new_records": accepted,
        "existing_records": context.existing_records,
        "final_records": outcome.final_records,
        "candidate_attempts": candidate_attempts,
        "rejected_candidates": outcome.stats.errors.load(Ordering::Relaxed),
        "concurrency": {
            "acceptance_workers": context.args.workers,
            "runtime": "tokio-multi-thread",
            "runtime_worker_threads": runtime_worker_threads(),
            "logical_cpus": logical_cpus,
        },
        "generation": {
            "model": context.args.model,
            "endpoint_origin": context.generation_provider.api_base.origin().ascii_serialization(),
            "input_tokens": outcome.stats.input_tokens.load(Ordering::Relaxed),
            "output_tokens": outcome.stats.output_tokens.load(Ordering::Relaxed),
            "priced_cost": generation_cost(outcome.stats, context.args.input_price, context.args.output_price),
        },
        "review": {
            "model": context.review_provider.model,
            "endpoint_origin": context.review_provider.api_base.origin().ascii_serialization(),
            "input_tokens": outcome.stats.review_input_tokens.load(Ordering::Relaxed),
            "output_tokens": outcome.stats.review_output_tokens.load(Ordering::Relaxed),
            "priced_cost": review_cost(
                outcome.stats,
                context.args.review_input_price.or(context.args.input_price),
                context.args.review_output_price.or(context.args.output_price),
            ),
        },
        "timing": {
            "generation_total_ms": outcome.generation_requests.total_ms,
            "review_total_ms": outcome.review_requests.total_ms,
        },
        "requests": {
            "generation": outcome.generation_requests,
            "review": outcome.review_requests,
        },
        "throughput": {
            "tasks_per_minute": if duration_minutes > 0.0 { accepted as f64 / duration_minutes } else { 0.0 },
            "candidates_per_minute": if duration_minutes > 0.0 { candidate_attempts as f64 / duration_minutes } else { 0.0 },
        },
        "rejections": rejection_summary(&context.paths.rejected)?,
        "dedup": {
            "mode": context.args.dedup_mode,
            "ngram": context.args.dedup_ngram,
            "jaccard_threshold": context.args.jaccard_threshold,
            "semantic_threshold": context.args.semantic_threshold,
            "semantic_model": context.semantic_model_id,
        },
        "artifacts": {
            "tasks": task_artifact,
            "reviews": artifact_descriptor(&context.paths.reviews, "reviews.jsonl")?,
            "rejected": artifact_descriptor(&context.paths.rejected, "rejected.jsonl")?,
            "run": {"file":"run.json"},
        },
    }))
}

fn select_terminal_error(
    execution_error: Option<String>,
    signal_reason: Option<&str>,
    accepted: usize,
    requested: usize,
    staged: usize,
    expected_staged: usize,
) -> Option<String> {
    if let Some(signal) = signal_reason {
        return Some(format!("run interrupted by {signal}"));
    }
    execution_error
        .or_else(|| {
            (accepted != requested)
                .then(|| format!("generation accepted {accepted} records, expected {requested}"))
        })
        .or_else(|| {
            (staged != expected_staged).then(|| {
                format!("staged dataset contains {staged} records, expected {expected_staged}")
            })
        })
}

fn spawn_shutdown_listener(
    cancel: Arc<AtomicBool>,
    signal_reason: Arc<std::sync::Mutex<Option<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        let reason = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = terminate.recv() => "SIGTERM",
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        };
        #[cfg(not(unix))]
        let reason = {
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        };

        *signal_reason.lock().unwrap() = Some(reason.to_string());
        cancel.store(true, Ordering::Relaxed);
    })
}

async fn run_generate(args: GenerateArgs) -> Result<()> {
    use review::CandidateReviewer;

    let started_at = chrono::Utc::now();
    let started_clock = std::time::Instant::now();
    let taxonomy = match args.taxonomy.as_deref() {
        Some(path) => taxonomy::TaxonomyCatalog::from_path(path)?,
        None => taxonomy::TaxonomyCatalog::embedded_itops()?,
    };
    let dist = match &args.distribution {
        Some(value) => parse_distribution(value)?,
        None => taxonomy.default_distribution(),
    };
    let diff_dist = match &args.difficulty {
        Some(value) => parse_difficulty(value)?,
        None => taxonomy.default_difficulty(),
    };
    taxonomy.validate_sampling_distributions(&dist, &diff_dist)?;
    let system_prompt = resolve_system_prompt(&args, &taxonomy)?;
    let review_system_prompt = resolve_review_system_prompt(&args, &taxonomy)?;

    let generation_credentials = provider::load_credential_pool(
        args.keyfile.as_deref(),
        args.api_key.clone(),
        "generation",
    )?;
    let effective_api_base = if args.free_models {
        OPENROUTER_API_BASE
    } else {
        &args.api_base
    };
    let generation_provider = provider::ProviderConfig {
        api_base: provider::normalize_api_base(effective_api_base)?,
        model: args.model.clone(),
        credentials: generation_credentials,
    };
    let review_credentials = if args.review_keyfile.is_some() || args.review_api_key.is_some() {
        Some(provider::load_credential_pool(
            args.review_keyfile.as_deref(),
            args.review_api_key.clone(),
            "review",
        )?)
    } else {
        None
    };
    let review_provider = provider::resolve_review_provider(
        &generation_provider,
        provider::ProviderOverrides {
            api_base: args.review_api_base.clone(),
            model: args.review_model.clone(),
            credentials: review_credentials,
        },
    )?;

    let dedup_config = dedup::DedupConfig {
        mode: args.dedup_mode,
        prompt_field: "prompt".into(),
        ngram: args.dedup_ngram,
        jaccard_threshold: args.jaccard_threshold,
        semantic_threshold: args.semantic_threshold,
    };
    dedup_config.validate()?;
    let effective_semantic_model = args.semantic_model.unwrap_or(if args.multilingual {
        dedup::SemanticModel::MultilingualE5Small
    } else {
        dedup::SemanticModel::AllMiniLmL6V2
    });
    let embedder: Option<Arc<dyn dedup::PromptEmbedder>> =
        if args.dedup_mode == dedup::DedupMode::Semantic {
            Some(Arc::new(dedup::FastEmbedder::initialize(
                effective_semantic_model,
                args.semantic_model_cache.clone(),
            )?))
        } else {
            None
        };
    let mut dedup_index = dedup::DedupIndex::new(dedup_config.clone(), embedder.clone())?;
    let existing = if let Some(source) = args.append_from.as_deref() {
        seed_existing_dedup(source, &mut dedup_index, embedder.as_ref()).await?
    } else {
        0
    };
    if existing > 0 {
        println!("Loaded {existing} existing tasks into the append dedup index");
    }

    let dedup_index = Arc::new(std::sync::Mutex::new(dedup_index));

    let clients: Arc<Vec<reqwest::Client>> = Arc::new(match &args.proxies {
        Some(proxy_path) => {
            let proxies = load_proxies(proxy_path)?;
            let timeout = std::time::Duration::from_secs(args.request_timeout_seconds);
            if args.rotating_proxy {
                let index = thread_rng().gen_range(0..proxies.len());
                vec![
                    reqwest::Client::builder()
                        .proxy(proxies.into_iter().nth(index).unwrap())
                        .timeout(timeout)
                        .build()?,
                ]
            } else {
                build_clients(&proxies, timeout)
            }
        }
        None => vec![
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(args.request_timeout_seconds))
                .build()?,
        ],
    });
    let proxy_counter = Arc::new(AtomicUsize::new(0));

    let free_models: Option<Arc<Vec<String>>> = if args.free_models {
        let credential = generation_provider.credentials.next();
        Some(Arc::new(
            fetch_free_models(&reqwest::Client::new(), credential.expose()).await?,
        ))
    } else {
        None
    };
    let model_counter = Arc::new(AtomicUsize::new(0));

    let mut rng = match args.seed {
        Some(seed) => StdRng::seed_from_u64(seed),
        None => StdRng::from_entropy(),
    };
    let slots: Vec<(taxonomy::SampledTask, Option<String>)> = (0..args.count)
        .map(|_| {
            let sample = taxonomy.sample(&mut rng, &dist, &diff_dist)?;
            let language = args.multilingual.then(|| {
                let index = rng.gen_range(0..LANGUAGES.len());
                LANGUAGES[index].0.to_string()
            });
            Ok((sample, language))
        })
        .collect::<Result<_>>()?;
    let slots = Arc::new(slots);
    let progress_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} | {msg}")?
        .progress_chars("##-");

    let run_id = format!("{:08x}", rand::random::<u32>());
    let timestamp = started_at.format("%Y%m%dT%H%M%SZ").to_string();
    let run_dir = args.run_dir.clone().unwrap_or_else(|| {
        artifacts::automatic_run_dir(
            std::path::Path::new("taskgen-runs"),
            &timestamp,
            taxonomy.id(),
            &run_id,
        )
    });
    let initial_report = serde_json::json!({
        "schema_version": "scogo.taskgen.run.v2",
        "run_id": run_id,
        "status": "running",
        "started_at": started_at.to_rfc3339(),
        "taxonomy_id": taxonomy.id(),
        "taxonomy_kind": format!("{:?}", taxonomy.kind()).to_ascii_lowercase(),
        "requested_new_records": args.count,
        "concurrency": {
            "acceptance_workers": args.workers,
            "runtime": "tokio-multi-thread",
            "runtime_worker_threads": runtime_worker_threads(),
            "logical_cpus": std::thread::available_parallelism().map(|value| value.get()).unwrap_or(1),
        }
    });
    let artifacts =
        artifacts::RunArtifacts::create(&run_dir, args.append_from.as_deref(), &initial_report)?;
    let run_paths = artifacts.paths().clone();
    println!("Run directory: {}", run_dir.display());
    let artifacts = Arc::new(std::sync::Mutex::new(Some(artifacts)));

    let stats = Arc::new(AtomicStats::new());
    let generation_telemetry = Arc::new(telemetry::RequestTelemetry::default());
    let review_telemetry = Arc::new(telemetry::RequestTelemetry::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let signal_reason = Arc::new(std::sync::Mutex::new(None));
    let shutdown_listener = spawn_shutdown_listener(cancel.clone(), signal_reason.clone());
    let consecutive_timeouts = Arc::new(AtomicUsize::new(0));
    let pb = ProgressBar::new(args.count as u64);
    pb.set_style(progress_style);
    pb.set_message("waiting for accepted prompts");
    let workers = args.workers;
    let results: Vec<Result<()>> = stream::iter(0..args.count)
        .map(|slot_index| {
            let slots = slots.clone();
            let clients = clients.clone();
            let proxy_counter = proxy_counter.clone();
            let generation_provider = generation_provider.clone();
            let review_provider = review_provider.clone();
            let free_models = free_models.clone();
            let model_counter = model_counter.clone();
            let artifacts = artifacts.clone();
            let dedup_index = dedup_index.clone();
            let embedder = embedder.clone();
            let stats = stats.clone();
            let generation_telemetry = generation_telemetry.clone();
            let review_telemetry = review_telemetry.clone();
            let cancel = cancel.clone();
            let consecutive_timeouts = consecutive_timeouts.clone();
            let pb = pb.clone();
            let system_prompt = system_prompt.clone();
            let review_system_prompt = review_system_prompt.clone();
            let taxonomy_id = taxonomy.id().to_string();
            let taxonomy_kind = format!("{:?}", taxonomy.kind()).to_ascii_lowercase();
            let explicit_review_model = args.review_model.is_some();
            let temperature = args.temperature;
            let max_output_tokens = args.max_output_tokens;
            let review_max_output_tokens = args.review_max_output_tokens.unwrap_or_else(|| {
                if review_provider.model.to_ascii_lowercase().contains("qwen") {
                    4096
                } else {
                    1024
                }
            });
            let max_attempts = args.max_attempts_per_slot;
            let input_price = args.input_price;
            let output_price = args.output_price;
            let review_input_price = args.review_input_price.or(args.input_price);
            let review_output_price = args.review_output_price.or(args.output_price);
            let budget = args.budget;

            async move {
                let (sample, language) = &slots[slot_index];
                let mut retry_guidance = String::new();
                for attempt in 1..=max_attempts {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("slot {} cancelled before acceptance", slot_index + 1);
                    }
                    if let Some(limit) = budget {
                        let spent = generation_cost(&stats, input_price, output_price)
                            + review_cost(&stats, review_input_price, review_output_price);
                        if spent >= limit {
                            cancel.store(true, Ordering::Relaxed);
                            bail!("budget exhausted before slot {} was accepted", slot_index + 1);
                        }
                    }
                    stats.attempts.fetch_add(1, Ordering::Relaxed);
                    let use_model = match &free_models {
                        Some(models) => {
                            let index = model_counter.fetch_add(1, Ordering::Relaxed) % models.len();
                            models[index].clone()
                        }
                        None => generation_provider.model.clone(),
                    };
                    let client_index =
                        proxy_counter.fetch_add(1, Ordering::Relaxed) % clients.len();
                    let client = &clients[client_index];
                    let credential = generation_provider.credentials.next();
                    let generated = generate_task(GenerateTaskRequest {
                        client,
                        api_base: generation_provider.api_base.as_str(),
                        api_key: credential.expose(),
                        model: &use_model,
                        system_prompt: &system_prompt,
                        sample,
                        temperature,
                        max_output_tokens,
                        language: language.as_deref(),
                        retry_guidance: (!retry_guidance.is_empty()).then_some(retry_guidance.as_str()),
                        cancel: &cancel,
                        consecutive_timeouts: &consecutive_timeouts,
                        progress: &pb,
                        telemetry: &generation_telemetry,
                    })
                    .await;
                    let (prompt, input_tokens, output_tokens) = match generated {
                        Ok(result) => result,
                        Err(error) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            let event = serde_json::json!({
                                "schema_version": "scogo.taskgen.rejection.v1",
                                "slot": slot_index + 1,
                                "attempt": attempt,
                                "stage": "generation",
                                "reason": error.to_string(),
                                "coordinate": sample,
                            });
                            artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                            if cancel.load(Ordering::Relaxed) {
                                bail!("slot {} generation stopped: {error}", slot_index + 1);
                            }
                            continue;
                        }
                    };
                    stats.input_tokens.fetch_add(input_tokens, Ordering::Relaxed);
                    stats.output_tokens.fetch_add(output_tokens, Ordering::Relaxed);
                    let entry = TaskEntry {
                        schema_version: Some("scogo.taskgen.task.v2".into()),
                        prompt,
                        category: sample.category_id.clone(),
                        domain: sample.domain_id.clone(),
                        subdomain: sample.subdomain_id.clone(),
                        difficulty: sample.difficulty,
                        coordinates: sample.coordinates.clone(),
                        language: language.clone(),
                        taskgen_model: use_model.clone(),
                        temperature,
                    };
                    let line = match serialize_task_entry(&entry) {
                        Ok(line) => line,
                        Err(error) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            let event = serde_json::json!({
                                "schema_version": "scogo.taskgen.rejection.v1",
                                "slot": slot_index + 1,
                                "attempt": attempt,
                                "stage": "schema_validation",
                                "reason": error.to_string(),
                                "candidate": entry,
                            });
                            artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                            continue;
                        }
                    };
                    let embedding = match &embedder {
                        Some(embedder) => Some(embedder.embed(&entry.prompt).await?),
                        None => None,
                    };
                    let candidate = dedup::DedupCandidate {
                        prompt: &entry.prompt,
                        language: entry.language.as_deref(),
                        domain: &entry.domain,
                        subdomain: &entry.subdomain,
                    };
                    let pre_duplicate = dedup_index
                        .lock()
                        .unwrap()
                        .find_duplicate(&candidate, embedding.as_deref())?;
                    if let Some(duplicate) = pre_duplicate {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        retry_guidance = "Generate a materially different incident scenario for the same coordinates.".into();
                        let event = serde_json::json!({
                            "schema_version": "scogo.taskgen.rejection.v1",
                            "slot": slot_index + 1,
                            "attempt": attempt,
                            "stage": "dedup_precheck",
                            "duplicate": duplicate,
                            "candidate": entry,
                        });
                        artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                        continue;
                    }

                    let mut effective_review_provider = review_provider.clone();
                    if free_models.is_some() && !explicit_review_model {
                        effective_review_provider.model = use_model.clone();
                    }
                    let reviewer = review::ReviewClient::new(
                        effective_review_provider,
                        client.clone(),
                        review_max_output_tokens,
                        review_telemetry.clone(),
                    )?;
                    let review = match reviewer
                        .review(review::ReviewRequest {
                            candidate: serde_json::to_value(&entry)?,
                            taxonomy_id: taxonomy_id.clone(),
                            taxonomy_kind: taxonomy_kind.clone(),
                            system_prompt: review_system_prompt.clone(),
                        })
                        .await
                    {
                        Ok(review) => review,
                        Err(error) => {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            retry_guidance = "Return a technically grounded, internally consistent replacement.".into();
                            let event = serde_json::json!({
                                "schema_version": "scogo.taskgen.rejection.v1",
                                "slot": slot_index + 1,
                                "attempt": attempt,
                                "stage": "review_error",
                                "reason": error.to_string(),
                                "candidate": entry,
                            });
                            artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                            continue;
                        }
                    };
                    stats
                        .review_input_tokens
                        .fetch_add(review.input_tokens, Ordering::Relaxed);
                    stats
                        .review_output_tokens
                        .fetch_add(review.output_tokens, Ordering::Relaxed);
                    if review.decision.verdict == review::ReviewVerdict::Reject {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        retry_guidance = review.decision.retry_guidance.clone();
                        let event = serde_json::json!({
                            "schema_version": "scogo.taskgen.rejection.v1",
                            "slot": slot_index + 1,
                            "attempt": attempt,
                            "stage": "model_review",
                            "review_model": review.model,
                            "decision": review.decision,
                            "candidate": entry,
                        });
                        artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                        continue;
                    }

                    let final_duplicate = {
                        let mut index = dedup_index.lock().unwrap();
                        if let Some(duplicate) =
                            index.find_duplicate(&candidate, embedding.as_deref())?
                        {
                            Some(duplicate)
                        } else {
                            index.insert(candidate, embedding)?;
                            None
                        }
                    };
                    if let Some(duplicate) = final_duplicate {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        retry_guidance = "Generate a materially different incident scenario for the same coordinates.".into();
                        let event = serde_json::json!({
                            "schema_version": "scogo.taskgen.rejection.v1",
                            "slot": slot_index + 1,
                            "attempt": attempt,
                            "stage": "dedup_final",
                            "duplicate": duplicate,
                            "review_model": review.model,
                            "decision": review.decision,
                            "candidate": entry,
                        });
                        artifacts.lock().unwrap().as_mut().unwrap().write_rejection(&event)?;
                        continue;
                    }

                    let review_event = serde_json::json!({
                        "schema_version": "scogo.taskgen.accepted-review.v1",
                        "slot": slot_index + 1,
                        "attempt": attempt,
                        "prompt_sha256": dedup::prompt_sha256(&entry.prompt),
                        "review_model": review.model,
                        "input_tokens": review.input_tokens,
                        "output_tokens": review.output_tokens,
                        "decision": review.decision,
                    });
                    {
                        let mut guard = artifacts.lock().unwrap();
                        let run = guard.as_mut().unwrap();
                        run.write_accepted_line(&line)?;
                        run.write_review(&review_event)?;
                    }
                    let accepted = stats.tasks.fetch_add(1, Ordering::Relaxed) + 1;
                    let rejected = stats.errors.load(Ordering::Relaxed);
                    pb.inc(1);
                    pb.set_message(format!("{accepted} accepted | {rejected} rejected"));
                    return Ok(());
                }
                bail!(
                    "slot {} exhausted {} attempts without an accepted candidate",
                    slot_index + 1,
                    max_attempts
                )
            }
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    shutdown_listener.abort();

    let execution_error = results
        .into_iter()
        .find_map(Result::err)
        .map(|error| format!("generation incomplete: {error}"));
    let accepted = stats.tasks.load(Ordering::Relaxed);
    let staged_path = artifacts
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .accepted_path()
        .to_path_buf();
    let staged_count = count_existing_tasks(&staged_path);
    let terminal_error = select_terminal_error(
        execution_error,
        signal_reason.lock().unwrap().as_deref(),
        accepted,
        args.count,
        staged_count,
        existing + args.count,
    );
    let report_context = GenerationReportContext {
        run_id: &run_id,
        started_at,
        args: &args,
        taxonomy: &taxonomy,
        generation_provider: &generation_provider,
        review_provider: &review_provider,
        semantic_model_id: effective_semantic_model.model_id(),
        existing_records: existing,
        paths: &run_paths,
    };
    if let Some(terminal_error) = terminal_error {
        cancel.store(true, Ordering::Relaxed);
        pb.abandon_with_message("incomplete; final output not published");
        let report = generation_run_report(
            &report_context,
            GenerationReportOutcome {
                status: "failed",
                terminal_error: Some(&terminal_error),
                elapsed: started_clock.elapsed(),
                final_records: staged_count,
                stats: &stats,
                generation_requests: generation_telemetry.snapshot(),
                review_requests: review_telemetry.snapshot(),
            },
        )?;
        let run = artifacts.lock().unwrap().take().unwrap();
        run.finish_incomplete(&report)?;
        bail!(
            "{terminal_error}. Partial audit artifacts retained at {}",
            run_dir.display()
        );
    }

    let report = generation_run_report(
        &report_context,
        GenerationReportOutcome {
            status: "success",
            terminal_error: None,
            elapsed: started_clock.elapsed(),
            final_records: staged_count,
            stats: &stats,
            generation_requests: generation_telemetry.snapshot(),
            review_requests: review_telemetry.snapshot(),
        },
    )?;
    let run = artifacts.lock().unwrap().take().unwrap();
    let published = run.publish(&report)?;
    let final_count = count_existing_tasks(&published.output);
    if final_count != staged_count {
        bail!("published dataset count changed unexpectedly: {final_count} != {staged_count}");
    }
    pb.finish_with_message("exact accepted count published");
    println!(
        "Generated exactly {} newly accepted tasks ({} total) -> {}",
        args.count,
        final_count,
        published.output.display()
    );
    println!("Accepted reviews: {}", published.reviews.display());
    println!("Rejected candidates: {}", published.rejected.display());
    println!("Run report: {}", published.run.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_openai_string_content() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        }"#,
        )
        .unwrap();
        assert_eq!(extract_completion(&v).unwrap(), "hello");
        assert_eq!(extract_usage(&v), (1, 2));
    }

    #[test]
    fn excludes_provider_reasoning_from_completion_content() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "content": "final prompt only",
                        "reasoning_content": "private planning that must not enter the dataset"
                    }
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(extract_completion(&v).unwrap(), "final prompt only");
    }

    #[test]
    fn rejects_length_truncated_completions() {
        let payload = r#"{
            "choices": [{
                "finish_reason": "length",
                "message": {"content": "unfinished prompt"}
            }]
        }"#;
        assert!(
            parse_chat_payload(payload)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }

    #[test]
    fn extracts_content_parts_and_null_usage_fields() {
        let v: serde_json::Value = serde_json::from_str(r#"{
            "choices": [{"message": {"content": [{"type": "text", "text": "part a"}, {"type": "text", "text": "part b"}]}}],
            "usage": {"prompt_tokens": null, "completion_tokens": "7", "input_tokens": 4}
        }"#).unwrap();
        assert_eq!(extract_completion(&v).unwrap(), "part apart b");
        assert_eq!(extract_usage(&v), (4, 7));
    }

    #[test]
    fn surfaces_error_payload() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"error": {"message": "max_tokens not supported"}}"#).unwrap();
        let err = extract_completion(&v).unwrap_err().to_string();
        assert!(err.contains("max_tokens not supported"), "{err}");
    }

    #[test]
    fn classifies_retryable_gateway_statuses() {
        assert!(is_transient_http_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_transient_http_status(
            reqwest::StatusCode::GATEWAY_TIMEOUT
        ));
        assert!(is_transient_http_status(
            reqwest::StatusCode::REQUEST_TIMEOUT
        ));
        assert!(!is_transient_http_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_transient_http_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn unwraps_sse() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\ndata: [DONE]\n";
        let (text, input, output) = parse_chat_payload(body).unwrap();
        assert_eq!(text, "hello");
        assert_eq!((input, output), (3, 2));
    }

    #[test]
    fn gpt5_request_omits_temperature_and_max_tokens() {
        assert!(restricted_sampling("scogoai/gpt-5.6-luna-max"));
        let req = chat_request("scogoai/gpt-5.6-luna-max", vec![], 0.9, 2048);
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("temperature").is_none());
        assert!(v.get("max_tokens").is_none());
        assert_eq!(v.get("max_completion_tokens").unwrap(), 2048);
        assert_eq!(v.get("stream").unwrap(), false);
    }

    #[test]
    fn qwen_request_disables_thinking_without_affecting_other_models() {
        let qwen =
            serde_json::to_value(chat_request("qwen/qwen3.8-max-free", vec![], 0.9, 2048)).unwrap();
        assert_eq!(qwen["enable_thinking"], false);
        assert_eq!(qwen["thinking_budget"], 0);
        assert_eq!(qwen["reasoning_effort"], "low");

        let other = serde_json::to_value(chat_request("gpt-4o-mini", vec![], 0.9, 2048)).unwrap();
        assert!(other.get("enable_thinking").is_none());
        assert!(other.get("thinking_budget").is_none());
        assert!(other.get("reasoning_effort").is_none());
    }

    #[test]
    fn generation_output_budget_is_larger_for_qwen() {
        assert_eq!(
            generation_max_output_tokens("qwen/qwen3.8-max-free", None),
            4096
        );
        assert_eq!(generation_max_output_tokens("gpt-4o-mini", None), 2048);
    }

    #[test]
    fn explicit_generation_output_budget_overrides_model_default() {
        assert_eq!(
            generation_max_output_tokens("qwen/qwen3.8-max-free", Some(3072)),
            3072
        );
        assert_eq!(
            generation_max_output_tokens("gpt-4o-mini", Some(3072)),
            3072
        );
    }

    #[test]
    fn rejects_obvious_model_planning_as_a_prompt() {
        let leaked = "We need answer user's request: generate one task prompt using constraints.";
        assert!(validate_generated_prompt(leaked).is_err());
        assert!(
            validate_generated_prompt("TASK enterprise_netops::layer3_routing/bgp_session")
                .is_err()
        );
        assert!(validate_generated_prompt(&vec!["word"; 801].join(" ")).is_err());
        assert!(validate_generated_prompt("Investigate why the BGP session is flapping.").is_ok());
    }

    #[test]
    fn qwen_user_message_uses_no_think_soft_switch() {
        let qwen = model_user_message("qwen/qwen3.8-max-free", "task");
        assert!(qwen.contains("800 words"));
        assert!(qwen.contains("vendor authenticity and causal consistency"));
        assert!(qwen.ends_with("/no_think"));
        assert_eq!(model_user_message("gpt-4o-mini", "task"), "task");
    }

    #[test]
    fn default_distribution_sums_to_one() {
        let catalog = taxonomy::TaxonomyCatalog::embedded_itops().unwrap();
        let distribution = catalog.default_distribution();
        let sum: f64 = distribution.values().sum();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        assert!(distribution.contains_key("oem"));
    }

    #[test]
    fn oem_catalog_includes_named_vendors_and_product_lines() {
        let catalog = taxonomy::TaxonomyCatalog::embedded_itops().unwrap();
        assert!(catalog.contains_hierarchical_subdomain("oem", "Fortinet", "fortigate"));
        assert!(catalog.contains_hierarchical_subdomain("oem", "Linux Distros", "debian"));
        let oem_domains = catalog.hierarchical_domain_count("oem");
        assert!(oem_domains >= 40, "{oem_domains}");
    }

    #[test]
    fn oem_user_message_asks_for_product_voice() {
        let sample = taxonomy::SampledTask {
            taxonomy_id: "scogo-itops-v3".into(),
            category_id: "oem".into(),
            domain_id: "fortinet".into(),
            domain_label: "Fortinet".into(),
            subdomain_id: "fortigate".into(),
            coordinates: None,
            difficulty: 6,
        };
        let msg = task_user_message(&sample, None);
        assert!(msg.contains("Vendor/platform: oem::Fortinet"));
        assert!(msg.contains("Product: fortigate"));
        assert!(msg.contains("SKU"));
        assert!(!msg.contains("generic Fortinet task"));
    }

    #[test]
    fn capability_user_message_keeps_failure_mode_wording() {
        let sample = taxonomy::SampledTask {
            taxonomy_id: "scogo-itops-v3".into(),
            category_id: "network".into(),
            domain_id: "firewall".into(),
            domain_label: "Firewall".into(),
            subdomain_id: "unused_rule".into(),
            coordinates: None,
            difficulty: 4,
        };
        let msg = task_user_message(&sample, None);
        assert!(msg.contains("Domain: network::Firewall"));
        assert!(msg.contains("Subdomain: unused_rule"));
    }

    #[test]
    fn dataset_readme_lists_subdomains_and_omits_donate() {
        let cli = Cli::parse_from(["taskgen", "generate", "--api-key", "x"]);
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        let stats = RunStats {
            total_input_tokens: 10,
            total_output_tokens: 20,
            total_tasks: 3,
            errors: 0,
        };
        let dist = HashMap::from([("oem".to_string(), 1.0)]);
        let diff = HashMap::from([(6u8, 1.0)]);
        let mut observed = DatasetCounts::default();
        observed.add("oem::Fortinet", "fortigate", 6);
        observed.add("oem::Fortinet", "fortigate", 6);
        observed.add("oem::AWS", "eks", 6);
        let md = generate_readme(
            &args,
            &stats,
            taxonomy::TaxonomyKind::Hierarchical,
            &dist,
            &diff,
            None,
            &observed,
        );
        assert!(!md.contains("Support / Donate"), "{md}");
        assert!(!md.contains("bc1q"), "{md}");
        assert!(md.contains("## Subdomain Distribution"), "{md}");
        assert!(md.contains("`oem::Fortinet`"), "{md}");
        assert!(md.contains("`fortigate`"), "{md}");
        assert!(md.contains("`eks`"), "{md}");
        assert!(md.contains("| Unique Domains | 2 |"), "{md}");
        assert!(md.contains("| Unique Subdomains | 2 |"), "{md}");
    }

    #[test]
    fn netops_dataset_readme_describes_compositional_records() {
        let cli = Cli::parse_from([
            "taskgen",
            "generate",
            "--api-key",
            "x",
            "--taxonomy",
            "docs/netops-taxonomy.yaml",
            "--seed",
            "42",
        ]);
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        let stats = RunStats {
            total_input_tokens: 10,
            total_output_tokens: 20,
            total_tasks: 1,
            errors: 0,
        };
        let dist = HashMap::from([("layer3_routing".to_string(), 1.0)]);
        let diff = HashMap::from([(8u8, 1.0)]);
        let mut observed = DatasetCounts::default();
        observed.add("enterprise_netops::layer3_routing", "bgp_route_leak", 8);
        let md = generate_readme(
            &args,
            &stats,
            taxonomy::TaxonomyKind::Compositional,
            &dist,
            &diff,
            None,
            &observed,
        );
        assert!(md.contains("scogo.taskgen.task.v2"), "{md}");
        assert!(md.contains("enterprise_netops::layer3_routing"), "{md}");
        assert!(md.contains("coordinates"), "{md}");
        assert!(md.contains("Coordinate Seed | `42`"), "{md}");
        assert!(
            md.contains("| layer3_routing | 1 | 100.0% | 100.0% |"),
            "{md}"
        );
    }

    #[test]
    fn parses_taxonomy_validate_subcommand_without_api_key() {
        let cli = Cli::try_parse_from([
            "taskgen",
            "taxonomy",
            "validate",
            "--taxonomy",
            "docs/it-ops-taxonomy.yaml",
        ])
        .expect("taxonomy validation command should parse");

        assert!(matches!(
            cli.command,
            Command::Taxonomy {
                command: TaxonomyCommand::Validate { .. }
            }
        ));
    }

    #[test]
    fn parses_atif_import_and_export_without_api_key() {
        for operation in ["import", "export"] {
            let cli = Cli::try_parse_from([
                "taskgen",
                "atif",
                operation,
                "--input",
                "input.jsonl",
                "--output",
                "output.jsonl",
            ])
            .unwrap();
            assert!(matches!(cli.command, Command::Atif { .. }));
        }
    }

    #[test]
    fn parses_generation_output_budget_override() {
        assert!(
            Cli::try_parse_from([
                "taskgen",
                "generate",
                "--api-key",
                "test-key",
                "--max-output-tokens",
                "3072",
            ])
            .is_ok()
        );
    }

    #[test]
    fn generation_uses_run_directory_and_append_source_contract() {
        let cli = Cli::try_parse_from([
            "taskgen",
            "generate",
            "--api-key",
            "test-key",
            "--run-dir",
            "runs/netops-001",
            "--append-from",
            "existing.jsonl",
        ])
        .unwrap();
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        assert_eq!(args.run_dir, Some(PathBuf::from("runs/netops-001")));
        assert_eq!(args.append_from, Some(PathBuf::from("existing.jsonl")));
        assert!(
            Cli::try_parse_from([
                "taskgen",
                "generate",
                "--api-key",
                "test-key",
                "--output",
                "legacy.jsonl",
            ])
            .is_err()
        );
    }

    #[test]
    fn generation_rejects_zero_count_and_zero_workers() {
        for args in [
            ["taskgen", "generate", "--api-key", "x", "--count", "0"],
            ["taskgen", "generate", "--api-key", "x", "--workers", "0"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn signal_reason_overrides_secondary_slot_cancellation_error() {
        let reason = select_terminal_error(
            Some("generation incomplete: slot cancelled".into()),
            Some("SIGTERM"),
            3,
            10,
            3,
            10,
        )
        .unwrap();
        assert_eq!(reason, "run interrupted by SIGTERM");
    }

    #[test]
    fn readme_documents_netops_and_atif_contracts() {
        let readme = include_str!("../README.md");
        for required in [
            "taskgen generate",
            "docs/it-ops-taxonomy.yaml",
            "docs/netops-taxonomy.yaml",
            "--system-prompt-file",
            "taskgen atif export",
            "taskgen atif import",
            "ATIF-v1.7",
            "external_atif_unverified",
            "prompt seeds",
        ] {
            assert!(readme.contains(required), "README missing {required}");
        }
    }

    #[test]
    fn rejects_inexact_or_negative_cli_distributions() {
        assert!(parse_distribution("network=0.99").is_err());
        assert!(parse_distribution("network=1.1,oem=-0.1").is_err());
        assert!(parse_difficulty("d1=0.5,d2=0.49").is_err());
        assert!(parse_difficulty("d1=1.1,d2=-0.1").is_err());
    }

    #[test]
    fn inline_system_prompt_precedes_taxonomy_default() {
        let cli = Cli::parse_from([
            "taskgen",
            "generate",
            "--api-key",
            "test-key",
            "--taxonomy",
            "docs/netops-taxonomy.yaml",
            "--system-prompt",
            "inline prompt",
        ]);
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        let catalog =
            taxonomy::TaxonomyCatalog::from_path(std::path::Path::new("docs/netops-taxonomy.yaml"))
                .unwrap();
        assert_eq!(
            resolve_system_prompt(&args, &catalog).unwrap(),
            "inline prompt"
        );
    }

    #[test]
    fn system_prompt_flags_conflict() {
        assert!(
            Cli::try_parse_from([
                "taskgen",
                "generate",
                "--api-key",
                "test-key",
                "--system-prompt",
                "inline",
                "--system-prompt-file",
                "prompt.txt",
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_help_never_displays_environment_secret_values() {
        use clap::CommandFactory;

        let help = Cli::command().render_long_help().to_string();
        if let Ok(secret) = std::env::var("OPENAI_API_KEY")
            && !secret.is_empty()
        {
            assert!(!help.contains(&secret));
        }
        if let Ok(secret) = std::env::var("TASKGEN_REVIEW_API_KEY")
            && !secret.is_empty()
        {
            assert!(!help.contains(&secret));
        }
    }

    #[test]
    fn keyfiles_parse_even_when_environment_keys_are_present() {
        let cli = Cli::try_parse_from([
            "taskgen",
            "generate",
            "--keyfile",
            "keys.txt",
            "--review-keyfile",
            "review-keys.txt",
        ])
        .unwrap();
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        assert_eq!(args.keyfile.unwrap(), PathBuf::from("keys.txt"));
        assert_eq!(
            args.review_keyfile.unwrap(),
            PathBuf::from("review-keys.txt")
        );
    }

    #[test]
    fn netops_task_message_contains_every_sampled_coordinate() {
        let sample = taxonomy::SampledTask {
            taxonomy_id: "scogo-enterprise-netops-v1".into(),
            category_id: "enterprise_netops".into(),
            domain_id: "layer3_routing".into(),
            domain_label: "Layer 3 Routing".into(),
            subdomain_id: "bgp_route_leak".into(),
            difficulty: 8,
            coordinates: Some(taxonomy::TaskCoordinates {
                taxonomy_id: "scogo-enterprise-netops-v1".into(),
                category_id: "enterprise_netops".into(),
                task_family: "troubleshooting_rca".into(),
                environment: "hybrid".into(),
                platform_scope: "multi_platform".into(),
                platforms: vec!["cisco_ios_xe".into(), "juniper_junos".into()],
                incident_mechanism: "misconfiguration".into(),
                evidence_condition: "contradictory".into(),
                evidence_bundle: "routing_tables".into(),
                action_risk: "read_only_investigation".into(),
                presentation: "war_room".into(),
            }),
        };

        let message = task_user_message(&sample, None);
        for expected in [
            "layer3_routing",
            "bgp_route_leak",
            "troubleshooting_rca",
            "hybrid",
            "multi_platform",
            "cisco_ios_xe",
            "juniper_junos",
            "misconfiguration",
            "contradictory",
            "routing_tables",
            "read_only_investigation",
            "war_room",
            "8/10",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
        assert!(message.contains("mandatory constraints"));
        assert!(message.ends_with("Output only the task prompt, nothing else."));
    }

    #[test]
    fn netops_task_record_serializes_schema_and_coordinates() {
        let entry = TaskEntry {
            schema_version: Some("scogo.taskgen.task.v2".into()),
            prompt: "Investigate the route leak safely.".into(),
            category: "enterprise_netops".into(),
            domain: "layer3_routing".into(),
            subdomain: "bgp_route_leak".into(),
            difficulty: 8,
            coordinates: Some(taxonomy::TaskCoordinates {
                taxonomy_id: "scogo-enterprise-netops-v2".into(),
                category_id: "enterprise_netops".into(),
                task_family: "troubleshooting_rca".into(),
                environment: "hybrid".into(),
                platform_scope: "multi_platform".into(),
                platforms: vec!["cisco_ios_xe".into(), "juniper_junos".into()],
                incident_mechanism: "misconfiguration".into(),
                evidence_condition: "contradictory".into(),
                evidence_bundle: "routing_tables".into(),
                action_risk: "read_only_investigation".into(),
                presentation: "war_room".into(),
            }),
            language: None,
            taskgen_model: "teacher".into(),
            temperature: 0.9,
        };

        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["schema_version"], "scogo.taskgen.task.v2");
        assert_eq!(value["category"], "enterprise_netops");
        assert_eq!(value["domain"], "layer3_routing");
        assert_eq!(value["subdomain"], "bgp_route_leak");
        assert_eq!(
            value["coordinates"]["action_risk"],
            "read_only_investigation"
        );
        serialize_task_entry(&entry).unwrap();
    }

    #[test]
    fn netops_task_record_requires_coordinates_before_write() {
        let entry = TaskEntry {
            schema_version: Some("scogo.taskgen.task.v2".into()),
            prompt: "Investigate safely.".into(),
            category: "enterprise_netops".into(),
            domain: "layer3_routing".into(),
            subdomain: "bgp_route_leak".into(),
            difficulty: 8,
            coordinates: None,
            language: None,
            taskgen_model: "teacher".into(),
            temperature: 0.9,
        };
        assert!(serialize_task_entry(&entry).is_err());
    }

    #[tokio::test]
    async fn generate_replaces_rejected_candidates_until_exact_count_is_published() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct TaskgenResponder {
            generations: Arc<AtomicUsize>,
            reviews: Arc<AtomicUsize>,
        }

        impl Respond for TaskgenResponder {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                if body.contains("Review this prompt seed") {
                    let review_number = self.reviews.fetch_add(1, Ordering::SeqCst) + 1;
                    let decision = if review_number == 1 {
                        serde_json::json!({
                            "schema_version": "scogo.taskgen.prompt-review.v1",
                            "verdict": "reject",
                            "reason_codes": ["ambiguous_or_unanswerable"],
                            "summary": "The first candidate lacks decisive evidence.",
                            "retry_guidance": "Include a concrete observed symptom and one conflicting signal."
                        })
                    } else {
                        serde_json::json!({
                            "schema_version": "scogo.taskgen.prompt-review.v1",
                            "verdict": "accept",
                            "reason_codes": [],
                            "summary": "Operationally coherent and coordinate-aligned.",
                            "retry_guidance": ""
                        })
                    };
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {"content": decision.to_string()}}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                    }));
                }
                let number = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {"content": format!(
                            "Candidate {number}: latency changed after the maintenance window, interface counters disagree with the flow telemetry, and rollback approval is pending—which read-only evidence should we collect first?"
                        )}
                    }],
                    "usage": {"prompt_tokens": 20, "completion_tokens": 12}
                }))
            }
        }

        let server = MockServer::start().await;
        let generations = Arc::new(AtomicUsize::new(0));
        let reviews = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(TaskgenResponder {
                generations: generations.clone(),
                reviews: reviews.clone(),
            })
            .mount(&server)
            .await;

        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("run-001");
        let cli = Cli::try_parse_from([
            "taskgen",
            "generate",
            "--api-base",
            &format!("{}/v1", server.uri()),
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--taxonomy",
            "docs/netops-taxonomy.yaml",
            "--count",
            "2",
            "--workers",
            "2",
            "--dedup-mode",
            "lexical",
            "--max-attempts-per-slot",
            "5",
            "--run-dir",
            run_dir.to_str().unwrap(),
        ])
        .unwrap();
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };
        run_generate(*args).await.unwrap();

        let paths = artifacts::PublishedPaths::for_run_dir(&run_dir);
        assert_eq!(
            std::fs::read_to_string(&paths.output)
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(paths.reviews)
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(
            std::fs::read_to_string(paths.rejected)
                .unwrap()
                .lines()
                .count()
                >= 1
        );
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(paths.run).unwrap()).unwrap();
        assert_eq!(report["accepted_new_records"], 2);
        assert_eq!(report["final_records"], 2);
        assert_eq!(report["status"], "success");
        assert_eq!(report["concurrency"]["acceptance_workers"], 2);
        assert!(report["started_at"].as_str().is_some());
        assert!(report["completed_at"].as_str().is_some());
        assert!(report["duration_seconds"].as_f64().unwrap() >= 0.0);
        assert!(report["duration_minutes"].as_f64().unwrap() >= 0.0);
        assert!(report["throughput"]["tasks_per_minute"].as_f64().unwrap() >= 0.0);
        assert!(report["timing"]["generation_total_ms"].as_u64().is_some());
        assert!(report["timing"]["review_total_ms"].as_u64().is_some());
        let generation_calls = generations.load(Ordering::SeqCst);
        let review_calls = reviews.load(Ordering::SeqCst);
        assert_eq!(
            report["requests"]["generation"]["requests"],
            generation_calls
        );
        assert_eq!(report["requests"]["review"]["requests"], review_calls);
        assert_eq!(
            report["rejections"]["by_reason"]["ambiguous_or_unanswerable"],
            1
        );
        assert_eq!(report["artifacts"]["tasks"]["file"], "tasks.jsonl");
        assert_eq!(
            report["artifacts"]["tasks"]["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert!(generation_calls >= 3);
        assert!(review_calls >= 3);
    }

    #[tokio::test]
    async fn failed_generation_finishes_report_and_keeps_partial_artifacts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct FailingReviewer;

        impl Respond for FailingReviewer {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let body = String::from_utf8_lossy(&request.body);
                if body.contains("Review this prompt seed") {
                    return ResponseTemplate::new(400).set_body_string("review unavailable");
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices":[{"finish_reason":"stop","message":{"content":
                        "Latency changed after maintenance and two read-only telemetry sources disagree; which evidence should the on-call collect next?"
                    }}],
                    "usage":{"prompt_tokens":20,"completion_tokens":12}
                }))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(FailingReviewer)
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("failed-run");
        let cli = Cli::try_parse_from([
            "taskgen",
            "generate",
            "--api-base",
            &format!("{}/v1", server.uri()),
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--taxonomy",
            "docs/netops-taxonomy.yaml",
            "--count",
            "1",
            "--workers",
            "1",
            "--dedup-mode",
            "lexical",
            "--max-attempts-per-slot",
            "1",
            "--run-dir",
            run_dir.to_str().unwrap(),
        ])
        .unwrap();
        let Command::Generate(args) = cli.command else {
            panic!("expected generate command");
        };

        assert!(run_generate(*args).await.is_err());
        let paths = artifacts::PublishedPaths::for_run_dir(&run_dir);
        assert!(!paths.output.exists());
        assert!(paths.partial.exists());
        assert!(paths.reviews.exists());
        assert!(paths.rejected.exists());
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(paths.run).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert!(report["completed_at"].as_str().is_some());
        assert!(report["duration_seconds"].as_f64().unwrap() >= 0.0);
        assert!(
            report["terminal_error"]
                .as_str()
                .unwrap()
                .contains("exhausted")
        );
    }
}
