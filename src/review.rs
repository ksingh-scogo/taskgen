use std::time::Duration;

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
}

impl ReviewClient {
    pub fn new(
        provider: ProviderConfig,
        client: reqwest::Client,
        max_output_tokens: u64,
    ) -> Result<Self> {
        if max_output_tokens == 0 {
            bail!("review max output tokens must be positive");
        }
        Ok(Self {
            provider,
            client,
            max_output_tokens,
        })
    }

    async fn request_once(&self, request: &ReviewRequest) -> Result<ReviewResult> {
        let user_content = serde_json::to_string_pretty(&json!({
            "instruction": "Review this prompt seed against every required quality dimension. Return only the required JSON decision. Do not solve the task.",
            "taxonomy_id": request.taxonomy_id,
            "taxonomy_kind": request.taxonomy_kind,
            "candidate": request.candidate,
        }))?;
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
        let response = self
            .client
            .post(url)
            .bearer_auth(credential.expose())
            .json(&body)
            .send()
            .await
            .context("review request failed")?;
        let status = response.status();
        let raw = response
            .text()
            .await
            .context("failed to read review response")?;
        if !status.is_success() {
            bail!("review API returned HTTP {status}: {}", truncate(&raw, 500));
        }
        let payload: Value = serde_json::from_str(&raw).context("invalid review API JSON")?;
        let content = extract_content(&payload)?;
        let decision = ReviewDecision::parse_and_validate(&content)?;
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
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(250 * 2u64.pow(attempt))).await;
                    }
                }
            }
        }
        Err(last_error.context("review request failed without an error")?)
    }
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
}
