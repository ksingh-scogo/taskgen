use anyhow::{Result, anyhow};
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub enum SchemaKind {
    Task,
    PromptReview,
    AuditTrajectory,
    SftTrajectory,
}

fn schema_source(kind: SchemaKind) -> &'static str {
    match kind {
        SchemaKind::Task => include_str!("../schemas/task-v2.schema.json"),
        SchemaKind::PromptReview => include_str!("../schemas/prompt-review-v1.schema.json"),
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
}
