use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::ProviderConfig;
use crate::schema::{self, SchemaKind};

const REVIEW_TEXT_MAX_CHARS: usize = 800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredOutputFormat {
    JsonSchema,
    JsonObject,
    PromptOnly,
}

impl StructuredOutputFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::JsonSchema => "json_schema",
            Self::JsonObject => "json_object",
            Self::PromptOnly => "prompt_only",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewRateLimiter {
    interval: Duration,
    next_request_at: Arc<tokio::sync::Mutex<Instant>>,
}

impl ReviewRateLimiter {
    pub fn from_requests_per_minute(requests_per_minute: Option<u32>) -> Result<Option<Arc<Self>>> {
        let Some(requests_per_minute) = requests_per_minute else {
            return Ok(None);
        };
        if requests_per_minute == 0 {
            bail!("review requests per minute must be positive");
        }
        let interval = Duration::from_secs_f64(60.0 / f64::from(requests_per_minute));
        Ok(Some(Arc::new(Self {
            interval,
            next_request_at: Arc::new(tokio::sync::Mutex::new(Instant::now())),
        })))
    }

    async fn acquire(&self) {
        let mut next_request_at = self.next_request_at.lock().await;
        let now = Instant::now();
        if *next_request_at > now {
            tokio::time::sleep(*next_request_at - now).await;
        }
        *next_request_at = Instant::now() + self.interval;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    Accept,
    Revise,
    Reject,
    NeedsVerification,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HardFailure {
    TechnicalInaccuracy,
    InventedPlatformFeature,
    InvalidCommandOrSyntax,
    ProtocolOrArchitectureError,
    UnsupportedCausality,
    NumericalOrTemporalInconsistency,
    InternalContradiction,
    CoordinateMismatch,
    InsufficientOrInvalidEvidence,
    NotOperational,
    UnsafeOrUnapprovedChange,
    HiddenAnswerOrSolutionLeakage,
    ScopeViolation,
    AmbiguousOrUnanswerable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DimensionCheck {
    pub status: CheckStatus,
    pub rationale: String,
    pub evidence_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewChecks {
    pub coordinate_realization: DimensionCheck,
    pub internal_consistency: DimensionCheck,
    pub operational_quality: DimensionCheck,
    pub safety: DimensionCheck,
    pub technical_authenticity: DimensionCheck,
}

impl ReviewChecks {
    fn values(&self) -> [&DimensionCheck; 5] {
        [
            &self.coordinate_realization,
            &self.internal_consistency,
            &self.operational_quality,
            &self.safety,
            &self.technical_authenticity,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerificationClaim {
    pub claim_id: String,
    pub claim: String,
    pub candidate_evidence_paths: Vec<String>,
    pub reference_query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimVerdict {
    Supported,
    Unsupported,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdjudicationOutcome {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdjudicatedClaim {
    pub claim_id: String,
    pub verdict: ClaimVerdict,
    pub rationale: String,
    pub citations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdjudicationDecision {
    pub schema_version: String,
    pub claims: Vec<AdjudicatedClaim>,
    pub outcome: AdjudicationOutcome,
    pub summary: String,
}

impl AdjudicationDecision {
    pub fn parse_and_validate(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let json_text = if trimmed.starts_with("```") {
            let start = trimmed
                .find('{')
                .context("adjudication response code fence contains no JSON object")?;
            let end = trimmed
                .rfind('}')
                .context("adjudication response code fence contains no complete JSON object")?;
            &trimmed[start..=end]
        } else {
            trimmed
        };
        let value: Value = serde_json::from_str(json_text).context("invalid adjudication JSON")?;
        schema::validate_instance(SchemaKind::PromptAdjudication, &value)
            .context("adjudication JSON failed schema validation")?;
        let decision: Self =
            serde_json::from_value(value).context("invalid adjudication decision")?;
        if decision.outcome == AdjudicationOutcome::Accept
            && decision
                .claims
                .iter()
                .any(|claim| claim.verdict != ClaimVerdict::Supported || claim.citations.is_empty())
        {
            bail!("accept adjudication contains an unsupported, unverified, or uncited claim");
        }
        Ok(decision)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    pub schema_version: String,
    pub outcome: ReviewOutcome,
    pub checks: ReviewChecks,
    pub hard_failures: Vec<HardFailure>,
    pub claims_requiring_verification: Vec<VerificationClaim>,
    pub summary: String,
    pub retry_guidance: String,
}

impl ReviewDecision {
    pub fn parse_and_validate(raw: &str) -> Result<Self> {
        Self::parse_and_validate_with_metadata(raw).map(|(decision, _)| decision)
    }

    fn parse_and_validate_with_metadata(raw: &str) -> Result<(Self, ReviewNormalization)> {
        let trimmed = raw.trim();
        let json_text = if trimmed.starts_with("```") {
            let start = trimmed
                .find('{')
                .context("review response code fence contains no JSON object")?;
            let end = trimmed
                .rfind('}')
                .context("review response code fence contains no complete JSON object")?;
            &trimmed[start..=end]
        } else {
            trimmed
        };
        Self::parse_and_validate_with_format(json_text, StructuredOutputFormat::PromptOnly)
    }

    fn parse_and_validate_with_format(
        raw: &str,
        response_format: StructuredOutputFormat,
    ) -> Result<(Self, ReviewNormalization)> {
        let mut value: Value = serde_json::from_str(raw).context("invalid review JSON")?;
        let (hard_failure_aliases_normalized, claim_ids_repaired) =
            normalize_review_contract(&mut value);
        let normalization = ReviewNormalization {
            summary_truncated: clip_string_field(&mut value, "summary", REVIEW_TEXT_MAX_CHARS),
            retry_guidance_truncated: clip_string_field(
                &mut value,
                "retry_guidance",
                REVIEW_TEXT_MAX_CHARS,
            ),
            hard_failure_aliases_normalized,
            claim_ids_repaired,
            response_format: response_format.as_str().to_string(),
        };
        schema::validate_instance(SchemaKind::PromptReviewV3, &value)
            .context("review JSON failed schema validation")?;
        let decision: Self = serde_json::from_value(value).context("invalid review decision")?;
        decision.validate_policy()?;
        Ok((decision, normalization))
    }

    fn validate_policy(&self) -> Result<()> {
        let has_fail = self
            .checks
            .values()
            .iter()
            .any(|check| check.status == CheckStatus::Fail);
        let has_unknown = self
            .checks
            .values()
            .iter()
            .any(|check| check.status == CheckStatus::Unknown);
        match self.outcome {
            ReviewOutcome::Accept => {
                if has_fail || has_unknown || !self.hard_failures.is_empty() {
                    bail!("accept review contains a failed, unknown, or hard-failure finding");
                }
            }
            ReviewOutcome::Revise => {
                if !has_fail || !self.hard_failures.is_empty() {
                    bail!("revise review must contain a failed check and no hard failure");
                }
            }
            ReviewOutcome::Reject => {
                if !has_fail || self.hard_failures.is_empty() {
                    bail!("reject review must prove a failed check and hard failure");
                }
            }
            ReviewOutcome::NeedsVerification => {
                if !has_unknown || self.claims_requiring_verification.is_empty() {
                    bail!("needs_verification review requires an unknown check and a claim");
                }
            }
        }
        Ok(())
    }
}

fn clip_string_field(value: &mut Value, field: &str, max_chars: usize) -> bool {
    let Some(slot) = value.get_mut(field) else {
        return false;
    };
    let Some(text) = slot.as_str() else {
        return false;
    };
    if text.chars().count() <= max_chars {
        return false;
    }
    let clipped = text.chars().take(max_chars).collect::<String>();
    *slot = Value::String(clipped);
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewNormalization {
    pub summary_truncated: bool,
    pub retry_guidance_truncated: bool,
    pub hard_failure_aliases_normalized: usize,
    pub claim_ids_repaired: usize,
    pub response_format: String,
}

fn normalize_review_contract(value: &mut Value) -> (usize, usize) {
    let mut hard_failure_aliases_normalized = 0;
    let mut claim_ids_repaired = 0;

    if let Some(hard_failures) = value.get_mut("hard_failures").and_then(Value::as_array_mut) {
        for failure in hard_failures {
            let Some(raw) = failure.as_str() else {
                continue;
            };
            let normalized = normalize_hard_failure(raw);
            if normalized != raw {
                *failure = Value::String(normalized.to_string());
                hard_failure_aliases_normalized += 1;
            }
        }
    }

    if let Some(claims) = value
        .get_mut("claims_requiring_verification")
        .and_then(Value::as_array_mut)
    {
        for (index, claim) in claims.iter_mut().enumerate() {
            let Some(object) = claim.as_object_mut() else {
                continue;
            };
            if object
                .get("claim_id")
                .and_then(Value::as_str)
                .is_some_and(|claim_id| !claim_id.trim().is_empty())
            {
                continue;
            }
            object.remove("claim_id");
            let alias = object.remove("claimId").or_else(|| object.remove("id"));
            object.insert(
                "claim_id".to_string(),
                alias.unwrap_or_else(|| Value::String(format!("claim-{}", index + 1))),
            );
            claim_ids_repaired += 1;
        }
    }

    (hard_failure_aliases_normalized, claim_ids_repaired)
}

fn normalize_hard_failure(raw: &str) -> String {
    let normalized = raw
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-', '/'], "_");
    match normalized.as_str() {
        "technical_inaccuracy" | "technical_error" => "technical_inaccuracy",
        "invented_platform_feature" | "invented_feature" => "invented_platform_feature",
        "invalid_command_or_syntax" | "invalid_command" => "invalid_command_or_syntax",
        "protocol_or_architecture_error" | "architecture_error" => "protocol_or_architecture_error",
        "unsupported_causality" => "unsupported_causality",
        "numerical_or_temporal_inconsistency" | "numeric_or_temporal_inconsistency" => {
            "numerical_or_temporal_inconsistency"
        }
        "internal_contradiction" => "internal_contradiction",
        "coordinate_mismatch" => "coordinate_mismatch",
        "insufficient_or_invalid_evidence" | "invalid_evidence" => {
            "insufficient_or_invalid_evidence"
        }
        "not_operational" => "not_operational",
        "unsafe_or_unapproved_change" | "unsafe_change" => "unsafe_or_unapproved_change",
        "hidden_answer_or_solution_leakage" | "solution_leakage" => {
            "hidden_answer_or_solution_leakage"
        }
        "scope_violation" => "scope_violation",
        "ambiguous_or_unanswerable" | "unanswerable" => "ambiguous_or_unanswerable",
        _ => return raw.to_string(),
    }
    .to_string()
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub candidate: Value,
    pub taxonomy_id: String,
    pub taxonomy_kind: String,
    pub system_prompt: String,
    pub deterministic_checks: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
    pub normalization: ReviewNormalization,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait]
pub trait CandidateReviewer: Send + Sync {
    async fn review(&self, request: ReviewRequest) -> Result<ReviewResult>;
}

#[derive(Clone)]
pub struct ReviewClient {
    provider: ProviderConfig,
    client: reqwest::Client,
    max_output_tokens: u64,
    telemetry: Arc<crate::telemetry::RequestTelemetry>,
    rate_limiter: Option<Arc<ReviewRateLimiter>>,
}

#[derive(Debug)]
enum ReviewAttemptError {
    RateLimit {
        retry_after_seconds: Option<u64>,
        message: String,
    },
    UnsupportedResponseFormat {
        message: String,
    },
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ReviewAttemptError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::RateLimit { message, .. } => anyhow::anyhow!(message),
            Self::UnsupportedResponseFormat { message } => anyhow::anyhow!(message),
            Self::Retryable(error) | Self::Fatal(error) => error,
        }
    }
}

struct StructuredResponse {
    payload: Value,
    content: String,
    elapsed_ms: u64,
    response_format: StructuredOutputFormat,
}

#[allow(clippy::too_many_arguments)]
async fn request_structured_once(
    provider: &ProviderConfig,
    client: &reqwest::Client,
    max_output_tokens: u64,
    telemetry: &crate::telemetry::RequestTelemetry,
    rate_limiter: Option<&Arc<ReviewRateLimiter>>,
    system_content: &str,
    user_content: String,
    operation: &str,
) -> std::result::Result<StructuredResponse, ReviewAttemptError> {
    let formats = preferred_response_formats(&provider.model);
    let mut unsupported = Vec::new();
    for response_format in formats {
        match request_structured_with_format(
            provider,
            client,
            max_output_tokens,
            telemetry,
            rate_limiter,
            system_content,
            user_content.clone(),
            operation,
            response_format,
        )
        .await
        {
            Err(ReviewAttemptError::UnsupportedResponseFormat { message }) => {
                unsupported.push(message);
            }
            result => return result,
        }
    }
    Err(ReviewAttemptError::Fatal(anyhow::anyhow!(
        "{operation} provider rejected every supported response format: {}",
        unsupported.join("; ")
    )))
}

#[allow(clippy::too_many_arguments)]
async fn request_structured_with_format(
    provider: &ProviderConfig,
    client: &reqwest::Client,
    max_output_tokens: u64,
    telemetry: &crate::telemetry::RequestTelemetry,
    rate_limiter: Option<&Arc<ReviewRateLimiter>>,
    system_content: &str,
    user_content: String,
    operation: &str,
    response_format: StructuredOutputFormat,
) -> std::result::Result<StructuredResponse, ReviewAttemptError> {
    let is_qwen = provider.model.to_ascii_lowercase().contains("qwen");
    let user_content = if is_qwen {
        format!("{user_content}\n/no_think")
    } else {
        user_content
    };
    let system_content = if is_qwen {
        format!("{system_content}\n/no_think")
    } else {
        system_content.to_string()
    };
    let mut body = json!({
        "model": provider.model,
        // Some OpenAI-compatible gateways (including Omniroute for GPT-5
        // families) default to SSE unless non-streaming is explicit. The
        // structured parser below consumes one complete JSON response.
        "stream": false,
        "messages": [
            {"role": "system", "content": system_content},
            {"role": "user", "content": user_content}
        ]
    });
    if restricted_sampling(&provider.model) {
        body["max_completion_tokens"] = json!(max_output_tokens);
    } else {
        body["max_tokens"] = json!(max_output_tokens);
        body["temperature"] = json!(0.0);
    }
    apply_model_reasoning_controls(&provider.model, &mut body);
    apply_model_response_format(&provider.model, &mut body, operation, response_format)
        .map_err(ReviewAttemptError::Fatal)?;
    let url = format!(
        "{}/chat/completions",
        provider.api_base.as_str().trim_end_matches('/')
    );
    let credential = provider.credentials.next();
    if let Some(rate_limiter) = rate_limiter {
        rate_limiter.acquire().await;
    }
    let started = Instant::now();
    let response = client
        .post(url)
        .bearer_auth(credential.expose())
        .json(&body)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            telemetry.record_timeout(elapsed_millis(started.elapsed()));
            return Err(ReviewAttemptError::Retryable(
                anyhow::anyhow!(error).context(format!("{operation} request timed out")),
            ));
        }
        Err(error) => {
            telemetry.record_error(elapsed_millis(started.elapsed()));
            return Err(ReviewAttemptError::Retryable(
                anyhow::anyhow!(error).context(format!("{operation} request failed")),
            ));
        }
    };
    let status = response.status();
    let retry_after_seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let raw = response.text().await.map_err(|error| {
        telemetry.record_error(elapsed_millis(started.elapsed()));
        ReviewAttemptError::Retryable(
            anyhow::anyhow!(error).context(format!("failed to read {operation} response")),
        )
    })?;
    let raw = crate::provider::redact_provider_text(&raw, credential.expose());
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        telemetry.record_rate_limit(elapsed_millis(started.elapsed()));
        return Err(ReviewAttemptError::RateLimit {
            retry_after_seconds,
            message: format!(
                "{operation} API returned HTTP {status}: {}",
                truncate(&raw, 500)
            ),
        });
    }
    if !status.is_success() {
        if (status == reqwest::StatusCode::BAD_REQUEST
            || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY)
            && response_format != StructuredOutputFormat::PromptOnly
            && looks_like_unsupported_response_format(&raw)
        {
            return Err(ReviewAttemptError::UnsupportedResponseFormat {
                message: format!(
                    "{operation} provider rejected {}: {}",
                    response_format.as_str(),
                    truncate(&raw, 500)
                ),
            });
        }
        telemetry.record_error(elapsed_millis(started.elapsed()));
        let error = anyhow::anyhow!(
            "{operation} API returned HTTP {status}: {}",
            truncate(&raw, 500)
        );
        return if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
            Err(ReviewAttemptError::Retryable(error))
        } else {
            Err(ReviewAttemptError::Fatal(error))
        };
    }
    let payload: Value = serde_json::from_str(&raw).map_err(|error| {
        telemetry.record_error(elapsed_millis(started.elapsed()));
        ReviewAttemptError::Retryable(anyhow::anyhow!(error).context("invalid structured API JSON"))
    })?;
    let content = extract_content(&payload).map_err(|error| {
        telemetry.record_error(elapsed_millis(started.elapsed()));
        ReviewAttemptError::Retryable(error)
    })?;
    Ok(StructuredResponse {
        payload,
        content,
        elapsed_ms: elapsed_millis(started.elapsed()),
        response_format,
    })
}

fn usage_tokens(payload: &Value) -> (u64, u64) {
    let usage = payload.get("usage").cloned().unwrap_or(Value::Null);
    (
        usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

async fn retry_structured<T, F, Fut>(
    telemetry: &crate::telemetry::RequestTelemetry,
    operation: &str,
    mut attempt_fn: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, ReviewAttemptError>>,
{
    let mut last_error = None;
    for attempt in 0..3u32 {
        match attempt_fn().await {
            Ok(result) => return Ok(result),
            Err(ReviewAttemptError::Fatal(error)) => return Err(error),
            Err(error) => {
                let delay = match &error {
                    ReviewAttemptError::RateLimit {
                        retry_after_seconds: Some(seconds),
                        ..
                    } => Duration::from_secs(*seconds),
                    _ => {
                        let base = 250u64 * 2u64.pow(attempt);
                        let jitter = rand::random::<u64>() % 251;
                        Duration::from_millis(base + jitter)
                    }
                };
                last_error = Some(error.into_anyhow());
                if attempt < 2 {
                    telemetry.record_retry();
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_error.context(format!("{operation} request failed without an error"))?)
}

impl ReviewClient {
    pub fn new(
        provider: ProviderConfig,
        client: reqwest::Client,
        max_output_tokens: u64,
        telemetry: Arc<crate::telemetry::RequestTelemetry>,
        rate_limiter: Option<Arc<ReviewRateLimiter>>,
    ) -> Result<Self> {
        if max_output_tokens == 0 {
            bail!("review max output tokens must be positive");
        }
        Ok(Self {
            provider,
            client,
            max_output_tokens,
            telemetry,
            rate_limiter,
        })
    }

    async fn request_once(
        &self,
        request: &ReviewRequest,
    ) -> std::result::Result<ReviewResult, ReviewAttemptError> {
        let user_content = serde_json::to_string_pretty(&json!({
            "instruction": "Review this prompt seed against every required quality dimension. Return only the required JSON decision. Do not solve the task.",
            "taxonomy_id": request.taxonomy_id,
            "taxonomy_kind": request.taxonomy_kind,
            "candidate": request.candidate,
            "deterministic_checks": request.deterministic_checks,
        }))
        .map_err(|error| ReviewAttemptError::Fatal(error.into()))?;
        let response = request_structured_once(
            &self.provider,
            &self.client,
            self.max_output_tokens,
            &self.telemetry,
            self.rate_limiter.as_ref(),
            &request.system_prompt,
            user_content,
            "review",
        )
        .await?;
        let (decision, normalization) = match ReviewDecision::parse_and_validate_with_format(
            &response.content,
            response.response_format,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.telemetry.record_error(response.elapsed_ms);
                return Err(ReviewAttemptError::Retryable(error));
            }
        };
        self.telemetry.record_success(response.elapsed_ms);
        let (input_tokens, output_tokens) = usage_tokens(&response.payload);
        Ok(ReviewResult {
            decision,
            normalization,
            model: self.provider.model.clone(),
            input_tokens,
            output_tokens,
        })
    }
}

#[async_trait]
impl CandidateReviewer for ReviewClient {
    async fn review(&self, request: ReviewRequest) -> Result<ReviewResult> {
        retry_structured(&self.telemetry, "review", || self.request_once(&request)).await
    }
}

#[derive(Debug, Clone)]
pub struct AdjudicationRequest {
    pub candidate: Value,
    pub review: ReviewDecision,
    pub references: Vec<crate::references::ReferenceExcerpt>,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdjudicationResult {
    pub decision: AdjudicationDecision,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait]
pub trait CandidateAdjudicator: Send + Sync {
    async fn adjudicate(&self, request: AdjudicationRequest) -> Result<AdjudicationResult>;
}

#[derive(Clone)]
pub struct AdjudicationClient {
    provider: ProviderConfig,
    client: reqwest::Client,
    max_output_tokens: u64,
    telemetry: Arc<crate::telemetry::RequestTelemetry>,
    rate_limiter: Option<Arc<ReviewRateLimiter>>,
}

impl AdjudicationClient {
    pub fn new(
        provider: ProviderConfig,
        client: reqwest::Client,
        max_output_tokens: u64,
        telemetry: Arc<crate::telemetry::RequestTelemetry>,
        rate_limiter: Option<Arc<ReviewRateLimiter>>,
    ) -> Result<Self> {
        if max_output_tokens == 0 {
            bail!("adjudication max output tokens must be positive");
        }
        Ok(Self {
            provider,
            client,
            max_output_tokens,
            telemetry,
            rate_limiter,
        })
    }

    async fn request_once(
        &self,
        request: &AdjudicationRequest,
    ) -> std::result::Result<AdjudicationResult, ReviewAttemptError> {
        let user_content = serde_json::to_string_pretty(&json!({
            "instruction": "Adjudicate only the listed claims using cited candidate evidence or local references.",
            "candidate": request.candidate,
            "review": request.review,
            "references": request.references,
        }))
        .map_err(|error| ReviewAttemptError::Fatal(error.into()))?;
        let response = request_structured_once(
            &self.provider,
            &self.client,
            self.max_output_tokens,
            &self.telemetry,
            self.rate_limiter.as_ref(),
            &request.system_prompt,
            user_content,
            "adjudication",
        )
        .await?;
        let decision = match AdjudicationDecision::parse_and_validate(&response.content) {
            Ok(decision) => decision,
            Err(error) => {
                self.telemetry.record_error(response.elapsed_ms);
                return Err(ReviewAttemptError::Retryable(error));
            }
        };
        let requested: std::collections::HashSet<&str> = request
            .review
            .claims_requiring_verification
            .iter()
            .map(|claim| claim.claim_id.as_str())
            .collect();
        let returned: std::collections::HashSet<&str> = decision
            .claims
            .iter()
            .map(|claim| claim.claim_id.as_str())
            .collect();
        if requested != returned {
            self.telemetry.record_error(response.elapsed_ms);
            return Err(ReviewAttemptError::Retryable(anyhow::anyhow!(
                "adjudication claim IDs do not match the review request"
            )));
        }
        self.telemetry.record_success(response.elapsed_ms);
        let (input_tokens, output_tokens) = usage_tokens(&response.payload);
        Ok(AdjudicationResult {
            decision,
            model: self.provider.model.clone(),
            input_tokens,
            output_tokens,
        })
    }
}

#[async_trait]
impl CandidateAdjudicator for AdjudicationClient {
    async fn adjudicate(&self, request: AdjudicationRequest) -> Result<AdjudicationResult> {
        retry_structured(&self.telemetry, "adjudication", || {
            self.request_once(&request)
        })
        .await
    }
}

fn elapsed_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn extract_content(payload: &Value) -> Result<String> {
    let content = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .context("review response contains no assistant content")?;
    match content {
        Value::String(value) if !value.trim().is_empty() => Ok(value.clone()),
        Value::Array(parts) => {
            let combined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>();
            if combined.trim().is_empty() {
                bail!("review response assistant content is empty");
            }
            Ok(combined)
        }
        _ => bail!("review response assistant content is empty"),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn restricted_sampling(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let name = model.rsplit('/').next().unwrap_or(&model);
    name.contains("gpt-5")
        || name.contains("gpt5")
        || name.contains("luna")
        || name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4")
}

fn apply_model_reasoning_controls(model: &str, body: &mut Value) {
    let model = model.to_ascii_lowercase();
    if model.contains("qwen") || model.contains("deepseek-v4") {
        body["enable_thinking"] = json!(false);
        body["thinking_budget"] = json!(0);
        body["reasoning_effort"] = json!("none");
        body["include_reasoning"] = json!(false);
        body["chat_template_kwargs"] = json!({
            "enable_thinking": false,
            "thinking": false
        });
    }
}

fn preferred_response_formats(model: &str) -> Vec<StructuredOutputFormat> {
    if model.to_ascii_lowercase().contains("qwen") {
        vec![
            StructuredOutputFormat::JsonObject,
            StructuredOutputFormat::JsonSchema,
            StructuredOutputFormat::PromptOnly,
        ]
    } else {
        vec![
            StructuredOutputFormat::JsonSchema,
            StructuredOutputFormat::JsonObject,
            StructuredOutputFormat::PromptOnly,
        ]
    }
}

#[cfg(test)]
fn apply_model_response_schema(model: &str, body: &mut Value, operation: &str) -> Result<()> {
    let response_format = preferred_response_formats(model)
        .into_iter()
        .next()
        .context("no structured response format configured")?;
    apply_model_response_format(model, body, operation, response_format)
}

fn apply_model_response_format(
    _model: &str,
    body: &mut Value,
    operation: &str,
    response_format: StructuredOutputFormat,
) -> Result<()> {
    if !matches!(operation, "review" | "adjudication") {
        bail!("unsupported structured review operation: {operation}");
    }
    if response_format == StructuredOutputFormat::PromptOnly {
        body.as_object_mut()
            .context("structured request body must be a JSON object")?
            .remove("response_format");
        return Ok(());
    }
    if response_format == StructuredOutputFormat::JsonObject {
        body["response_format"] = json!({"type": "json_object"});
        return Ok(());
    }
    let (name, raw_schema) = match operation {
        "review" => (
            "prompt_review_v3",
            include_str!("../schemas/prompt-review-v3.schema.json"),
        ),
        "adjudication" => (
            "prompt_adjudication_v1",
            include_str!("../schemas/prompt-adjudication-v1.schema.json"),
        ),
        other => bail!("unsupported structured review operation: {other}"),
    };
    let mut schema: Value = serde_json::from_str(raw_schema)
        .with_context(|| format!("failed to parse bundled {operation} JSON schema"))?;
    relax_vllm_decoder_schema(&mut schema);
    body["response_format"] = json!({
        "type": "json_schema",
        "json_schema": {
            "name": name,
            "strict": true,
            "schema": schema
        }
    });
    Ok(())
}

fn looks_like_unsupported_response_format(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    [
        "response_format",
        "json_schema",
        "json mode",
        "structured output",
        "unsupported parameter",
        "unsupported",
        "not supported",
        "does not support",
        "invalid response format",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn relax_vllm_decoder_schema(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for unsupported in [
                "$schema",
                "$id",
                "title",
                "allOf",
                "if",
                "then",
                "else",
                "uniqueItems",
            ] {
                object.remove(unsupported);
            }
            for child in object.values_mut() {
                relax_vllm_decoder_schema(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                relax_vllm_decoder_schema(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn schema_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key) || object.values().any(|child| schema_contains_key(child, key))
        }
        Value::Array(items) => items.iter().any(|item| schema_contains_key(item, key)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_review_prompts_state_schema_string_limits() {
        for prompt in [
            include_str!("../prompts/itops-prompt-review-system-v3.txt"),
            include_str!("../prompts/netops-prompt-review-system-v3.txt"),
        ] {
            assert!(prompt.contains("summary must be 1 to 800 characters"));
            assert!(prompt.contains("retry_guidance must be at most 800 characters"));
        }
    }

    #[test]
    fn netops_review_prompt_allows_clearly_normalized_generic_evidence() {
        let prompt = include_str!("../prompts/netops-prompt-review-system-v3.txt");
        assert!(prompt.contains("SD-WAN controllers and edges"));
        assert!(prompt.contains("Do not reject normalized standards-based evidence"));
    }

    #[test]
    fn bundled_review_prompts_require_concrete_defects_instead_of_epistemic_rejection() {
        for prompt in [
            include_str!("../prompts/itops-prompt-review-system-v3.txt"),
            include_str!("../prompts/netops-prompt-review-system-v3.txt"),
        ] {
            assert!(prompt.contains("Do not reject merely because you lack external lookup"));
            assert!(
                prompt.contains("A rejection must identify at least one specific material defect")
            );
        }
    }

    #[test]
    fn review_v3_accept_has_all_passes_and_no_repair_fields() {
        let raw = include_str!("../tests/fixtures/canonical/valid-review-v3.json");
        let decision = ReviewDecision::parse_and_validate(raw).unwrap();
        assert_eq!(decision.outcome, ReviewOutcome::Accept);
    }

    #[test]
    fn review_v3_reject_requires_failed_dimension_and_hard_failure() {
        let mut value: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/canonical/valid-review-v3.json"
        ))
        .unwrap();
        value["outcome"] = json!("reject");
        assert!(ReviewDecision::parse_and_validate(&value.to_string()).is_err());
    }

    #[test]
    fn strips_json_code_fence_before_validation() {
        let raw = r#"```json
        {"schema_version":"scogo.taskgen.prompt-review.v3","outcome":"accept","checks":{"coordinate_realization":{"status":"pass","rationale":"Coordinates are material.","evidence_paths":["$.candidate.prompt"]},"internal_consistency":{"status":"pass","rationale":"Internally consistent.","evidence_paths":["$.candidate.prompt"]},"operational_quality":{"status":"pass","rationale":"Operationally actionable.","evidence_paths":["$.candidate.prompt"]},"safety":{"status":"pass","rationale":"Read-only first.","evidence_paths":["$.candidate.prompt"]},"technical_authenticity":{"status":"pass","rationale":"Supported by fixture.","evidence_paths":["$.candidate.prompt"]}},"hard_failures":[],"claims_requiring_verification":[],"summary":"Coherent.","retry_guidance":""}
        ```"#;
        assert_eq!(
            ReviewDecision::parse_and_validate(raw).unwrap().outcome,
            ReviewOutcome::Accept
        );
    }

    #[test]
    fn clips_overlong_explanatory_fields_before_schema_validation() {
        let raw = serde_json::json!({
            "schema_version": "scogo.taskgen.prompt-review.v3",
            "outcome": "revise",
            "checks": {
                "coordinate_realization": {"status":"fail","rationale":"Mismatch.","evidence_paths":["$.candidate.prompt"]},
                "internal_consistency": {"status":"pass","rationale":"Consistent.","evidence_paths":[]},
                "operational_quality": {"status":"pass","rationale":"Operational.","evidence_paths":[]},
                "safety": {"status":"pass","rationale":"Safe.","evidence_paths":[]},
                "technical_authenticity": {"status":"pass","rationale":"Supported.","evidence_paths":[]}
            },
            "hard_failures": [],
            "claims_requiring_verification": [],
            "summary": "s".repeat(1025),
            "retry_guidance": "r".repeat(901),
        })
        .to_string();

        let (decision, normalization) =
            ReviewDecision::parse_and_validate_with_metadata(&raw).unwrap();
        assert_eq!(decision.summary.chars().count(), 800);
        assert_eq!(decision.retry_guidance.chars().count(), 800);
        assert!(normalization.summary_truncated);
        assert!(normalization.retry_guidance_truncated);
    }

    #[test]
    fn normalizes_qwen_hard_failure_labels_and_missing_claim_ids() {
        let raw = serde_json::json!({
            "schema_version": "scogo.taskgen.prompt-review.v3",
            "outcome": "reject",
            "checks": {
                "coordinate_realization": {"status":"pass","rationale":"Coordinates are material.","evidence_paths":[]},
                "internal_consistency": {"status":"fail","rationale":"The supplied timeline contradicts itself.","evidence_paths":["$.candidate.prompt"]},
                "operational_quality": {"status":"pass","rationale":"The task is operational.","evidence_paths":[]},
                "safety": {"status":"pass","rationale":"The task is read-only first.","evidence_paths":[]},
                "technical_authenticity": {"status":"pass","rationale":"The concepts are plausible.","evidence_paths":[]}
            },
            "hard_failures": ["technical inaccuracy"],
            "claims_requiring_verification": [],
            "summary": "The prompt contains a proven technical defect.",
            "retry_guidance": "Correct the contradictory technical statement."
        })
        .to_string();

        let (decision, _normalization) =
            ReviewDecision::parse_and_validate_with_metadata(&raw).unwrap();
        assert_eq!(
            decision.hard_failures,
            vec![HardFailure::TechnicalInaccuracy]
        );

        let raw = serde_json::json!({
            "schema_version": "scogo.taskgen.prompt-review.v3",
            "outcome": "needs_verification",
            "checks": {
                "coordinate_realization": {"status":"pass","rationale":"Coordinates are material.","evidence_paths":[]},
                "internal_consistency": {"status":"pass","rationale":"The supplied facts are consistent.","evidence_paths":[]},
                "operational_quality": {"status":"pass","rationale":"The task is operational.","evidence_paths":[]},
                "safety": {"status":"pass","rationale":"The task is read-only first.","evidence_paths":[]},
                "technical_authenticity": {"status":"unknown","rationale":"The vendor behavior needs a reference check.","evidence_paths":["$.candidate.prompt"]}
            },
            "hard_failures": [],
            "claims_requiring_verification": [{
                "claim": "The selected release supports the supplied feature.",
                "candidate_evidence_paths": ["$.candidate.prompt"],
                "reference_query": "selected release feature support"
            }],
            "summary": "One vendor claim needs verification.",
            "retry_guidance": ""
        })
        .to_string();

        let (decision, _normalization) =
            ReviewDecision::parse_and_validate_with_metadata(&raw).unwrap();
        assert_eq!(
            decision.claims_requiring_verification[0].claim_id,
            "claim-1"
        );
    }

    #[test]
    fn needs_verification_requires_unknown_check_and_claim() {
        let mut value: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/canonical/valid-review-v3.json"
        ))
        .unwrap();
        value["outcome"] = json!("needs_verification");
        value["checks"]["technical_authenticity"]["status"] = json!("unknown");
        value["claims_requiring_verification"] = json!([{
            "claim_id":"claim-1",
            "claim":"Vendor release X supports command Y.",
            "candidate_evidence_paths":["$.candidate.prompt"],
            "reference_query":"vendor release X command Y"
        }]);
        let decision = ReviewDecision::parse_and_validate(&value.to_string()).unwrap();
        assert_eq!(decision.outcome, ReviewOutcome::NeedsVerification);
    }

    #[test]
    fn adjudication_accept_requires_every_claim_supported_and_cited() {
        let valid = include_str!("../tests/fixtures/canonical/valid-adjudication-v1.json");
        assert_eq!(
            AdjudicationDecision::parse_and_validate(valid)
                .unwrap()
                .outcome,
            AdjudicationOutcome::Accept
        );

        let mut invalid: Value = serde_json::from_str(valid).unwrap();
        invalid["claims"][0]["verdict"] = json!("unverified");
        assert!(AdjudicationDecision::parse_and_validate(&invalid.to_string()).is_err());
    }

    #[test]
    fn restricted_review_models_are_detected_without_provider_prefix() {
        assert!(restricted_sampling("openai/gpt-5.4"));
        assert!(restricted_sampling("scogoai/gpt-5.6-luna-max"));
        assert!(!restricted_sampling("qwen/qwen3.8-max-free"));
    }

    #[test]
    fn deepseek_v4_review_uses_bounded_direct_structured_output() {
        let mut body = json!({});
        apply_model_reasoning_controls("deepseek-v4-flash-0731", &mut body);
        apply_model_response_schema("deepseek-v4-flash-0731", &mut body, "review").unwrap();

        assert_eq!(body["enable_thinking"], false);
        assert_eq!(body["thinking_budget"], 0);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["include_reasoning"], false);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["chat_template_kwargs"]["thinking"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "prompt_review_v3"
        );
        assert!(
            body["response_format"]["json_schema"]["schema"]
                .get("allOf")
                .is_none()
        );
        assert!(!schema_contains_key(
            &body["response_format"]["json_schema"]["schema"],
            "uniqueItems"
        ));
    }

    #[test]
    fn deepseek_v4_adjudication_uses_its_own_schema() {
        let mut body = json!({});
        apply_model_response_schema("deepseek-v4-flash-0731", &mut body, "adjudication").unwrap();
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "prompt_adjudication_v1"
        );
        assert!(
            body["response_format"]["json_schema"]["schema"]
                .get("allOf")
                .is_none()
        );
    }

    #[test]
    fn qwen_review_uses_json_object_response_format() {
        let mut body = json!({});
        apply_model_response_schema("qwen/qwen3.8-max-free", &mut body, "review").unwrap();
        assert_eq!(body["response_format"]["type"], "json_object");
    }

    #[test]
    fn generic_review_uses_strict_json_schema_response_format() {
        let mut body = json!({});
        apply_model_response_schema("openrouter/reviewer-model", &mut body, "review").unwrap();
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn other_review_models_use_provider_neutral_schema_negotiation() {
        let mut body = json!({});
        apply_model_response_schema("gpt-4o-mini", &mut body, "review").unwrap();
        assert_eq!(body["response_format"]["type"], "json_schema");
    }

    #[tokio::test]
    async fn reviewer_honors_rate_limit_and_records_retry_telemetry() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct RateLimitedOnce {
            calls: Arc<AtomicUsize>,
        }

        impl Respond for RateLimitedOnce {
            fn respond(&self, _request: &Request) -> ResponseTemplate {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(429).insert_header("retry-after", "0")
                } else {
                    let decision: Value = serde_json::from_str(include_str!(
                        "../tests/fixtures/canonical/valid-review-v3.json"
                    ))
                    .unwrap();
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices":[{"message":{"content":decision.to_string()}}],
                        "usage":{"prompt_tokens":10,"completion_tokens":5}
                    }))
                }
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(RateLimitedOnce {
                calls: Arc::new(AtomicUsize::new(0)),
            })
            .mount(&server)
            .await;
        let telemetry = Arc::new(crate::telemetry::RequestTelemetry::default());
        let provider = ProviderConfig {
            api_base: crate::provider::normalize_api_base(&format!("{}/v1", server.uri())).unwrap(),
            model: "review-model".into(),
            credentials: crate::provider::CredentialPool::new(vec![
                crate::provider::SecretString::new("test-key"),
            ])
            .unwrap(),
        };
        let reviewer = ReviewClient::new(
            provider,
            reqwest::Client::new(),
            512,
            telemetry.clone(),
            None,
        )
        .unwrap();

        let result = reviewer
            .review(ReviewRequest {
                candidate: serde_json::json!({"prompt":"Investigate read-only evidence."}),
                taxonomy_id: "test-taxonomy".into(),
                taxonomy_kind: "compositional".into(),
                system_prompt: "Return the required JSON decision.".into(),
                deterministic_checks: None,
            })
            .await
            .unwrap();

        assert_eq!(result.decision.outcome, ReviewOutcome::Accept);
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.rate_limits, 1);
        assert_eq!(snapshot.retries, 1);
    }

    #[tokio::test]
    async fn review_rate_limiter_spaces_requests() {
        let limiter = ReviewRateLimiter::from_requests_per_minute(Some(120))
            .unwrap()
            .unwrap();
        limiter.acquire().await;
        let started = Instant::now();
        limiter.acquire().await;
        assert!(started.elapsed() >= Duration::from_millis(450));
    }

    #[tokio::test]
    async fn reviewer_falls_back_when_provider_rejects_strict_schema() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

        #[derive(Clone)]
        struct SchemaFallback {
            calls: Arc<AtomicUsize>,
        }

        impl Respond for SchemaFallback {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["stream"], false);
                if call == 0 {
                    assert_eq!(body["response_format"]["type"], "json_schema");
                    return ResponseTemplate::new(400).set_body_json(json!({
                        "error": {"message": "response_format json_schema is unsupported"}
                    }));
                }
                assert_eq!(body["response_format"]["type"], "json_object");
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{"message": {"content": include_str!(
                        "../tests/fixtures/canonical/valid-review-v3.json"
                    )}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                }))
            }
        }

        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(SchemaFallback {
                calls: calls.clone(),
            })
            .mount(&server)
            .await;
        let provider = ProviderConfig {
            api_base: crate::provider::normalize_api_base(&format!("{}/v1", server.uri())).unwrap(),
            model: "openrouter/reviewer-model".into(),
            credentials: crate::provider::CredentialPool::new(vec![
                crate::provider::SecretString::new("test-key"),
            ])
            .unwrap(),
        };
        let reviewer = ReviewClient::new(
            provider,
            reqwest::Client::new(),
            512,
            Arc::new(crate::telemetry::RequestTelemetry::default()),
            None,
        )
        .unwrap();

        let result = reviewer
            .review(ReviewRequest {
                candidate: json!({"prompt":"Investigate read-only evidence."}),
                taxonomy_id: "test-taxonomy".into(),
                taxonomy_kind: "compositional".into(),
                system_prompt: "Return exactly one JSON object.".into(),
                deterministic_checks: None,
            })
            .await
            .unwrap();

        assert_eq!(result.decision.outcome, ReviewOutcome::Accept);
        assert_eq!(result.normalization.response_format, "json_object");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
