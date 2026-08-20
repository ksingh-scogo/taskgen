use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::ProviderConfig;
use crate::schema::{self, SchemaKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
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
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    pub schema_version: String,
    pub verdict: ReviewVerdict,
    pub reason_codes: Vec<ReviewReason>,
    pub summary: String,
    pub retry_guidance: String,
}

impl ReviewDecision {
    pub fn parse_and_validate(raw: &str) -> Result<Self> {
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
        let value: Value = serde_json::from_str(json_text).context("invalid review JSON")?;
        schema::validate_instance(SchemaKind::PromptReview, &value)
            .context("review JSON failed schema validation")?;
        serde_json::from_value(value).context("invalid review decision")
    }
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub candidate: Value,
    pub taxonomy_id: String,
    pub taxonomy_kind: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
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
}

#[derive(Debug)]
enum ReviewAttemptError {
    RateLimit {
        retry_after_seconds: Option<u64>,
        message: String,
    },
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ReviewAttemptError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::RateLimit { message, .. } => anyhow::anyhow!(message),
            Self::Retryable(error) | Self::Fatal(error) => error,
        }
    }
}

impl ReviewClient {
    pub fn new(
        provider: ProviderConfig,
        client: reqwest::Client,
        max_output_tokens: u64,
        telemetry: Arc<crate::telemetry::RequestTelemetry>,
    ) -> Result<Self> {
        if max_output_tokens == 0 {
            bail!("review max output tokens must be positive");
        }
        Ok(Self {
            provider,
            client,
            max_output_tokens,
            telemetry,
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
        }))
        .map_err(|error| ReviewAttemptError::Fatal(error.into()))?;
        let user_content = if self.provider.model.to_ascii_lowercase().contains("qwen") {
            format!("{user_content}\n/no_think")
        } else {
            user_content
        };
        let system_content = if self.provider.model.to_ascii_lowercase().contains("qwen") {
            format!("{}\n/no_think", request.system_prompt)
        } else {
            request.system_prompt.clone()
        };
        let mut body = json!({
            "model": self.provider.model,
            "messages": [
                {"role": "system", "content": system_content},
                {"role": "user", "content": user_content}
            ],
            "max_completion_tokens": self.max_output_tokens
        });
        if !restricted_sampling(&self.provider.model) {
            body["temperature"] = json!(0.0);
        }
        if self.provider.model.to_ascii_lowercase().contains("qwen") {
            body["enable_thinking"] = json!(false);
            body["thinking_budget"] = json!(0);
            body["reasoning_effort"] = json!("low");
        }
        let url = format!(
            "{}/chat/completions",
            self.provider.api_base.as_str().trim_end_matches('/')
        );
        let credential = self.provider.credentials.next();
        let started = Instant::now();
        let response = self
            .client
            .post(url)
            .bearer_auth(credential.expose())
            .json(&body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                self.telemetry
                    .record_timeout(elapsed_millis(started.elapsed()));
                return Err(ReviewAttemptError::Retryable(
                    anyhow::anyhow!(error).context("review request timed out"),
                ));
            }
            Err(error) => {
                self.telemetry
                    .record_error(elapsed_millis(started.elapsed()));
                return Err(ReviewAttemptError::Retryable(
                    anyhow::anyhow!(error).context("review request failed"),
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
            self.telemetry
                .record_error(elapsed_millis(started.elapsed()));
            ReviewAttemptError::Retryable(
                anyhow::anyhow!(error).context("failed to read review response"),
            )
        })?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.telemetry
                .record_rate_limit(elapsed_millis(started.elapsed()));
            return Err(ReviewAttemptError::RateLimit {
                retry_after_seconds,
                message: format!("review API returned HTTP {status}: {}", truncate(&raw, 500)),
            });
        }
        if !status.is_success() {
            self.telemetry
                .record_error(elapsed_millis(started.elapsed()));
            let error =
                anyhow::anyhow!("review API returned HTTP {status}: {}", truncate(&raw, 500));
            return if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
                Err(ReviewAttemptError::Retryable(error))
            } else {
                Err(ReviewAttemptError::Fatal(error))
            };
        }
        let parsed = (|| -> Result<(Value, ReviewDecision)> {
            let payload: Value = serde_json::from_str(&raw).context("invalid review API JSON")?;
            let content = extract_content(&payload)?;
            let decision = ReviewDecision::parse_and_validate(&content)?;
            Ok((payload, decision))
        })();
        let (payload, decision) = match parsed {
            Ok(value) => value,
            Err(error) => {
                self.telemetry
                    .record_error(elapsed_millis(started.elapsed()));
                return Err(ReviewAttemptError::Retryable(error));
            }
        };
        self.telemetry
            .record_success(elapsed_millis(started.elapsed()));
        let usage = payload.get("usage").cloned().unwrap_or(Value::Null);
        Ok(ReviewResult {
            decision,
            model: self.provider.model.clone(),
            input_tokens: usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }
}

