use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use rand::SeedableRng;
use rand::prelude::*;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};

pub mod atif;
pub mod schema;
pub mod taxonomy;

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

const OEM_SYSTEM_ADDENDUM: &str = r#"

When the domain is a vendor, ISV, or platform, the subdomain is a product line — not a generic failure mode. Write as an operator of THAT product. Use real SKU, firmware, CLI, console, TAC, and license language an operator would actually type in Slack. Do not write a generic capability ticket (no "firewall HA" if the product is FortiGate). Kubernetes and Linux distros count as platforms. Product names and versions are in play when an operator would mention them."#;

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
        let vendors = if coordinates.vendors.is_empty() {
            "none; stay vendor-neutral".to_string()
        } else {
            coordinates.vendors.join(", ")
        };
        format!(
            "Generate one task prompt using all of these mandatory constraints:\n\nTaxonomy: {}\nDomain: {} ({})\nSubdomain: {}\nTask family: {}\nEnvironment: {}\nVendor scope: {}\nVendors/platforms: {}\nIncident mechanism: {}\nEvidence condition: {}\nEvidence bundle: {}\nAction risk: {}\nPresentation: {}\nDifficulty: {}/10 ({})\n\nMake every coordinate materially affect the scenario. Do not merely list these labels in the generated prompt. Output only the task prompt, nothing else.{}",
            sample.taxonomy_id,
            sample.domain_id,
            sample.domain_label,
            sample.subdomain_id,
            coordinates.task_family,
            coordinates.environment,
            coordinates.vendor_scope,
            vendors,
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
    Generate(GenerateArgs),
    Atif {
        #[command(subcommand)]
        command: AtifCommand,
    },
    Taxonomy {
        #[command(subcommand)]
        command: TaxonomyCommand,
    },
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

#[derive(ClapArgs, Debug, Clone)]
struct GenerateArgs {
    #[arg(long, default_value = "https://api.openai.com/v1")]
    api_base: String,

    #[arg(long, env = "OPENAI_API_KEY")]
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

    #[arg(short, long, default_value_t = 250)]
    count: usize,

    #[arg(long)]
    distribution: Option<String>,

    #[arg(long)]
    difficulty: Option<String>,

    #[arg(short, long, default_value_t = 0.9)]
    temperature: f64,

    #[arg(short, long, default_value_t = 5)]
    workers: usize,

    #[arg(short, long, default_value = "output.jsonl")]
    output: PathBuf,

    #[arg(long)]
    append: bool,

    #[arg(long)]
    proxies: Option<PathBuf>,

    #[arg(long)]
    rotating_proxy: bool,

    #[arg(long)]
    keyfile: Option<PathBuf>,

    #[arg(long)]
    dedup: bool,

    #[arg(long, default_value_t = 0.6)]
    dedup_threshold: f64,

    #[arg(long)]
    free_models: bool,

    /// Rescan interval in minutes for free model availability (default: 10)
    #[arg(long, default_value_t = 10)]
    free_rescan: u64,

    #[arg(long)]
    input_price: Option<f64>,

    #[arg(long)]
    output_price: Option<f64>,

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
    if entry.schema_version.as_deref() == Some("scogo.netops.task.v1") {
        schema::validate_instance(schema::SchemaKind::NetOpsTask, &value)?;
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
    if restricted_sampling(model) {
        ChatRequest {
            model: model.to_string(),
            messages,
            temperature: None,
            max_tokens: None,
            max_completion_tokens: Some(max_out),
            stream: false,
        }
    } else {
        ChatRequest {
            model: model.to_string(),
            messages,
            temperature: Some(temperature),
            max_tokens: Some(max_out),
            max_completion_tokens: None,
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
    if let Some(delta) = c0.get("delta") {
        for key in ["content", "reasoning_content", "reasoning"] {
            if let Some(s) = delta.get(key).and_then(content_from_value) {
                buf.push_str(&s);
            }
        }
    }
    if let Some(msg) = c0.get("message") {
        for key in ["content", "reasoning_content", "reasoning"] {
            if let Some(s) = msg.get(key).and_then(content_from_value) {
                if buf.is_empty() {
                    buf.push_str(&s);
                }
            }
        }
    }
    if buf.is_empty() {
        if let Some(s) = c0.get("text").and_then(content_from_value) {
            buf.push_str(&s);
        }
    }
}

fn parse_chat_payload(raw: &str) -> Result<(String, u64, u64)> {
    let t = raw.trim_start();
    if t.starts_with("data:") {
        let mut text = String::new();
        let mut usage = (0u64, 0u64);
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
            if let Some(err) = v.get("error") {
                if !err.is_null() {
                    bail!("API error payload: {}", err);
                }
            }
            append_choice_text(&mut text, &v);
            let u = extract_usage(&v);
            if u != (0, 0) {
                usage = u;
            }
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            bail!("no completion text in streamed API response");
        }
        return Ok((text, usage.0, usage.1));
    }

    let value: serde_json::Value = serde_json::from_str(raw)?;
    let mut text = String::new();
    append_choice_text(&mut text, &value);
    if text.trim().is_empty() {
        text = extract_completion(&value)?;
    }
    let usage = extract_usage(&value);
    Ok((text.trim().to_string(), usage.0, usage.1))
}

fn extract_completion(v: &serde_json::Value) -> Result<String> {
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            bail!("API error payload: {}", err);
        }
    }
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

const MAX_MODEL_FAILURES: usize = 3;

struct ModelFailures {
    counts: std::sync::Mutex<HashMap<String, usize>>,
    rescan_notify: tokio::sync::Notify,
}

impl ModelFailures {
    fn new() -> Self {
        Self {
            counts: std::sync::Mutex::new(HashMap::new()),
            rescan_notify: tokio::sync::Notify::new(),
        }
    }

    /// Record a failure. Returns true if the model just crossed the threshold.
    fn record(&self, model: &str) -> bool {
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(model.to_string()).or_insert(0);
        *count += 1;
        *count == MAX_MODEL_FAILURES
    }

    /// Remove a model from tracking (called after rescan replaces the list).
    fn reset(&self) {
        let mut counts = self.counts.lock().unwrap();
        counts.clear();
    }
}

struct AtomicStats {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    tasks: AtomicUsize,
    errors: AtomicUsize,
}

impl AtomicStats {
    fn new() -> Self {
        Self {
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            tasks: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
        }
    }
}

struct RunStats {
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tasks: usize,
    errors: usize,
}

#[derive(Debug, Default)]
struct DatasetCounts {
    categories: HashMap<String, usize>,
    domains: HashMap<String, usize>,
    subdomains: HashMap<(String, String), usize>,
    difficulties: HashMap<u8, usize>,
    n: usize,
}

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

fn tally_jsonl(path: &std::path::Path) -> DatasetCounts {
    let mut counts = DatasetCounts::default();
    let Ok(file) = File::open(path) else {
        return counts;
    };
    for line in BufReader::new(file).lines().flatten() {
        let Ok(entry) = serde_json::from_str::<TaskEntry>(&line) else {
            continue;
        };
        counts.add(&entry.domain, &entry.subdomain, entry.difficulty);
    }
    counts
}

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
        let d: u8 = if key.starts_with('d') {
            key[1..]
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

fn build_clients(proxies: &[reqwest::Proxy]) -> Vec<reqwest::Client> {
    proxies
        .iter()
        .map(|p| {
            reqwest::Client::builder()
                .proxy(p.clone())
                .build()
                .expect("failed to build client with proxy")
        })
        .collect()
}

fn word_trigrams(text: &str) -> HashSet<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 3 {
        return words.iter().map(|w| w.to_string()).collect();
    }
    words.windows(3).map(|w| w.join(" ")).collect()
}

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

fn load_api_keys(path: &PathBuf) -> Result<Vec<String>> {
    let file = File::open(path).context(format!("failed to open keyfile: {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut keys = Vec::new();
    for line in reader.lines() {
        let line = line.context("failed to read keyfile")?;
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        keys.push(line);
    }
    if keys.is_empty() {
        bail!("keyfile is empty: {}", path.display());
    }
    Ok(keys)
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
    free.sort_by(|a, b| b.2.cmp(&a.2));

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
    Billing(String),
    Timeout,
    Other(anyhow::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::RateLimit(s) => write!(f, "rate limited (retry after {:?}s)", s),
            ApiError::Billing(msg) => write!(f, "billing error: {}", msg),
            ApiError::Timeout => write!(f, "request timed out"),
            ApiError::Other(e) => write!(f, "{}", e),
        }
    }
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
    parse_chat_payload(&raw).map_err(|e| {
        ApiError::Other(anyhow::anyhow!(
            "{} | body: {}",
            e,
            truncate_body(&raw, 500)
        ))
    })
}

const MAX_RETRIES: u32 = 5;

async fn generate_task(
    client: &reqwest::Client,
    api_base: &str,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    sample: &taxonomy::SampledTask,
    temperature: f64,
    language: Option<&str>,
    cancel: &AtomicBool,
    consecutive_timeouts: &AtomicUsize,
    pb: &ProgressBar,
) -> std::result::Result<(String, u64, u64), ApiError> {
    let user_msg = task_user_message(sample, language);
    let system = if sample.coordinates.is_none() && sample.category_id == "oem" {
        format!("{system_prompt}{OEM_SYSTEM_ADDENDUM}")
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
        2048,
    );

    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));

    let mut retries = 0u32;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ApiError::Other(anyhow::anyhow!("cancelled")));
        }

        match api_request(client, &url, api_key, &body).await {
            Ok(result) => {
                consecutive_timeouts.store(0, Ordering::Relaxed);
                return Ok(result);
            }
            Err(ApiError::RateLimit(retry_after)) => {
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(ApiError::RateLimit(retry_after));
                }
                let wait = retry_after.unwrap_or_else(|| 2u64.pow(retries).min(60));
                pb.suspend(|| {
                    eprintln!(
                        "[RATE] 429 hit, waiting {}s (retry {}/{})",
                        wait, retries, MAX_RETRIES
                    );
                });
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
            }
            Err(ApiError::Timeout) => {
                let count = consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= 5 {
                    pb.suspend(|| {
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
                let wait = 2u64.pow(retries).min(30);
                pb.suspend(|| {
                    eprintln!(
                        "[TIMEOUT] request timed out, waiting {}s (retry {}/{}, {} consecutive)",
                        wait, retries, MAX_RETRIES, count
                    );
                });
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
            }
            Err(ApiError::Billing(msg)) => {
                pb.suspend(|| {
                    eprintln!("[FATAL] billing error, shutting down gracefully: {}", msg);
                });
                cancel.store(true, Ordering::Relaxed);
                return Err(ApiError::Billing(msg));
            }
            Err(e) => return Err(e),
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
            .filter_map(|l| l.ok())
            .filter(|l| !l.trim().is_empty())
            .count(),
        None => 0,
    }
}

fn share(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 * 100.0 / total as f64
    }
}

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
        md.push_str("  \"schema_version\": \"scogo.netops.task.v1\",\n");
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
        Command::Generate(args) => run_generate(args).await,
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

