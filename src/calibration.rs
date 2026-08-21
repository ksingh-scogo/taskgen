use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoldLabel {
    pub candidate_id: String,
    pub expected_outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservedLabel {
    pub candidate_id: String,
    pub outcome: String,
    pub adjudicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutcomeMetrics {
    pub precision: f64,
    pub recall: f64,
    pub true_positive: usize,
    pub predicted: usize,
    pub expected: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalibrationReport {
    pub labelled_records: usize,
    pub matched_records: usize,
    pub confusion: BTreeMap<String, BTreeMap<String, usize>>,
    pub per_outcome: BTreeMap<String, OutcomeMetrics>,
    pub false_accept_rate: f64,
    pub false_reject_rate: f64,
    pub invalid_response_rate: f64,
    pub adjudication_rate: f64,
}

pub fn load_gold(path: &Path) -> Result<Vec<GoldLabel>> {
    let mut labels = Vec::new();
    for (index, line) in BufReader::new(
        File::open(path)
            .with_context(|| format!("failed to open gold labels: {}", path.display()))?,
    )
    .lines()
    .enumerate()
    {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        labels.push(
            serde_json::from_str(&line).with_context(|| {
                format!("invalid gold label at {}:{}", path.display(), index + 1)
            })?,
        );
    }
    Ok(labels)
}

pub fn evaluate(gold: &[GoldLabel], observed: &[ObservedLabel]) -> CalibrationReport {
    let observed_by_id: HashMap<&str, &ObservedLabel> = observed
        .iter()
        .map(|label| (label.candidate_id.as_str(), label))
        .collect();
    let mut confusion: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut outcomes: BTreeSet<String> = ["accept", "revise", "reject", "needs_verification"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut matched = 0usize;
    let mut adjudicated = 0usize;
    let mut false_accepts = 0usize;
    let mut false_rejects = 0usize;
    let expected_accepts = gold
        .iter()
        .filter(|label| label.expected_outcome == "accept")
        .count();
    let expected_non_accepts = gold.len().saturating_sub(expected_accepts);

    for expected in gold {
        outcomes.insert(expected.expected_outcome.clone());
        let Some(actual) = observed_by_id.get(expected.candidate_id.as_str()) else {
            continue;
        };
        matched += 1;
        adjudicated += usize::from(actual.adjudicated);
        outcomes.insert(actual.outcome.clone());
        *confusion
            .entry(expected.expected_outcome.clone())
            .or_default()
            .entry(actual.outcome.clone())
            .or_default() += 1;
        if actual.outcome == "accept" && expected.expected_outcome != "accept" {
            false_accepts += 1;
        }
        if expected.expected_outcome == "accept" && actual.outcome != "accept" {
            false_rejects += 1;
        }
    }

    let mut per_outcome = BTreeMap::new();
    for outcome in outcomes {
        let true_positive = confusion
            .get(&outcome)
            .and_then(|row| row.get(&outcome))
            .copied()
            .unwrap_or(0);
        let expected_count = confusion
            .get(&outcome)
            .map(|row| row.values().sum())
            .unwrap_or(0);
        let predicted = confusion
            .values()
            .map(|row| row.get(&outcome).copied().unwrap_or(0))
            .sum();
        per_outcome.insert(
            outcome,
            OutcomeMetrics {
                precision: ratio(true_positive, predicted),
                recall: ratio(true_positive, expected_count),
                true_positive,
                predicted,
                expected: expected_count,
            },
        );
    }

    CalibrationReport {
        labelled_records: gold.len(),
        matched_records: matched,
        confusion,
        per_outcome,
        false_accept_rate: ratio(false_accepts, expected_non_accepts),
        false_reject_rate: ratio(false_rejects, expected_accepts),
        invalid_response_rate: ratio(gold.len().saturating_sub(matched), gold.len()),
        adjudication_rate: ratio(adjudicated, matched),
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_reports_confusion_and_false_reject_rate() {
        let gold = vec![
            GoldLabel {
                candidate_id: "a".into(),
                expected_outcome: "accept".into(),
            },
            GoldLabel {
                candidate_id: "b".into(),
                expected_outcome: "reject".into(),
            },
        ];
        let observed = vec![
            ObservedLabel {
                candidate_id: "a".into(),
                outcome: "reject".into(),
                adjudicated: false,
            },
            ObservedLabel {
                candidate_id: "b".into(),
                outcome: "reject".into(),
                adjudicated: false,
            },
        ];

        let report = evaluate(&gold, &observed);

        assert_eq!(report.confusion["accept"]["reject"], 1);
        assert_eq!(report.false_reject_rate, 1.0);
        assert_eq!(report.false_accept_rate, 0.0);
    }
}
