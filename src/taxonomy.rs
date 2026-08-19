use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rand::Rng;
use rand::distributions::{Distribution, WeightedIndex};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: &str = "scogo.taskgen.taxonomy.v1";
const WEIGHT_TOLERANCE: f64 = 0.000_001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaxonomyKind {
    Hierarchical,
    Compositional,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCoordinates {
    pub taxonomy_id: String,
    pub task_family: String,
    pub environment: String,
    pub vendor_scope: String,
    pub vendors: Vec<String>,
    pub incident_mechanism: String,
    pub evidence_condition: String,
    pub evidence_bundle: String,
    pub action_risk: String,
    pub presentation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledTask {
    pub taxonomy_id: String,
    pub category_id: String,
    pub domain_id: String,
    pub domain_label: String,
    pub subdomain_id: String,
    pub coordinates: Option<TaskCoordinates>,
    pub difficulty: u8,
}

#[derive(Debug, Clone, Deserialize)]
struct RawTaxonomy {
    schema_version: String,
    id: String,
    kind: String,
    label: String,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    categories: Vec<RawCategory>,
    #[serde(default)]
    vendor_groups: Vec<RawVendorGroup>,
    axes: Option<RawAxes>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDefaults {
    system_prompt_file: Option<PathBuf>,
    #[serde(default)]
    difficulty_distribution: HashMap<u8, f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCategory {
    id: String,
    label: String,
    weight: f64,
    #[serde(default)]
    domains: Vec<RawHierarchicalDomain>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHierarchicalDomain {
    name: String,
    #[serde(default)]
    subdomains: Vec<RawSubdomain>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawSubdomain {
    Id(String),
    Definition {
        id: String,
        label: Option<String>,
        weight: Option<f64>,
    },
}

impl RawSubdomain {
    fn id(&self) -> &str {
        match self {
            Self::Id(id) | Self::Definition { id, .. } => id,
        }
    }

    fn weight(&self) -> f64 {
        match self {
            Self::Id(_) => 1.0,
            Self::Definition { weight, .. } => weight.unwrap_or(1.0),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawAxes {
    #[serde(default)]
    domains: Vec<RawCompositionalDomain>,
    #[serde(default)]
    task_families: Vec<RawWeightedOption>,
    #[serde(default)]
    environments: Vec<RawWeightedOption>,
    #[serde(default)]
    vendor_scopes: Vec<RawWeightedOption>,
    #[serde(default)]
    incident_mechanisms: Vec<RawWeightedOption>,
    #[serde(default)]
    evidence_conditions: Vec<RawWeightedOption>,
    #[serde(default)]
    evidence_bundles: Vec<RawWeightedOption>,
    #[serde(default)]
    action_risks: Vec<RawWeightedOption>,
    #[serde(default)]
    presentations: Vec<RawWeightedOption>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCompositionalDomain {
    id: String,
    label: String,
    weight: f64,
    #[serde(default)]
    vendor_groups: Vec<String>,
    #[serde(default)]
    environments: Vec<String>,
    #[serde(default)]
    incident_mechanisms: Vec<String>,
    #[serde(default)]
    evidence_bundles: Vec<String>,
    #[serde(default)]
    subdomains: Vec<RawSubdomain>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawWeightedOption {
    id: String,
    #[serde(default)]
    label: String,
    weight: f64,
    #[serde(default = "default_true")]
    enabled: bool,
    difficulty_min: Option<u8>,
    difficulty_max: Option<u8>,
    #[serde(default)]
    difficulty_multiplier: HashMap<u8, f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawVendorGroup {
    id: String,
    weight: f64,
    #[serde(default)]
    vendors: Vec<RawVendor>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawVendor {
    id: String,
    #[serde(default)]
    label: String,
    weight: f64,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct TaxonomyCatalog {
    source_path: Option<PathBuf>,
    raw: RawTaxonomy,
    kind: TaxonomyKind,
}

impl TaxonomyCatalog {
    pub fn from_yaml(source: &str, source_path: Option<&Path>) -> Result<Self> {
        let raw: RawTaxonomy = serde_yaml_ng::from_str(source).context("invalid taxonomy YAML")?;
        let kind = match raw.kind.as_str() {
            "hierarchical" => TaxonomyKind::Hierarchical,
            "compositional" => TaxonomyKind::Compositional,
            other => bail!("unsupported taxonomy kind '{other}'"),
        };
        let catalog = Self {
            source_path: source_path.map(Path::to_path_buf),
            raw,
            kind,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read taxonomy: {}", path.display()))?;
        Self::from_yaml(&source, Some(path))
            .with_context(|| format!("invalid taxonomy: {}", path.display()))
    }

    pub fn embedded_itops() -> Result<Self> {
        Self::from_yaml(include_str!("../docs/it-ops-taxonomy.yaml"), None)
    }

    pub fn validate(&self) -> Result<()> {
        if self.raw.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported taxonomy schema '{}'; expected {SCHEMA_VERSION}",
                self.raw.schema_version
            );
        }
        validate_nonempty_id("taxonomy", &self.raw.id)?;
        if self.raw.label.trim().is_empty() {
            bail!("taxonomy label must not be empty");
        }
        validate_difficulty(&self.raw.defaults.difficulty_distribution)?;

        match self.kind {
            TaxonomyKind::Hierarchical => self.validate_hierarchical(),
            TaxonomyKind::Compositional => self.validate_compositional(),
        }
    }

    fn validate_hierarchical(&self) -> Result<()> {
        if self.raw.categories.is_empty() {
            bail!("hierarchical taxonomy requires categories");
        }
        validate_unique_ids(
            "category",
            self.raw.categories.iter().map(|item| item.id.as_str()),
        )?;
        validate_complete_weights(
            "category distribution",
            self.raw
                .categories
                .iter()
                .map(|item| (item.id.as_str(), item.weight)),
        )?;
        for category in &self.raw.categories {
            if category.label.trim().is_empty() {
                bail!("category '{}' label must not be empty", category.id);
            }
            if category.domains.is_empty() {
                bail!(
                    "category '{}' must contain at least one domain",
                    category.id
                );
            }
            validate_unique_ids(
                &format!("domain in category '{}'", category.id),
                category.domains.iter().map(|item| item.name.as_str()),
            )?;
            for domain in &category.domains {
                if domain.subdomains.is_empty() {
                    bail!(
                        "domain '{}' in category '{}' must contain at least one subdomain",
                        domain.name,
                        category.id
                    );
                }
                validate_subdomains(&category.id, &domain.name, &domain.subdomains)?;
            }
        }
        Ok(())
    }

    fn validate_compositional(&self) -> Result<()> {
        if !self.raw.categories.is_empty() {
            bail!("compositional taxonomy must not define categories");
        }
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("compositional taxonomy requires axes")?;
        if axes.domains.is_empty() {
            bail!("compositional taxonomy requires axes.domains");
        }
        validate_unique_ids("domain", axes.domains.iter().map(|item| item.id.as_str()))?;
        validate_complete_weights(
            "domain distribution",
            axes.domains
                .iter()
                .map(|item| (item.id.as_str(), item.weight)),
        )?;

        let option_axes = [
            ("task_families", &axes.task_families),
            ("environments", &axes.environments),
            ("vendor_scopes", &axes.vendor_scopes),
            ("incident_mechanisms", &axes.incident_mechanisms),
            ("evidence_conditions", &axes.evidence_conditions),
            ("evidence_bundles", &axes.evidence_bundles),
            ("action_risks", &axes.action_risks),
            ("presentations", &axes.presentations),
        ];
        for (name, options) in option_axes {
            validate_options(name, options)?;
        }
        validate_unique_ids(
            "vendor group",
            self.raw.vendor_groups.iter().map(|item| item.id.as_str()),
        )?;
        for group in &self.raw.vendor_groups {
            validate_finite_weight(
                &format!("vendor group '{}': weight", group.id),
                group.weight,
            )?;
            if group.vendors.is_empty() {
                bail!("vendor group '{}' must contain vendors", group.id);
            }
            validate_unique_ids(
                &format!("vendor in group '{}'", group.id),
                group.vendors.iter().map(|item| item.id.as_str()),
            )?;
            for vendor in &group.vendors {
                if vendor.label.trim().is_empty() {
                    bail!(
                        "vendor '{}' in group '{}' label must not be empty",
                        vendor.id,
                        group.id
                    );
                }
            }
            validate_complete_weights(
                &format!("vendor distribution in group '{}'", group.id),
                group
                    .vendors
                    .iter()
                    .map(|item| (item.id.as_str(), item.weight)),
            )?;
        }

        let environment_ids = enabled_ids(&axes.environments);
        let mechanism_ids = enabled_ids(&axes.incident_mechanisms);
        let evidence_ids = enabled_ids(&axes.evidence_bundles);
        let vendor_group_ids: HashSet<&str> = self
            .raw
            .vendor_groups
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        for domain in &axes.domains {
            validate_nonempty_id("domain", &domain.id)?;
            if domain.label.trim().is_empty() {
                bail!("domain '{}' label must not be empty", domain.id);
            }
            validate_subdomains(&domain.id, &domain.label, &domain.subdomains)?;
            validate_references(
                &format!("domain '{}'.environments", domain.id),
                &domain.environments,
                &environment_ids,
            )?;
            validate_references(
                &format!("domain '{}'.incident_mechanisms", domain.id),
                &domain.incident_mechanisms,
                &mechanism_ids,
            )?;
            validate_references(
                &format!("domain '{}'.evidence_bundles", domain.id),
                &domain.evidence_bundles,
                &evidence_ids,
            )?;
            validate_references(
                &format!("domain '{}'.vendor_groups", domain.id),
                &domain.vendor_groups,
                &vendor_group_ids,
            )?;
            if domain.environments.is_empty()
                || domain.incident_mechanisms.is_empty()
                || domain.evidence_bundles.is_empty()
                || domain.vendor_groups.is_empty()
            {
                bail!("domain '{}' has an empty required allow-list", domain.id);
            }
            let distinct_vendors: HashSet<&str> = self
                .raw
                .vendor_groups
                .iter()
                .filter(|group| domain.vendor_groups.contains(&group.id))
                .flat_map(|group| group.vendors.iter().map(|vendor| vendor.id.as_str()))
                .collect();
            if distinct_vendors.len() < 2 {
                bail!(
                    "domain '{}' needs at least two distinct eligible vendors for multi_vendor sampling",
                    domain.id
                );
            }
        }
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.raw.id
    }

    pub fn kind(&self) -> TaxonomyKind {
        self.kind
    }

    pub fn default_system_prompt_path(&self) -> Option<PathBuf> {
        let configured = self.raw.defaults.system_prompt_file.as_ref()?;
        if configured.is_absolute() {
            return Some(configured.clone());
        }
        match self.source_path.as_deref().and_then(Path::parent) {
            Some(parent) => Some(parent.join(configured)),
            None => Some(configured.clone()),
        }
    }

    pub fn available_distribution_ids(&self) -> Vec<&str> {
        match self.kind {
            TaxonomyKind::Hierarchical => self
                .raw
                .categories
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
            TaxonomyKind::Compositional => self
                .raw
                .axes
                .as_ref()
                .map(|axes| axes.domains.iter().map(|item| item.id.as_str()).collect())
                .unwrap_or_default(),
        }
    }

    pub fn default_distribution(&self) -> HashMap<String, f64> {
        match self.kind {
            TaxonomyKind::Hierarchical => self
                .raw
                .categories
                .iter()
                .map(|item| (item.id.clone(), item.weight))
                .collect(),
            TaxonomyKind::Compositional => self
                .raw
                .axes
                .as_ref()
                .map(|axes| {
                    axes.domains
                        .iter()
                        .map(|item| (item.id.clone(), item.weight))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn default_difficulty(&self) -> HashMap<u8, f64> {
        self.raw.defaults.difficulty_distribution.clone()
    }

    pub fn domain_count(&self) -> usize {
        match self.kind {
            TaxonomyKind::Hierarchical => self
                .raw
                .categories
                .iter()
                .map(|category| category.domains.len())
                .sum(),
            TaxonomyKind::Compositional => self
                .raw
                .axes
                .as_ref()
                .map(|axes| axes.domains.len())
                .unwrap_or(0),
        }
    }

    pub fn contains_hierarchical_subdomain(
        &self,
        category_id: &str,
        domain_name: &str,
        subdomain_id: &str,
    ) -> bool {
        self.kind == TaxonomyKind::Hierarchical
            && self.raw.categories.iter().any(|category| {
                category.id == category_id
                    && category.domains.iter().any(|domain| {
                        domain.name == domain_name
                            && domain
                                .subdomains
                                .iter()
                                .any(|subdomain| subdomain.id() == subdomain_id)
                    })
            })
    }

    pub fn hierarchical_domain_count(&self, category_id: &str) -> usize {
        if self.kind != TaxonomyKind::Hierarchical {
            return 0;
        }
        self.raw
            .categories
            .iter()
            .find(|category| category.id == category_id)
            .map(|category| category.domains.len())
            .unwrap_or(0)
    }

    pub fn subdomain_count(&self) -> usize {
        match self.kind {
            TaxonomyKind::Hierarchical => self
                .raw
                .categories
                .iter()
                .flat_map(|category| &category.domains)
                .map(|domain| domain.subdomains.len())
                .sum(),
            TaxonomyKind::Compositional => self
                .raw
                .axes
                .as_ref()
                .map(|axes| {
                    axes.domains
                        .iter()
                        .map(|domain| domain.subdomains.len())
                        .sum()
                })
                .unwrap_or(0),
        }
    }

    pub fn default_domain_weight_sum(&self) -> f64 {
        self.default_distribution().values().sum()
    }

    pub fn sample_defaults<R: Rng + ?Sized>(&self, rng: &mut R) -> Result<SampledTask> {
        self.sample(
            rng,
            &self.default_distribution(),
            &self.default_difficulty(),
        )
    }

    pub fn sample<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        self.validate_sampling_distributions(distribution, difficulty)?;
        match self.kind {
            TaxonomyKind::Hierarchical => self.sample_hierarchical(rng, distribution, difficulty),
            TaxonomyKind::Compositional => self.sample_compositional(rng, distribution, difficulty),
        }
    }

    pub fn validate_sampling_distributions(
        &self,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<()> {
        validate_override_distribution(self, distribution)?;
        validate_difficulty(difficulty)
    }

    fn sample_hierarchical<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        let mut candidates = Vec::new();
        let mut weights = Vec::new();
        for category in &self.raw.categories {
            let category_weight = distribution[&category.id];
            let domain_weight = category_weight / category.domains.len() as f64;
            for domain in &category.domains {
                let subdomain_total: f64 = domain.subdomains.iter().map(RawSubdomain::weight).sum();
                for subdomain in &domain.subdomains {
                    candidates.push((category, domain, subdomain));
                    weights.push(domain_weight * subdomain.weight() / subdomain_total);
                }
            }
        }
        let selected = sample_index(rng, &weights, "hierarchical domain pool")?;
        let (category, domain, subdomain) = candidates[selected];
        Ok(SampledTask {
            taxonomy_id: self.raw.id.clone(),
            category_id: category.id.clone(),
            domain_id: slugify(&domain.name),
            domain_label: domain.name.clone(),
            subdomain_id: subdomain.id().to_string(),
            coordinates: None,
            difficulty: sample_difficulty(rng, difficulty, 1, 10, &HashMap::new())?,
        })
    }

    fn sample_compositional<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        distribution: &HashMap<String, f64>,
        difficulty: &HashMap<u8, f64>,
    ) -> Result<SampledTask> {
        let axes = self
            .raw
            .axes
            .as_ref()
            .context("missing compositional axes")?;
        let domain_weights: Vec<f64> = axes
            .domains
            .iter()
            .map(|item| distribution[&item.id])
            .collect();
        let domain = &axes.domains[sample_index(rng, &domain_weights, "domain distribution")?];
        let sub_weights: Vec<f64> = domain.subdomains.iter().map(RawSubdomain::weight).collect();
        let subdomain =
            &domain.subdomains[sample_index(rng, &sub_weights, "subdomain distribution")?];

        let task_family = sample_option(rng, &axes.task_families, None, "task family")?;
        let environment = sample_option(
            rng,
            &axes.environments,
            Some(&domain.environments),
            "environment",
        )?;
        let vendor_scope = sample_option(rng, &axes.vendor_scopes, None, "vendor scope")?;
        let mechanism = sample_option(
            rng,
            &axes.incident_mechanisms,
            Some(&domain.incident_mechanisms),
            "incident mechanism",
        )?;
        let evidence_condition =
            sample_option(rng, &axes.evidence_conditions, None, "evidence condition")?;
        let evidence_bundle = sample_option(
            rng,
            &axes.evidence_bundles,
            Some(&domain.evidence_bundles),
            "evidence bundle",
        )?;
        let action_risk = sample_option(rng, &axes.action_risks, None, "action risk")?;
        let presentation = sample_option(rng, &axes.presentations, None, "presentation")?;
        let vendors = self.sample_vendors(rng, domain, &vendor_scope.id)?;
        let min = task_family.difficulty_min.unwrap_or(1);
        let max = task_family.difficulty_max.unwrap_or(10);
        let selected_difficulty = sample_difficulty(
            rng,
            difficulty,
            min,
            max,
            &task_family.difficulty_multiplier,
        )?;

        Ok(SampledTask {
            taxonomy_id: self.raw.id.clone(),
            category_id: "enterprise_netops".to_string(),
            domain_id: domain.id.clone(),
            domain_label: domain.label.clone(),
            subdomain_id: subdomain.id().to_string(),
            coordinates: Some(TaskCoordinates {
                taxonomy_id: self.raw.id.clone(),
                task_family: task_family.id.clone(),
                environment: environment.id.clone(),
                vendor_scope: vendor_scope.id.clone(),
                vendors,
                incident_mechanism: mechanism.id.clone(),
                evidence_condition: evidence_condition.id.clone(),
                evidence_bundle: evidence_bundle.id.clone(),
                action_risk: action_risk.id.clone(),
                presentation: presentation.id.clone(),
            }),
            difficulty: selected_difficulty,
        })
    }

    fn sample_vendors<R: Rng + ?Sized>(
        &self,
        rng: &mut R,
        domain: &RawCompositionalDomain,
        scope: &str,
    ) -> Result<Vec<String>> {
        let count = match scope {
            "vendor_neutral" => return Ok(Vec::new()),
            "single_vendor" => 1,
            "multi_vendor" => 2,
            other => bail!("unsupported vendor scope '{other}'"),
        };
        let mut vendors = Vec::new();
        let mut weights = Vec::new();
        for group in &self.raw.vendor_groups {
            if !domain.vendor_groups.contains(&group.id) {
                continue;
            }
            for vendor in &group.vendors {
                if let Some(existing) = vendors.iter().position(|id: &String| id == &vendor.id) {
                    weights[existing] += group.weight * vendor.weight;
                } else {
                    vendors.push(vendor.id.clone());
                    weights.push(group.weight * vendor.weight);
                }
            }
        }
        if vendors.len() < count {
            bail!(
                "domain '{}' cannot satisfy vendor scope '{scope}'",
                domain.id
            );
        }
        let mut selected = Vec::new();
        for _ in 0..count {
            let index = sample_index(rng, &weights, "vendor distribution")?;
            selected.push(vendors.remove(index));
            weights.remove(index);
        }
        Ok(selected)
    }
}

fn validate_nonempty_id(kind: &str, id: &str) -> Result<()> {
    if id.trim().is_empty() {
        bail!("{kind} id must not be empty");
    }
    Ok(())
}

fn validate_unique_ids<'a>(kind: &str, ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        validate_nonempty_id(kind, id)?;
        if !seen.insert(id) {
            bail!("duplicate {kind} id '{id}'");
        }
    }
    Ok(())
}

fn validate_finite_weight(name: &str, weight: f64) -> Result<()> {
    if !weight.is_finite() || weight < 0.0 {
        bail!("{name} must be finite and non-negative, got {weight}");
    }
    Ok(())
}

fn validate_complete_weights<'a>(
    name: &str,
    weights: impl Iterator<Item = (&'a str, f64)>,
) -> Result<()> {
    let mut total = 0.0;
    let mut count = 0;
    for (id, weight) in weights {
        validate_finite_weight(&format!("{name} '{id}'"), weight)?;
        total += weight;
        count += 1;
    }
    if count == 0 {
        bail!("{name} must not be empty");
    }
    if (total - 1.0).abs() > WEIGHT_TOLERANCE {
        bail!("{name} weights must sum to 1.0, got {total}");
    }
    Ok(())
}

fn validate_difficulty(weights: &HashMap<u8, f64>) -> Result<()> {
    for level in weights.keys() {
        if !(1..=10).contains(level) {
            bail!("difficulty level must be 1-10, got {level}");
        }
    }
    validate_complete_weights(
        "difficulty distribution",
        weights
            .iter()
            .map(|(level, weight)| ("difficulty", *weight + (*level as f64 * 0.0))),
    )
}

fn validate_subdomains(category: &str, domain: &str, subdomains: &[RawSubdomain]) -> Result<()> {
    if subdomains.is_empty() {
        bail!("domain '{domain}' in '{category}' must contain subdomains");
    }
    validate_unique_ids(
        &format!("subdomain in '{category}/{domain}'"),
        subdomains.iter().map(RawSubdomain::id),
    )?;
    let mut total_weight = 0.0;
    for subdomain in subdomains {
        validate_finite_weight(
            &format!("subdomain '{category}/{domain}/{}' weight", subdomain.id()),
            subdomain.weight(),
        )?;
        total_weight += subdomain.weight();
        if let RawSubdomain::Definition {
            label: Some(label), ..
        } = subdomain
            && label.trim().is_empty()
        {
            bail!(
                "subdomain '{category}/{domain}/{}' label must not be empty",
                subdomain.id()
            );
        }
    }
    if total_weight <= 0.0 {
        bail!("domain '{category}/{domain}' must have a positive subdomain weight");
    }
    Ok(())
}

fn validate_options(name: &str, options: &[RawWeightedOption]) -> Result<()> {
    let enabled: Vec<&RawWeightedOption> = options.iter().filter(|item| item.enabled).collect();
    validate_unique_ids(name, options.iter().map(|item| item.id.as_str()))?;
    validate_complete_weights(
        name,
        enabled.iter().map(|item| (item.id.as_str(), item.weight)),
    )?;
    for option in options {
        if option.label.trim().is_empty() {
            bail!("{name} option '{}' label must not be empty", option.id);
        }
        if let Some(min) = option.difficulty_min
            && !(1..=10).contains(&min)
        {
            bail!("{name} option '{}' difficulty_min must be 1-10", option.id);
        }
        if let Some(max) = option.difficulty_max
            && !(1..=10).contains(&max)
        {
            bail!("{name} option '{}' difficulty_max must be 1-10", option.id);
        }
        if option.difficulty_min.unwrap_or(1) > option.difficulty_max.unwrap_or(10) {
            bail!(
                "{name} option '{}' has inverted difficulty bounds",
                option.id
            );
        }
        for (level, multiplier) in &option.difficulty_multiplier {
            if !(1..=10).contains(level) {
                bail!(
                    "{name} option '{}' has invalid difficulty multiplier level {level}",
                    option.id
                );
            }
            validate_finite_weight(
                &format!("{name} option '{}' difficulty multiplier", option.id),
                *multiplier,
            )?;
        }
    }
    Ok(())
}

fn enabled_ids(options: &[RawWeightedOption]) -> HashSet<&str> {
    options
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.id.as_str())
        .collect()
}

fn validate_references(name: &str, references: &[String], valid: &HashSet<&str>) -> Result<()> {
    let mut seen = HashSet::new();
    for reference in references {
        if !seen.insert(reference) {
            bail!("{name} contains duplicate reference '{reference}'");
        }
        if !valid.contains(reference.as_str()) {
            bail!("{name} references unknown id '{reference}'");
        }
    }
    Ok(())
}

fn validate_override_distribution(
    catalog: &TaxonomyCatalog,
    distribution: &HashMap<String, f64>,
) -> Result<()> {
    let valid: HashSet<&str> = catalog.available_distribution_ids().into_iter().collect();
    for id in distribution.keys() {
        if !valid.contains(id.as_str()) {
            bail!("distribution references unknown id '{id}'");
        }
    }
    validate_complete_weights(
        "distribution",
        distribution
            .iter()
            .map(|(id, weight)| (id.as_str(), *weight)),
    )
}

fn sample_index<R: Rng + ?Sized>(rng: &mut R, weights: &[f64], name: &str) -> Result<usize> {
    let distribution =
        WeightedIndex::new(weights).with_context(|| format!("invalid or empty {name}"))?;
    Ok(distribution.sample(rng))
}

fn sample_option<'a, R: Rng + ?Sized>(
    rng: &mut R,
    options: &'a [RawWeightedOption],
    allow: Option<&[String]>,
    name: &str,
) -> Result<&'a RawWeightedOption> {
    let eligible: Vec<&RawWeightedOption> = options
        .iter()
        .filter(|item| item.enabled)
        .filter(|item| allow.is_none_or(|allowed| allowed.contains(&item.id)))
        .collect();
    let weights: Vec<f64> = eligible.iter().map(|item| item.weight).collect();
    let index = sample_index(rng, &weights, name)?;
    Ok(eligible[index])
}

fn sample_difficulty<R: Rng + ?Sized>(
    rng: &mut R,
    distribution: &HashMap<u8, f64>,
    min: u8,
    max: u8,
    multipliers: &HashMap<u8, f64>,
) -> Result<u8> {
    let mut levels: Vec<u8> = distribution
        .keys()
        .copied()
        .filter(|level| *level >= min && *level <= max)
        .collect();
    levels.sort_unstable();
    let weights: Vec<f64> = levels
        .iter()
        .map(|level| distribution[level] * multipliers.get(level).copied().unwrap_or(1.0))
        .collect();
    let index = sample_index(rng, &weights, "difficulty distribution")?;
    Ok(levels[index])
}

fn slugify(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn loads_and_validates_embedded_itops_taxonomy() {
        let catalog =
            TaxonomyCatalog::from_yaml(include_str!("../docs/it-ops-taxonomy.yaml"), None)
                .expect("embedded IT Ops taxonomy should parse");

        assert_eq!(catalog.id(), "scogo-itops-v3");
        assert_eq!(catalog.kind(), TaxonomyKind::Hierarchical);
        catalog
            .validate()
            .expect("embedded taxonomy should validate");
    }

    #[test]
    fn rejects_distribution_that_does_not_sum_to_one() {
        let yaml = r#"
schema_version: scogo.taskgen.taxonomy.v1
id: invalid
kind: hierarchical
label: Invalid
defaults:
  difficulty_distribution: {1: 1.0}
categories:
  - id: only
    label: Only
    weight: 0.9
    domains:
      - name: Domain
        subdomains: [failure]
"#;

        let error = TaxonomyCatalog::from_yaml(yaml, None).unwrap_err();
        assert!(error.to_string().contains("sum to 1.0"), "{error:#}");
    }

    #[test]
    fn rejects_domain_with_no_positive_subdomain_weight() {
        let yaml = r#"
schema_version: scogo.taskgen.taxonomy.v1
id: invalid
kind: hierarchical
label: Invalid
defaults:
  difficulty_distribution: {1: 1.0}
categories:
  - id: only
    label: Only
    weight: 1.0
    domains:
      - name: Domain
        subdomains:
          - {id: disabled_a, weight: 0.0}
          - {id: disabled_b, weight: 0.0}
"#;

        let error = TaxonomyCatalog::from_yaml(yaml, None).unwrap_err();
        assert!(
            error.to_string().contains("positive subdomain weight"),
            "{error:#}"
        );
    }

    #[test]
    fn netops_taxonomy_has_exact_inventory_and_seeded_sampling() {
        let catalog = TaxonomyCatalog::from_path(Path::new("docs/netops-taxonomy.yaml"))
            .expect("checked-in NetOps taxonomy should parse");
        assert_eq!(catalog.id(), "scogo-enterprise-netops-v1");
        assert_eq!(catalog.kind(), TaxonomyKind::Compositional);
        assert_eq!(catalog.domain_count(), 25);
        assert_eq!(catalog.subdomain_count(), 531);
        assert!((catalog.default_domain_weight_sum() - 1.0).abs() < WEIGHT_TOLERANCE);

        let mut first = StdRng::seed_from_u64(42);
        let mut second = StdRng::seed_from_u64(42);
        assert_eq!(
            catalog.sample_defaults(&mut first).expect("first sample"),
            catalog.sample_defaults(&mut second).expect("second sample")
        );
    }

    #[test]
    fn netops_prompts_preserve_scope_and_harness_boundary() {
        let taskgen = include_str!("../prompts/netops-taskgen-system-v1.txt");
        let teacher = include_str!("../prompts/netops-teacher-system-v1.txt");
        for excluded in [
            "3GPP",
            "EPC",
            "5GC",
            "IMS",
            "OSS/BSS",
            "service-provider core",
        ] {
            assert!(taskgen.contains(excluded), "missing exclusion {excluded}");
        }
        assert!(taskgen.contains("operational behavior, not certification recall"));
        assert!(taskgen.contains("internally and causally consistent"));
        assert!(taskgen.contains("Never disguise pseudocode as captured production output"));
        assert!(taskgen.contains("Output only the final user task prompt."));
        assert!(teacher.contains("ATIF serialization"));
        assert!(teacher.contains("The harness, not you"));
    }
}
