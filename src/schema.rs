use anyhow::{Result, anyhow};
use serde_json::Value;

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

pub fn validate_instance(kind: SchemaKind, instance: &Value) -> Result<()> {
    let schema = schema_value(kind)?;
    jsonschema::draft202012::validate(&schema, instance)
        .map_err(|error| anyhow!("{}: {}", error.instance_path(), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(source: &str) -> Value {
        serde_json::from_str(source).unwrap()
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
    fn adjudication_reject_with_all_supported_claims_should_be_rejected() {
        let mut v = fixture(include_str!(
            "../tests/fixtures/canonical/valid-adjudication-v1.json"
        ));
        v["outcome"] = serde_json::json!("reject");
        let res = validate_instance(SchemaKind::PromptAdjudication, &v);
        assert!(
            res.is_err(),
            "BUG: adjudication outcome=reject with all claims 'supported'+cited passed schema validation: {:?}",
            res
        );
    }

    #[test]
    fn adjudication_reject_with_mixed_verdict_claims_is_valid() {
        let mut v = fixture(include_str!(
            "../tests/fixtures/canonical/valid-adjudication-v1.json"
        ));
        v["outcome"] = serde_json::json!("reject");
        v["claims"][0]["verdict"] = serde_json::json!("unsupported");
        v["claims"].as_array_mut().unwrap().push(serde_json::json!({
            "claim_id": "claim-2",
            "verdict": "supported",
            "rationale": "A second claim is directly entailed by the cited evidence.",
            "citations": ["candidate:$.prompt"]
        }));
        assert!(validate_instance(SchemaKind::PromptAdjudication, &v).is_ok());
    }

    #[test]
    fn adjudication_reject_with_supported_uncited_claim_is_valid() {
        let mut v = fixture(include_str!(
            "../tests/fixtures/canonical/valid-adjudication-v1.json"
        ));
        v["outcome"] = serde_json::json!("reject");
        v["claims"][0]["citations"] = serde_json::json!([]);
        assert!(validate_instance(SchemaKind::PromptAdjudication, &v).is_ok());
    }
}
