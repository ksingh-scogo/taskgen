use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::schema::{self, SchemaKind};

const ATIF_VERSION: &str = "ATIF-v1.7";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifTrajectory {
    pub schema_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,
    pub agent: AtifAgent,
    pub steps: Vec<AtifStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_metrics: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_trajectory_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_trajectories: Vec<AtifTrajectory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifAgent {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifStep {
    pub step_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Value>,
    pub message: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AtifToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_call_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_copied_context: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifToolCall {
    pub tool_call_id: String,
    pub function_name: String,
    pub arguments: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifObservation {
    pub results: Vec<AtifObservationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifObservationResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_trajectory_ref: Option<Vec<AtifSubagentRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AtifSubagentRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trajectory_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Map<String, Value>>,
}

pub fn validate_trajectory(trajectory: &AtifTrajectory) -> Result<()> {
    validate_trajectory_inner(trajectory, false, "root")
}

fn validate_trajectory_inner(
    trajectory: &AtifTrajectory,
    embedded: bool,
    location: &str,
) -> Result<()> {
    if trajectory.schema_version != ATIF_VERSION {
        bail!(
            "{location}.schema_version must be '{ATIF_VERSION}', got '{}'",
            trajectory.schema_version
        );
    }
    if embedded
        && trajectory
            .trajectory_id
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    {
        bail!("{location}.trajectory_id is required for embedded trajectories");
    }
    if trajectory.agent.name.trim().is_empty() || trajectory.agent.version.trim().is_empty() {
        bail!("{location}.agent name and version must not be empty");
    }
    if trajectory
        .continued_trajectory_ref
        .as_deref()
        .is_some_and(|reference| reference.trim().is_empty())
    {
        bail!("{location}.continued_trajectory_ref must not be empty");
    }

    let mut child_ids = HashSet::new();
    for (index, child) in trajectory.subagent_trajectories.iter().enumerate() {
        let child_location = format!("{location}.subagent_trajectories[{index}]");
        let id = child
            .trajectory_id
            .as_deref()
            .context(format!("{child_location}.trajectory_id is required"))?;
        if !child_ids.insert(id) {
            bail!("{location} contains duplicate embedded trajectory_id '{id}'");
        }
        validate_trajectory_inner(child, true, &child_location)?;
    }

    let mut all_call_ids = HashSet::new();
    for (index, step) in trajectory.steps.iter().enumerate() {
        let expected = index as u64 + 1;
        if step.step_id != expected {
            bail!(
                "{location}.steps[{index}].step_id must be {expected}, got {}",
                step.step_id
            );
        }
        validate_step(step, location, index, &child_ids, &mut all_call_ids)?;
    }
    Ok(())
}

fn validate_step(
    step: &AtifStep,
    location: &str,
    index: usize,
    child_ids: &HashSet<&str>,
    all_call_ids: &mut HashSet<String>,
) -> Result<()> {
    let step_location = format!("{location}.steps[{index}]");
    if !matches!(step.source.as_str(), "system" | "user" | "agent") {
        bail!("{step_location}.source must be system, user, or agent");
    }
    validate_content(&step.message, &format!("{step_location}.message"))?;
    if let Some(timestamp) = &step.timestamp {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .with_context(|| format!("{step_location}.timestamp is not RFC 3339"))?;
    }
    if let Some(effort) = &step.reasoning_effort
        && !effort.is_string()
        && !effort.is_number()
    {
        bail!("{step_location}.reasoning_effort must be a string or number");
    }
    if let Some(metrics) = &step.metrics
        && !metrics.is_object()
    {
        bail!("{step_location}.metrics must be an object");
    }

    match step.source.as_str() {
        "agent" => {}
        "system" => {
            if step.model_name.is_some()
                || step.reasoning_effort.is_some()
                || step.reasoning_content.is_some()
                || step.tool_calls.is_some()
                || step.metrics.is_some()
            {
                bail!("{step_location} contains agent-only fields on a system step");
            }
        }
        "user" => {
            if step.model_name.is_some()
                || step.reasoning_effort.is_some()
                || step.reasoning_content.is_some()
                || step.tool_calls.is_some()
                || step.observation.is_some()
                || step.metrics.is_some()
            {
                bail!("{step_location} contains agent/system-only fields on a user step");
            }
        }
        _ => unreachable!(),
    }
    if step.source == "agent"
        && step.llm_call_count == Some(0)
        && (step.metrics.is_some() || step.reasoning_content.is_some())
    {
        bail!("{step_location} with llm_call_count=0 must omit metrics and reasoning_content");
    }

    let step_call_ids: HashSet<&str> = step
        .tool_calls
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|call| call.tool_call_id.as_str())
        .collect();
    if step_call_ids.len() != step.tool_calls.as_deref().unwrap_or_default().len() {
        bail!("{step_location} contains duplicate tool_call_id values");
    }
    for call in step.tool_calls.as_deref().unwrap_or_default() {
        if call.tool_call_id.trim().is_empty() || call.function_name.trim().is_empty() {
            bail!("{step_location} contains an empty tool call ID or function name");
        }
        if !all_call_ids.insert(call.tool_call_id.clone()) {
            bail!(
                "{step_location} reuses tool_call_id '{}' from another step",
                call.tool_call_id
            );
        }
    }
    if let Some(observation) = &step.observation {
        for (result_index, result) in observation.results.iter().enumerate() {
            let result_location = format!("{step_location}.observation.results[{result_index}]");
            if result.content.is_none() && result.subagent_trajectory_ref.is_none() {
                bail!("{result_location} requires content or subagent_trajectory_ref");
            }
            if let Some(content) = &result.content {
                validate_content(content, &format!("{result_location}.content"))?;
            }
            if let Some(source_call_id) = result.source_call_id.as_deref()
                && !step_call_ids.contains(source_call_id)
            {
                bail!(
                    "{result_location}.source_call_id '{source_call_id}' does not match a tool call in the same step"
                );
            }
            for reference in result
                .subagent_trajectory_ref
                .as_deref()
                .unwrap_or_default()
            {
                if reference.trajectory_id.is_none() && reference.trajectory_path.is_none() {
                    bail!(
                        "{result_location} subagent reference requires trajectory_id or trajectory_path"
                    );
                }
                if reference.trajectory_path.is_none()
                    && !child_ids.contains(reference.trajectory_id.as_deref().unwrap_or_default())
                {
                    bail!(
                        "{result_location} references missing embedded trajectory_id '{}'",
                        reference.trajectory_id.as_deref().unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_content(content: &Value, location: &str) -> Result<()> {
    if content.is_string() {
        return Ok(());
    }
    let parts = content
        .as_array()
        .context(format!("{location} must be a string or content-part array"))?;
    for (index, part) in parts.iter().enumerate() {
        let object = part
            .as_object()
            .context(format!("{location}[{index}] must be an object"))?;
        match object.get("type").and_then(Value::as_str) {
            Some("text") if object.get("text").is_some_and(Value::is_string) => {
                if object.contains_key("source") {
                    bail!("{location}[{index}] text part must omit source");
                }
            }
            Some("image") => {
                if object.contains_key("text") {
                    bail!("{location}[{index}] image part must omit text");
                }
                let source = object
                    .get("source")
                    .and_then(Value::as_object)
                    .context(format!("{location}[{index}] image part requires source"))?;
                let media = source.get("media_type").and_then(Value::as_str);
                if !matches!(
                    media,
                    Some("image/jpeg" | "image/png" | "image/gif" | "image/webp")
                ) || !source.get("path").is_some_and(Value::is_string)
                {
                    bail!("{location}[{index}] contains an invalid image source");
                }
            }
            _ => bail!("{location}[{index}] must be a valid text or image part"),
        }
    }
    Ok(())
}

pub fn export_audit(audit: &Value) -> Result<AtifTrajectory> {
    schema::validate_instance(SchemaKind::AuditTrajectory, audit)
        .context("invalid canonical audit trajectory")?;
    let generation = &audit["generation"];
    let messages = audit["messages"]
        .as_array()
        .context("audit.messages must be an array")?;
    let mut steps = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        let role = message["role"]
            .as_str()
            .context("message role is missing")?;
        if role == "tool" {
            bail!("audit contains a tool message without a preceding assistant tool call");
        }
        let source = match role {
            "system" => "system",
            "user" => "user",
            "assistant" => "agent",
            other => bail!("unsupported canonical message role '{other}'"),
        };
        let atif_calls = if role == "assistant" {
            let calls: Vec<AtifToolCall> = message["tool_calls"]
                .as_array()
                .context("assistant tool_calls must be an array")?
                .iter()
                .map(canonical_tool_call_to_atif)
                .collect::<Result<_>>()?;
            (!calls.is_empty()).then_some(calls)
        } else {
            None
        };
        let issued_ids: HashSet<&str> = atif_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|call| call.tool_call_id.as_str())
            .collect();
        let mut results = Vec::new();
        let mut next = index + 1;
        while role == "assistant" && next < messages.len() && messages[next]["role"] == "tool" {
            let tool_message = &messages[next];
            let call_id = tool_message["tool_call_id"]
                .as_str()
                .context("tool message is missing tool_call_id")?;
            if !issued_ids.contains(call_id) {
                bail!("tool message references unknown call '{call_id}'");
            }
            results.push(AtifObservationResult {
                source_call_id: Some(call_id.to_string()),
                content: Some(tool_message["content"].clone()),
                subagent_trajectory_ref: None,
                extra: Some(json_object([("scogo_message", tool_message.clone())])),
            });
            next += 1;
        }
        let observation = (!results.is_empty()).then_some(AtifObservation { results });
        let mut step_extra = Map::new();
        step_extra.insert("scogo_message".into(), message.clone());
        steps.push(AtifStep {
            step_id: steps.len() as u64 + 1,
            timestamp: optional_string(&message["timestamp"]),
            source: source.into(),
            model_name: (role == "assistant")
                .then(|| optional_string(&generation["teacher_model"]))
                .flatten(),
            reasoning_effort: None,
            message: message["content"].clone(),
            reasoning_content: None,
            tool_calls: atif_calls,
            observation,
            metrics: None,
            extra: Some(step_extra),
            llm_call_count: (role == "assistant").then_some(1),
            is_copied_context: None,
        });
        index = next;
    }

    let mut extra = Map::new();
    extra.insert("scogo".into(), audit.clone());
    let trajectory = AtifTrajectory {
        schema_version: ATIF_VERSION.into(),
        session_id: optional_string(&generation["run_id"]),
        trajectory_id: optional_string(&audit["trajectory_id"]),
        agent: AtifAgent {
            name: "scogo-netops-teacher".into(),
            version: generation["system_prompt_version"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            model_name: optional_string(&generation["teacher_model"]),
            tool_definitions: audit["tools"].as_array().cloned(),
            extra: None,
        },
        steps,
        notes: Some(
            "Exported from the Scogo canonical audit schema; hidden reasoning omitted.".into(),
        ),
        final_metrics: None,
        continued_trajectory_ref: None,
        extra: Some(extra),
        subagent_trajectories: Vec::new(),
    };
    validate_trajectory(&trajectory)?;
    Ok(trajectory)
}

fn canonical_tool_call_to_atif(call: &Value) -> Result<AtifToolCall> {
    Ok(AtifToolCall {
        tool_call_id: call["id"]
            .as_str()
            .context("canonical tool call is missing id")?
            .to_string(),
        function_name: call["function"]["name"]
            .as_str()
            .context("canonical tool call is missing function.name")?
            .to_string(),
        arguments: call["function"]["arguments"]
            .as_object()
            .context("canonical tool call arguments must be an object")?
            .clone(),
        extra: None,
    })
}

pub fn import_trajectory(trajectory: &AtifTrajectory) -> Result<Value> {
    validate_trajectory(trajectory)?;
    if let Some(scogo) = trajectory
        .extra
        .as_ref()
        .and_then(|extra| extra.get("scogo"))
    {
        schema::validate_instance(SchemaKind::AuditTrajectory, scogo)
            .context("ATIF extra.scogo is not a valid canonical audit record")?;
        return Ok(scogo.clone());
    }

    let trajectory_value = serde_json::to_value(trajectory)?;
    let content_hash = sha256_json(&trajectory_value)?;
    let short_hash = &content_hash[..16];
    let prompt_value = trajectory
        .steps
        .iter()
        .find(|step| step.source == "user")
        .map(|step| step.message.clone())
        .unwrap_or_else(|| Value::String(String::new()));
    let prompt = content_to_string(&prompt_value)?;
    let prompt_hash = sha256_bytes(prompt.as_bytes());
    let trajectory_id = trajectory
        .trajectory_id
        .clone()
        .unwrap_or_else(|| format!("atif-{short_hash}"));
    let run_id = trajectory
        .session_id
        .clone()
        .unwrap_or_else(|| format!("atif-run-{short_hash}"));
    let generated_at = trajectory
        .steps
        .iter()
        .find_map(|step| step.timestamp.clone())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into());
    let tools = canonical_tool_definitions(trajectory.agent.tool_definitions.as_deref())?;
    let allowed_tool_names = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let messages = atif_steps_to_messages(trajectory)?;

    let imported = json!({
        "schema_version": "scogo.netops.teacher-trajectory.audit.v1",
        "record_kind": "imported",
        "sample_id": format!("imported-{short_hash}"),
        "trajectory_id": trajectory_id,
        "task": {
            "prompt_id": format!("imported-prompt-{short_hash}"),
            "prompt_sha256": prompt_hash,
            "prompt": prompt,
            "taxonomy_id": "external_atif_unverified",
            "coordinates": {
                "taxonomy_id": "external_atif_unverified",
                "task_family": "external_atif_unverified",
                "environment": "external_atif_unverified",
                "vendor_scope": "external_atif_unverified",
                "vendors": [],
                "incident_mechanism": "external_atif_unverified",
                "evidence_condition": "external_atif_unverified",
                "evidence_bundle": "external_atif_unverified",
                "action_risk": "external_atif_unverified",
                "presentation": "external_atif_unverified"
            },
            "difficulty": null,
            "action_risk": "external_atif_unverified",
            "split_group_id": format!("external-atif-{short_hash}")
        },
        "generation": {
            "run_id": run_id,
            "provider": "atif_import",
            "teacher_model": trajectory.agent.model_name.clone().unwrap_or_else(|| "unknown".into()),
            "model_revision": null,
            "temperature": null,
            "seed": null,
            "system_prompt_version": "external_atif_unverified",
            "generated_at": generated_at,
            "raw_response_ref": null
        },
        "environment": {
            "mode": "replay",
            "environment_id": "external_atif_unverified",
            "topology_ref": null,
            "fixture_ref": null,
            "reset_ref": null,
            "initial_state_sha256": null,
            "allowed_tool_names": allowed_tool_names
        },
        "tools": tools,
        "messages": messages,
        "evidence": [],
        "approval": {
            "required": false,
            "requested": false,
            "granted": null,
            "scope": null,
            "decision_source": null,
            "decided_at": null
        },
        "outcome": {
            "status": "unknown",
            "root_cause": {"summary": null, "entities": [], "evidence_refs": []},
            "confidence": null,
            "uncertainty": ["Imported ATIF has not been independently replayed or verified."],
            "remediation": {"planned": [], "executed": []},
            "verification_summary": null,
            "rollback_summary": null,
            "abstention_reason": null,
            "escalation_target": null
        },
        "verification": {
            "oracle": null,
            "checks": [],
            "passed": null,
            "pre_state_sha256": null,
            "post_state_sha256": null,
            "regressions": [],
            "rollback_tested": null,
            "verifier": null
        },
        "safety": {
            "read_before_write": null,
            "write_without_approval": false,
            "prohibited_actions": [],
            "destructive_actions": [],
            "secrets_exposed": false,
            "policy_pass": null,
            "violations": []
        },
        "quality": {
            "schema_valid": true,
            "tool_calls_valid": true,
            "grounded": null,
            "terminal_claim_valid": null,
            "accepted": false,
            "rejection_reasons": ["external_atif_unverified"],
            "grader_refs": []
        },
        "provenance": {
            "taskgen_run_id": "external_atif_import",
            "source_prompt_ref": format!("atif://{short_hash}"),
            "source_refs": [],
            "license_review": "pending",
            "contamination_review": "pending",
            "content_sha256": content_hash
        },
        "interop": {
            "source_format": "atif",
            "source_schema_version": ATIF_VERSION,
            "original_atif": trajectory_value
        }
    });
    schema::validate_instance(SchemaKind::AuditTrajectory, &imported)?;
    Ok(imported)
}

fn canonical_tool_definitions(definitions: Option<&[Value]>) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for (index, definition) in definitions.unwrap_or_default().iter().enumerate() {
        let function = definition["function"].as_object().with_context(|| {
            format!("agent.tool_definitions[{index}].function must be an object")
        })?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .with_context(|| format!("agent.tool_definitions[{index}] is missing function.name"))?;
        let parameters = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !parameters.is_object() && !parameters.is_boolean() {
            bail!(
                "agent.tool_definitions[{index}].function.parameters must be an object or boolean"
            );
        }
        let mut canonical_function = Map::new();
        canonical_function.insert("name".into(), Value::String(name.into()));
        if let Some(description) = function.get("description").and_then(Value::as_str) {
            canonical_function.insert("description".into(), Value::String(description.into()));
        }
        canonical_function.insert("parameters".into(), parameters);
        output.push(json!({"type": "function", "function": canonical_function}));
    }
    Ok(output)
}

fn atif_steps_to_messages(trajectory: &AtifTrajectory) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut message_index = 1;
    for step in &trajectory.steps {
        let role = match step.source.as_str() {
            "system" => "system",
            "user" => "user",
            "agent" => "assistant",
            _ => unreachable!(),
        };
        let tool_calls = step
            .tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|call| {
                json!({
                    "id": call.tool_call_id,
                    "type": "function",
                    "function": {
                        "name": call.function_name,
                        "arguments": call.arguments
                    }
                })
            })
            .collect::<Vec<_>>();
        messages.push(json!({
            "message_id": format!("atif-message-{message_index}"),
            "role": role,
            "content": step.message,
            "tool_calls": tool_calls,
            "tool_call_id": null,
            "evidence_refs": [],
            "timestamp": step.timestamp
        }));
        message_index += 1;
        for result in step
            .observation
            .as_ref()
            .map(|observation| observation.results.as_slice())
            .unwrap_or_default()
        {
            let Some(call_id) = result.source_call_id.as_deref() else {
                continue;
            };
            let Some(content) = result.content.clone() else {
                continue;
            };
            messages.push(json!({
                "message_id": format!("atif-message-{message_index}"),
                "role": "tool",
                "content": content,
                "tool_calls": [],
                "tool_call_id": call_id,
                "evidence_refs": [],
                "timestamp": step.timestamp
            }));
            message_index += 1;
        }
    }
    if messages.is_empty() {
        messages.push(json!({
            "message_id": "atif-message-1",
            "role": "system",
            "content": "Empty external ATIF trajectory.",
            "tool_calls": [],
            "tool_call_id": null,
            "evidence_refs": [],
            "timestamp": null
        }));
    }
    Ok(messages)
}

fn optional_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn content_to_string(value: &Value) -> Result<String> {
    if let Some(text) = value.as_str() {
        return Ok(text.to_string());
    }
    serde_json::to_string(value).map_err(Into::into)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_json(value: &Value) -> Result<String> {
    Ok(sha256_bytes(&serde_json::to_vec(value)?))
}

fn json_object<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub enum ConversionDirection {
    Export,
    Import,
}

#[derive(Debug, Clone, Copy)]
pub enum Container {
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy)]
pub struct ConversionStats {
    pub records: usize,
}

pub fn infer_container(path: &Path) -> Result<Container> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => Ok(Container::Json),
        Some("jsonl") => Ok(Container::Jsonl),
        _ => bail!(
            "cannot infer container from '{}'; use --container",
            path.display()
        ),
    }
}

pub fn convert_file(
    direction: ConversionDirection,
    input: &Path,
    output: &Path,
    container: Container,
    overwrite: bool,
) -> Result<ConversionStats> {
    if output.exists() && !overwrite {
        bail!(
            "destination already exists: {} (use --overwrite)",
            output.display()
        );
    }
    let values = read_container(input, container)?;
    let mut converted = Vec::with_capacity(values.len());
    for (line, value) in values {
        let result = (match direction {
            ConversionDirection::Export => {
                serde_json::to_value(export_audit(&value)?).map_err(Into::into)
            }
            ConversionDirection::Import => {
                let trajectory: AtifTrajectory = serde_json::from_value(value)
                    .with_context(|| format!("record {line}: invalid ATIF object"))?;
                import_trajectory(&trajectory)
            }
        })
        .with_context(|| format!("record {line} conversion failed"))?;
        converted.push(result);
    }
    if matches!(container, Container::Json) && converted.len() != 1 {
        bail!("JSON container requires exactly one record");
    }

    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .context("output path must have a UTF-8 file name")?;
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut guard = TempOutput::create(temp_path)?;
    {
        let mut writer = BufWriter::new(guard.file.take().context("temporary file missing")?);
        match container {
            Container::Json => {
                serde_json::to_writer_pretty(&mut writer, &converted[0])?;
                writer.write_all(b"\n")?;
            }
            Container::Jsonl => {
                for value in &converted {
                    serde_json::to_writer(&mut writer, value)?;
                    writer.write_all(b"\n")?;
                }
            }
        }
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
    }
    fs::rename(&guard.path, output)
        .with_context(|| format!("failed to atomically replace {}", output.display()))?;
    guard.committed = true;
    Ok(ConversionStats {
        records: converted.len(),
    })
}

fn read_container(path: &Path, container: Container) -> Result<Vec<(usize, Value)>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    match container {
        Container::Json => Ok(vec![(
            1,
            serde_json::from_reader(BufReader::new(file))
                .with_context(|| format!("invalid JSON in {}", path.display()))?,
        )]),
        Container::Jsonl => {
            let mut values = Vec::new();
            for (index, line) in BufReader::new(file).lines().enumerate() {
                let line_number = index + 1;
                let line = line.with_context(|| format!("failed to read line {line_number}"))?;
                if line.trim().is_empty() {
                    continue;
                }
                values.push((
                    line_number,
                    serde_json::from_str(&line)
                        .with_context(|| format!("invalid JSONL at line {line_number}"))?,
                ));
            }
            Ok(values)
        }
    }
}

struct TempOutput {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl TempOutput {
    fn create(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create temporary output {}", path.display()))?;
        Ok(Self {
            path,
            file: Some(file),
            committed: false,
        })
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atif_fixture(source: &str) -> AtifTrajectory {
        serde_json::from_str(source).unwrap()
    }

    #[test]
    fn validates_atif_v1_7_tools_content_parts_and_context_markers() {
        let tool = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/valid-tool-trajectory.json"
        ));
        validate_trajectory(&tool).unwrap();
        let context = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/valid-copied-context.json"
        ));
        validate_trajectory(&context).unwrap();
        let scogo = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/valid-scogo-roundtrip.json"
        ));
        validate_trajectory(&scogo).unwrap();
    }

    #[test]
    fn rejects_wrong_version_step_sequence_and_tool_reference() {
        let mut wrong_version = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/valid-tool-trajectory.json"
        ));
        wrong_version.schema_version = "ATIF-v1.6".into();
        assert!(validate_trajectory(&wrong_version).is_err());

        let step = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/invalid-step-id.json"
        ));
        assert!(validate_trajectory(&step).is_err());
        let tool = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/invalid-tool-reference.json"
        ));
        assert!(validate_trajectory(&tool).is_err());
    }

    #[test]
    fn canonical_atif_round_trip_preserves_scogo_audit_sections() {
        let audit: Value =
            serde_json::from_str(include_str!("../tests/fixtures/canonical/valid-audit.json"))
                .unwrap();
        let atif = export_audit(&audit).unwrap();
        validate_trajectory(&atif).unwrap();
        let imported = import_trajectory(&atif).unwrap();
        assert_eq!(imported["trajectory_id"], audit["trajectory_id"]);
        assert_eq!(imported["approval"], audit["approval"]);
        assert_eq!(imported["verification"], audit["verification"]);
    }

    #[test]
    fn external_atif_import_is_unverified_and_unaccepted() {
        let atif = atif_fixture(include_str!(
            "../tests/fixtures/atif-v1.7/valid-tool-trajectory.json"
        ));
        let imported = import_trajectory(&atif).unwrap();
        assert_eq!(imported["record_kind"], "imported");
        assert_eq!(imported["task"]["difficulty"], Value::Null);
        assert_eq!(imported["outcome"]["status"], "unknown");
        assert_eq!(imported["quality"]["accepted"], false);
        assert_eq!(
            imported["quality"]["rejection_reasons"][0],
            "external_atif_unverified"
        );
        assert!(imported["interop"]["original_atif"].is_object());
    }

    #[test]
    fn jsonl_conversion_is_atomic_and_round_trips() {
        let directory = std::env::temp_dir().join(format!(
            "taskgen-atif-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&directory).unwrap();
        let audit_path = directory.join("audit.jsonl");
        let atif_path = directory.join("trajectory.jsonl");
        let restored_path = directory.join("restored.jsonl");
        let audit: Value =
            serde_json::from_str(include_str!("../tests/fixtures/canonical/valid-audit.json"))
                .unwrap();
        fs::write(
            &audit_path,
            format!("{}\n", serde_json::to_string(&audit).unwrap()),
        )
        .unwrap();

        let exported = convert_file(
            ConversionDirection::Export,
            &audit_path,
            &atif_path,
            Container::Jsonl,
            false,
        )
        .unwrap();
        assert_eq!(exported.records, 1);
        let imported = convert_file(
            ConversionDirection::Import,
            &atif_path,
            &restored_path,
            Container::Jsonl,
            false,
        )
        .unwrap();
        assert_eq!(imported.records, 1);
        let restored: Value =
            serde_json::from_str(fs::read_to_string(&restored_path).unwrap().trim()).unwrap();
        assert_eq!(restored["trajectory_id"], "trajectory-bgp-001");
        assert_eq!(restored["quality"]["accepted"], true);

        let invalid_path = directory.join("invalid.jsonl");
        let failed_output = directory.join("must-not-exist.jsonl");
        fs::write(&invalid_path, "{\"schema_version\":\"ATIF-v1.6\"}\n").unwrap();
        assert!(
            convert_file(
                ConversionDirection::Import,
                &invalid_path,
                &failed_output,
                Container::Jsonl,
                false,
            )
            .is_err()
        );
        assert!(!failed_output.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
