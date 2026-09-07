use anyhow::{Result, anyhow};
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub enum SchemaKind {
    Task,
    PromptReviewV3,
    PromptAdjudication,
    AuditTrajectory,
    SftTrajectory,
}

fn schema_source(kind: SchemaKind) -> &'static str {
    match kind {
        SchemaKind::Task => include_str!("../schemas/task-v2.schema.json"),
        SchemaKind::PromptReviewV3 => include_str!("../schemas/prompt-review-v3.schema.json"),
        SchemaKind::PromptAdjudication => {
            include_str!("../schemas/prompt-adjudication-v1.schema.json")
        }
        SchemaKind::AuditTrajectory => {
            include_str!("../schemas/netops-teacher-trajectory-audit-v1.schema.json")
        }
        SchemaKind::SftTrajectory => {
            include_str!("../schemas/netops-teacher-trajectory-sft-v1.schema.json")
        }
    }
}

pub fn schema_value(kind: SchemaKind) -> Result<Value> {
    serde_json::from_str(schema_source(kind)).map_err(Into::into)
}

fn compiled_validator(kind: SchemaKind) -> Result<&'static jsonschema::Validator> {
    static TASK: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    static REVIEW: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    static ADJUDICATION: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    static AUDIT: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    static SFT: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();

    let cell = match kind {
        SchemaKind::Task => &TASK,
        SchemaKind::PromptReviewV3 => &REVIEW,
        SchemaKind::PromptAdjudication => &ADJUDICATION,
        SchemaKind::AuditTrajectory => &AUDIT,
        SchemaKind::SftTrajectory => &SFT,
    };
    let result = cell.get_or_init(|| {
        let schema = schema_value(kind).map_err(|error| error.to_string())?;
        jsonschema::draft202012::new(&schema).map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map_err(|error| anyhow!("failed to compile {:?} schema: {error}", kind))
}

pub fn validate_instance(kind: SchemaKind, instance: &Value) -> Result<()> {
    compiled_validator(kind)?
        .validate(instance)
        .map_err(|error| anyhow!("{}: {}", error.instance_path(), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(source: &str) -> Value {
        serde_json::from_str(source).unwrap()
    }

    fn validate_uncached(kind: SchemaKind, instance: &Value) -> Result<()> {
        let schema = schema_value(kind)?;
        jsonschema::draft202012::validate(&schema, instance)
            .map_err(|error| anyhow!("{}: {}", error.instance_path(), error))
    }

    #[test]
    fn schemas_are_valid_draft_2020_12_and_accept_fixtures() {
        let cases = [
            (
                SchemaKind::Task,
                include_str!("../tests/fixtures/canonical/valid-task.json"),
            ),
            (
                SchemaKind::PromptReviewV3,
                include_str!("../tests/fixtures/canonical/valid-review-v3.json"),
            ),
            (
                SchemaKind::PromptAdjudication,
                include_str!("../tests/fixtures/canonical/valid-adjudication-v1.json"),
            ),
            (
                SchemaKind::AuditTrajectory,
                include_str!("../tests/fixtures/canonical/valid-audit.json"),
            ),
            (
                SchemaKind::SftTrajectory,
                include_str!("../tests/fixtures/canonical/valid-sft.json"),
            ),
        ];

        for (kind, source) in cases {
            let schema = schema_value(kind).unwrap();
            jsonschema::draft202012::meta::validate(&schema).unwrap();
            validate_instance(kind, &fixture(source)).unwrap();
        }
    }

    #[test]
    fn cached_validation_matches_original_for_all_schema_kinds() {
        let cases = [
            (
                SchemaKind::Task,
                include_str!("../tests/fixtures/canonical/valid-task.json"),
            ),
            (
                SchemaKind::PromptReviewV3,
                include_str!("../tests/fixtures/canonical/valid-review-v3.json"),
            ),
            (
                SchemaKind::PromptAdjudication,
                include_str!("../tests/fixtures/canonical/valid-adjudication-v1.json"),
            ),
            (
                SchemaKind::AuditTrajectory,
                include_str!("../tests/fixtures/canonical/valid-audit.json"),
            ),
            (
                SchemaKind::SftTrajectory,
                include_str!("../tests/fixtures/canonical/valid-sft.json"),
            ),
        ];

        for (kind, source) in cases {
            let valid = fixture(source);
            let uncached = validate_uncached(kind, &valid)
                .err()
                .map(|error| error.to_string());
            let cached = validate_instance(kind, &valid)
                .err()
                .map(|error| error.to_string());
            assert_eq!(uncached, cached, "valid result changed for {kind:?}");

            let invalid = Value::Null;
            let uncached = validate_uncached(kind, &invalid)
                .err()
                .map(|error| error.to_string());
            let cached = validate_instance(kind, &invalid)
                .err()
                .map(|error| error.to_string());
            assert_eq!(uncached, cached, "invalid result changed for {kind:?}");
        }
    }

    #[test]
    fn review_v3_rejects_unknown_as_accept_and_unproven_reject() {
        let mut review = fixture(include_str!(
            "../tests/fixtures/canonical/valid-review-v3.json"
        ));
        review["checks"]["technical_authenticity"]["status"] = serde_json::json!("unknown");
        assert!(validate_instance(SchemaKind::PromptReviewV3, &review).is_err());

        review["outcome"] = serde_json::json!("reject");
        review["checks"]["technical_authenticity"]["status"] = serde_json::json!("pass");
        assert!(validate_instance(SchemaKind::PromptReviewV3, &review).is_err());
    }

    #[test]
    fn schemas_reject_missing_coordinates_and_hidden_sft_fields() {
        let mut task = fixture(include_str!("../tests/fixtures/canonical/valid-task.json"));
        task.as_object_mut().unwrap().remove("coordinates");
        assert!(validate_instance(SchemaKind::Task, &task).is_err());

        let mut sft = fixture(include_str!("../tests/fixtures/canonical/valid-sft.json"));
        sft.as_object_mut()
            .unwrap()
            .insert("grader_output".into(), serde_json::json!({"reward": 1}));
        assert!(validate_instance(SchemaKind::SftTrajectory, &sft).is_err());
    }

    #[test]
    fn universal_task_schema_rejects_v1_vendor_coordinate_names() {
        let mut task = fixture(include_str!("../tests/fixtures/canonical/valid-task.json"));
        let coordinates = task["coordinates"].as_object_mut().unwrap();
        coordinates.insert("vendor_scope".into(), serde_json::json!("multi_vendor"));
        coordinates.insert(
            "vendors".into(),
            serde_json::json!(["cisco_ios_xe", "juniper_junos"]),
        );
        assert!(validate_instance(SchemaKind::Task, &task).is_err());
    }

    #[test]
    fn adjudication_reject_with_all_supported_cited_claims_is_rejected() {
        let mut value = fixture(include_str!(
            "../tests/fixtures/canonical/valid-adjudication-v1.json"
        ));
        value["outcome"] = serde_json::json!("reject");
        assert!(validate_instance(SchemaKind::PromptAdjudication, &value).is_err());
    }

    #[test]
    fn adjudication_reject_with_mixed_verdicts_remains_valid() {
        let mut value = fixture(include_str!(
            "../tests/fixtures/canonical/valid-adjudication-v1.json"
        ));
        value["outcome"] = serde_json::json!("reject");
        value["claims"][0]["verdict"] = serde_json::json!("unsupported");
        assert!(validate_instance(SchemaKind::PromptAdjudication, &value).is_ok());
    }

    #[test]
    #[ignore = "performance harness; run explicitly in release mode"]
    fn schema_validation_benchmark() {
        use std::time::Instant;

        let value = fixture(include_str!("../tests/fixtures/canonical/valid-task.json"));
        const ITERATIONS: usize = 20_000;

        let measure = |cached: bool| {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                let result = if cached {
                    validate_instance(SchemaKind::Task, std::hint::black_box(&value))
                } else {
                    validate_uncached(SchemaKind::Task, std::hint::black_box(&value))
                };
                assert!(result.is_ok());
                std::hint::black_box(result.is_ok());
            }
            started.elapsed().as_millis()
        };

        // Warm both paths before alternating their order to reduce one-time
        // allocator and instruction-cache effects in the paired samples.
        measure(false);
        measure(true);
        for sample in 0..5 {
            let uncached_first = sample % 2 == 0;
            let first = measure(!uncached_first);
            let second = measure(uncached_first);
            let (uncached_ms, cached_ms) = if uncached_first {
                (first, second)
            } else {
                (second, first)
            };
            eprintln!(
                "schema_validation_benchmark sample={sample} order={} uncached_ms={uncached_ms} cached_ms={cached_ms}",
                if uncached_first {
                    "uncached-first"
                } else {
                    "cached-first"
                }
            );
        }
    }
}