#[async_trait]
impl CandidateReviewer for ReviewClient {
    async fn review(&self, request: ReviewRequest) -> Result<ReviewResult> {
        let mut last_error = None;
        for attempt in 0..3u32 {
            match self.request_once(&request).await {
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
                        self.telemetry.record_retry();
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(last_error.context("review request failed without an error")?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_review_has_no_reasons_or_retry_guidance() {
        let raw = r#"{
          "schema_version":"scogo.taskgen.prompt-review.v1",
          "verdict":"accept",
          "reason_codes":[],
          "summary":"The prompt is operationally coherent.",
          "retry_guidance":""
        }"#;
        let decision = ReviewDecision::parse_and_validate(raw).unwrap();
        assert_eq!(decision.verdict, ReviewVerdict::Accept);
    }

    #[test]
    fn rejected_review_requires_reason_and_guidance() {
        let raw = r#"{
          "schema_version":"scogo.taskgen.prompt-review.v1",
          "verdict":"reject",
          "reason_codes":[],
          "summary":"Bad prompt.",
          "retry_guidance":""
        }"#;
        assert!(ReviewDecision::parse_and_validate(raw).is_err());
    }

    #[test]
    fn strips_json_code_fence_before_validation() {
        let raw = r#"```json
        {"schema_version":"scogo.taskgen.prompt-review.v1","verdict":"accept","reason_codes":[],"summary":"Coherent.","retry_guidance":""}
        ```"#;
        assert_eq!(
            ReviewDecision::parse_and_validate(raw).unwrap().verdict,
            ReviewVerdict::Accept
        );
    }

    #[test]
    fn restricted_review_models_are_detected_without_provider_prefix() {
        assert!(restricted_sampling("openai/gpt-5.4"));
        assert!(restricted_sampling("scogoai/gpt-5.6-luna-max"));
        assert!(!restricted_sampling("qwen/qwen3.8-max-free"));
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
                    let decision = serde_json::json!({
                        "schema_version":"scogo.taskgen.prompt-review.v1",
                        "verdict":"accept",
                        "reason_codes":[],
                        "summary":"Operationally coherent.",
                        "retry_guidance":""
                    });
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
        let reviewer =
            ReviewClient::new(provider, reqwest::Client::new(), 512, telemetry.clone()).unwrap();

        let result = reviewer
            .review(ReviewRequest {
                candidate: serde_json::json!({"prompt":"Investigate read-only evidence."}),
                taxonomy_id: "test-taxonomy".into(),
                taxonomy_kind: "compositional".into(),
                system_prompt: "Return the required JSON decision.".into(),
            })
            .await
            .unwrap();

        assert_eq!(result.decision.verdict, ReviewVerdict::Accept);
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.rate_limits, 1);
        assert_eq!(snapshot.retries, 1);
    }
}