async fn run_generate(args: GenerateArgs) -> Result<()> {
    let taxonomy = match args.taxonomy.as_deref() {
        Some(path) => taxonomy::TaxonomyCatalog::from_path(path)?,
        None => taxonomy::TaxonomyCatalog::embedded_itops()?,
    };
    let dist: HashMap<String, f64> = match &args.distribution {
        Some(distribution) => parse_distribution(distribution)?,
        None => taxonomy.default_distribution(),
    };
    let diff_dist: HashMap<u8, f64> = match &args.difficulty {
        Some(difficulty) => parse_difficulty(difficulty)?,
        None => taxonomy.default_difficulty(),
    };
    let system_prompt = resolve_system_prompt(&args, &taxonomy)?;
    taxonomy.validate_sampling_distributions(&dist, &diff_dist)?;

    let api_keys: Arc<Vec<String>> = Arc::new(match &args.keyfile {
        Some(path) => {
            let keys = load_api_keys(path)?;
            println!("Loaded {} API keys (round-robin)", keys.len());
            keys
        }
        None => {
            let key = args
                .api_key
                .clone()
                .context("API key required. Use --api-key, set OPENAI_API_KEY, or use --keyfile")?;
            vec![key]
        }
    });
    let key_counter = Arc::new(AtomicUsize::new(0));

    // discover free models from OpenRouter if requested
    let api_base = if args.free_models {
        OPENROUTER_API_BASE.to_string()
    } else {
        args.api_base.clone()
    };

    let model_failures = Arc::new(ModelFailures::new());

    let free_model_list: Option<Arc<tokio::sync::RwLock<Vec<String>>>> = if args.free_models {
        let discovery_client = reqwest::Client::new();
        let models = fetch_free_models(&discovery_client, &api_keys[0]).await?;
        Some(Arc::new(tokio::sync::RwLock::new(models)))
    } else {
        None
    };
    let model_counter = Arc::new(AtomicUsize::new(0));

    let existing = if args.append {
        count_existing_tasks(&args.output)
    } else {
        0
    };
    if existing > 0 {
        println!("Appending to existing file with {} tasks", existing);
    }

    let file = if args.append && args.output.exists() {
        OpenOptions::new().append(true).open(&args.output)?
    } else {
        File::create(&args.output)?
    };

    let clients: Arc<Vec<reqwest::Client>> = Arc::new(match &args.proxies {
        Some(proxy_path) => {
            let proxies = load_proxies(proxy_path)?;
            let total = proxies.len();
            if args.rotating_proxy {
                let idx = thread_rng().gen_range(0..total);
                println!("Using rotating proxy (sticky): proxy #{}", idx + 1);
                vec![
                    reqwest::Client::builder()
                        .proxy(proxies.into_iter().nth(idx).unwrap())
                        .build()?,
                ]
            } else {
                println!("Loaded {} proxies (round-robin)", total);
                build_clients(&proxies)
            }
        }
        None => vec![reqwest::Client::new()],
    });
    let proxy_counter = Arc::new(AtomicUsize::new(0));

    let file = Arc::new(std::sync::Mutex::new(file));
    let stats = Arc::new(AtomicStats::new());
    let cancel = Arc::new(AtomicBool::new(false));
    let consecutive_timeouts = Arc::new(AtomicUsize::new(0));

    let budget = args.budget;
    let input_price = args.input_price;
    let output_price = args.output_price;
    let count = args.count;
    let workers = args.workers;

    // pre-sample all domain/difficulty/language tuples to avoid RNG contention in workers
    let mut rng = match args.seed {
        Some(seed) => StdRng::seed_from_u64(seed),
        None => StdRng::from_entropy(),
    };
    let multilingual = args.multilingual;
    let presampled: Vec<(taxonomy::SampledTask, Option<String>)> = (0..count)
        .map(|_| {
            let sample = taxonomy.sample(&mut rng, &dist, &diff_dist)?;
            let lang = if multilingual {
                let idx = rng.gen_range(0..LANGUAGES.len());
                Some(LANGUAGES[idx].0.to_string())
            } else {
                None
            };
            Ok((sample, lang))
        })
        .collect::<Result<Vec<_>>>()?;

    let presampled = Arc::new(presampled);

    let pb = ProgressBar::new(count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({per_sec}) | {msg}")
            .unwrap()
            .progress_chars("##-"),
    );
    pb.set_message("starting...");

    // spawn background rescan task for free models
    let rescan_handle = if let Some(ref model_list) = free_model_list {
        let model_list = model_list.clone();
        let cancel = cancel.clone();
        let model_failures = model_failures.clone();
        let api_key = api_keys[0].clone();
        let rescan_mins = args.free_rescan;
        let pb = pb.clone();
        Some(tokio::spawn(async move {
            let client = reqwest::Client::new();
            loop {
                // wait for either the timer or an immediate rescan trigger
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(rescan_mins * 60)) => {},
                    _ = model_failures.rescan_notify.notified() => {},
                }
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                pb.suspend(|| println!("[RESCAN] refreshing free model list..."));
                match fetch_free_models(&client, &api_key).await {
                    Ok(new_models) => {
                        let count = new_models.len();
                        model_failures.reset();
                        let mut list = model_list.write().await;
                        *list = new_models;
                        pb.suspend(|| println!("[RESCAN] updated: {} models available", count));
                    }
                    Err(e) => {
                        pb.suspend(|| {
                            eprintln!("[RESCAN] failed to refresh: {}, keeping current list", e)
                        });
                    }
                }
            }
        }))
    } else {
        None
    };

    stream::iter(0..count)
        .for_each_concurrent(workers, |i| {
            let clients = clients.clone();
            let proxy_counter = proxy_counter.clone();
            let file = file.clone();
            let stats = stats.clone();
            let cancel = cancel.clone();
            let consecutive_timeouts = consecutive_timeouts.clone();
            let api_base = api_base.clone();
            let api_keys = api_keys.clone();
            let key_counter = key_counter.clone();
            let model = args.model.clone();
            let free_model_list = free_model_list.clone();
            let model_counter = model_counter.clone();
            let model_failures = model_failures.clone();
            let system_prompt = system_prompt.clone();
            let presampled = presampled.clone();
            let temperature = args.temperature;
            let pb = pb.clone();

            async move {
                if cancel.load(Ordering::Relaxed) {
                    pb.inc(1);
                    return;
                }

                let (ref sample, ref lang) = presampled[i];

                if let (Some(b), Some(ip), Some(op)) = (budget, input_price, output_price) {
                    let in_tok = stats.input_tokens.load(Ordering::Relaxed) as f64;
                    let out_tok = stats.output_tokens.load(Ordering::Relaxed) as f64;
                    let cost = (ip * in_tok / 1_000_000.0) + (op * out_tok / 1_000_000.0);
                    if cost >= b {
                        cancel.store(true, Ordering::Relaxed);
                        pb.inc(1);
                        return;
                    }
                }

                let use_model = match &free_model_list {
                    Some(models) => {
                        let list = models.read().await;
                        let idx = model_counter.fetch_add(1, Ordering::Relaxed) % list.len();
                        list[idx].clone()
                    }
                    None => model.clone(),
                };

                let client_idx = proxy_counter.fetch_add(1, Ordering::Relaxed) % clients.len();
                let client = &clients[client_idx];
                let key_idx = key_counter.fetch_add(1, Ordering::Relaxed) % api_keys.len();
                let api_key = &api_keys[key_idx];

                match generate_task(
                    client,
                    &api_base,
                    api_key,
                    &use_model,
                    &system_prompt,
                    sample,
                    temperature,
                    lang.as_deref(),
                    &cancel,
                    &consecutive_timeouts,
                    &pb,
                )
                .await
                {
                    Ok((prompt, in_tok, out_tok)) => {
                        if prompt.trim().is_empty() {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            pb.inc(1);
                            return;
                        }
                        let entry = TaskEntry {
                            schema_version: sample.coordinates.as_ref().map(|_| "scogo.netops.task.v1".to_string()),
                            prompt,
                            domain: if sample.coordinates.is_some() {
                                format!("enterprise_netops::{}", sample.domain_id)
                            } else {
                                format!("{}::{}", sample.category_id, sample.domain_label)
                            },
                            subdomain: sample.subdomain_id.clone(),
                            difficulty: sample.difficulty,
                            coordinates: sample.coordinates.clone(),
                            language: lang.clone(),
                            taskgen_model: use_model,
                            temperature,
                        };
                        let line = match serialize_task_entry(&entry) {
                            Ok(line) => line + "\n",
                            Err(error) => {
                                stats.errors.fetch_add(1, Ordering::Relaxed);
                                pb.suspend(|| eprintln!("[SCHEMA] task rejected: {error}"));
                                pb.inc(1);
                                return;
                            }
                        };
                        let write_result = {
                            let mut f = file.lock().unwrap();
                            f.write_all(line.as_bytes()).and_then(|_| f.flush())
                        };
                        if let Err(error) = write_result {
                            stats.errors.fetch_add(1, Ordering::Relaxed);
                            pb.suspend(|| eprintln!("[WRITE] task rejected: {error}"));
                            pb.inc(1);
                            return;
                        }
                        stats.input_tokens.fetch_add(in_tok, Ordering::Relaxed);
                        stats.output_tokens.fetch_add(out_tok, Ordering::Relaxed);
                        let done = stats.tasks.fetch_add(1, Ordering::Relaxed) + 1;
                        let errs = stats.errors.load(Ordering::Relaxed);
                        let cur_in = stats.input_tokens.load(Ordering::Relaxed) as f64;
                        let cur_out = stats.output_tokens.load(Ordering::Relaxed) as f64;
                        let total_tok = (cur_in + cur_out) as u64;
                        let cost_str = match (input_price, output_price) {
                            (Some(ip), Some(op)) => {
                                let cost = (ip * cur_in / 1_000_000.0) + (op * cur_out / 1_000_000.0);
                                if let Some(b) = budget {
                                    if cost >= b {
                                        cancel.store(true, Ordering::Relaxed);
                                    }
                                }
                                format!(" | ${:.4}", cost)
                            }
                            _ => String::new(),
                        };
                        pb.set_message(format!(
                            "{} ok | {} err | {}k tok{}",
                            done, errs, total_tok / 1000, cost_str
                        ));
                    }
                    Err(e) => {
                        stats.errors.fetch_add(1, Ordering::Relaxed);
                        if !cancel.load(Ordering::Relaxed) {
                            pb.suspend(|| eprintln!("[ERROR] task {}: {}", i + 1, e));
                        }
                        // track per-model failures for free model rotation
                        if free_model_list.is_some() {
                            let tripped = model_failures.record(&use_model);
                            if tripped {
                                pb.suspend(|| {
                                    eprintln!(
                                        "[RESCAN] {} failed {} times, marking offline and triggering rescan",
                                        use_model, MAX_MODEL_FAILURES
                                    );
                                });
                                model_failures.rescan_notify.notify_one();
                            }
                        }
                    }
                }
                pb.inc(1);
            }
        })
        .await;

    // stop the rescan task
    if let Some(handle) = rescan_handle {
        handle.abort();
    }

    let was_cancelled = cancel.load(Ordering::Relaxed);
    if was_cancelled {
        pb.finish_with_message("stopped early — saving progress");
    } else {
        pb.finish_with_message("done");
    }

    let total_tasks = stats.tasks.load(Ordering::Relaxed);
    let total_errors = stats.errors.load(Ordering::Relaxed);
    let total_in = stats.input_tokens.load(Ordering::Relaxed);
    let total_out = stats.output_tokens.load(Ordering::Relaxed);

    if was_cancelled {
        println!(
            "\nGraceful shutdown — saved {} tasks before exit",
            total_tasks
        );
    }
    println!("Generated {} tasks ({} errors)", total_tasks, total_errors);
    println!("Tokens: {} in / {} out", total_in, total_out);

    let stats = RunStats {
        total_input_tokens: total_in,
        total_output_tokens: total_out,
        total_tasks,
        errors: total_errors,
    };

    if args.dedup && args.output.exists() {
        println!(
            "\nRunning deduplication (threshold: {:.2})...",
            args.dedup_threshold
        );

        let reader = BufReader::new(File::open(&args.output)?);
        let mut lines: Vec<String> = Vec::new();
        let mut entries: Vec<Option<TaskEntry>> = Vec::new();

        for line in reader.lines().flatten() {
            let entry = serde_json::from_str::<TaskEntry>(&line).ok();
            entries.push(entry);
            lines.push(line);
        }

        // pass 1: exact duplicates
        let mut seen: HashSet<String> = HashSet::new();
        let mut keep = vec![true; lines.len()];
        let mut exact_dupes = 0usize;

        for (i, entry) in entries.iter().enumerate() {
            if let Some(e) = entry {
                let normalized: String = e.prompt.to_lowercase().split_whitespace().collect();
                if !seen.insert(normalized) {
                    keep[i] = false;
                    exact_dupes += 1;
                }
            }
        }

        if exact_dupes > 0 {
            println!("Removed {} exact duplicates", exact_dupes);
        }

        // pass 2: semantic duplicates via word-trigram jaccard
        let kept_indices: Vec<usize> = (0..lines.len()).filter(|&i| keep[i]).collect();
        let trigrams: Vec<Option<HashSet<String>>> = kept_indices
            .iter()
            .map(|&i| {
                entries[i]
                    .as_ref()
                    .map(|e| word_trigrams(&e.prompt.to_lowercase()))
            })
            .collect();

        let mut semantic_dupes = 0usize;
        for j in 1..kept_indices.len() {
            if !keep[kept_indices[j]] {
                continue;
            }
            let trig_b = match &trigrams[j] {
                Some(t) => t,
                None => continue,
            };
            for k in 0..j {
                if !keep[kept_indices[k]] {
                    continue;
                }
                let trig_a = match &trigrams[k] {
                    Some(t) => t,
                    None => continue,
                };
                if jaccard_similarity(trig_a, trig_b) >= args.dedup_threshold {
                    keep[kept_indices[j]] = false;
                    semantic_dupes += 1;
                    break;
                }
            }
        }

        if semantic_dupes > 0 {
            println!(
                "Removed {} semantic duplicates (similarity >= {:.2})",
                semantic_dupes, args.dedup_threshold
            );
        }

        let total_removed = exact_dupes + semantic_dupes;
        if total_removed > 0 {
            let mut f = File::create(&args.output)?;
            for (i, line) in lines.iter().enumerate() {
                if keep[i] {
                    f.write_all(line.as_bytes())?;
                    f.write_all(b"\n")?;
                }
            }
            let remaining = lines.len() - total_removed;
            println!(
                "Deduplication complete: {} removed, {} remaining",
                total_removed, remaining
            );
        } else {
            println!("No duplicates found");
        }
    }

    // split output into per-language files when --multilingual is set
    let lang_counts: Option<HashMap<String, usize>> = if multilingual && args.output.exists() {
        println!("\nSplitting output by language...");
        let reader = BufReader::new(File::open(&args.output)?);
        let mut lang_buckets: HashMap<String, Vec<String>> = HashMap::new();

        for line in reader.lines().flatten() {
            let lang_code = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| {
                    v.get("language")
                        .and_then(|l| l.as_str().map(|s| s.to_string()))
                })
                .unwrap_or_else(|| "en".to_string());
            lang_buckets.entry(lang_code).or_default().push(line);
        }

        let out_dir = args.output.parent().unwrap_or(std::path::Path::new("."));
        let stem = args
            .output
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        let ext = args
            .output
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();

        let counts: HashMap<String, usize> = lang_buckets
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect();

        for (lang, lines) in &lang_buckets {
            let lang_path = out_dir.join(format!("{}_{}{}", stem, lang, ext));
            let mut f = File::create(&lang_path)?;
            for line in lines {
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")?;
            }
            println!(
                "  {} — {} tasks -> {}",
                lang,
                lines.len(),
                lang_path.display()
            );
        }

        Some(counts)
    } else {
        None
    };

    let observed = if args.output.exists() {
        tally_jsonl(&args.output)
    } else {
        DatasetCounts::default()
    };
    let readme = generate_readme(
        &args,
        &stats,
        taxonomy.kind(),
        &dist,
        &diff_dist,
        lang_counts.as_ref(),
        &observed,
    );
    let out_dir = args.output.parent().unwrap_or(std::path::Path::new("."));
    let stem = args
        .output
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let readme_path = out_dir.join(format!("{}.README.md", stem));
    let mut rf = File::create(&readme_path).context("failed to create dataset README")?;
    rf.write_all(readme.as_bytes())?;
    println!("dataset README written to {}", readme_path.display());

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
        assert!(md.contains("scogo.netops.task.v1"), "{md}");
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
                task_family: "troubleshooting_rca".into(),
                environment: "hybrid".into(),
                vendor_scope: "multi_vendor".into(),
                vendors: vec!["cisco_ios_xe".into(), "juniper_junos".into()],
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
            "multi_vendor",
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
            schema_version: Some("scogo.netops.task.v1".into()),
            prompt: "Investigate the route leak safely.".into(),
            domain: "enterprise_netops::layer3_routing".into(),
            subdomain: "bgp_route_leak".into(),
            difficulty: 8,
            coordinates: Some(taxonomy::TaskCoordinates {
                taxonomy_id: "scogo-enterprise-netops-v1".into(),
                task_family: "troubleshooting_rca".into(),
                environment: "hybrid".into(),
                vendor_scope: "multi_vendor".into(),
                vendors: vec!["cisco_ios_xe".into(), "juniper_junos".into()],
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
        assert_eq!(value["schema_version"], "scogo.netops.task.v1");
        assert_eq!(value["domain"], "enterprise_netops::layer3_routing");
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
            schema_version: Some("scogo.netops.task.v1".into()),
            prompt: "Investigate safely.".into(),
            domain: "enterprise_netops::layer3_routing".into(),
            subdomain: "bgp_route_leak".into(),
            difficulty: 8,
            coordinates: None,
            language: None,
            taskgen_model: "teacher".into(),
            temperature: 0.9,
        };
        assert!(serialize_task_entry(&entry).is_err());
    }
}
